//! Route Trace (FR-12.14) and the diagnostic flight recorder (FR-13).
//!
//! Both are **metadata only**: no prompt or response content, no secrets. The
//! Route Trace is the primary explanation surface for routing decisions; the
//! flight recorder is the primary stream-lifecycle diagnostic surface. They
//! correlate through the request id.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Route Trace
// ---------------------------------------------------------------------------

/// One step in the routing decision, in the order it happened.
#[derive(Debug, Clone, Serialize)]
pub struct TraceStep {
    /// Machine-readable stage: resolve | candidate | skip | attempt | commit |
    /// result | warning.
    pub stage: String,
    /// The target this step is about (model display name or account label).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Whether the candidate was eligible (for `candidate` steps).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eligible: Option<bool>,
    /// Three-valued predicate result, when a predicate was evaluated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    /// Why the candidate was skipped, or the attempt outcome.
    pub detail: String,
    /// Transport resolved for this target attempt (admin/internal metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_transport: Option<String>,
    /// Shared failure classification and policy effects, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_health_effect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_hint: Option<String>,
    /// Wall-clock offset from request start.
    pub elapsed_ms: u64,
}

/// One plugin-contributed routing fact, rendered in the Route Trace (§19).
#[derive(Debug, Clone, Serialize)]
pub struct PluginFactTrace {
    pub fact: String,
    pub value: serde_json::Value,
    pub source: serde_json::Value,
}

/// A failed routing-fact provider, rendered in the Route Trace as `unknown`
/// **with the reason** so explainability does not degrade where routing is
/// hardest (§6.4, §19).
#[derive(Debug, Clone, Serialize)]
pub struct PluginFactFailureTrace {
    pub plugin: String,
    pub result: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteTrace {
    pub request_id: String,
    /// Opaque, admin-resolvable id (FR-12.15). Never reveals topology.
    pub opaque_route_id: String,
    pub requested_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_name: Option<String>,
    /// Internally selected serving target (admin-only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_target: Option<String>,
    /// not_committed | committed
    pub commit_state: String,
    /// success | failed | cancelled
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_failure_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_allowed: Option<bool>,
    pub steps: Vec<TraceStep>,
    /// Client-visible warnings (e.g. strip_with_warning portability actions).
    pub warnings: Vec<String>,
    /// Plugin-contributed facts used for this request (§19).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub plugin_facts: Vec<PluginFactTrace>,
    /// Plugin fact-provider failures, recorded as `unknown` with a reason.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub plugin_fact_failures: Vec<PluginFactFailureTrace>,
    #[serde(skip)]
    started: Instant,
}

impl RouteTrace {
    pub fn new(request_id: String, requested_model: String) -> Self {
        RouteTrace {
            opaque_route_id: format!("krt_{}", uuid::Uuid::new_v4().simple()),
            request_id,
            requested_model,
            route_id: None,
            route_name: None,
            final_target: None,
            commit_state: "not_committed".into(),
            outcome: "failed".into(),
            stream_outcome: None,
            terminal_failure_kind: None,
            fallback_allowed: None,
            steps: Vec::new(),
            warnings: Vec::new(),
            plugin_facts: Vec::new(),
            plugin_fact_failures: Vec::new(),
            started: Instant::now(),
        }
    }

    pub fn step(&mut self, stage: &str, target: Option<String>, detail: impl Into<String>) {
        self.steps.push(TraceStep {
            stage: stage.to_string(),
            target,
            eligible: None,
            predicate: None,
            detail: detail.into(),
            resolved_transport: None,
            failure_kind: None,
            failure_category: None,
            failure_reason: None,
            fallback_rule: None,
            account_health_effect: None,
            client_status: None,
            retry_hint: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        });
    }

    pub fn resolved_transport(&mut self, target: impl Into<String>, transport: &str) {
        self.steps.push(TraceStep {
            stage: "attempt".into(),
            target: Some(target.into()),
            eligible: None,
            predicate: None,
            detail: "target execution profile resolved".into(),
            resolved_transport: Some(transport.to_string()),
            failure_kind: None,
            failure_category: None,
            failure_reason: None,
            fallback_rule: None,
            account_health_effect: None,
            client_status: None,
            retry_hint: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        });
    }

