-- Subagent message delivery moves from a JSON `source` object to typed columns.
-- Every subagent message records the request that delivered it; no other message
-- type has a sender. Legacy subagent messages without a request ID get a stable
-- synthetic one, since they predate request tracking.
CREATE TABLE event_message_sender (
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
    CHECK ((message_type = 'subagent') = (request_id IS NOT NULL)),
    CHECK (message_type = 'subagent' OR (source_agent_id IS NULL AND source_name IS NULL)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
INSERT INTO event_message_sender (
    event_pk, kind, message_type, delivery, content, metadata, description,
    source_agent_id, source_name, request_id
)
SELECT
    event_pk, kind, message_type, delivery, content, metadata, description,
    CASE WHEN message_type = 'subagent' THEN json_extract(source, '$.agent_id') END,
    CASE WHEN message_type = 'subagent' THEN json_extract(source, '$.name') END,
    CASE WHEN message_type = 'subagent'
        THEN COALESCE(json_extract(source, '$.request_id'), 'legacy-' || event_pk)
    END
FROM event_message;
DROP TABLE event_message;
ALTER TABLE event_message_sender RENAME TO event_message;
