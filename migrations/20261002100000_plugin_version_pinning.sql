-- Operator-controlled plugin version pins.
ALTER TABLE plugins ADD COLUMN pinned_version TEXT;
