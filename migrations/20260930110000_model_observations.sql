-- Durable source observations are immutable evidence, separate from effective model configuration.
CREATE TABLE IF NOT EXISTS model_observations (
    id            TEXT PRIMARY KEY,
    model_id      TEXT NOT NULL,
    kind          TEXT NOT NULL,
    source        TEXT NOT NULL,
    observed_at   TEXT NOT NULL,
    scope_json    TEXT NOT NULL DEFAULT '{}',
    value_json    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_model_observations_model_time
    ON model_observations(model_id, observed_at DESC, id DESC);

CREATE TRIGGER IF NOT EXISTS model_observations_immutable_update
BEFORE UPDATE ON model_observations
BEGIN
    SELECT RAISE(ABORT, 'model observations are immutable');
END;

CREATE TRIGGER IF NOT EXISTS model_observations_immutable_delete
BEFORE DELETE ON model_observations
BEGIN
    SELECT RAISE(ABORT, 'model observations are append-only');
END;
