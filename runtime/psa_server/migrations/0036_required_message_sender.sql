-- Every subagent message now names its sending agent: the server resolves the
-- sender's name from its ID at submission. Legacy rows without a captured name
-- take the sender's current name. A legacy subagent row without a sender ID
-- violates the new check and fails this migration.
CREATE TABLE event_message_required_sender (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'message' CHECK (kind = 'message'),
    message_type TEXT NOT NULL CHECK (message_type IN ('human','agent','harness','system','subagent')),
    delivery TEXT CHECK (delivery IN ('queue','steer')),
    content TEXT NOT NULL CHECK (json_valid(content) AND json_type(content) = 'array'),
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    description TEXT,
    source_agent_id INTEGER REFERENCES agents(id),
    source_name TEXT,
    request_id TEXT CHECK (request_id IS NULL OR request_id <> ''),
    CHECK (
        CASE WHEN message_type = 'subagent'
            THEN request_id IS NOT NULL AND source_agent_id IS NOT NULL AND source_name IS NOT NULL
            ELSE request_id IS NULL AND source_agent_id IS NULL AND source_name IS NULL
        END
    ),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
INSERT INTO event_message_required_sender (
    event_pk, kind, message_type, delivery, content, metadata, description,
    source_agent_id, source_name, request_id
)
SELECT
    m.event_pk, m.kind, m.message_type, m.delivery, m.content, m.metadata, m.description,
    m.source_agent_id,
    CASE WHEN m.message_type = 'subagent' THEN COALESCE(m.source_name, a.name) END,
    m.request_id
FROM event_message m
LEFT JOIN agents a ON a.id = m.source_agent_id;
DROP TABLE event_message;
ALTER TABLE event_message_required_sender RENAME TO event_message;
