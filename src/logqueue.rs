//! Asynchronous, bounded usage-log queue (FR-6.4, NFR-1.7).
//!
//! Logging must never block or fail a client request. Each bounded queue item
//! contains one request row and all its attempt rows; saturation drops the
//! bundle rather than persisting partial accounting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::db::{self, Pool, UsageAccountingBundle};

#[derive(Clone)]
pub struct UsageLogQueue {
    tx: mpsc::Sender<UsageAccountingBundle>,
    flush_tx: mpsc::Sender<oneshot::Sender<()>>,
    dropped: Arc<AtomicU64>,
    depth: Arc<AtomicU64>,
    capacity: usize,
}

async fn write_batch(
    pool: &Pool,
    batch: &mut Vec<UsageAccountingBundle>,
    depth: &AtomicU64,
    dropped: &AtomicU64,
) {
    for bundle in batch.drain(..) {
        depth.fetch_sub(1, Ordering::Relaxed);
        if let Err(error) = db::insert_usage_bundle(pool, &bundle).await {
            tracing::warn!(%error, "failed to write usage accounting bundle");
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl UsageLogQueue {
    pub fn new(pool: Pool, capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<UsageAccountingBundle>(capacity);
        let (flush_tx, mut flush_rx) = mpsc::channel::<oneshot::Sender<()>>(4);
        let dropped = Arc::new(AtomicU64::new(0));
        let depth = Arc::new(AtomicU64::new(0));

        let dropped_task = dropped.clone();
        let depth_task = depth.clone();
        tokio::spawn(async move {
            let mut batch: Vec<UsageAccountingBundle> = Vec::with_capacity(64);
            loop {
                tokio::select! {
                    first = rx.recv() => {
                        // Drain whatever is available, then write.
                        let Some(first) = first else { break };
                        batch.push(first);
                        while batch.len() < 128 {
                            match rx.try_recv() {
                                Ok(row) => batch.push(row),
                                Err(_) => break,
                            }
                        }
                        write_batch(&pool, &mut batch, &depth_task, &dropped_task).await;
                    }
                    Some(ack) = flush_rx.recv() => {
                        while let Ok(row) = rx.try_recv() {
                            batch.push(row);
                        }
                        write_batch(&pool, &mut batch, &depth_task, &dropped_task).await;
                        let _ = ack.send(());
                    }
                }
            }
        });

        UsageLogQueue {
            tx,
            flush_tx,
            dropped,
            depth,
            capacity,
        }
    }

    /// Write every bundle enqueued so far (graceful shutdown). Resolves once
    /// they are persisted, or immediately if the worker is gone.
    pub async fn flush(&self) {
        let (ack, done) = oneshot::channel();
        if self.flush_tx.send(ack).await.is_ok() {
            let _ = done.await;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Graceful shutdown relies on `flush` persisting every bundle enqueued
    /// before it, even when the worker has not been scheduled yet.
    #[tokio::test]
    async fn flush_persists_every_enqueued_bundle() {
        let root = std::env::temp_dir().join(format!(
            "kinetix-logqueue-flush-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pool = db::connect(&format!("sqlite://{}", root.join("k.db").display()))
            .await
            .unwrap();
        db::migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO virtual_keys (id, key_hash, name, owner, created_at, monthly_budget)
             VALUES ('usage-view-key', 'flush-hash', 'Flush key', 'test', '2026-01-01T00:00:00Z', 1.0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let queue = UsageLogQueue::new(pool.clone(), 16);
        for n in 0..5 {
            let mut request = db::usage_request_log_tests::request_row(
                &format!("flush-{n}"),
                "2026-01-02T10:00:00Z",
                "success",
                200,
                1,
                1,
                0.0,
                0,
                "acct",
            );
            request.request_id = format!("flush-request-{n}");
            queue.enqueue_bundle(UsageAccountingBundle {
                request,
                attempts: Vec::new(),
            });
        }
        queue.flush().await;

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_logs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 5);
        assert_eq!(queue.depth(), 0);
        let _ = std::fs::remove_dir_all(root);
    }
}
