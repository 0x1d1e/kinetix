CREATE TABLE IF NOT EXISTS plugin_catalog_pins (
    plugin_id   TEXT PRIMARY KEY,
    version     TEXT NOT NULL,
    pinned_at   TEXT NOT NULL,
    FOREIGN KEY (plugin_id) REFERENCES plugins(id) ON DELETE CASCADE
);
