-- Compaction moved from a before_request hook to a bundle that also installs the
-- manual Compact command handler. Move stored hook configurations into bundles,
-- preserving their settings; inherited bundle lists already gain the default.

UPDATE agent_definitions
SET bundles = (
        SELECT json_group_array(json(item)) FROM (
            SELECT 0 AS part, key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item FROM json_each(agent_definitions.bundles)
            UNION ALL
            SELECT 1, key, value FROM json_each(agent_definitions.before_request_hooks)
            WHERE type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL
                AND NOT EXISTS (
                    SELECT 1 FROM json_each(agent_definitions.bundles) AS existing
                    WHERE existing.type = 'object'
                        AND json_extract(existing.value, '$.Compaction') IS NOT NULL
                )
            ORDER BY part, key
        )
    ),
    before_request_hooks = (
        SELECT json_group_array(json(item)) FROM (
            SELECT key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item FROM json_each(agent_definitions.before_request_hooks)
            WHERE NOT (type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL)
            ORDER BY key
        )
    )
WHERE EXISTS (SELECT 1 FROM json_each(agent_definitions.before_request_hooks) WHERE type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL);

UPDATE agent_configs
SET payload = CASE
    WHEN json_type(payload, '$.bundles') IS NULL
        THEN json_set(payload, '$.hooks.before_request', json((
        SELECT json_group_array(json(item)) FROM (
            SELECT key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item FROM json_each(agent_configs.payload, '$.hooks.before_request')
            WHERE NOT (type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL)
            ORDER BY key
        )
    )))
    ELSE json_set(
        payload,
        '$.hooks.before_request', json((
        SELECT json_group_array(json(item)) FROM (
            SELECT key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item FROM json_each(agent_configs.payload, '$.hooks.before_request')
            WHERE NOT (type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL)
            ORDER BY key
        )
    )),
        '$.bundles', json((
        SELECT json_group_array(json(item)) FROM (
            SELECT 0 AS part, key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item FROM json_each(agent_configs.payload, '$.bundles')
            UNION ALL
            SELECT 1, key, value FROM json_each(agent_configs.payload, '$.hooks.before_request')
            WHERE type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL
                AND NOT EXISTS (
                    SELECT 1 FROM json_each(agent_configs.payload, '$.bundles') AS existing
                    WHERE existing.type = 'object'
                        AND json_extract(existing.value, '$.Compaction') IS NOT NULL
                )
            ORDER BY part, key
        )
    ))
    )
END
WHERE EXISTS (SELECT 1 FROM json_each(agent_configs.payload, '$.hooks.before_request') WHERE type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL);

UPDATE config_snapshots
SET config = CASE
    WHEN json_type(config, '$.bundles') IS NULL
        THEN json_set(config, '$.hooks.before_request', json((
        SELECT json_group_array(json(item)) FROM (
            SELECT key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item FROM json_each(config_snapshots.config, '$.hooks.before_request')
            WHERE NOT (type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL)
            ORDER BY key
        )
    )))
    ELSE json_set(
        config,
        '$.hooks.before_request', json((
        SELECT json_group_array(json(item)) FROM (
            SELECT key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item FROM json_each(config_snapshots.config, '$.hooks.before_request')
            WHERE NOT (type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL)
            ORDER BY key
        )
    )),
        '$.bundles', json((
        SELECT json_group_array(json(item)) FROM (
            SELECT 0 AS part, key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item FROM json_each(config_snapshots.config, '$.bundles')
            UNION ALL
            SELECT 1, key, value FROM json_each(config_snapshots.config, '$.hooks.before_request')
            WHERE type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL
                AND NOT EXISTS (
                    SELECT 1 FROM json_each(config_snapshots.config, '$.bundles') AS existing
                    WHERE existing.type = 'object'
                        AND json_extract(existing.value, '$.Compaction') IS NOT NULL
                )
            ORDER BY part, key
        )
    ))
    )
END
WHERE EXISTS (SELECT 1 FROM json_each(config_snapshots.config, '$.hooks.before_request') WHERE type = 'object' AND json_extract(value, '$.Compaction') IS NOT NULL);
