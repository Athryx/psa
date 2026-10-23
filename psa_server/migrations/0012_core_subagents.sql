-- Subagent ancestry is distinct from parent_agent_id, which denotes fork lineage.
ALTER TABLE agents ADD COLUMN subagent_parent_id INTEGER REFERENCES agents(id);
CREATE INDEX agents_subagent_parent ON agents(session_id, subagent_parent_id);

CREATE TABLE event_message_subagents (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'message' CHECK (kind = 'message'),
    message_type TEXT NOT NULL CHECK (message_type IN ('human','agent','harness','system','subagent')),
    delivery TEXT CHECK (delivery IN ('queue','steer')),
    content TEXT NOT NULL CHECK (json_valid(content) AND json_type(content) = 'array'),
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    source TEXT CHECK (source IS NULL OR (json_valid(source) AND json_type(source) = 'object')),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
INSERT INTO event_message_subagents (event_pk,kind,message_type,delivery,content,metadata)
SELECT event_pk,kind,message_type,delivery,content,metadata FROM event_message;
DROP TABLE event_message;
ALTER TABLE event_message_subagents RENAME TO event_message;
