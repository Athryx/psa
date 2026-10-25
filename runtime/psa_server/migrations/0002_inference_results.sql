-- Keep the original envelope discriminator values so existing foreign keys and
-- event IDs remain valid. Rust names and payload tables now describe results.
ALTER TABLE event_inference_start RENAME TO event_inference_result_start;
ALTER TABLE event_inference_end RENAME TO event_inference_result_end;

-- Nullable only for legacy records lacking any preceding model configuration.
-- New writes always supply the complete snapshot; unreadable legacy snapshots
-- are reported explicitly by the codec rather than inventing a model.
ALTER TABLE event_inference_result_start ADD COLUMN model_provider TEXT
    CHECK (model_provider IN ('openai_codex','openrouter'));
ALTER TABLE event_inference_result_start ADD COLUMN model_name TEXT
    CHECK (length(trim(model_name)) > 0 AND trim(model_name) = model_name);
ALTER TABLE event_inference_result_start ADD COLUMN model_effort TEXT
    CHECK (model_effort IN ('low','medium','high','xhigh','max'))
    CHECK ((model_provider IS NULL AND model_name IS NULL AND model_effort IS NULL)
        OR (model_provider IS NOT NULL AND model_name IS NOT NULL AND model_effort IS NOT NULL));

-- Recover the effective model at each old request-start marker, including model
-- events inherited through a fork. Every ancestor is capped at the fork point.
WITH RECURSIVE lineage(result_pk, agent_id, upper_seq) AS (
    SELECT e.event_pk, e.agent_id, e.event_seq - 1
    FROM events e JOIN event_inference_result_start s ON s.event_pk = e.event_pk
    UNION
    SELECT l.result_pk, a.parent_agent_id, MIN(l.upper_seq, a.first_event_seq - 1)
    FROM lineage l JOIN agents a ON a.id = l.agent_id
    WHERE a.parent_agent_id IS NOT NULL
), models AS (
    SELECT l.result_pk, m.provider, m.model, m.effort,
           ROW_NUMBER() OVER (PARTITION BY l.result_pk ORDER BY e.event_seq DESC) AS priority
    FROM lineage l
    JOIN agents a ON a.id = l.agent_id
    JOIN events e ON e.agent_id = l.agent_id
        AND e.event_seq BETWEEN a.first_event_seq AND l.upper_seq
    JOIN event_model m ON m.event_pk = e.event_pk
)
UPDATE event_inference_result_start AS s
SET (model_provider, model_name, model_effort) = (
    SELECT provider, model, effort FROM models
    WHERE result_pk = s.event_pk AND priority = 1
);
