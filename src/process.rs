//! Process-owned lifecycle for background work and detached flights (#199,
//! #205).
//!
//! Every task the process starts outside a request handler is spawned through
//! [`ProcessLifecycle`] so graceful shutdown can account for it:
//!
//! - **Background loops** (credential refresh, registry reload, probes, ...)
//!   observe [`ProcessLifecycle::next_tick`] and stop scheduling new
//!   iterations once shutdown begins; an iteration already running finishes so
//!   it never leaves a half-applied write.
//! - **Flights** (plugin hook jobs, coalesced provider work) are one-shot
//!   futures. New hook flights are refused once shutdown begins.
//!
//! Both are tracked. Shutdown waits for them until the grace deadline, then
//! fires the abort token, which drops every remaining tracked future (and
//! with it any plugin invocation or upstream call it owns) and also cancels
//! client requests still in flight.
//!
//! Requests may start owned and tracked flights while they drain, so the
//! flight set is sealed only by [`ProcessLifecycle::drain_flights`], after
//! request handlers are quiescent. Closing a `TaskTracker` does not stop
//! spawns, and its `wait` resolves whenever a closed tracker is momentarily
//! empty; sealing earlier would let a late flight escape the drain.

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::time::{Instant, Interval};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How long aborted work is given to unwind after the abort token fires.
pub const ABORT_UNWIND: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
pub struct ProcessLifecycle {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    stopping: CancellationToken,
    aborting: CancellationToken,
    background: TaskTracker,
    flights: TaskTracker,
    /// Set by [`ProcessLifecycle::drain_flights`]. Spawns hold the read lock
    /// across the check and the spawn so none can slip past the seal.
    flights_sealed: RwLock<bool>,
}

/// How the tracked work ended during [`ProcessLifecycle::drain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Every tracked task finished before the deadline.
    Drained,
    /// The deadline passed; remaining tasks were aborted.
    Aborted,
}

