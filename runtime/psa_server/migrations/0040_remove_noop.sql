-- The Noop bundle and its tool were removed. Drop the bundle from stored
-- definitions, configs, and config snapshots, keeping the order of the rest.
-- Unit bundles are JSON strings, so text values are re-quoted.
UPDATE agent_definitions
SET bundles = (
    SELECT json_group_array(json(item)) FROM (
        SELECT CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item
        FROM json_each(agent_definitions.bundles)
        WHERE NOT (type = 'text' AND value = 'Noop')
        ORDER BY key
    )
)
WHERE EXISTS (
    SELECT 1 FROM json_each(agent_definitions.bundles) WHERE type = 'text' AND value = 'Noop'
);

UPDATE agent_configs
SET payload = json_set(payload, '$.bundles', (
    SELECT json_group_array(json(item)) FROM (
        SELECT CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item
        FROM json_each(agent_configs.payload, '$.bundles')
        WHERE NOT (type = 'text' AND value = 'Noop')
        ORDER BY key
    )
))
WHERE EXISTS (
    SELECT 1 FROM json_each(agent_configs.payload, '$.bundles')
    WHERE type = 'text' AND value = 'Noop'
);

UPDATE config_snapshots
SET config = json_set(config, '$.bundles', (
    SELECT json_group_array(json(item)) FROM (
        SELECT CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item
        FROM json_each(config_snapshots.config, '$.bundles')
        WHERE NOT (type = 'text' AND value = 'Noop')
        ORDER BY key
    )
))
WHERE EXISTS (
    SELECT 1 FROM json_each(config_snapshots.config, '$.bundles')
    WHERE type = 'text' AND value = 'Noop'
);

-- Stored noop tool definitions become log events, so every event keeps its ID
-- and transcripts stay contiguous. Children reference (event_pk, kind), so the
-- payload moves out before the event's kind changes.
CREATE TEMP TABLE noop_definitions AS
SELECT event_pk FROM event_tool_definition WHERE tool_id = 'noop';

DELETE FROM event_tool_definition WHERE event_pk IN (SELECT event_pk FROM noop_definitions);
UPDATE events SET kind = 'log' WHERE event_pk IN (SELECT event_pk FROM noop_definitions);
INSERT INTO event_log (event_pk, error, message)
SELECT event_pk, 0, 'Definition of the removed noop tool.' FROM noop_definitions;

DROP TABLE noop_definitions;
