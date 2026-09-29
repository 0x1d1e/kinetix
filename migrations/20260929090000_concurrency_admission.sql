ALTER TABLE virtual_keys
ADD COLUMN max_concurrent_requests INTEGER
CHECK (max_concurrent_requests IS NULL OR max_concurrent_requests > 0);

ALTER TABLE routes
ADD COLUMN max_concurrent_requests INTEGER
CHECK (max_concurrent_requests IS NULL OR max_concurrent_requests > 0);
