CREATE INDEX events_result_sequence ON events(agent_id, event_seq) WHERE kind = 'result';

CREATE TABLE event_result (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'result' CHECK (kind = 'result'),
    value TEXT NOT NULL CHECK (json_valid(value)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
