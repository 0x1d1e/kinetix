-- Persist each integration's declared feature ceiling with its host-owned provider.
ALTER TABLE providers
ADD COLUMN integration_features TEXT
CHECK (integration_features IS NULL OR json_valid(integration_features));
