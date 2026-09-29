# Usage, Cost and Accounting

Kinetix records a usage row per request (asynchronously, never blocking the data
plane) and computes cost from the model's configured prices. A guiding principle
is **"unknown means unknown"**: Kinetix never invents token counts or costs.

## What is recorded

Each usage row holds: request id, timestamp, key id/name, client format,
requested/effective model, Route id/name, fallback hops/path, status + status
code, latency, TTFT, input/output/cached/thinking tokens, cost + `cost_known`,
`usage_confidence`, `commit_state`, retry count, cache status, serving
account/provider (admin-only), the opaque Route id, the upstream request id, a
`flagged` marker, and any error message.

## Confidence states

| Field | Values | Meaning |
| --- | --- | --- |
| `usage_confidence` | `provider_reported` | The upstream reported complete input and output totals. |
| | `estimated` | A count was derived rather than reported. |
| | `unknown` | A required total is missing or unavailable (including partial reports). |
| | `not_dispatched` | No upstream request was dispatched; token counts and cost are known zero. |
| `cost_known` | `1` / `0` | Whether cost is known from complete priced totals or because no request was dispatched. |

Unknown token counts are **omitted** from client responses rather than coerced to
zero. A request that failed before dispatch contributes known zero tokens and
cost. A spend total is never presented as complete when some usage is unpriced —
the overview/metrics also expose `unknown_usage_requests`, `estimated_usage_requests`,
and `unknown_cost_requests`.

## Cost

Cost is billed per 1M tokens: `(input − cached) × input + cached × cached_price +
output × output + thinking × thinking_price`. Both canonical input and output
totals must be known; a missing total is not treated as zero. Cached defaults to
the input price and thinking to the output price. **If prices or required totals
are unavailable, cost is `None` (unknown), not `0.0`.** A `price_versions` table
keeps price history so past costs stay reproducible.

## Per-key limits and budgets

Enforced before proxying:

- **RPM** / **TPM** over the last 60 seconds.
- **Daily** / **monthly** USD budgets.
- Status (revoked → 401, disabled → 403), expiry, allowed models, IP allowlist.

Inference admission is atomic per virtual key. Kinetix reserves one RPM slot,
a conservative token allowance, and conservative priced spend before dispatch.
Active reservations participate in later admission decisions immediately, so a
concurrent burst cannot all observe the same stale counter. An active budget
reservation applies to the current UTC day and month, even if the request began
in an earlier period. Completion settles its cost into the completion period,
matching usage-log timestamps and restart reconstruction. Complete provider
usage reconciles the reservation after the request. If token usage is unknown,
the live TPM ledger retains the conservative estimate. After a restart, a
persisted unknown token total fails closed for the rest of that 60-second
window because the estimate is not part of reported usage. Failures before
upstream dispatch reconcile RPM with known zero tokens and cost, both live and
after restart. Unknown token values remain unknown in reports; they are never
counted as zero.

The in-memory ledger is seeded once from durable usage history. After that,
usage-log writes are for reporting/accounting durability rather than admission
correctness: an async/dropped log cannot reopen capacity in the running process.
If historical counters cannot be read during startup/first use, Kinetix starts
that key's in-memory ledger from zero so a control-plane outage still does not
take down the data plane.

USD reservation remains unavailable when any possible target is unpriced.
Kinetix does not invent vendor prices.

The `usage_request_logs` view exposes one row per request, consolidating legacy
per-attempt rows where necessary. New requests store their aggregate in
`usage_logs`; `usage_attempts` stores per-attempt token and cost attribution.
Request counts and exports use request rows; token and spend totals use attempt
rows, with legacy request rows included when no attempt data exists.

## Usage views

- **Dashboard → Usage & Spend** — spend vs budgets per key, a Today / 24h / 7d /
  30d window, and per-day exports.
- **Dashboard → Request Inspector** — per-request rows with a live view.
- **Admin API** — `GET /admin/api/usage` (alias `/requests`) returns rows +
  summary.
- **Client API** - `GET /v1/usage` uses the caller's virtual key and returns only
  that key's usage. Daily and monthly windows use UTC `[from,to)` bounds from
  each period's start to request time; `resets` gives the next UTC boundaries.
  `admission` exposes the current in-flight count, not reservation details.

```json
{
  "periods": {
    "daily": { "from": "...", "to": "...", "timezone": "UTC" },
    "monthly": { "from": "...", "to": "...", "timezone": "UTC" }
  },
  "usage": {
    "daily": {
      "requests": 0,
      "input_tokens": 0,
      "output_tokens": 0,
      "known_cost_usd": 0,
      "unknown_cost_requests": 0,
      "unknown_usage_requests": 0
    },
    "monthly": {
      "requests": 0,
      "input_tokens": 0,
      "output_tokens": 0,
      "known_cost_usd": 0,
      "unknown_cost_requests": 0,
      "unknown_usage_requests": 0
    }
  },
  "limits": {
    "rpm": null,
    "tpm": null,
    "concurrency": null,
    "daily_budget_usd": null,
    "monthly_budget_usd": null
  },
  "remaining": { "daily_budget_usd": null, "monthly_budget_usd": null },
  "resets": { "daily": "...", "monthly": "..." },
  "admission": {
    "in_flight": 0,
    "budget": {
      "daily": {
        "settled_spend_usd": 0,
        "active_reserved_usd": 0,
        "unknown_active_cost": false
      },
      "monthly": {
        "settled_spend_usd": 0,
        "active_reserved_usd": 0,
        "unknown_active_cost": false
      }
    }
  }
}
```

`usage` includes failed requests with usage rows. Token totals are `null` if any
request in the window lacks that token count. `known_cost_usd` is the numeric
subtotal for priced requests, including `0` when none are priced;
`unknown_cost_requests` counts unpriced requests and signals that total spend is
incomplete. Remaining budgets are `null` when any request is unpriced or an
active request has unknown cost. Otherwise, remaining budgets subtract settled
admission spend and active conservative reservations, matching budget admission.
`admission.budget` exposes only per-period aggregates: settled spend, active
reserved cost, and whether any active cost is unknown. It never exposes
individual reservations. Admission also retains conservative cost reservations
for failed requests with unknown cost across restarts; these reservations are not
reported as known cost. Unset limits are `null`. The endpoint never returns
model, Route, provider, account, or other key identities. Database failures
during key authentication or usage aggregation return a generic 503; details are
logged server-side.

## Exports (JSONL + CSV)

Per-day exports are written to `$KINETIX_DATA_DIR/exports`:

- `usage-<day>.jsonl` - one JSON object per request.
- `usage-<day>.csv` — a flat per-request table.
- `summary-<day>.csv` — day totals.

An hourly task exports the last 40 days (skipping days already exported) and
prunes files older than `KINETIX_EXPORT_RETENTION_DAYS` (default 30). You can also
export on demand from the dashboard or with `kinetix export run [--day DATE]`.

## Body logging (opt-in)

Off by default. When a virtual key sets `body_logging`, Kinetix stores a
**redacted** request body (every path) and, for non-streaming requests, a redacted
response body, retained 7 days and purged hourly. Streaming responses are **not**
buffered, so only their request is retained. Redaction replaces anything
that looks like a secret (`sk-`, `AIza`, `AQ.`, `gsk_`, `sk-ant`, `ya29.`).
