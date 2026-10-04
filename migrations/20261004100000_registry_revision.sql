-- Monotonic control-plane revision bumped by every write to a table the
-- registry snapshot is built from. The registry reload loop polls this single
-- row and rebuilds its snapshot only when it changes (#203). Any later
-- migration that rebuilds one of these tables must recreate its triggers.
CREATE TABLE IF NOT EXISTS registry_revision (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL
);
INSERT OR IGNORE INTO registry_revision (id, revision) VALUES (1, 0);

CREATE TRIGGER IF NOT EXISTS registry_revision_providers_insert AFTER INSERT ON providers
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_providers_update AFTER UPDATE ON providers
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_providers_delete AFTER DELETE ON providers
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_accounts_insert AFTER INSERT ON accounts
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_accounts_update AFTER UPDATE ON accounts
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_accounts_delete AFTER DELETE ON accounts
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_models_insert AFTER INSERT ON models
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_models_update AFTER UPDATE ON models
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_models_delete AFTER DELETE ON models
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_aliases_insert AFTER INSERT ON aliases
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_aliases_update AFTER UPDATE ON aliases
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_aliases_delete AFTER DELETE ON aliases
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_routes_insert AFTER INSERT ON routes
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_routes_update AFTER UPDATE ON routes
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_routes_delete AFTER DELETE ON routes
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_route_targets_insert AFTER INSERT ON route_targets
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_route_targets_update AFTER UPDATE ON route_targets
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;

CREATE TRIGGER IF NOT EXISTS registry_revision_route_targets_delete AFTER DELETE ON route_targets
BEGIN UPDATE registry_revision SET revision = revision + 1 WHERE id = 1; END;
