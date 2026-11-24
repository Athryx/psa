-- no-transaction
-- Decision-model calls are recorded in the transcript. SQLite cannot alter the
-- kind CHECK in place, so rebuild `events` with foreign keys disabled; payload
-- tables keep referencing it by name and primary keys are preserved. Idempotent,
-- since the migration is only recorded after this script commits.
PRAGMA foreign_keys = OFF;
BEGIN;

DROP TABLE IF EXISTS events_decision_call_rebuild;
CREATE TABLE events_decision_call_rebuild (
    event_pk INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    event_seq INTEGER NOT NULL CHECK (event_seq >= 1),
    session_position INTEGER NOT NULL CHECK (session_position >= 1),
    timestamp_us INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('status_change', 'model', 'new_context', 'message', 'thinking', 'provider_item', 'tool_definition', 'tool_call', 'tool_result', 'compaction', 'log', 'result', 'inference_start', 'inference_end', 'command', 'decision_call')),
    UNIQUE (agent_id, event_seq),
    UNIQUE (event_pk, kind)
);
INSERT INTO events_decision_call_rebuild (event_pk, agent_id, event_seq, session_position, timestamp_us, kind)
SELECT event_pk, agent_id, event_seq, session_position, timestamp_us, kind FROM events;
DROP TABLE events;
ALTER TABLE events_decision_call_rebuild RENAME TO events;
CREATE INDEX events_position ON events(agent_id, session_position);
CREATE INDEX events_result_sequence ON events(agent_id, event_seq) WHERE kind = 'result';
CREATE INDEX events_agent_kind_sequence ON events(agent_id, kind, event_seq);

CREATE TABLE IF NOT EXISTS event_decision_call (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'decision_call' CHECK (kind = 'decision_call'),
    value TEXT NOT NULL CHECK (json_valid(value)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);

COMMIT;
PRAGMA foreign_keys = ON;
