-- CodeMode tool definitions share the public tool-definition payload.
CREATE TABLE event_tool_definition_codemode (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'tool_definition' CHECK (kind = 'tool_definition'),
    tool_id TEXT NOT NULL CHECK (tool_id IN (
        'noop', 'read', 'edit', 'write', 'bash', 'interact_bash', 'close_bash',
        'set_goal', 'set_goal_status', 'create_agent', 'send_message', 'wait_message', 'list_agents',
        'ask_question', 'record_result', 'codemode'
    )),
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    parameters TEXT NOT NULL CHECK (json_valid(parameters)),
    strict INTEGER CHECK (strict IN (0,1)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
INSERT INTO event_tool_definition_codemode (event_pk, kind, tool_id, name, description, parameters, strict)
SELECT event_pk, kind, tool_id, name, description, parameters, strict FROM event_tool_definition;
DROP TABLE event_tool_definition;
ALTER TABLE event_tool_definition_codemode RENAME TO event_tool_definition;

-- Internal state only: never included in event payloads or public event projections.
CREATE TABLE codemode_snapshots (
    event_pk INTEGER PRIMARY KEY REFERENCES event_tool_result(event_pk),
    snapshot BLOB NOT NULL
);