    pub fn candidate(
        &mut self,
        target: impl Into<String>,
        eligible: bool,
        predicate: Option<String>,
        detail: impl Into<String>,
    ) {
        self.steps.push(TraceStep {
            stage: "candidate".into(),
            target: Some(target.into()),
            eligible: Some(eligible),
            predicate,
            detail: detail.into(),
            resolved_transport: None,
            failure_kind: None,
            failure_category: None,
            failure_reason: None,
            fallback_rule: None,
            account_health_effect: None,
            client_status: None,
            retry_hint: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        });
    }

    pub fn warn(&mut self, warning: impl Into<String>) {
        let w = warning.into();
        self.steps.push(TraceStep {
            stage: "warning".into(),
            target: None,
            eligible: None,
            predicate: None,
            detail: w.clone(),
            resolved_transport: None,
            failure_kind: None,
            failure_category: None,
            failure_reason: None,
            fallback_rule: None,
            account_health_effect: None,
            client_status: None,
            retry_hint: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        });
        self.warnings.push(w);
    }

    pub fn commit(&mut self) {
        self.commit_state = "committed".into();
        self.step("commit", None, "first client bytes sent; no further retry");
    }

    pub fn finish(&mut self, outcome: &str) {
        self.outcome = outcome.to_string();
        self.step("result", None, format!("outcome={outcome}"));
    }

    /// Record the canonical stream terminal state and its routing effects.
    pub fn stream_termination(
        &mut self,
        termination: crate::stream_outcome::StreamTermination,
        fallback_allowed: Option<bool>,
    ) {
        self.stream_outcome = Some(termination.outcome.as_str().to_string());
        self.terminal_failure_kind = termination
            .failure_kind
            .map(|kind| kind.as_str().to_string());
        self.fallback_allowed = fallback_allowed;
        let detail = format!(
            "stream_outcome={} commit_state={} terminal_failure_kind={} fallback_allowed={}",
            termination.outcome.as_str(),
            termination.commit_state.as_trace_str(),
            self.terminal_failure_kind.as_deref().unwrap_or("none"),
            fallback_allowed
                .map(|allowed| allowed.to_string())
                .unwrap_or_else(|| "n/a".into()),
        );
        self.step("stream_termination", None, detail);
    }

    pub fn plugin_fact(
        &mut self,
        name: &str,
        value: &serde_json::Value,
        source: &serde_json::Value,
    ) {
        self.plugin_facts.push(PluginFactTrace {
            fact: name.to_string(),
            value: value.clone(),
            source: source.clone(),
        });
        self.steps.push(TraceStep {
            stage: "plugin_fact".into(),
            target: Some(name.to_string()),
            eligible: None,
            predicate: None,
            detail: format!("= {value}"),
            resolved_transport: None,
            failure_kind: None,
            failure_category: None,
            failure_reason: None,
            fallback_rule: None,
            account_health_effect: None,
            client_status: None,
            retry_hint: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        });
    }

    /// Record a classified upstream failure and its shared policy effects.
    pub fn failure(
        &mut self,
        target: Option<String>,
        detail: impl Into<String>,
        kind: crate::types::FailureKind,
        upstream_status: Option<u16>,
    ) {
        let policy = kind.policy();
        self.steps.push(TraceStep {
            stage: "attempt".into(),
            target,
            eligible: None,
            predicate: None,
            detail: detail.into(),
            resolved_transport: None,
            failure_kind: Some(kind.as_str().into()),
            failure_category: Some(policy.category.as_str().into()),
            failure_reason: Some(kind.reason_code().into()),
            fallback_rule: Some(policy.fallback.as_str().into()),
            account_health_effect: Some(policy.account_health.as_str().into()),
            client_status: kind.client_status(upstream_status),
            retry_hint: Some(policy.retry_hint.as_str().into()),
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        });
    }

    /// Record a routing-fact provider failure as `unknown` with a reason.
    pub fn plugin_fact_failure(&mut self, plugin_id: &str, reason: &str) {
        self.plugin_fact_failures.push(PluginFactFailureTrace {
            plugin: plugin_id.to_string(),
            result: "unknown".into(),
        });
        self.steps.push(TraceStep {
            stage: "plugin_fact_failure".into(),
            target: Some(plugin_id.to_string()),
            eligible: None,
            predicate: Some("unknown".into()),
            detail: format!("fact provider failed: {reason}"),
            resolved_transport: None,
            failure_kind: None,
            failure_category: None,
            failure_reason: None,
            fallback_rule: None,
            account_health_effect: None,
            client_status: None,
            retry_hint: None,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        });
    }

