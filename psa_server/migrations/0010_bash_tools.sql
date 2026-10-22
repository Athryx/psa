-- Register the process tools supplied by the Bash bundle.
CREATE TABLE event_tool_definition_bash (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'tool_definition' CHECK (kind = 'tool_definition'),
    tool_id TEXT NOT NULL CHECK (tool_id IN ('noop', 'read', 'edit', 'write', 'bash', 'interact_bash', 'close_bash')),
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    parameters TEXT NOT NULL CHECK (json_valid(parameters)),
    strict INTEGER CHECK (strict IN (0,1)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);

INSERT INTO event_tool_definition_bash
    (event_pk, kind, tool_id, name, description, parameters, strict)
SELECT event_pk, kind, tool_id, name, description, parameters, strict
FROM event_tool_definition;

DROP TABLE event_tool_definition;
ALTER TABLE event_tool_definition_bash RENAME TO event_tool_definition;
