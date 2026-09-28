-- Persist serving-economics scope separately from endpoint/model identity.
-- Direct provider prices are unsafe for OAuth, subscription, free-account, and
-- plugin-backed integrations unless the integration explicitly opts in.
ALTER TABLE providers
ADD COLUMN pricing_scope TEXT NOT NULL DEFAULT 'direct_api'
CHECK (pricing_scope IN ('direct_api', 'integration'));

UPDATE providers
SET pricing_scope = 'integration'
WHERE credential_mode <> 'manual'
   OR COALESCE(wire_plugin, '') <> ''
   OR COALESCE(credential_plugin, '') <> ''
   OR COALESCE(model_source_plugin, '') <> ''
   OR source_plugin_id IS NOT NULL
   OR source_integration_id IS NOT NULL;
