CREATE TABLE probes (
    id TEXT PRIMARY KEY NOT NULL,
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    created_at INTEGER NOT NULL,
    last_connected_at INTEGER NOT NULL
);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    parent_session_id INTEGER REFERENCES sessions(id),
    first_position INTEGER NOT NULL CHECK (first_position >= 1),
    next_position INTEGER NOT NULL CHECK (next_position >= first_position),
    name TEXT NOT NULL,
    working_directory TEXT NOT NULL,
    probe_id TEXT NOT NULL REFERENCES probes(id),
    created_at_us INTEGER NOT NULL,
    updated_at_us INTEGER NOT NULL,
    CHECK (parent_session_id IS NOT NULL OR first_position = 1)
);
CREATE INDEX sessions_parent ON sessions(parent_session_id);

CREATE TABLE agents (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    parent_agent_id INTEGER REFERENCES agents(id),
    first_event_seq INTEGER NOT NULL CHECK (first_event_seq >= 1),
    next_event_seq INTEGER NOT NULL CHECK (next_event_seq >= first_event_seq),
    created_position INTEGER NOT NULL CHECK (created_position >= 1),
    name TEXT NOT NULL,
    created_at_us INTEGER NOT NULL,
    updated_at_us INTEGER NOT NULL,
    CHECK (parent_agent_id IS NOT NULL OR first_event_seq = 1),
    UNIQUE (session_id, parent_agent_id)
);
CREATE INDEX agents_session ON agents(session_id);
CREATE INDEX agents_parent ON agents(parent_agent_id);

CREATE TABLE events (
    event_pk INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    event_seq INTEGER NOT NULL CHECK (event_seq >= 1),
    session_position INTEGER NOT NULL CHECK (session_position >= 1),
    timestamp_us INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('status_change', 'model', 'new_context', 'message', 'thinking', 'provider_item', 'tool_definition', 'tool_call', 'tool_result', 'compaction', 'log', 'inference_start', 'inference_end')),
    UNIQUE (agent_id, event_seq),
    UNIQUE (event_pk, kind)
);
CREATE INDEX events_position ON events(agent_id, session_position);

CREATE TABLE event_status_change (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'status_change' CHECK (kind = 'status_change'),
    status TEXT NOT NULL CHECK (status IN ('idle','running','interrupted')),
    reason TEXT,
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_model (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'model' CHECK (kind = 'model'),
    provider TEXT NOT NULL CHECK (provider IN ('openai_codex','openrouter')),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0 AND trim(model) = model),
    effort TEXT NOT NULL CHECK (effort IN ('low','medium','high','xhigh','max')),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_new_context (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'new_context' CHECK (kind = 'new_context'),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_message (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'message' CHECK (kind = 'message'),
    message_type TEXT NOT NULL CHECK (message_type IN ('human','agent','harness','system')),
    delivery TEXT CHECK (delivery IN ('queue','steer')),
    contents TEXT NOT NULL,
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    replay_content TEXT CHECK (replay_content IS NULL OR json_valid(replay_content)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_thinking (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'thinking' CHECK (kind = 'thinking'),
    reasoning TEXT NOT NULL CHECK (json_valid(reasoning)),
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_provider_item (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'provider_item' CHECK (kind = 'provider_item'),
    item TEXT NOT NULL CHECK (json_valid(item)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_tool_definition (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'tool_definition' CHECK (kind = 'tool_definition'),
    tool_id TEXT NOT NULL CHECK (tool_id = 'noop'),
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    parameters TEXT NOT NULL CHECK (json_valid(parameters)),
    strict INTEGER CHECK (strict IN (0,1)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_tool_call (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'tool_call' CHECK (kind = 'tool_call'),
    call_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    arguments TEXT NOT NULL,
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_tool_result (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'tool_result' CHECK (kind = 'tool_result'),
    call_id TEXT NOT NULL,
    contents TEXT NOT NULL,
    error INTEGER NOT NULL CHECK (error IN (0,1)),
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    replay_content TEXT CHECK (replay_content IS NULL OR json_valid(replay_content)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_compaction (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'compaction' CHECK (kind = 'compaction'),
    summary_type TEXT NOT NULL CHECK (summary_type IN ('text','provider_item')),
    summary TEXT NOT NULL,
    input_tokens INTEGER CHECK (input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens >= 0),
    total_tokens INTEGER CHECK (total_tokens >= 0),
    cached_tokens INTEGER CHECK (cached_tokens >= 0),
    reasoning_tokens INTEGER CHECK (reasoning_tokens >= 0),
    usage_details TEXT NOT NULL CHECK (json_valid(usage_details)),
    CHECK (summary_type = 'text' OR json_valid(summary)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_log (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'log' CHECK (kind = 'log'),
    error INTEGER NOT NULL CHECK (error IN (0,1)),
    message TEXT NOT NULL,
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_inference_start (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'inference_start' CHECK (kind = 'inference_start'),
    inference_id INTEGER NOT NULL CHECK (inference_id >= 0),
    max_output_tokens INTEGER CHECK (max_output_tokens > 0),
    tool_choice TEXT CHECK (tool_choice IS NULL OR json_valid(tool_choice)),
    parallel_tool_calls INTEGER CHECK (parallel_tool_calls IN (0,1)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
CREATE TABLE event_inference_end (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'inference_end' CHECK (kind = 'inference_end'),
    inference_id INTEGER NOT NULL CHECK (inference_id >= 0),
    outcome TEXT NOT NULL CHECK (outcome IN ('completed','incomplete','failed','cancelled')),
    stop_reason TEXT CHECK (stop_reason IS NULL OR json_valid(stop_reason)),
    failure TEXT CHECK (failure IS NULL OR json_valid(failure)),
    request_time_us INTEGER NOT NULL CHECK (request_time_us >= 0),
    response_id TEXT,
    provider_metadata TEXT NOT NULL CHECK (json_valid(provider_metadata)),
    input_tokens INTEGER CHECK (input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens >= 0),
    total_tokens INTEGER CHECK (total_tokens >= 0),
    cached_tokens INTEGER CHECK (cached_tokens >= 0),
    reasoning_tokens INTEGER CHECK (reasoning_tokens >= 0),
    usage_details TEXT NOT NULL CHECK (json_valid(usage_details)),
    CHECK ((outcome IN ('completed','incomplete') AND stop_reason IS NOT NULL AND failure IS NULL)
        OR (outcome = 'failed' AND failure IS NOT NULL AND stop_reason IS NULL)
        OR (outcome = 'cancelled' AND stop_reason IS NULL AND failure IS NULL)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
