# Routing and Fallback

A **Route** is a named routing policy that clients address like a model. It holds
an ordered list of **targets** (an account + upstream model, optionally with a
typed predicate and parameter overrides), a selection strategy, fallback triggers,
a state-portability policy, and optional prompt-cache affinity.

## Resolving a requested name

When a client asks for a model name, Kinetix resolves it in this order:

1. An **alias** with that name (exact match).
2. A **Route** with that name.
3. `provider/model-id` (provider matched by name or id).
4. A bare upstream model id.

Aliases point at either a single model (`target_type = "model"`) or a Route
(`target_type = "route"`).

## Targets

Each Route target names a model and may optionally pin one account. If the
account is omitted, the target owns the provider's executable account pool:
Kinetix applies account health, priority, and weight, then tries eligible sibling
accounts before advancing to the next logical Route target. Targets may span
providers and wire formats.

## Selection strategies

| Strategy | Behavior |
| --- | --- |
| `priority` | Try targets in priority order. |
| `round-robin` | Rotate the starting target per request. |
| `weighted` | Choose proportionally to target weight. |
| `least-used` | Prefer the target with the fewest lifetime requests. |
| `adaptive` | Prefer available target-local concurrency, account-global quota headroom, then lower overload/error EWMAs and TTFT. Unknown quota is neutral; cold TTFT uses the route-wide median observed TTFT. |

For `adaptive` routes, Kinetix holds a target-local concurrency permit for the
full upstream stream and records dispatch-to-first-semantic-event TTFT immediately.
First-event health is provisional while the stream is open: it can neutralize
older/no failure telemetry for routing, but only terminal success commits the
healthy EWMA sample. Failure signals are timestamped independently, so a newer
5xx/connection/overload from another request remains visible even while validated
streams are still open; a later error on the same stream likewise replaces its
provisional health with a real failure. Limit updates are wall-clock-window gated so request
density cannot accelerate concurrency growth. Failure/overload penalties decay
toward neutral with wall-clock time even when a target is idle, so transient
failures do not permanently sideline recovered targets. TTFT observations also
become cold after five minutes without a new sample, allowing a previously slow
target to return to the route's neutral TTFT baseline. Adaptive ordering runs only
after hard eligibility (predicate, provider restriction, capability, and context
checks). Account-global quota headroom is the next sort key after capacity;
only fresh explicit account-global observations contribute, while generic
response-header buckets remain diagnostic. The Route Trace records the frozen
quota evidence from that ordering pass. Within the remaining candidates, only
currently dispatchable accounts
(healthy accounts plus legitimate circuit half-open probes) contribute adaptive
telemetry or represent a logical route target; unavailable siblings remain in the
fallback list but cannot shift its ranking. One immutable score is computed per
dispatchable route candidate for the whole ordering pass; cold TTFT candidates
receive the median observed TTFT of those eligible candidates. The Gradient2 queue
allowance scales with the current limit (capped at 4) rather than using a fixed
allowance equal to the initial limit. A 429 applies immediate loss backoff; a
timeout backs off only when the target was meaningfully loaded; generic
5xx/connection failures affect error telemetry without being treated as direct
congestion.

Cache/sticky affinity is applied after adaptive ordering. An eligible affine
target therefore stays preferred; if it is only concurrency-saturated Kinetix
waits briefly (bounded to 300 ms) before spilling to another target.

## Predicates

A target may carry a **typed, side-effect-free predicate** — an expression tree
with **three-valued** evaluation (`true` / `false` / `unknown`) and a
human-readable explanation. Arbitrary code/eval is forbidden. When a fact is
unknown, the target's `when_unknown` policy decides (`skip` by default, or
`allow`).

Fact vocabulary:

| Fact | Meaning |
| --- | --- |
| `has_tools` | The request carries tool definitions. |
| `has_images` | The request carries image parts. |
| `has_reasoning` | The request sets a thinking/reasoning control. |
| `frontend` | `openai` or `anthropic`. |
| `requested_model` / `requested_alias` / `requested_route` | Names the client used. |
| `key_tag` | The virtual key's tag. |
| `input_tokens` | Known input size. |
| `target_model` / `target_model_id` | The candidate model. |
| `target_provider` / `target_provider_id` | The candidate provider. |
| `target_capability(arg)` | A declared capability (unknown if unconfigured). |
| `target_context_window` / `target_max_output_tokens` | Model limits. |
| `plugin.<id>.<name>` | Typed fact contributed by an installed plugin (unknown if missing or stale). |

Wire format (as stored/returned by the admin API):

```json
{
  "expr": { "fact": "has_tools", "op": "eq", "value": true },
  "when_unknown": "skip"
}
```

`op` is one of `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`, `contains`
(default `eq`). `fact` is a bare name string or `{ "name": ..., "arg": ... }`.

Example: send tool requests to a tool-capable model, everything else to a cheap
one:

```json
targets: [
  { "model": "Gemini/gemini-2.5-flash", "predicate": { "expr": { "fact": "has_tools", "op": "eq", "value": true } } },
  { "model": "9router/free",            "predicate": { "expr": { "fact": "has_tools", "op": "eq", "value": false } } }
]
```

### Plugin-contributed routing facts

Plugins implementing the `RoutingFactProvider` capability can export typed facts into the predicate engine:

