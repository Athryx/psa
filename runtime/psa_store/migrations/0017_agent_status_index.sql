-- Support fork-aware lookups of the newest model, context and inference markers.
CREATE INDEX events_agent_kind_sequence ON events(agent_id, kind, event_seq);
