//! Client connection-close monitoring for canceling uncommitted inference work.

use std::{fmt, io, sync::Arc, time::Instant};

use parking_lot::Mutex;
use tokio::{
    net::TcpStream,
    sync::watch,
    time::{sleep, Duration},
};
use tokio_util::sync::CancellationToken;

struct Inner {
    disconnected_at: watch::Sender<Option<Instant>>,
    connection_closed: watch::Sender<bool>,
    monitor: Mutex<Option<TcpStream>>,
    /// Process abort token. Firing it (graceful-shutdown deadline) cancels
    /// uncommitted work exactly like a client disconnect.
    abort: CancellationToken,
}

/// Cancellation signal for work that has not committed a response yet.
#[derive(Clone)]
pub struct ClientDisconnect {
    inner: Arc<Inner>,
}

impl fmt::Debug for ClientDisconnect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientDisconnect")
            .field(
                "disconnected",
                &self.inner.disconnected_at.borrow().is_some(),
            )
            .finish()
    }
}

impl ClientDisconnect {
    pub(crate) fn new(monitor: TcpStream, abort: CancellationToken) -> Self {
        let (disconnected_at, _) = watch::channel(None);
        let (connection_closed, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                disconnected_at,
                connection_closed,
                monitor: Mutex::new(Some(monitor)),
                abort,
            }),
        }
    }

    pub(crate) fn unmonitored() -> Self {
        let (disconnected_at, _) = watch::channel(None);
        let (connection_closed, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                disconnected_at,
                connection_closed,
                monitor: Mutex::new(None),
                abort: CancellationToken::new(),
            }),
        }
    }

    /// Start monitoring the connection once request-body extraction is done.
    pub fn start_monitor(&self) {
        let Some(stream) = self.inner.monitor.lock().take() else {
            return;
        };
        let disconnect = self.clone();
        let mut connection_closed = self.inner.connection_closed.subscribe();
        tokio::spawn(async move {
            let mut byte = [0_u8; 1];
            loop {
                let peeked = tokio::select! {
                    _ = connection_closed.changed() => return,
                    result = stream.peek(&mut byte) => result,
                };
                match peeked {
                    Ok(0) => {
                        disconnect.signal();
                        return;
                    }
                    Ok(_) => {
                        // The HTTP server owns reads from the original socket.
                        // Let it consume any pending request/pipelined bytes
                        // before peeking again on this shared socket.
                        tokio::select! {
                            _ = connection_closed.changed() => return,
                            _ = sleep(Duration::from_millis(10)) => {}
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::select! {
                            _ = connection_closed.changed() => return,
                            _ = sleep(Duration::from_millis(10)) => {}
                        }
                    }
                    Err(_) => {
                        disconnect.signal();
                        return;
                    }
                }
            }
        });
    }

    /// Wait until the peer has closed or reset its connection, or the process
    /// abort token fired.
    pub async fn cancelled(&self) {
        let mut disconnected_at = self.inner.disconnected_at.subscribe();
        if disconnected_at.borrow().is_some() {
            return;
        }
        tokio::select! {
            _ = disconnected_at.changed() => {}
            _ = self.inner.abort.cancelled() => self.signal(),
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        if self.inner.abort.is_cancelled() {
            self.signal();
        }
        self.inner.disconnected_at.borrow().is_some()
    }

    pub fn cancellation_latency_ms(&self) -> u64 {
        self.inner
            .disconnected_at
            .borrow()
            .as_ref()
            .map(|at| at.elapsed().as_millis() as u64)
            .unwrap_or(0)
    }

    pub(crate) fn connection_closed(&self) {
        self.signal();
        self.inner.connection_closed.send_replace(true);
    }

    fn signal(&self) {
        self.inner.disconnected_at.send_if_modified(|at| {
            if at.is_some() {
                false
            } else {
                *at = Some(Instant::now());
                true
            }
        });
    }
}
