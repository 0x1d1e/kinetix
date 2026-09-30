//! Provider-neutral quota evidence used by adaptive routing.
//!
//! Quota is an optional routing signal, never a synthetic health score. Missing
//! or stale evidence stays unknown.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use reqwest::header::HeaderMap;
use serde::Serialize;

const DEFAULT_MAX_AGE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize)]
pub struct QuotaSnapshot {
    pub remaining_fraction: Option<f64>,
    pub reset_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub source: String,
    pub max_age_secs: u64,
}

impl QuotaSnapshot {
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        if self.reset_at.is_some_and(|reset_at| reset_at <= now) {
            return false;
        }
        now.signed_duration_since(self.observed_at)
            .to_std()
            .map(|age| age <= Duration::from_secs(self.max_age_secs))
            .unwrap_or(true)
    }

    /// Unknown quota is neutral. Known headroom above 50% is positive evidence;
    /// known low headroom is negative evidence. A near reset can only make a
    /// small bounded adjustment.
    pub fn preference(&self, now: DateTime<Utc>) -> f64 {
        if !self.is_fresh(now) {
            return 0.0;
        }
        let Some(remaining) = self.remaining_fraction else {
            return 0.0;
        };
        let mut score = remaining.clamp(0.0, 1.0) - 0.5;
        if score < 0.0 {
            if let Some(reset_at) = self.reset_at {
                let secs = (reset_at - now).num_seconds().max(0);
                let reset_bonus = if secs <= 60 {
                    0.10
                } else if secs <= 5 * 60 {
                    0.05
                } else {
                    0.0
                };
                score += reset_bonus;
            }
        }
        score.clamp(-0.5, 0.5)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QuotaHeaderObservation {
    pub remaining_fraction: f64,
    pub reset_at: Option<DateTime<Utc>>,
}

impl QuotaHeaderObservation {
    pub fn retry_after_secs(self, now: DateTime<Utc>) -> Option<u64> {
        let millis = (self.reset_at? - now).num_milliseconds();
        (millis > 0).then(|| (millis as u64).div_ceil(1000))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QuotaPluginObservation {
    pub remaining_fraction: Option<f64>,
    pub reset_at: Option<DateTime<Utc>>,
    pub exhausted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "model_id")]
pub enum PluginQuotaScope {
    Account,
    Model(String),
    Unknown,
}

/// A validated quota bucket reported by an optional plugin health-v2 probe.
#[derive(Debug, Clone, Serialize)]
pub struct PluginQuotaSnapshot {
    pub scope: PluginQuotaScope,
    pub group: Option<String>,
    pub bucket_id: Option<String>,
    pub remaining_fraction: Option<f64>,
    pub remaining: Option<f64>,
    pub limit: Option<f64>,
    pub unit: Option<String>,
    pub window: Option<String>,
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginQuotaEvidence {
    pub snapshots: Vec<PluginQuotaSnapshot>,
    pub observed_at: DateTime<Utc>,
    pub max_age_secs: u64,
}

impl PluginQuotaEvidence {
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        now.signed_duration_since(self.observed_at)
            .to_std()
            .map(|age| age <= Duration::from_secs(self.max_age_secs))
            .unwrap_or(true)
    }
}

#[derive(Clone, Default)]
pub struct QuotaRegistry {
    /// Latest evidence from any source, retained for operator diagnostics.
    inner: Arc<DashMap<(String, String), QuotaSnapshot>>,
    /// Only explicitly account-global evidence may influence adaptive routing.
    account_global: Arc<DashMap<(String, String), QuotaSnapshot>>,
    /// Structured plugin buckets, including model/group-scoped evidence that
    /// must not be flattened into account-global routing signals.
    plugin_evidence: Arc<DashMap<(String, String), PluginQuotaEvidence>>,
}

impl QuotaRegistry {
    pub fn observe(
        &self,
        provider_id: &str,
        account_id: &str,
        remaining_fraction: Option<f64>,
        reset_at: Option<DateTime<Utc>>,
        source: impl Into<String>,
        max_age: Duration,
    ) {
        let remaining_fraction = remaining_fraction
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(0.0, 1.0));
        self.inner.insert(
            (provider_id.to_string(), account_id.to_string()),
            QuotaSnapshot {
                remaining_fraction,
                reset_at,
                observed_at: Utc::now(),
                source: source.into(),
                max_age_secs: max_age.as_secs().max(1),
            },
        );
    }

    pub fn observe_account_global(
        &self,
        provider_id: &str,
        account_id: &str,
        remaining_fraction: Option<f64>,
        reset_at: Option<DateTime<Utc>>,
        source: impl Into<String>,
        max_age: Duration,
    ) {
        let remaining_fraction = remaining_fraction
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(0.0, 1.0));
        let key = (provider_id.to_string(), account_id.to_string());
        let snapshot = QuotaSnapshot {
            remaining_fraction,
            reset_at,
            observed_at: Utc::now(),
            source: source.into(),
            max_age_secs: max_age.as_secs().max(1),
        };
        self.inner.insert(key.clone(), snapshot.clone());
        self.account_global.insert(key, snapshot);
    }

    pub fn observe_exhausted(
        &self,
        provider_id: &str,
        account_id: &str,
        reset_at: Option<DateTime<Utc>>,
        source: &str,
    ) {
        self.observe_account_global(
            provider_id,
            account_id,
            Some(0.0),
            reset_at,
            source,
            DEFAULT_MAX_AGE,
        );
    }

    pub fn observe_plugin(
        &self,
        provider_id: &str,
        account_id: &str,
        quota_state: Option<&str>,
        reset_at: Option<&str>,
    ) -> Option<QuotaPluginObservation> {
        let reset_at = reset_at.and_then(|value| {
            DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        });
        let remaining_fraction = quota_state
            .and_then(parse_quota_state)
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(0.0, 1.0));
        let exhausted = remaining_fraction.is_some_and(|remaining| remaining <= 0.0);
        if quota_state.is_some() || reset_at.is_some() {
            self.observe_account_global(
                provider_id,
                account_id,
                remaining_fraction,
                reset_at,
                "plugin_health_probe",
                Duration::from_secs(45),
            );
            Some(QuotaPluginObservation {
                remaining_fraction,
                reset_at,
                exhausted,
            })
        } else {
            None
        }
    }

    /// Retain all structured plugin buckets for diagnostics. Only one explicit
    /// account-scoped bucket may project into the scalar adaptive-routing registry;
    /// model, unknown, and ambiguous multi-window evidence stays non-routing.
    pub fn observe_plugin_snapshots(
        &self,
        provider_id: &str,
        account_id: &str,
        snapshots: Vec<PluginQuotaSnapshot>,
    ) -> Option<QuotaPluginObservation> {
        let snapshots = snapshots
            .into_iter()
            .map(validate_plugin_snapshot)
            .collect::<Vec<_>>();
        let evidence = PluginQuotaEvidence {
            snapshots,
            observed_at: Utc::now(),
            max_age_secs: 45,
        };
        self.plugin_evidence.insert(
            (provider_id.to_string(), account_id.to_string()),
            evidence.clone(),
        );

        let mut account_snapshots = evidence
            .snapshots
            .iter()
            .filter(|snapshot| matches!(snapshot.scope, PluginQuotaScope::Account));
        let snapshot = account_snapshots.next()?;
        if account_snapshots.next().is_some() {
            return None;
        }
        let reset_at = snapshot.reset_at.as_deref().and_then(|value| {
            DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        });
        let remaining_fraction = snapshot
            .remaining_fraction
            .filter(|value| value.is_finite() && (0.0..=1.0).contains(value));
        if remaining_fraction.is_none() && reset_at.is_none() {
            return None;
        }

        self.observe_account_global(
            provider_id,
            account_id,
            remaining_fraction,
            reset_at,
            "plugin_health_probe_v2",
            Duration::from_secs(evidence.max_age_secs),
        );
        Some(QuotaPluginObservation {
            remaining_fraction,
            reset_at,
            exhausted: remaining_fraction.is_some_and(|remaining| remaining <= 0.0),
        })
    }

    /// Current structured evidence for operator-facing diagnostics.
    pub fn plugin_observations(&self) -> Vec<(String, String, PluginQuotaEvidence, bool)> {
        let now = Utc::now();
        self.plugin_evidence
            .iter()
            .map(|entry| {
                let evidence = entry.value().clone();
                (
                    entry.key().0.clone(),
                    entry.key().1.clone(),
                    evidence.clone(),
                    evidence.is_fresh(now),
                )
            })
            .collect()
    }

    pub fn observe_headers(
        &self,
        provider_id: &str,
        account_id: &str,
        headers: &HeaderMap,
    ) -> Option<QuotaHeaderObservation> {
        let pairs = [
            (
                "anthropic-ratelimit-requests-remaining",
                "anthropic-ratelimit-requests-limit",
            ),
            (
                "x-ratelimit-remaining-requests",
                "x-ratelimit-limit-requests",
            ),
            ("x-ratelimit-remaining", "x-ratelimit-limit"),
            ("ratelimit-remaining", "ratelimit-limit"),
        ];
        for (remaining_name, limit_name) in pairs {
            let Some(remaining) = header_f64(headers, remaining_name) else {
                continue;
            };
            let reset_at = parse_reset_header(headers);
            let remaining_fraction = if remaining <= 0.0 {
                0.0
            } else {
                let Some(limit) = header_f64(headers, limit_name) else {
                    continue;
                };
                if limit <= 0.0 {
                    continue;
                }
                (remaining / limit).clamp(0.0, 1.0)
            };
            self.observe(
                provider_id,
                account_id,
                Some(remaining_fraction),
                reset_at,
                format!("response_header:{remaining_name}"),
                DEFAULT_MAX_AGE,
            );
            return Some(QuotaHeaderObservation {
                remaining_fraction,
                reset_at,
            });
        }
        None
    }

    pub fn snapshot(&self, provider_id: &str, account_id: &str) -> Option<QuotaSnapshot> {
        let key = (provider_id.to_string(), account_id.to_string());
        let value = self.inner.get(&key)?.clone();
        value.is_fresh(Utc::now()).then_some(value)
    }

    /// Fresh account-global observations available to adaptive routing.
    pub fn adaptive_snapshot(&self, provider_id: &str, account_id: &str) -> Option<QuotaSnapshot> {
        let key = (provider_id.to_string(), account_id.to_string());
        let value = self.account_global.get(&key)?.clone();
        value.is_fresh(Utc::now()).then_some(value)
    }

    pub fn snapshots(&self) -> Vec<(String, String, QuotaSnapshot)> {
        self.observations()
            .into_iter()
            .filter_map(|(provider_id, account_id, snapshot, fresh)| {
                fresh.then_some((provider_id, account_id, snapshot))
            })
            .collect()
    }

    /// Operator-facing view retains stale observations so the dashboard can
    /// distinguish stale evidence from a source that has never reported quota.
    /// Adaptive routing uses `adaptive_snapshot()` and account-global sources only.
    pub fn observations(&self) -> Vec<(String, String, QuotaSnapshot, bool)> {
        let now = Utc::now();
        self.inner
            .iter()
            .map(|entry| {
                let value = entry.value().clone();
                (
                    entry.key().0.clone(),
                    entry.key().1.clone(),
                    value.clone(),
                    value.is_fresh(now),
                )
            })
            .collect()
    }

    /// Operator-facing account-global evidence; freshness determines whether
    /// adaptive routing is currently allowed to use each observation.
    pub fn routing_observations(&self) -> Vec<(String, String, QuotaSnapshot, bool)> {
        let now = Utc::now();
        self.account_global
            .iter()
            .map(|entry| {
                let value = entry.value().clone();
                (
                    entry.key().0.clone(),
                    entry.key().1.clone(),
                    value.clone(),
                    value.is_fresh(now),
                )
            })
            .collect()
    }
}

