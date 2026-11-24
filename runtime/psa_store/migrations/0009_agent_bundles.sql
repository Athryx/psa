-- Existing immutable definitions keep their explicit tools and hooks unchanged.
ALTER TABLE agent_definitions
ADD COLUMN bundles TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(bundles));
