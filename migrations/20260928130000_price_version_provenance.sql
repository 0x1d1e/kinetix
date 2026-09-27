-- Immutable pricing snapshots carry their source so historical cost attribution
-- remains explainable even after model metadata changes.
ALTER TABLE price_versions ADD COLUMN source TEXT NOT NULL DEFAULT 'operator';
ALTER TABLE price_versions ADD COLUMN source_metadata TEXT NOT NULL DEFAULT '{}';

CREATE INDEX IF NOT EXISTS idx_price_versions_model_created
    ON price_versions(model_id, created_at DESC);
