-- Persist each integration's input and upstream protocol declarations with its provider.
ALTER TABLE providers
ADD COLUMN integration_protocols TEXT
CHECK (integration_protocols IS NULL OR json_valid(integration_protocols));