    pub fn steps_json(&self) -> String {
        serde_json::to_string(&self.steps).unwrap_or_else(|_| "[]".into())
    }
    pub fn warnings_json(&self) -> String {
        serde_json::to_string(&self.warnings).unwrap_or_else(|_| "[]".into())
    }
}

// ---------------------------------------------------------------------------
// Flight recorder (FR-13)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct FlightEvent {
    pub seq: u64,
    pub elapsed_ms: u64,
    pub event: String,
    pub detail: String,
}

struct FlightInner {
    order: VecDeque<String>,
    by_request: HashMap<String, Vec<FlightEvent>>,
    seq: u64,
}

/// A bounded, metadata-only lifecycle event recorder.
///
/// Bounded by **request count** and by **events per request**. Saturation drops
/// the oldest request's diagnostics rather than blocking the data plane
/// (FR-13.3) — every `record` is a non-blocking, best-effort operation.
pub struct FlightRecorder {
    inner: Mutex<FlightInner>,
    max_requests: usize,
    max_events_per_request: usize,
    dropped_requests: AtomicU64,
    dropped_events: AtomicU64,
}

impl FlightRecorder {
    pub fn new(max_requests: usize, max_events_per_request: usize) -> Self {
        FlightRecorder {
            inner: Mutex::new(FlightInner {
                order: VecDeque::new(),
                by_request: HashMap::new(),
                seq: 0,
            }),
            max_requests,
            max_events_per_request,
            dropped_requests: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
        }
    }

    pub fn record(
        &self,
        request_id: &str,
        elapsed_ms: u64,
        event: &str,
        detail: impl Into<String>,
    ) {
        let mut inner = self.inner.lock();
        inner.seq += 1;
        let seq = inner.seq;
        if !inner.by_request.contains_key(request_id) {
            if inner.order.len() >= self.max_requests {
                if let Some(oldest) = inner.order.pop_front() {
                    inner.by_request.remove(&oldest);
                    self.dropped_requests.fetch_add(1, Ordering::Relaxed);
                }
            }
            inner.order.push_back(request_id.to_string());
        }
        let entry = inner.by_request.entry(request_id.to_string()).or_default();
        if entry.len() >= self.max_events_per_request {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
            return;
        }
        entry.push(FlightEvent {
            seq,
            elapsed_ms,
            event: event.to_string(),
            detail: detail.into(),
        });
    }

    /// Events recorded for one request (empty when evicted or never seen).
    pub fn events(&self, request_id: &str) -> Vec<FlightEvent> {
        self.inner
            .lock()
            .by_request
            .get(request_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn request_count(&self) -> usize {
        self.inner.lock().order.len()
    }

    pub fn dropped_requests(&self) -> u64 {
        self.dropped_requests.load(Ordering::Relaxed)
    }

    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }
}

impl Default for FlightRecorder {
    fn default() -> Self {
        Self::new(512, 128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_trace_serializes_resolved_transport_per_attempt() {
        let mut trace = RouteTrace::new("req-1".into(), "model".into());
        trace.resolved_transport("model-a", "openai-responses");
        let value: serde_json::Value = serde_json::from_str(&trace.steps_json()).unwrap();
        assert_eq!(value[0]["stage"], "attempt");
        assert_eq!(value[0]["resolved_transport"], "openai-responses");
        assert_eq!(value[0]["target"], "model-a");
    }

    #[test]
    fn flight_recorder_is_bounded() {
        let fr = FlightRecorder::new(2, 4);
        for i in 0..5 {
            for e in 0..10 {
                fr.record(&format!("req{i}"), e, "event", format!("{e}"));
            }
        }
        // Only the last two requests survive.
        assert_eq!(fr.request_count(), 2);
        assert!(fr.events("req0").is_empty());
        assert_eq!(fr.events("req4").len(), 4);
        assert!(fr.dropped_requests() >= 3);
        assert!(fr.dropped_events() > 0);
    }
}
