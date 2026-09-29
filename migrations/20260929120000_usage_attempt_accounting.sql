CREATE TABLE IF NOT EXISTS usage_attempts (
    id                  TEXT PRIMARY KEY,
    request_id          TEXT NOT NULL,
    attempt_number      INTEGER NOT NULL,
    ts                  TEXT NOT NULL,
    key_id              TEXT,
    key_name            TEXT,
    effective_model     TEXT,
    route_id            TEXT,
    route_name          TEXT,
    serving_account_id  TEXT,
    serving_account     TEXT,
    serving_provider    TEXT,
    upstream_request_id TEXT,
    status              TEXT NOT NULL,
    status_code         INTEGER NOT NULL,
    input_tokens        INTEGER,
    output_tokens       INTEGER,
    cached_tokens       INTEGER,
    cache_write_tokens  INTEGER,
    thinking_tokens     INTEGER,
    cost_usd            REAL,
    cost_known          INTEGER NOT NULL DEFAULT 0,
    price_version_id    TEXT,
    usage_confidence    TEXT NOT NULL DEFAULT 'unknown',
    commit_state        TEXT NOT NULL DEFAULT '',
    error_message       TEXT,
    opaque_route_id     TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_attempt_request_number
    ON usage_attempts(request_id, attempt_number);
CREATE INDEX IF NOT EXISTS idx_usage_attempt_ts ON usage_attempts(ts);
CREATE INDEX IF NOT EXISTS idx_usage_attempt_key ON usage_attempts(key_id, ts);
CREATE INDEX IF NOT EXISTS idx_usage_attempt_account ON usage_attempts(serving_account_id, ts);

-- Earlier versions wrote failed provider attempts into usage_logs. Preserve
-- their attribution before exposing usage_logs as request-level history.
INSERT OR IGNORE INTO usage_attempts (
    id, request_id, attempt_number, ts, key_id, key_name, effective_model,
    route_id, route_name, serving_account_id, serving_account, serving_provider,
    upstream_request_id, status, status_code, input_tokens, output_tokens,
    cached_tokens, cache_write_tokens, thinking_tokens, cost_usd, cost_known,
    price_version_id, usage_confidence, commit_state, error_message, opaque_route_id
)
SELECT
    'legacy_usage_attempt_' || id, request_id, attempt_number, ts, key_id, key_name,
    effective_model, route_id, route_name, serving_account_id, serving_account,
    serving_provider, upstream_request_id, status, status_code, input_tokens,
    output_tokens, cached_tokens, cache_write_tokens, thinking_tokens, cost_usd,
    cost_known, price_version_id, usage_confidence, commit_state, error_message,
    opaque_route_id
FROM (
    SELECT usage_logs.*,
           ROW_NUMBER() OVER (PARTITION BY request_id ORDER BY ts ASC, rowid ASC) AS attempt_number
    FROM usage_logs
);

-- One accounting row per provider attempt, with a legacy request-row fallback
-- for requests created before attempt accounting was introduced.
CREATE VIEW IF NOT EXISTS usage_accounting_rows AS
SELECT request_id, ts, key_id, serving_account_id, input_tokens, output_tokens,
       cached_tokens, cache_write_tokens, thinking_tokens, cost_usd, cost_known,
       usage_confidence
FROM usage_attempts
UNION ALL
SELECT u.request_id, u.ts, u.key_id, u.serving_account_id, u.input_tokens,
       u.output_tokens, u.cached_tokens, u.cache_write_tokens, u.thinking_tokens,
       u.cost_usd, u.cost_known, u.usage_confidence
FROM usage_logs AS u
WHERE NOT EXISTS (
    SELECT 1 FROM usage_attempts AS a WHERE a.request_id = u.request_id
);

-- Per-request totals retain known attempt values for aggregate accounting while
-- marking whether each field is complete across every provider attempt.
CREATE VIEW IF NOT EXISTS usage_request_accounting AS
SELECT request_id,
       COUNT(*) AS attempt_count,
       SUM(input_tokens) AS reported_input_tokens,
       COUNT(input_tokens) AS input_report_count,
       SUM(output_tokens) AS reported_output_tokens,
       COUNT(output_tokens) AS output_report_count,
       SUM(cached_tokens) AS reported_cached_tokens,
       COUNT(cached_tokens) AS cached_report_count,
       SUM(cache_write_tokens) AS reported_cache_write_tokens,
       COUNT(cache_write_tokens) AS cache_write_report_count,
       SUM(thinking_tokens) AS reported_thinking_tokens,
       COUNT(thinking_tokens) AS thinking_report_count,
       SUM(CASE WHEN cost_known != 0 AND cost_usd IS NOT NULL THEN cost_usd ELSE 0.0 END)
           AS known_cost_usd,
       SUM(CASE WHEN cost_known != 0 AND cost_usd IS NOT NULL THEN 1 ELSE 0 END)
           AS known_cost_attempt_count,
       CASE
           WHEN MIN(CASE WHEN usage_confidence = 'provider_reported' THEN 1 ELSE 0 END) = 1
               THEN 'provider_reported'
           WHEN MIN(CASE WHEN usage_confidence IN ('provider_reported', 'estimated') THEN 1 ELSE 0 END) = 1
               THEN 'estimated'
           ELSE 'unknown'
       END AS usage_confidence
FROM usage_accounting_rows
GROUP BY request_id;

-- A stable one-row-per-request read surface keeps reports, limits, alerts,
-- live-request enrichment, and exports from interpreting attempts as requests.
CREATE VIEW IF NOT EXISTS usage_request_logs AS
WITH ranked AS (
    SELECT usage_logs.*,
           ROW_NUMBER() OVER (PARTITION BY request_id ORDER BY ts DESC, rowid DESC) AS request_rank
    FROM usage_logs
)
SELECT
    latest.id, latest.request_id, latest.ts, latest.key_id, latest.key_name,
    latest.client_format, latest.requested_model, latest.effective_model,
    latest.route_id, latest.route_name, latest.fallback_hops, latest.fallback_path,
    latest.status, latest.status_code, latest.latency_ms, latest.ttft_ms,
    CASE WHEN totals.input_report_count = totals.attempt_count
         THEN totals.reported_input_tokens END AS input_tokens,
    CASE WHEN totals.output_report_count = totals.attempt_count
         THEN totals.reported_output_tokens END AS output_tokens,
    CASE WHEN totals.cached_report_count = totals.attempt_count
         THEN totals.reported_cached_tokens END AS cached_tokens,
    CASE WHEN totals.cache_write_report_count = totals.attempt_count
         THEN totals.reported_cache_write_tokens END AS cache_write_tokens,
    CASE WHEN totals.thinking_report_count = totals.attempt_count
         THEN totals.reported_thinking_tokens END AS thinking_tokens,
    CASE WHEN totals.known_cost_attempt_count = totals.attempt_count
         THEN totals.known_cost_usd END AS cost_usd,
    CASE WHEN totals.known_cost_attempt_count = totals.attempt_count THEN 1 ELSE 0 END AS cost_known,
    CASE WHEN totals.attempt_count = 1 THEN latest.price_version_id END AS price_version_id,
    latest.cache_status, latest.serving_account_id, latest.serving_account,
    latest.serving_provider, latest.upstream_request_id, latest.flagged,
    latest.error_message, totals.usage_confidence, latest.commit_state,
    latest.retry_count, latest.route_trace_id, latest.opaque_route_id
FROM ranked AS latest
JOIN usage_request_accounting AS totals USING (request_id)
WHERE latest.request_rank = 1;