fn validate_plugin_snapshot(mut snapshot: PluginQuotaSnapshot) -> PluginQuotaSnapshot {
    if matches!(&snapshot.scope, PluginQuotaScope::Model(model) if model.trim().is_empty()) {
        snapshot.scope = PluginQuotaScope::Unknown;
    }
    snapshot.remaining_fraction = snapshot
        .remaining_fraction
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value));
    snapshot.remaining = snapshot
        .remaining
        .filter(|value| value.is_finite() && *value >= 0.0);
    snapshot.limit = snapshot
        .limit
        .filter(|value| value.is_finite() && *value >= 0.0);
    if snapshot
        .reset_at
        .as_deref()
        .is_some_and(|value| DateTime::parse_from_rfc3339(value).is_err())
    {
        snapshot.reset_at = None;
    }
    snapshot
}

fn parse_quota_state(value: &str) -> Option<f64> {
    let normalized = value.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "exhausted" | "empty" | "depleted" | "none"
    ) {
        return Some(0.0);
    }
    if let Some(percent) = normalized.strip_suffix('%') {
        return percent
            .trim()
            .parse::<f64>()
            .ok()
            .map(|value| value / 100.0);
    }
    normalized
        .parse::<f64>()
        .ok()
        .map(|value| if value > 1.0 { value / 100.0 } else { value })
}

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn parse_reset_header(headers: &HeaderMap) -> Option<DateTime<Utc>> {
    for name in [
        "anthropic-ratelimit-requests-reset",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset",
        "ratelimit-reset",
    ] {
        let Some(raw) = headers.get(name).and_then(|value| value.to_str().ok()) else {
            continue;
        };
        let raw = raw.trim();
        if let Ok(value) = DateTime::parse_from_rfc3339(raw) {
            return Some(value.with_timezone(&Utc));
        }
        if let Ok(value) = raw.parse::<f64>() {
            if value.is_finite()
                && value >= 0.0
                && value >= 1_000_000_000_000.0
                && value <= i64::MAX as f64
            {
                return DateTime::from_timestamp_millis(value as i64);
            }
            if value.is_finite() && value >= 1_000_000_000.0 && value <= i64::MAX as f64 {
                return DateTime::from_timestamp(value as i64, 0);
            }
            if let Some(reset_at) = reset_after_seconds(value) {
                return Some(reset_at);
            }
        } else if let Some(seconds) = parse_reset_duration_secs(raw) {
            if let Some(reset_at) = reset_after_seconds(seconds) {
                return Some(reset_at);
            }
        }
    }
    None
}

