ALTER TABLE accounts
    ADD COLUMN status_reason TEXT NOT NULL DEFAULT 'unknown';

ALTER TABLE accounts
    ADD COLUMN status_changed_at TEXT;

UPDATE accounts
SET status_reason = CASE status
        WHEN 'cooldown' THEN 'rate_limited'
        WHEN 'exhausted' THEN 'account_quota_exhausted'
        WHEN 'disabled' THEN 'existing_disabled'
        ELSE 'healthy'
    END,
    status_changed_at = created_at;
