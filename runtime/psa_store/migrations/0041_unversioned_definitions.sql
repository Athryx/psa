-- no-transaction
-- Agent definitions are immutable snapshots, so the version chain is removed.
-- SQLite cannot drop a foreign-key column in place, so rebuild the table with
-- foreign keys disabled; `agents` and `config_snapshots` keep referencing it by
-- name and primary keys are preserved. Idempotent, since the migration is only
-- recorded after this script commits.
PRAGMA foreign_keys = OFF;
BEGIN;

DROP TABLE IF EXISTS agent_definitions_unversioned;
CREATE TABLE agent_definitions_unversioned (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    selectable BOOLEAN NOT NULL,
    model_config TEXT CHECK (model_config IS NULL OR json_valid(model_config)),
    skills TEXT NOT NULL CHECK (json_valid(skills)),
    permitted_subagents TEXT NOT NULL CHECK (json_valid(permitted_subagents)),
    bundles TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(bundles))
);
INSERT INTO agent_definitions_unversioned
    (id, name, description, selectable, model_config, skills, permitted_subagents, bundles)
    SELECT id, name, description, selectable, model_config, skills, permitted_subagents, bundles
    FROM agent_definitions;
DROP TABLE agent_definitions;
ALTER TABLE agent_definitions_unversioned RENAME TO agent_definitions;

COMMIT;
PRAGMA foreign_keys = ON;
