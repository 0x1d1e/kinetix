//! Asynchronous, bounded usage-log queue (FR-6.4, NFR-1.7).
//!
//! Logging must never block or fail a client request. Each bounded queue item
//! contains one request row and all its attempt rows; saturation drops the
//! bundle rather than persisting partial accounting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::db::{self, Pool, UsageAccountingBundle};

#[derive(Clone)]
pub struct UsageLogQueue {
    tx: mpsc::Sender<UsageAccountingBundle>,
    dropped: Arc<AtomicU64>,
    depth: Arc<AtomicU64>,
    capacity: usize,
}

impl UsageLogQueue {
    pub fn new(pool: Pool, capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<UsageAccountingBundle>(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let depth = Arc::new(AtomicU64::new(0));

        let dropped_task = dropped.clone();
        let depth_task = depth.clone();
        tokio::spawn(async move {
            let mut batch: Vec<UsageAccountingBundle> = Vec::with_capacity(64);
            loop {
                // Drain whatever is available, then flush.
                let first = rx.recv().await;
                let Some(first) = first else { break };
                batch.push(first);
                while batch.len() < 128 {
                    match rx.try_recv() {
                        Ok(row) => batch.push(row),
                        Err(_) => break,
                    }
                }
                for bundle in batch.drain(..) {
                    depth_task.fetch_sub(1, Ordering::Relaxed);
                    if let Err(error) = db::insert_usage_bundle(&pool, &bundle).await {
                        tracing::warn!(%error, "failed to write usage accounting bundle");
                        dropped_task.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });

        UsageLogQueue {
            tx,
            dropped,
            depth,
            capacity,
        }
    }

    /// The bounded capacity, for queue-saturation alerting (Monitoring).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Enqueue a complete request accounting bundle. Never blocks; drops the
    /// request and all its attempts together when full.
    pub fn enqueue_bundle(&self, bundle: UsageAccountingBundle) {
        match self.tx.try_send(bundle) {
            Ok(()) => {
                self.depth.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn depth(&self) -> u64 {
        self.depth.load(Ordering::Relaxed)
    }
}