* **Naming convention**: `plugin.<plugin-id>.<fact-name>` (e.g. `plugin.dev.example.geo.region` or `plugin.dev.example.compute.capacity_tier`).
* **Evaluation timing**: Fact providers run once per request before target candidate planning, caching values across the evaluation of all Route targets.
* **Determinism**:
  * `pure` fact providers are side-effect-free and forbidden from making outbound network calls.
  * `cached` fact providers periodically poll their upstream source. If a cached observation exceeds `max_age_ms`, it is dropped.
* **Failures and unknowns**: If a plugin fact provider times out, traps, or is disabled, the fact evaluates as `unknown`. The Route's `when_unknown` policy (`skip` or `allow`) then governs target eligibility.
* **Traceability**: The Route Trace records the evaluated fact values along with their source plugin ID and version, or the exact failure reason (e.g. timeout or circuit open).

## Fallback and the commit point

- Retry/fallback is allowed **only before the first client byte** (the *commit
  point*). The state machine is
  `SELECT → CONNECT → WAITING_FOR_FIRST_BYTE → COMMITTED → STREAMING/COMPLETE`.
- After commit, a failure **terminates the stream** with a format-correct error
  (OpenAI: an `error` object + a chunk with `finish_reason: "error"` + `[DONE]`;
  Anthropic: a single terminal `error` event). Nothing is silently spliced.
- The attempt loop is bounded by `max_attempts` (default 5, capped) and a 30-second
  pre-commit deadline, with bounded backoff between attempts (100 ms → 1 s).
The host, native adapters, and plugins use the same failure kinds:
`rate_limit`, `quota_exhausted`, `auth_error`, `target_error`, `server_error`,
`connection_error`, `timeout`, `bad_request`, `malformed_upstream`,
`plugin_failure`, `policy_rejected`, and `client_cancelled`.

| Failure kind(s) | Account health | Route fallback | Client status |
| --- | --- | --- | --- |
| `rate_limit` | Cooldown; use `Retry-After` when available. | `on429` (enabled by default). | 429 |
| `quota_exhausted` | Exhausted until the quota reset. | `onQuota` (enabled by default). | 429 |
| `auth_error` | Disable the rejected credential. | Try another account/target. | 502 |
| `target_error` | No credential state change. | Try another logical Route target; direct requests do not rotate accounts for this error. | Preserve upstream 403/404, otherwise 502 |
| `server_error`, `connection_error`, `malformed_upstream` | No account cooldown; qualifying upstream failures may affect the provider circuit. | `on5xx` (enabled by default). | 502 |
| `timeout` | No account cooldown. | `onTimeout` (enabled by default). | 504 |
| `plugin_failure` | No provider/account health effect. | `on5xx` (enabled by default). | 502 |
| `bad_request`, `policy_rejected` | No account state change. | Never. | 400 |
| `client_cancelled` | No provider/plugin health effect. | Never. | No error response |

Core portability, parameter-policy, unsupported-translation, and thinking-translation rejections are traced as `policy_rejected` and return 400. Other unsupported model or configuration responses retain their own status. Route Trace records the shared failure policy fields for these host rejections too.

A Route can disable the `on429`, `onQuota`, `on5xx`, and `onTimeout` triggers;
missing triggers default to enabled. Auth and target-local failures remain eligible
for fallback, while non-retryable request, policy, and cancellation failures never
fall back. Account circuit health counts credential-scoped failures separately
from provider-level circuit evidence.

## Portability policy

When a fallback crosses providers, opaque provider state (e.g. reasoning
signatures) cannot travel. The Route's `portability_policy` decides:

- `strip_with_warning` (default): remove the non-portable state, record it in the
  Route Trace, and emit an `X-Kinetix-Warning` response header. Silent stripping
  is forbidden.
- `reject`: fail the request with a format-correct error rather than stripping.

## Prompt-cache affinity

When a Route has `cache_affinity` or `sticky_routing` and the request carries
an explicit session header, Kinetix remembers the last successful target for that
session and prefers it while still eligible. `cache_affinity` is intended for
prompt-cache locality; `sticky_routing` is the general session-affinity switch.
It does **not** create or prove an upstream prompt-cache hit.

For Claude Code OAuth translated requests, Kinetix emits deterministic 1-hour
cache breakpoints on the stable system prefix and final cacheable tool definition.
Same-format Anthropic requests preserve valid client `cache_control` markers.
Actual cache behavior is reported from Anthropic
`cache_read_input_tokens` / `cache_creation_input_tokens`; changing models is
a different upstream cache identity and a miss is expected.

Session identity is taken **only** from an explicit header — never guessed:

```
X-Kinetix-Session | X-Session-Id | X-Conversation-Id | X-Session-Affinity | Session-Id
```

## Client-visible routing headers

Clients see only opaque routing metadata:

| Header | Meaning |
| --- | --- |
| `X-Request-Id` | The request id. |
| `X-Kinetix-Route-Id` | Opaque `krt_…` id; resolvable to a Route Trace by an admin. |
| `X-Kinetix-Cache` | `hit` / `miss` / `bypass`. |
| `X-Kinetix-Fallback` | `1` when a fallback occurred (omitted otherwise). |
| `X-Kinetix-Warning` | JSON array of warnings (e.g. a portability strip). |

Serving account/provider names are **not** exposed to clients. An admin can
resolve the opaque id at `GET /admin/api/route-traces/{opaque_id}`.

## Related

- [Admin API](Admin-API) — create/manage Routes and run a Route Dry Run.
- [Observability](Observability) — the Route Trace and flight recorder.