fn reset_after_seconds(seconds: f64) -> Option<DateTime<Utc>> {
    let millis = seconds * 1000.0;
    (seconds.is_finite() && seconds >= 0.0 && millis <= i64::MAX as f64).then(|| {
        Utc::now().checked_add_signed(chrono::Duration::milliseconds(millis.round() as i64))
    })?
}

/// Parse provider countdowns such as `60s`, `500ms`, and `2m59.56s`.
fn parse_reset_duration_secs(raw: &str) -> Option<f64> {
    let value = raw.trim().to_ascii_lowercase();
    let bytes = value.as_bytes();
    let (mut index, mut total, mut parts) = (0, 0.0, 0);
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }
        let start = index;
        let mut decimal = false;
        while index < bytes.len() {
            match bytes[index] {
                b'0'..=b'9' => index += 1,
                b'.' if !decimal => {
                    decimal = true;
                    index += 1;
                }
                _ => break,
            }
        }
        let amount = value[start..index].parse::<f64>().ok()?;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let (unit, multiplier) = if value[index..].starts_with("ms") {
            (2, 0.001)
        } else {
            match bytes.get(index).copied()? {
                b's' => (1, 1.0),
                b'm' => (1, 60.0),
                b'h' => (1, 3600.0),
                _ => return None,
            }
        };
        total += amount * multiplier;
        if !total.is_finite() {
            return None;
        }
        index += unit;
        parts += 1;
    }
    (parts > 0).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_neutral_and_known_headroom_is_evidence() {
        let now = Utc::now();
        let unknown = QuotaSnapshot {
            remaining_fraction: None,
            reset_at: None,
            observed_at: now,
            source: "test".into(),
            max_age_secs: 60,
        };
        let healthy = QuotaSnapshot {
            remaining_fraction: Some(0.8),
            reset_at: None,
            observed_at: now,
            source: "test".into(),
            max_age_secs: 60,
        };
        let low = QuotaSnapshot {
            remaining_fraction: Some(0.1),
            reset_at: None,
            observed_at: now,
            source: "test".into(),
            max_age_secs: 60,
        };

        assert_eq!(unknown.preference(now), 0.0);
        assert!(healthy.preference(now) > unknown.preference(now));
        assert!(low.preference(now) < unknown.preference(now));
    }

    #[test]
    fn near_reset_adjustment_is_bounded() {
        let now = Utc::now();
        let snapshot = QuotaSnapshot {
            remaining_fraction: Some(0.1),
            reset_at: Some(now + chrono::Duration::seconds(30)),
            observed_at: now,
            source: "test".into(),
            max_age_secs: 60,
        };
        assert!((-0.5..=0.5).contains(&snapshot.preference(now)));
        assert!(snapshot.preference(now) < 0.0);
    }

    #[test]
    fn expired_reset_makes_recent_quota_unknown_immediately() {
        let now = Utc::now();
        let snapshot = QuotaSnapshot {
            remaining_fraction: Some(0.05),
            reset_at: Some(now - chrono::Duration::seconds(1)),
            observed_at: now,
            source: "test".into(),
            max_age_secs: 300,
        };

        assert!(!snapshot.is_fresh(now));
        assert_eq!(snapshot.preference(now), 0.0);
    }

    #[test]
    fn structured_plugin_scopes_are_preserved_without_account_routing_projection() {
        let registry = QuotaRegistry::default();
        let observation = registry.observe_plugin_snapshots(
            "p",
            "a",
            vec![
                PluginQuotaSnapshot {
                    scope: PluginQuotaScope::Unknown,
                    group: Some("Gemini Models".into()),
                    bucket_id: Some("gemini-5h".into()),
                    remaining_fraction: Some(0.6),
                    remaining: Some(600.0),
                    limit: Some(1_000.0),
                    unit: Some("requests".into()),
                    window: Some("5h".into()),
                    reset_at: Some("2030-01-01T00:00:00Z".into()),
                },
                PluginQuotaSnapshot {
                    scope: PluginQuotaScope::Model("gemini-2.5-pro".into()),
                    group: None,
                    bucket_id: None,
                    remaining_fraction: Some(0.2),
                    remaining: None,
                    limit: None,
                    unit: Some("tokens".into()),
                    window: Some("weekly".into()),
                    reset_at: None,
                },
            ],
        );

        assert!(observation.is_none());
        assert!(registry.adaptive_snapshot("p", "a").is_none());
        let observations = registry.plugin_observations();
        assert_eq!(observations.len(), 1);
        let evidence = &observations[0].2;
        assert!(observations[0].3);
        assert_eq!(evidence.snapshots.len(), 2);
        assert!(matches!(
            &evidence.snapshots[0].scope,
            PluginQuotaScope::Unknown
        ));
        assert_eq!(
            evidence.snapshots[0].group.as_deref(),
            Some("Gemini Models")
        );
        assert_eq!(evidence.snapshots[0].window.as_deref(), Some("5h"));
        assert!(matches!(
            &evidence.snapshots[1].scope,
            PluginQuotaScope::Model(model) if model == "gemini-2.5-pro"
        ));
    }

    #[test]
    fn structured_plugin_account_windows_are_not_collapsed() {
        let registry = QuotaRegistry::default();
        let snapshots = ["5h", "weekly"]
            .into_iter()
            .map(|window| PluginQuotaSnapshot {
                scope: PluginQuotaScope::Account,
                group: None,
                bucket_id: None,
                remaining_fraction: Some(0.5),
                remaining: None,
                limit: None,
                unit: Some("requests".into()),
                window: Some(window.into()),
                reset_at: None,
            })
            .collect();

        assert!(registry
            .observe_plugin_snapshots("p", "a", snapshots)
            .is_none());
        assert!(registry.adaptive_snapshot("p", "a").is_none());
        assert_eq!(registry.plugin_observations()[0].2.snapshots.len(), 2);
    }

    #[test]
    fn one_explicit_plugin_account_snapshot_can_update_routing_quota() {
        let registry = QuotaRegistry::default();
        let observation = registry
            .observe_plugin_snapshots(
                "p",
                "a",
                vec![PluginQuotaSnapshot {
                    scope: PluginQuotaScope::Account,
                    group: None,
                    bucket_id: Some("account-5h".into()),
                    remaining_fraction: Some(0.0),
                    remaining: Some(0.0),
                    limit: Some(100.0),
                    unit: Some("requests".into()),
                    window: Some("5h".into()),
                    reset_at: Some("2030-01-01T00:00:00Z".into()),
                }],
            )
            .unwrap();

        assert!(observation.exhausted);
        let routing = registry.adaptive_snapshot("p", "a").unwrap();
        assert_eq!(routing.remaining_fraction, Some(0.0));
        assert_eq!(routing.source, "plugin_health_probe_v2");
    }

    #[test]
    fn plugin_numeric_zero_variants_are_reported_as_hard_exhaustion() {
        for quota_state in ["0", "0%", "0.0", "0.00%", " exhausted "] {
            let registry = QuotaRegistry::default();
            let observation = registry
                .observe_plugin("p", "a", Some(quota_state), None)
                .unwrap();

            assert!(observation.exhausted, "quota_state={quota_state:?}");
            assert_eq!(
                observation.remaining_fraction,
                Some(0.0),
                "quota_state={quota_state:?}"
            );
            assert_eq!(
                registry.snapshot("p", "a").unwrap().remaining_fraction,
                Some(0.0),
                "quota_state={quota_state:?}"
            );
            assert_eq!(
                registry
                    .adaptive_snapshot("p", "a")
                    .unwrap()
                    .remaining_fraction,
                Some(0.0),
                "quota_state={quota_state:?}"
            );
        }

        let registry = QuotaRegistry::default();
        let observation = registry
            .observe_plugin("p", "a", Some("0.1"), None)
            .unwrap();
        assert!(!observation.exhausted);
        assert_eq!(observation.remaining_fraction, Some(0.1));
    }

    #[test]
    fn successful_zero_remaining_header_is_diagnostic_not_account_global_routing_evidence() {
        let registry = QuotaRegistry::default();
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset-requests", "2m59.56s".parse().unwrap());

        let observation = registry.observe_headers("p", "a", &headers).unwrap();
        assert_eq!(observation.remaining_fraction, 0.0);
        let reset_in_ms = (observation.reset_at.unwrap() - Utc::now()).num_milliseconds();
        assert!((179_000..=179_560).contains(&reset_in_ms));
        let diagnostic = registry.snapshot("p", "a").unwrap();
        assert_eq!(diagnostic.remaining_fraction, Some(0.0));
        assert_eq!(
            diagnostic.source,
            "response_header:x-ratelimit-remaining-requests"
        );
        assert!(registry.adaptive_snapshot("p", "a").is_none());
    }

    #[test]
    fn ambiguous_header_does_not_overwrite_explicit_account_global_routing_evidence() {
        let registry = QuotaRegistry::default();
        registry.observe_account_global(
            "p",
            "a",
            Some(0.75),
            None,
            "account_quota",
            Duration::from_secs(60),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
        registry.observe_headers("p", "a", &headers).unwrap();

        let diagnostic = registry.snapshot("p", "a").unwrap();
        assert_eq!(
            diagnostic.source,
            "response_header:x-ratelimit-remaining-requests"
        );
        assert_eq!(diagnostic.remaining_fraction, Some(0.0));
        let routing = registry.adaptive_snapshot("p", "a").unwrap();
        assert_eq!(routing.source, "account_quota");
        assert_eq!(routing.remaining_fraction, Some(0.75));
        let (_, _, routing_observation, fresh) = registry.routing_observations().pop().unwrap();
        assert!(fresh);
        assert_eq!(routing_observation.remaining_fraction, Some(0.75));
    }

    #[test]
    fn generic_zero_remaining_without_reset_is_not_account_exhaustion() {
        let registry = QuotaRegistry::default();
        let mut headers = HeaderMap::new();
        headers.insert("ratelimit-remaining", "0".parse().unwrap());

        let observation = registry.observe_headers("p", "a", &headers).unwrap();
        assert_eq!(observation.remaining_fraction, 0.0);
        assert_eq!(observation.reset_at, None);
        assert!(registry.adaptive_snapshot("p", "a").is_none());
    }

    #[test]
    fn throttled_zero_remaining_header_parses_short_duration_reset() {
        let registry = QuotaRegistry::default();
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset-requests", "60s".parse().unwrap());

        let observation = registry.observe_headers("p", "a", &headers).unwrap();
        let reset_in_secs = (observation.reset_at.unwrap() - Utc::now()).num_seconds();
        assert!((55..=60).contains(&reset_in_secs));
    }

    #[test]
    fn reset_duration_parser_handles_fractional_compound_and_millisecond_values() {
        for (raw, expected_secs) in [
            ("1s", 1.0),
            ("500ms", 0.5),
            ("1m30s", 90.0),
            ("1h", 3600.0),
            ("1h30m", 5400.0),
            ("2m59.56s", 179.56),
        ] {
            assert_eq!(parse_reset_duration_secs(raw), Some(expected_secs), "{raw}");
        }
        assert_eq!(parse_reset_duration_secs("1m30s "), Some(90.0));
    }

    #[test]
    fn reset_header_supports_numeric_seconds_and_absolute_timestamps() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset-requests", "60".parse().unwrap());
        let relative = parse_reset_header(&headers).unwrap();
        assert!((55..=60).contains(&(relative - Utc::now()).num_seconds()));

        for (raw, expected) in [
            (
                "1800000000",
                DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            ),
            (
                "1800000000000",
                DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            ),
            (
                "2030-01-01T00:00:00Z",
                DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
        ] {
            headers.insert("x-ratelimit-reset-requests", raw.parse().unwrap());
            assert_eq!(parse_reset_header(&headers), Some(expected), "{raw}");
        }
    }

    #[test]
    fn explicit_account_quota_exhaustion_remains_distinct_from_headers() {
        let registry = QuotaRegistry::default();
        registry.observe_exhausted("p", "a", None, "upstream_error");

        let snapshot = registry.snapshot("p", "a").unwrap();
        assert_eq!(snapshot.remaining_fraction, Some(0.0));
        assert_eq!(snapshot.source, "upstream_error");
        assert_eq!(
            registry.adaptive_snapshot("p", "a").unwrap().source,
            "upstream_error"
        );
    }

    #[test]
    fn stale_quota_is_unknown_for_routing_but_retained_for_diagnostics() {
        let registry = QuotaRegistry::default();
        registry.inner.insert(
            ("p".into(), "a".into()),
            QuotaSnapshot {
                remaining_fraction: Some(0.9),
                reset_at: None,
                observed_at: Utc::now() - chrono::Duration::seconds(2),
                source: "test".into(),
                max_age_secs: 1,
            },
        );

        assert!(registry.snapshot("p", "a").is_none());

        let observations = registry.observations();
        assert_eq!(observations.len(), 1);
        assert!(!observations[0].3, "stale evidence must be marked stale");
    }

    #[test]
    fn plugin_percent_is_normalized() {
        assert_eq!(parse_quota_state("80%"), Some(0.8));
        assert_eq!(parse_quota_state("0.25"), Some(0.25));
        assert_eq!(parse_quota_state("exhausted"), Some(0.0));
    }
}
