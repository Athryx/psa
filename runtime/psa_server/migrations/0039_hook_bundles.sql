-- Hooks are no longer configured directly; bundles install all of them. Convert
-- stored hook lists into the equivalent bundles appended after existing ones:
-- DefaultStartup or ToolDefinitions become Startup (with base instructions, and
-- ProjectContext, Cwd, and Skills as its flags; it also installs SessionModelUpdate), ExtraSystemPrompt
-- hooks become one ExtraSystemPrompt bundle joining their prompts, and an
-- after_request StopWithoutTools becomes the StopWithoutTools bundle. Then drop
-- the hook lists from definitions, configs, and config snapshots.

CREATE TEMP TABLE hook_migration (
    kind INTEGER NOT NULL,
    row_key TEXT NOT NULL,
    payload TEXT NOT NULL,
    bundles TEXT
);

-- 0: agent_definitions, 1: agent_configs, 2: config_snapshots. Each payload has
-- the shape {"bundles": [...], "hooks": {"<stage>": [...]}}.
INSERT INTO hook_migration (kind, row_key, payload)
SELECT 0, CAST(id AS TEXT), json_object(
    'bundles', json(bundles),
    'hooks', json_object(
        'agent_create', json(agent_create_hooks),
        'session_update', json(session_update_hooks),
        'before_request', json(before_request_hooks),
        'after_request', json(after_request_hooks)
    )
)
FROM agent_definitions;
INSERT INTO hook_migration (kind, row_key, payload)
SELECT 1, config_key, payload FROM agent_configs;
INSERT INTO hook_migration (kind, row_key, payload)
SELECT 2, CAST(definition_id AS TEXT), config FROM config_snapshots;

-- Unit hooks are strings directly inside a stage array, at a path without `[`.
UPDATE hook_migration
SET bundles = (
    SELECT json_group_array(json(item)) FROM (
        SELECT 0 AS part, key, CASE WHEN type = 'text' THEN json_quote(value) ELSE value END AS item
        FROM json_each(hook_migration.payload, '$.bundles')
        UNION ALL
        SELECT 1, 0, json_object('Startup', json_object(
            'base_prompt', json('true'),
            'project_context', json(CASE WHEN EXISTS (
                SELECT 1 FROM json_tree(hook_migration.payload, '$.hooks')
                WHERE type = 'text' AND value = 'ProjectContext' AND path NOT LIKE '%[%'
            ) THEN 'true' ELSE 'false' END),
            'cwd', json(CASE WHEN EXISTS (
                SELECT 1 FROM json_tree(hook_migration.payload, '$.hooks')
                WHERE type = 'text' AND value = 'Cwd' AND path NOT LIKE '%[%'
            ) THEN 'true' ELSE 'false' END),
            'skills', json(CASE WHEN EXISTS (
                SELECT 1 FROM json_tree(hook_migration.payload, '$.hooks')
                WHERE type = 'text' AND value = 'Skills' AND path NOT LIKE '%[%'
            ) THEN 'true' ELSE 'false' END)
        ))
        WHERE EXISTS (
            SELECT 1 FROM json_tree(hook_migration.payload, '$.hooks')
            WHERE type = 'text' AND value IN ('DefaultStartup', 'ToolDefinitions')
                AND path NOT LIKE '%[%'
        )
        UNION ALL
        SELECT 2, 0, json_object('ExtraSystemPrompt', json_object('prompt', (
            SELECT group_concat(json_extract(value, '$.ExtraSystemPrompt.prompt'), char(10) || char(10))
            FROM json_tree(hook_migration.payload, '$.hooks')
            WHERE type = 'object' AND json_extract(value, '$.ExtraSystemPrompt.prompt') IS NOT NULL
        )))
        WHERE EXISTS (
            SELECT 1 FROM json_tree(hook_migration.payload, '$.hooks')
            WHERE type = 'object' AND json_extract(value, '$.ExtraSystemPrompt.prompt') IS NOT NULL
        )
        UNION ALL
        SELECT 3, 0, json_quote('StopWithoutTools')
        WHERE EXISTS (
            SELECT 1 FROM json_each(hook_migration.payload, '$.hooks.after_request')
            WHERE type = 'text' AND value = 'StopWithoutTools'
        )
        ORDER BY part, key
    )
);

UPDATE agent_definitions
SET bundles = (
    SELECT bundles FROM hook_migration
    WHERE kind = 0 AND row_key = CAST(agent_definitions.id AS TEXT)
);
UPDATE agent_configs
SET payload = (
    SELECT json_set(json_remove(agent_configs.payload, '$.hooks'), '$.bundles', json(hook_migration.bundles))
    FROM hook_migration
    WHERE kind = 1 AND row_key = agent_configs.config_key
);
UPDATE config_snapshots
SET config = (
    SELECT json_set(json_remove(config_snapshots.config, '$.hooks'), '$.bundles', json(hook_migration.bundles))
    FROM hook_migration
    WHERE kind = 2 AND row_key = CAST(config_snapshots.definition_id AS TEXT)
);

DROP TABLE hook_migration;

ALTER TABLE agent_definitions DROP COLUMN agent_create_hooks;
ALTER TABLE agent_definitions DROP COLUMN session_update_hooks;
ALTER TABLE agent_definitions DROP COLUMN before_request_hooks;
ALTER TABLE agent_definitions DROP COLUMN after_request_hooks;
