//! Per-integration idle-contract instrumentation and demand tracking (#199).
//!
//! An integration is a Provider (`provider:<id>`) or a plugin
//! (`plugin:<id>`). Background schedulers record every unit of
//! integration-specific work here, and only schedule demand-driven work
//! (health probes, cached routing-fact refresh) for integrations a request
//! recently used. An idle integration therefore costs no network calls, probes,
//! or WASM instantiations; see `docs/guarantees.md`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::time::Instant;

/// How long a request keeps an integration "demanded" for demand-driven
/// background work.
pub const DEMAND_WINDOW: Duration = Duration::from_secs(15 * 60);

/// One kind of integration-specific work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationWork {
    /// A scheduler evaluated whether this integration has due work.
    Poll,
    /// A scheduler woke specifically to run work for this integration.
    Wake,
    /// Model discovery / reconciliation ran.
    Discovery,
    /// A health probe ran.
    Probe,
    /// A scheduled credential refresh ran.
    CredentialRefresh,
}

#[derive(Default)]
struct Counters {
    poll: AtomicU64,
    wake: AtomicU64,
    discovery: AtomicU64,
    probe: AtomicU64,
    credential_refresh: AtomicU64,
}

/// Point-in-time counters for one integration. WASM instantiations are owned
/// by the plugin runtime and merged in by the metrics surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrationCounters {
    pub integration: String,
    pub poll_count: u64,
    pub wake_count: u64,
    pub discovery_runs: u64,
    pub probe_runs: u64,
    pub credential_refresh_runs: u64,
}

#[derive(Clone, Default)]
pub struct IntegrationActivity {
    counters: Arc<DashMap<String, Arc<Counters>>>,
    demand: Arc<DashMap<String, Instant>>,
}

pub fn provider_key(provider_id: &str) -> String {
    format!("provider:{provider_id}")
}

pub fn plugin_key(plugin_id: &str) -> String {
    format!("plugin:{plugin_id}")
}

impl IntegrationActivity {
    pub fn record(&self, integration: &str, work: IntegrationWork) {
        let counters = match self.counters.get(integration) {
            Some(counters) => counters.clone(),
            None => self
                .counters
                .entry(integration.to_owned())
                .or_default()
                .clone(),
        };
        let counter = match work {
            IntegrationWork::Poll => &counters.poll,
            IntegrationWork::Wake => &counters.wake,
            IntegrationWork::Discovery => &counters.discovery,
            IntegrationWork::Probe => &counters.probe,
            IntegrationWork::CredentialRefresh => &counters.credential_refresh,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a request used this integration.
    pub fn note_demand(&self, integration: &str) {
        let now = Instant::now();
        if let Some(mut at) = self.demand.get_mut(integration) {
            *at = now;
            return;
        }
        self.demand.insert(integration.to_owned(), now);
    }

    /// Whether a request used this integration within `window`.
    pub fn demanded_within(&self, integration: &str, window: Duration) -> bool {
        self.demand
            .get(integration)
            .is_some_and(|at| at.elapsed() <= window)
    }

    /// Integrations demanded within `window`, sorted. Older entries are pruned
    /// so the demand map stays bounded by recently used integrations.
    pub fn demanded(&self, window: Duration) -> Vec<String> {
        self.demand.retain(|_, at| at.elapsed() <= window);
        let mut keys: Vec<String> = self.demand.iter().map(|e| e.key().clone()).collect();
        keys.sort();
        keys
    }

    pub fn snapshot(&self) -> Vec<IntegrationCounters> {
        let mut rows: Vec<IntegrationCounters> = self
            .counters
            .iter()
            .map(|entry| {
                let c = entry.value();
                IntegrationCounters {
                    integration: entry.key().clone(),
                    poll_count: c.poll.load(Ordering::Relaxed),
                    wake_count: c.wake.load(Ordering::Relaxed),
                    discovery_runs: c.discovery.load(Ordering::Relaxed),
                    probe_runs: c.probe.load(Ordering::Relaxed),
                    credential_refresh_runs: c.credential_refresh.load(Ordering::Relaxed),
                }
            })
            .collect();
        rows.sort_by(|a, b| a.integration.cmp(&b.integration));
        rows
    }

    pub fn get(&self, integration: &str) -> IntegrationCounters {
        self.snapshot()
            .into_iter()
            .find(|row| row.integration == integration)
            .unwrap_or_else(|| IntegrationCounters {
                integration: integration.to_owned(),
                ..Default::default()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn demand_expires_after_window() {
        let activity = IntegrationActivity::default();
        activity.note_demand("provider:a");
        assert!(activity.demanded_within("provider:a", DEMAND_WINDOW));
        assert_eq!(activity.demanded(DEMAND_WINDOW), vec!["provider:a"]);
        tokio::time::advance(DEMAND_WINDOW + Duration::from_secs(1)).await;
        assert!(!activity.demanded_within("provider:a", DEMAND_WINDOW));
        assert!(activity.demanded(DEMAND_WINDOW).is_empty());
    }

    #[test]
    fn counters_are_per_integration() {
        let activity = IntegrationActivity::default();
        activity.record("provider:a", IntegrationWork::Probe);
        activity.record("provider:a", IntegrationWork::Probe);
        activity.record("plugin:p", IntegrationWork::Poll);
        assert_eq!(activity.get("provider:a").probe_runs, 2);
        assert_eq!(activity.get("plugin:p").poll_count, 1);
        assert_eq!(
            activity.get("provider:z"),
            IntegrationCounters {
                integration: "provider:z".into(),
                ..Default::default()
            }
        );
    }
}
