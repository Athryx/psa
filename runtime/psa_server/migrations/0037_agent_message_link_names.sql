-- Agent message links record both agents' names as captured at send time, on
-- the received message and on the delivering tool result. Each event's own
-- agent is one side of the link, so only its name is stored there. Existing
-- rows take the agents' current names.
ALTER TABLE event_message ADD COLUMN recipient_name TEXT;
UPDATE event_message
SET recipient_name = (
    SELECT a.name
    FROM events e
    JOIN agents a ON a.id = e.agent_id
    WHERE e.event_pk = event_message.event_pk
)
WHERE message_type = 'subagent';

ALTER TABLE event_tool_result ADD COLUMN delivery_sender_name TEXT;
ALTER TABLE event_tool_result ADD COLUMN delivery_recipient_name TEXT;
UPDATE event_tool_result
SET
    delivery_sender_name = (
        SELECT a.name
        FROM events e
        JOIN agents a ON a.id = e.agent_id
        WHERE e.event_pk = event_tool_result.event_pk
    ),
    delivery_recipient_name = (
        SELECT a.name FROM agents a WHERE a.id = event_tool_result.delivery_agent_id
    )
WHERE delivery_agent_id IS NOT NULL;
