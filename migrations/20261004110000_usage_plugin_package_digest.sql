-- Package provenance for plugin-served requests (#206): the SHA-256 of the
-- plugin package whose adapter served each attempt. NULL for built-in adapters.
ALTER TABLE usage_logs ADD COLUMN plugin_package_sha256 TEXT;
ALTER TABLE usage_attempts ADD COLUMN plugin_package_sha256 TEXT;

-- The request-log view selects explicit columns, so rebuild it to expose the
-- new request-level column to row decoders.
DROP VIEW IF EXISTS usage_request_logs;

CREATE VIEW usage_request_logs AS
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
    latest.retry_count, latest.route_trace_id, latest.opaque_route_id,
    latest.admission_cost_usd, latest.plugin_package_sha256
FROM ranked AS latest
JOIN usage_request_accounting AS totals USING (request_id)
WHERE latest.request_rank = 1;
