-- Docker sandboxes managed by probes; each runs a nested probe identified by probe_id.
CREATE TABLE sandboxes (
    probe_id TEXT PRIMARY KEY NOT NULL,
    manager_id TEXT NOT NULL REFERENCES probes(id),
    name TEXT NOT NULL,
    image TEXT NOT NULL,
    mounts TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (manager_id, name)
);
