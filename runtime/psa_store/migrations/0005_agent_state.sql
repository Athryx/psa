ALTER TABLE agents ADD COLUMN state TEXT NOT NULL DEFAULT 'idle'
    CHECK (state IN ('idle','running','interrupted'));

-- Reconstruct the latest visible lifecycle state, respecting each ancestor's
-- fork boundary. A stream without state-change events remains idle.
WITH RECURSIVE lineage(target_id, agent_id, upper_seq) AS (
    SELECT id, id, next_event_seq - 1 FROM agents
    UNION
    SELECT l.target_id, a.parent_agent_id, MIN(l.upper_seq, a.first_event_seq - 1)
    FROM lineage l JOIN agents a ON a.id = l.agent_id
    WHERE a.parent_agent_id IS NOT NULL
), states AS (
    SELECT l.target_id, s.status,
           ROW_NUMBER() OVER (PARTITION BY l.target_id ORDER BY e.event_seq DESC) AS priority
    FROM lineage l
    JOIN agents a ON a.id = l.agent_id
    JOIN events e ON e.agent_id = l.agent_id
        AND e.event_seq BETWEEN a.first_event_seq AND l.upper_seq
    JOIN event_status_change s ON s.event_pk = e.event_pk
)
UPDATE agents AS a
SET state = COALESCE((
    SELECT status FROM states WHERE target_id = a.id AND priority = 1
), 'idle');
