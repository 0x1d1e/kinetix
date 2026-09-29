ALTER TABLE route_traces ADD COLUMN stream_outcome TEXT;
ALTER TABLE route_traces ADD COLUMN terminal_failure_kind TEXT;
ALTER TABLE route_traces ADD COLUMN fallback_allowed INTEGER;
