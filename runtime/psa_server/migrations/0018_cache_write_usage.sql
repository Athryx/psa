-- Reconstruct exact cache-write totals over each agent's bounded lineage.
-- Missing or invalid provider counts remain unknown, not zero.
WITH RECURSIVE lineage(target_id, agent_id, upper_seq) AS (
    SELECT id, id, next_event_seq - 1 FROM agents
    UNION
    SELECT l.target_id, a.parent_agent_id, MIN(l.upper_seq, a.first_event_seq - 1)
    FROM lineage l JOIN agents a ON a.id = l.agent_id
    WHERE a.parent_agent_id IS NOT NULL
), calls AS (
    SELECT event_pk, usage_details FROM event_inference_result_end
    UNION ALL
    SELECT event_pk, usage_details FROM event_compaction
), visible AS (
    SELECT l.target_id, CASE
        WHEN json_type(c.usage_details, '$.prompt_tokens_details.cache_write_tokens') = 'integer'
          AND json_extract(c.usage_details, '$.prompt_tokens_details.cache_write_tokens') >= 0
        THEN json_extract(c.usage_details, '$.prompt_tokens_details.cache_write_tokens')
    END AS cache_write
    FROM lineage l JOIN agents a ON a.id = l.agent_id
    JOIN events e ON e.agent_id = l.agent_id
        AND e.event_seq BETWEEN a.first_event_seq AND l.upper_seq
    JOIN calls c ON c.event_pk = e.event_pk
)
UPDATE agents AS a SET usage = json_set(usage, '$.cache_write_tokens', (
    SELECT CASE WHEN COUNT(*) > COUNT(cache_write) THEN NULL ELSE COALESCE(SUM(cache_write), 0) END
    FROM visible WHERE target_id = a.id
));
