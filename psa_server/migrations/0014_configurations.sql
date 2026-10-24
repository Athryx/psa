-- Mutable registry is separate from immutable legacy definition rows.
CREATE TABLE agent_configs (
    config_key TEXT PRIMARY KEY NOT NULL,
    config_group TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    discovered BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE TABLE registered_skills (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    config_key TEXT UNIQUE NOT NULL,
    config_group TEXT NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    active BOOLEAN NOT NULL DEFAULT TRUE,
    discovered BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE TABLE config_snapshots (
    definition_id INTEGER PRIMARY KEY REFERENCES agent_definitions(id),
    config TEXT NOT NULL CHECK(json_valid(config)),
    skill_ids TEXT NOT NULL CHECK(json_valid(skill_ids))
);
CREATE TABLE config_snapshot_cache (
    signature TEXT PRIMARY KEY NOT NULL,
    definition_id INTEGER NOT NULL REFERENCES config_snapshots(definition_id)
);
CREATE TABLE session_projects (
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    config_group TEXT NOT NULL CHECK(json_valid(config_group))
);
