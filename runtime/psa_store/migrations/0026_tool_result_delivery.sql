-- Tool results that delivered an agent message (send_message, create_agent)
-- record the recipient and request ID for clients. Neither is model-facing.
ALTER TABLE event_tool_result ADD COLUMN delivery_agent_id INTEGER REFERENCES agents(id);
ALTER TABLE event_tool_result ADD COLUMN delivery_request_id TEXT
    CHECK (delivery_request_id IS NULL OR delivery_request_id <> '');