impl ProcessLifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether shutdown has begun. No new background work may start after.
    pub fn is_stopping(&self) -> bool {
        self.inner.stopping.is_cancelled()
    }

    /// Resolves once shutdown has begun.
    pub async fn stopping(&self) {
        self.inner.stopping.cancelled().await
    }

    /// Token fired at the drain deadline. Client connections observe it as a
    /// disconnect so their upstream calls are cancelled.
    pub fn abort_token(&self) -> CancellationToken {
        self.inner.aborting.clone()
    }

    /// Spawn a long-lived background loop. Loops must use [`Self::next_tick`]
    /// (or [`Self::sleep`]) so they stop scheduling work once shutdown begins.
    /// Returns `false` without spawning when shutdown already began.
    pub fn spawn_background<F>(&self, task: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.is_stopping() {
            return false;
        }
        let aborting = self.inner.aborting.clone();
        self.inner.background.spawn(async move {
            tokio::select! {
                biased;
                _ = aborting.cancelled() => {}
                _ = task => {}
            }
        });
        true
    }

    /// Spawn a one-shot flight that shutdown drains, then aborts at the
    /// deadline. Returns `false` without spawning when shutdown already began.
    pub fn spawn_flight<F>(&self, task: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.is_stopping() {
            return false;
        }
        self.spawn_owned(task);
        true
    }

    /// Spawn a flight a caller is already awaiting (for example coalesced
    /// provider work an in-flight request depends on). It is accepted during
    /// the drain so draining requests can finish, and aborted at the deadline.
    /// Once the flight set is sealed the task is dropped unrun.
    pub fn spawn_owned<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let aborting = self.inner.aborting.clone();
        self.spawn_unsealed(async move {
            tokio::select! {
                biased;
                _ = aborting.cancelled() => {}
                _ = task => {}
            }
        });
    }

    /// Spawn a flight that must observe [`Self::abort_token`] itself so it can
    /// finish cleanly (for example finalize accounting) when aborted. It is
    /// tracked by the drain but not dropped at the deadline. Once the flight
    /// set is sealed the task is dropped unrun.
    pub fn spawn_tracked<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_unsealed(task);
    }

    fn spawn_unsealed<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let sealed = self
            .inner
            .flights_sealed
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *sealed {
            tracing::warn!("flight spawned after shutdown sealed flights; dropping it");
            return;
        }
        self.inner.flights.spawn(task);
    }

    /// Wait for the next interval tick. Returns `false` once shutdown begins,
    /// in which case the loop must exit without starting another iteration.
    pub async fn next_tick(&self, interval: &mut Interval) -> bool {
        if self.is_stopping() {
            return false;
        }
        tokio::select! {
            biased;
            _ = self.inner.stopping.cancelled() => false,
            _ = interval.tick() => true,
        }
    }

    /// Sleep until `deadline`, or return `false` early once shutdown begins.
    pub async fn sleep_until(&self, deadline: Instant) -> bool {
        if self.is_stopping() {
            return false;
        }
        tokio::select! {
            biased;
            _ = self.inner.stopping.cancelled() => false,
            _ = tokio::time::sleep_until(deadline) => true,
        }
    }

    /// Sleep for `duration`, or return `false` early once shutdown begins.
    pub async fn sleep(&self, duration: Duration) -> bool {
        self.sleep_until(Instant::now() + duration).await
    }

    /// Stop scheduling: background loops exit at their next tick and new hook
    /// flights are refused. Owned and tracked flights stay accepted until
    /// [`Self::drain_flights`] seals them.
    pub fn begin_shutdown(&self) {
        self.inner.stopping.cancel();
        self.inner.background.close();
    }

    /// Fire the abort token immediately, cancelling in-flight client requests
    /// and every tracked task.
    pub fn abort(&self) {
        self.inner.aborting.cancel();
    }

    /// Number of tracked tasks still running.
    pub fn tracked_len(&self) -> usize {
        self.inner.background.len() + self.inner.flights.len()
    }

    /// Drain background loops, then seal and drain flights. Only for callers
    /// with no request handlers left that could spawn flights; a server
    /// drains requests alongside [`Self::drain_background`] and calls
    /// [`Self::drain_flights`] afterwards.
    pub async fn drain(&self, deadline: Instant) -> DrainOutcome {
        let background = self.drain_background(deadline).await;
        let flights = self.drain_flights(deadline).await;
        if background == DrainOutcome::Aborted || flights == DrainOutcome::Aborted {
            DrainOutcome::Aborted
        } else {
            DrainOutcome::Drained
        }
    }

    /// Wait for background loops until `deadline`, then abort. Call after
    /// [`Self::begin_shutdown`].
    pub async fn drain_background(&self, deadline: Instant) -> DrainOutcome {
        self.wait_or_abort(&self.inner.background, deadline).await
    }

    /// Seal the flight set, then wait for it until `deadline` and abort the
    /// rest. Call once nothing can start owned or tracked flights anymore,
    /// i.e. after request handlers finished.
    pub async fn drain_flights(&self, deadline: Instant) -> DrainOutcome {
        *self
            .inner
            .flights_sealed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.inner.flights.close();
        self.wait_or_abort(&self.inner.flights, deadline).await
    }

    /// Wait for `tracker` until `deadline`, then abort the rest and give it
    /// [`ABORT_UNWIND`] to unwind. `tracker` must already be closed.
    async fn wait_or_abort(&self, tracker: &TaskTracker, deadline: Instant) -> DrainOutcome {
        if tokio::time::timeout_at(deadline, tracker.wait())
            .await
            .is_ok()
        {
            return DrainOutcome::Drained;
        }
        self.abort();
        if tokio::time::timeout(ABORT_UNWIND, tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                remaining = tracker.len(),
                "tracked work did not unwind after abort"
            );
        }
        DrainOutcome::Aborted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn background_loop_finishes_iteration_and_stops_scheduling() {
        let lifecycle = ProcessLifecycle::new();
        let iterations = Arc::new(AtomicUsize::new(0));
        let completed_write = Arc::new(AtomicBool::new(false));
        {
            let lifecycle_loop = lifecycle.clone();
            let iterations = iterations.clone();
            let completed_write = completed_write.clone();
            assert!(lifecycle.spawn_background(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                while lifecycle_loop.next_tick(&mut tick).await {
                    iterations.fetch_add(1, Ordering::SeqCst);
                    // A multi-step write must not be cut in half by stopping.
                    completed_write.store(false, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    completed_write.store(true, Ordering::SeqCst);
                }
            }));
        }
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        lifecycle.begin_shutdown();
        let before = iterations.load(Ordering::SeqCst);
        assert_eq!(
            lifecycle
                .drain(Instant::now() + Duration::from_secs(5))
                .await,
            DrainOutcome::Drained
        );
        assert!(completed_write.load(Ordering::SeqCst));
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(iterations.load(Ordering::SeqCst), before);
        assert!(!lifecycle.spawn_background(async {}));
        assert!(!lifecycle.spawn_flight(async {}));
        assert_eq!(lifecycle.tracked_len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn drain_aborts_flights_past_the_deadline() {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let lifecycle = ProcessLifecycle::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        assert!(lifecycle.spawn_flight(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        }));
        lifecycle.begin_shutdown();
        let outcome = lifecycle
            .drain(Instant::now() + Duration::from_millis(100))
            .await;
        assert_eq!(outcome, DrainOutcome::Aborted);
        assert!(
            dropped.load(Ordering::SeqCst),
            "aborted flight must be dropped"
        );
        assert!(lifecycle.abort_token().is_cancelled());
        assert_eq!(lifecycle.tracked_len(), 0);
    }

    #[tokio::test]
    async fn owned_flights_are_accepted_during_drain() {
        let lifecycle = ProcessLifecycle::new();
        lifecycle.begin_shutdown();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_flag = ran.clone();
        lifecycle.spawn_owned(async move {
            ran_flag.store(true, Ordering::SeqCst);
        });
        assert_eq!(
            lifecycle
                .drain(Instant::now() + Duration::from_secs(1))
                .await,
            DrainOutcome::Drained
        );
        assert!(ran.load(Ordering::SeqCst));
    }

    /// A request still draining may start an owned flight after background
    /// work already drained. The flight drain must still wait for it, and a
    /// flight spawned after the seal must not run untracked.
    #[tokio::test(start_paused = true)]
    async fn owned_flight_spawned_after_empty_drain_is_still_tracked() {
        let lifecycle = ProcessLifecycle::new();
        lifecycle.begin_shutdown();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(
            lifecycle.drain_background(deadline).await,
            DrainOutcome::Drained
        );

        let finished = Arc::new(AtomicBool::new(false));
        let finished_flag = finished.clone();
        lifecycle.spawn_owned(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            finished_flag.store(true, Ordering::SeqCst);
        });
        assert_eq!(
            lifecycle.drain_flights(deadline).await,
            DrainOutcome::Drained
        );
        assert!(finished.load(Ordering::SeqCst));

        let late = Arc::new(AtomicBool::new(false));
        let late_flag = late.clone();
        lifecycle.spawn_owned(async move {
            late_flag.store(true, Ordering::SeqCst);
        });
        tokio::task::yield_now().await;
        assert!(!late.load(Ordering::SeqCst));
        assert_eq!(lifecycle.tracked_len(), 0);
    }
}
