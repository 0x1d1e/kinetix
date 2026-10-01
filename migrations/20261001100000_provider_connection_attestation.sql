ALTER TABLE providers
ADD COLUMN connection_parameters_attested INTEGER NOT NULL DEFAULT 1
CHECK (connection_parameters_attested IN (0, 1));

UPDATE providers
SET connection_parameters_attested = 0
WHERE (source_plugin_id IS NOT NULL OR source_integration_id IS NOT NULL)
  AND connection_parameters IS NOT NULL;
