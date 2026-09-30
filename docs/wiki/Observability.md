# Observability

Kinetix exposes Prometheus metrics, a structured Route Trace, a bounded flight
recorder, a live in-flight view, and webhook alerts. All diagnostics are
**metadata-only** by default — never prompt/completion bodies.

## Health

`GET /healthz`:

```json
{"status":"ok","uptime_secs":3,"database":"ok","data_plane":"serving","control_plane":"ok"}
```

It stays **HTTP 200** while the data plane is serviceable. `control_plane:
degraded` means the store is unavailable but inference still works from the
in-memory snapshot, so the instance is not dropped from a load balancer.

## Runtime health

`GET /admin/api/health/runtime` requires admin auth. Its `window` query accepts
`5m`, `1h`, or `24h` (default `1h`). The response combines persisted telemetry
at provider, account, and model scope with live provider circuit states and
quota observations.

Telemetry keeps separate counters for rate limits, quota exhaustion, server
errors, connection errors, timeouts, authentication errors, target errors, and
bad requests. Rate-limit headers remain visible as bucket headroom and reset
diagnostics, and may set a 429 cooldown. Their scope can be request- or
model-specific, so they do not affect account-wide adaptive ordering or exhaust
an account. Adaptive quota preference and hard exhaustion use explicit
account-global evidence, such as account quota status or a plugin health probe.
Kinetix parses countdown reset values with `ms`, `s`, `m`, and `h` units, as
well as numeric seconds, Unix timestamps, and RFC3339 timestamps. Expired
observations are neutral to routing.

Runtime Health quota rows keep the latest diagnostic observation in their
existing top-level fields. The nested `routing` object reports the separate
account-global snapshot (`scope`, source, headroom, reset, and freshness)
eligible to influence adaptive ordering; `routing_eligible` is false for stale
evidence, and `null` means no account-global observation has been reported.

The Runtime Health provider-circuit rejection count is a target-candidate count,
not a unique-request count. A logical request may contribute multiple rejects
when it skips multiple targets behind an open provider circuit.

## Metrics

`GET /admin/api/metrics` emits Prometheus text (requires admin auth):

| Metric | Meaning |
| --- | --- |
| `kinetix_requests_total` | Total requests. |
| `kinetix_error_rate` | Error fraction (gauge). |
| `kinetix_cost_usd_total` | Total known spend. |
| `kinetix_avg_latency_ms` / `kinetix_avg_ttft_ms` | Averages. |
| `kinetix_cached_tokens_total` / `kinetix_cache_write_tokens_total` | Cache-read / cache-write token totals. |
| `kinetix_fallback_hops_total` / `kinetix_route_fallbacks_total` / `kinetix_route_skip_total` | Routing. |
| `kinetix_failures_pre_commit_total` / `kinetix_failures_post_commit_total` | Failures before/after the commit point. |
| `kinetix_cancellations_total` / `kinetix_cancellation_latency_ms` | Client disconnects + detection latency. |
| `kinetix_active_streams` / `kinetix_live_view_dropped_total` | Live view. |
| `kinetix_log_queue_depth` / `kinetix_log_queue_dropped_total` | Usage-log queue. |
| `kinetix_flight_recorder_requests` / `kinetix_flight_recorder_dropped_total` | Flight recorder. |
| `kinetix_account_status{status}` | Accounts by state. |
| `kinetix_control_plane_degraded` | `1` when the store is down. |
| `kinetix_usage_unknown_total` / `kinetix_usage_estimated_total` / `kinetix_usage_unknown_cost_total` | Accounting confidence. |
| `kinetix_credential_failures_total` | Credential-strategy failures. |
| `kinetix_ip_rate_limited_total` | Per-IP rejections. |
| `kinetix_admission_reservations_active` / `kinetix_admission_reserved_tokens_active` | Active per-key rate/budget reservations and their conservative token estimates. |
| `kinetix_admission_reservations_total` / `kinetix_admission_reserved_tokens_total` | Cumulative reservations and estimated tokens. |
| `kinetix_admission_reconciliations_total{usage}` / `kinetix_admission_reservations_dropped_total` | Complete/incomplete usage reconciliations and reservations dropped before finalization. |
| `kinetix_admission_reconciled_tokens_total` / `kinetix_admission_token_adjustment_total{direction}` | Provider-reported tokens and reservation decreases/increases when usage is complete. |
| `kinetix_admission_rejections_total{reason}` | Local RPM, TPM, budget, and concurrency rejections; label values are a fixed set. |
| `kinetix_admission_inflight_inferences` | Current requests counted against the instance-wide cap. |
| `kinetix_admission_reservation_oldest_age_seconds` | Age of the oldest active reservation; use workload-specific alert thresholds. |
| `kinetix_provider_work_scheduled_total` | Provider-scoped work submitted to the coordinator. |
| `kinetix_provider_work_executed_total` | Work started after acquiring a provider budget permit. |
| `kinetix_provider_work_coalesced_total` | Calls sharing equivalent in-flight provider work. |
| `kinetix_provider_work_rate_limited_total` | Work delayed by local rate or spacing budgets. |
| `kinetix_provider_work_provider_throttled_total` | Failures classified as provider-wide throttling or transient unavailability. |
| `kinetix_provider_work_backed_off_total` | Work suppressed by provider backoff. |
| `kinetix_provider_work_failed_total` | Failed or interrupted provider work. |
| `kinetix_allocations_total` / `kinetix_alloc_bytes_total` | Allocations per request (only when built `--features alloc-stats`; `0` otherwise = honestly "not measured"). |

