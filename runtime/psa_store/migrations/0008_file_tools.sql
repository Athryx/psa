-- Extend the persisted tool discriminator for the native file tools.
CREATE TABLE event_tool_definition_file_tools (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'tool_definition' CHECK (kind = 'tool_definition'),
    tool_id TEXT NOT NULL CHECK (tool_id IN ('noop', 'read', 'edit', 'write')),
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    parameters TEXT NOT NULL CHECK (json_valid(parameters)),
    strict INTEGER CHECK (strict IN (0,1)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);

INSERT INTO event_tool_definition_file_tools
    (event_pk, kind, tool_id, name, description, parameters, strict)
SELECT event_pk, kind, tool_id, name, description, parameters, strict
FROM event_tool_definition;

DROP TABLE event_tool_definition;
ALTER TABLE event_tool_definition_file_tools RENAME TO event_tool_definition;
