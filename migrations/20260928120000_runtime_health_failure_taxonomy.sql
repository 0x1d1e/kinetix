ALTER TABLE target_health_buckets
    ADD COLUMN auth_errors INTEGER NOT NULL DEFAULT 0;

ALTER TABLE target_health_buckets
    ADD COLUMN target_errors INTEGER NOT NULL DEFAULT 0;

ALTER TABLE target_health_buckets
    ADD COLUMN bad_requests INTEGER NOT NULL DEFAULT 0;
