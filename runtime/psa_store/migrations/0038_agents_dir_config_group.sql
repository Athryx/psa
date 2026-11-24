-- The `.agents` directory config group is `AgentsDir`, not `Project`, so it is
-- not confused with a session's `project_id` label. Stored config IDs and
-- configs embed the group as JSON; rename the variant in place. Discovered
-- configs are rewritten with the new inclusion rules on the next refresh.
ALTER TABLE session_projects RENAME TO session_agents_dirs;
UPDATE session_agents_dirs
SET config_group = replace(config_group, '{"Project":', '{"AgentsDir":');
UPDATE agent_configs
SET
    config_key = replace(config_key, '{"Project":', '{"AgentsDir":'),
    config_group = replace(config_group, '{"Project":', '{"AgentsDir":'),
    payload = replace(payload, '{"Project":', '{"AgentsDir":');
UPDATE registered_skills
SET
    config_key = replace(config_key, '{"Project":', '{"AgentsDir":'),
    config_group = replace(config_group, '{"Project":', '{"AgentsDir":'),
    payload = replace(payload, '{"Project":', '{"AgentsDir":');
UPDATE config_snapshots
SET config = replace(config, '{"Project":', '{"AgentsDir":');
