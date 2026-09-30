//! Per-IP abuse rate limiting (NFR-3.6).
//!
//! A bounded in-memory fixed-window limiter keyed by client IP, applied at the
//! very start of the public `/v1` handlers (before virtual-key auth) so an
//! unauthenticated flood is rejected cheaply. It is deliberately coarse: its
//! job is abuse protection, not fair-share scheduling (per-key RPM/TPM and
//! budgets are the precise limits).
//!
//! Bounded memory (NFR-1.3): the map is swept of stale windows once it grows
//! past a cap, so a spoofed-IP flood cannot grow it without bound. The limiter
//! never blocks: a poisoned/slow path is impossible because it only touches a
//! DashMap.
//!
//! Because Kinetix trusts Cloudflare as its sole ingress (NFR-3.1), the client
//! IP is taken from the same trusted headers as the IP allowlist
//! ([`crate::limits::client_ip`]). When no client IP can be determined the
//! limiter fails **open** (a request is allowed) rather than locking everyone
//! out.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Hard cap on tracked IPs; beyond this, stale windows are evicted.
const MAX_ENTRIES: usize = 100_000;

#[derive(Clone)]
pub struct IpLimiter {
    /// Requests allowed per 60-second window per IP; 0 disables the limiter.
    limit: u64,
    window: Duration,
    map: std::sync::Arc<DashMap<String, Window>>,
    limited: std::sync::Arc<AtomicU64>,
}

#[derive(Clone, Copy)]
struct Window {
    started: Instant,
    count: u64,
}

impl IpLimiter {
    pub fn new(limit_per_min: u64) -> Self {
        Self {
            limit: limit_per_min,
            window: Duration::from_secs(60),
            map: std::sync::Arc::new(DashMap::new()),
            limited: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.limit > 0
    }

    /// Total requests rejected by the limiter (metric counter).
    pub fn limited_total(&self) -> u64 {
        self.limited.load(Ordering::Relaxed)
    }

    /// Record a request from `ip`. Returns `Err(retry_after_secs)` when the IP
    /// has exceeded the window.
    pub fn check(&self, ip: Option<std::net::IpAddr>) -> Result<(), u64> {
        if !self.enabled() {
            return Ok(());
        }
        let Some(ip) = ip else {
            // No trusted client IP: fail open rather than block everyone.
            return Ok(());
        };
        let now = Instant::now();
        if self.map.len() > MAX_ENTRIES {
            self.sweep(now);
        }
        let mut entry = self.map.entry(ip.to_string()).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(entry.started) >= self.window {
            entry.started = now;
            entry.count = 0;
        }
        entry.count += 1;
        if entry.count > self.limit {
            let elapsed = now.duration_since(entry.started).as_secs();
            let retry = self.window.as_secs().saturating_sub(elapsed).max(1);
            self.limited.fetch_add(1, Ordering::Relaxed);
            return Err(retry);
        }
        Ok(())
    }

    /// Drop windows that have fully expired (bounded memory).
    fn sweep(&self, now: Instant) {
        self.map
            .retain(|_, w| now.duration_since(w.started) < self.window * 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_rejects() {
        let l = IpLimiter::new(3);
        for _ in 0..3 {
            assert!(l.check(Some("1.2.3.4".parse().unwrap())).is_ok());
        }
        let err = l.check(Some("1.2.3.4".parse().unwrap())).unwrap_err();
        assert!((1..=60).contains(&err));
        // A different IP is unaffected.
        assert!(l.check(Some("5.6.7.8".parse().unwrap())).is_ok());
        assert_eq!(l.limited_total(), 1);
    }

    #[test]
    fn disabled_when_limit_is_zero_and_fails_open_without_ip() {
        assert!(IpLimiter::new(0)
            .check(Some("1.2.3.4".parse().unwrap()))
            .is_ok());
        let l = IpLimiter::new(1);
        assert!(l.check(None).is_ok());
    }
}
