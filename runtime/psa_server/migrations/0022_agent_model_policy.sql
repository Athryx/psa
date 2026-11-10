-- Definitions store a model policy instead of a resolved configuration.
-- Legacy pinned configurations become fully fixed policies; NULL still follows the session.
UPDATE agent_definitions
SET model_config = json_object(
    'Fixed', json_object(
        'model', json(json_extract(model_config, '$.model')),
        'effort', json_object('Fixed', json_extract(model_config, '$.effort'))
    )
)
WHERE model_config IS NOT NULL AND json_extract(model_config, '$.Fixed') IS NULL;