Provider-work counters omit provider, account, and model labels. The coordinator
budgets refresh-capable credential resolution, discovery, probes, pricing refresh,
and cached routing-fact refresh. Each actual per-account discovery operation gets
its own permit while reconciliation remains singleflight. Cached credential reads
bypass this coordinator on inference paths; probes, discovery, and routing-fact
refresh stay off inference paths. The coordinator uses the scope assigned by
Kinetix to each operation: account-scoped rate limits retain account cooldown
behavior, while provider-scoped evidence can trigger bounded provider backoff.
This describes Kinetix policy, not upstream quota semantics.

Admission metrics omit key and Route identities to keep label cardinality bounded. Incomplete usage keeps the conservative token reservation; the oldest-reservation age is exposed without imposing a fixed stale threshold.

Cache status is derived from final normalized provider usage, not cache affinity or
a static request flag:

- `cached_tokens > 0` → `hit`
- otherwise `cache_write_tokens > 0` → `miss`
- neither observed → `bypass`

For Anthropic streaming, `message_start` normally reports cache usage before
Kinetix commits the downstream response, so `X-Kinetix-Cache` uses that
pre-commit value. Persisted request/usage records are authoritative and are
recomputed from the final usage even when a protocol cannot expose cache usage
before response commit.

## Route Trace

Every request produces a Route Trace: candidate enumeration, predicate/capability
results, skip reasons, the selected account/model (internally), attempt outcomes,
fallback causes, the commit point, and the final result.

- Retrieve by request: `GET /admin/api/requests/{id}/route-trace`.
- Retrieve by the client's opaque id: `GET /admin/api/route-traces/{krt_…}`.

Trace steps look like `resolve → candidate → skip → attempt → commit → result`
with per-step timings and warnings. Adaptive quota-evidence steps use the exact
frozen account-global snapshots captured for that ordering pass, rather than a
later registry read. When Kinetix opens its provider circuit, the
trace records a `provider_circuit` step at the transition, including transitions
recorded while finalizing a committed stream; it does not require a later target to
be rejected by that circuit. Each target attempt includes its structured
`resolved_transport` value (for example, `openai-responses`) so transport and
endpoint decisions can be diagnosed without exposing credentials. Classified
failures include `failure_kind`, `failure_category`, `failure_reason`,
`fallback_rule`, `account_health_effect`, `client_status`, and `retry_hint`.
Account skip steps include the lifecycle reason and any known `retry_at`.

When a request evaluates plugin routing facts (`plugin.<id>.<name>`) or targets a plugin-backed provider (`wire_plugin` / `credential_plugin`), the Route Trace records:
- Evaluated fact values and their source (`plugin_id`, `plugin_version`, `capability`).
- Explicit skip reasons if a bound plugin is uninstalled, disabled, or tripping its circuit breaker.
- Credential lease acquisition or refresh steps.

## Plugin metrics and audit

Plugin performance and reliability are monitored independently of core proxy traffic:

* **Per-plugin metrics (`GET /admin/api/plugins/{id}/metrics`)**:
  * `host_invocations_total`: Total guest function invocations.
  * `host_faults_total`: Traps, panics, and internal plugin errors.
  * `host_timeouts_total`: Executions preempted by epoch interruption.
  * `host_cancellations_total`: Client disconnects during guest execution (never penalizes circuit).
  * `host_http_requests_total`: Outbound HTTP requests made through the host HTTP capability.
  * `circuit_state` & `consecutive_failures`: Current circuit-breaker status (`closed`, `open`, `half_open`).
  * `kv_bytes`: Total encrypted storage consumed by the plugin in `plugin_kv`.
* **Usage log attribution**: The `usage_logs` database table records `plugin_id` and `plugin_version` on every completed request that used a plugin, ensuring complete auditability and usage analytics.
* **Plugin audit trail (`GET /admin/api/plugins/{id}/audit`)**: Dedicated view of lifecycle actions (install, enable, disable, permission approvals/revocations, removal).

## Flight recorder

A bounded, metadata-only ring of lifecycle events (request accepted, auth
complete, upstream connect/headers/first frame, tool/reasoning event classes,
commit, cancellation, usage finalized), correlated with the request id. Retrieve
via `GET /admin/api/requests/{id}/diagnostics`. Bounded by count/bytes/time so it
can never grow unbounded.

## Live in-flight view

`GET /admin/api/requests/live` returns in-flight requests (phase
`selecting → committed → done`, commit state, fallback hops, retries, tokens,
latency, TTFT). Surfaced in the dashboard's Request Inspector and reflected in the
`kinetix_active_streams` metric.

> Client connection closure cancels pre-commit upstream work. After commit,
> dropping the response stream cancels active stream work and releases its
> concurrency capacity.

## Alerts

Set `KINETIX_ALERT_WEBHOOK_URL` to receive JSON webhooks (`{event: "alert"|"resolved", ...}`).
Alerts are **edge-triggered** (fire once, resolve when clear) and are control-plane
only — a webhook failure never affects inference. Covered conditions:

- high fallback rate / high error rate,
- an account unhealthy (exhausted / circuit-open / disabled),
- a Route with no healthy target,
- a key crossing 80% of its monthly budget,
- usage-log queue saturation (≥80%) or dropped rows,
- scheduled-backup failure,
- p95 added proxy latency above `KINETIX_ALERT_P95_LATENCY_MS` sustained 10 min,
- repeated credential-strategy failures.

## Logging

Operational JSON logs (stdout/journald) cover startup, config activation, upstream
errors, retries, account-state transitions, DNS destinations, control-plane
degradation, and admin actions — with secrets redacted. Prompts/completions,
virtual keys, credentials, and auth headers are **never** logged by default.
