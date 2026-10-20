ALTER TABLE agents ADD COLUMN usage TEXT NOT NULL DEFAULT
    '{"input_tokens":0,"output_tokens":0,"total_tokens":0,"cached_input_tokens":0,"reasoning_output_tokens":0}'
    CHECK (json_valid(usage) AND json_type(usage) = 'object');

-- Each stream inherits only its bounded prefix, not its parent's current total.
WITH RECURSIVE lineage(target_id, agent_id, upper_seq) AS (
    SELECT id, id, next_event_seq - 1 FROM agents
    UNION
    SELECT l.target_id, a.parent_agent_id, MIN(l.upper_seq, a.first_event_seq - 1)
    FROM lineage l JOIN agents a ON a.id = l.agent_id
    WHERE a.parent_agent_id IS NOT NULL
), calls AS (
    SELECT event_pk, input_tokens, output_tokens, total_tokens, cached_tokens, reasoning_tokens
    FROM event_inference_result_end
    UNION ALL
    SELECT event_pk, input_tokens, output_tokens, total_tokens, cached_tokens, reasoning_tokens
    FROM event_compaction
), visible AS (
    SELECT l.target_id, c.* FROM lineage l
    JOIN agents a ON a.id = l.agent_id
    JOIN events e ON e.agent_id = l.agent_id
        AND e.event_seq BETWEEN a.first_event_seq AND l.upper_seq
    JOIN calls c ON c.event_pk = e.event_pk
)
UPDATE agents AS a SET usage = (
    SELECT json_object(
        'input_tokens', CASE WHEN COUNT(*) > COUNT(input_tokens) THEN NULL ELSE COALESCE(SUM(input_tokens),0) END,
        'output_tokens', CASE WHEN COUNT(*) > COUNT(output_tokens) THEN NULL ELSE COALESCE(SUM(output_tokens),0) END,
        'total_tokens', CASE WHEN COUNT(*) > COUNT(total_tokens) THEN NULL ELSE COALESCE(SUM(total_tokens),0) END,
        'cached_input_tokens', CASE WHEN COUNT(*) > COUNT(cached_tokens) THEN NULL ELSE COALESCE(SUM(cached_tokens),0) END,
        'reasoning_output_tokens', CASE WHEN COUNT(*) > COUNT(reasoning_tokens) THEN NULL ELSE COALESCE(SUM(reasoning_tokens),0) END
    ) FROM visible WHERE target_id = a.id
);
