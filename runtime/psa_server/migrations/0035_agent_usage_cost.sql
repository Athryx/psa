-- Reconstruct each agent's cumulative cost over its bounded lineage, matching
-- `add_usage`: inference results always count, compactions only when they
-- reported usage, and decision calls unless they failed. Any counted call
-- without a cost makes the total unknown.
WITH RECURSIVE lineage(target_id, agent_id, upper_seq) AS (
    SELECT id, id, next_event_seq - 1 FROM agents
    UNION
    SELECT l.target_id, a.parent_agent_id, MIN(l.upper_seq, a.first_event_seq - 1)
    FROM lineage l JOIN agents a ON a.id = l.agent_id
    WHERE a.parent_agent_id IS NOT NULL
), calls AS (
    SELECT event_pk, cost FROM event_inference_result_end
    UNION ALL
    SELECT event_pk, cost FROM event_compaction
    WHERE input_tokens IS NOT NULL OR output_tokens IS NOT NULL OR total_tokens IS NOT NULL
        OR cached_tokens IS NOT NULL OR cache_write_tokens IS NOT NULL
        OR reasoning_tokens IS NOT NULL OR cost IS NOT NULL OR usage_details <> '{}'
    UNION ALL
    SELECT event_pk, CASE
        WHEN json_type(value, '$.usage.cost') IN ('integer', 'real')
        THEN json_extract(value, '$.usage.cost')
    END
    FROM event_decision_call
    WHERE json_type(value, '$.outcome') = 'text'
), visible AS (
    SELECT l.target_id, c.cost
    FROM lineage l JOIN agents a ON a.id = l.agent_id
    JOIN events e ON e.agent_id = l.agent_id
        AND e.event_seq BETWEEN a.first_event_seq AND l.upper_seq
    JOIN calls c ON c.event_pk = e.event_pk
)
UPDATE agents AS a SET usage = json_set(usage, '$.cost', (
    SELECT CASE WHEN COUNT(*) > COUNT(cost) THEN NULL ELSE COALESCE(SUM(cost), 0.0) END
    FROM visible WHERE target_id = a.id
));
