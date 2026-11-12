-- Deleted sandboxes keep their record so their nested probe and its sessions stay
-- associated with the manager. Names only need to be unique among live sandboxes.
CREATE TABLE sandboxes_new (
    probe_id TEXT PRIMARY KEY NOT NULL,
    manager_id TEXT NOT NULL REFERENCES probes(id),
    name TEXT NOT NULL,
    image TEXT NOT NULL,
    mounts TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    deleted_at INTEGER
);
INSERT INTO sandboxes_new (probe_id, manager_id, name, image, mounts, created_at)
    SELECT probe_id, manager_id, name, image, mounts, created_at FROM sandboxes;
DROP TABLE sandboxes;
ALTER TABLE sandboxes_new RENAME TO sandboxes;
CREATE UNIQUE INDEX sandboxes_live_name ON sandboxes(manager_id, name) WHERE deleted_at IS NULL;
