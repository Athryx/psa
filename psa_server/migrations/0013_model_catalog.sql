CREATE TABLE model_catalog (
    provider TEXT NOT NULL CHECK (provider IN ('openai_codex','openrouter')),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0 AND trim(model) = model),
    info TEXT NOT NULL CHECK (json_valid(info)),
    PRIMARY KEY (provider,model)
);
-- Pins intentionally survive deletion/reappearance of upstream model records.
CREATE TABLE model_pins (
    provider TEXT NOT NULL CHECK (provider IN ('openai_codex','openrouter')),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0 AND trim(model) = model),
    PRIMARY KEY (provider,model)
);
CREATE TABLE model_catalog_sync (
    provider TEXT PRIMARY KEY CHECK (provider IN ('openai_codex','openrouter')),
    state TEXT NOT NULL CHECK (json_valid(state))
);

CREATE TABLE event_model_efforts (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'model' CHECK (kind = 'model'),
    provider TEXT NOT NULL CHECK (provider IN ('openai_codex','openrouter')),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0 AND trim(model) = model),
    effort TEXT NOT NULL CHECK (effort IN ('off','minimal','low','medium','high','xhigh','max')),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
INSERT INTO event_model_efforts SELECT * FROM event_model;
DROP TABLE event_model;
ALTER TABLE event_model_efforts RENAME TO event_model;

CREATE TABLE event_inference_start_efforts (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'inference_start' CHECK (kind = 'inference_start'),
    inference_id INTEGER NOT NULL CHECK (inference_id >= 0),
    max_output_tokens INTEGER CHECK (max_output_tokens > 0),
    tool_choice TEXT CHECK (tool_choice IS NULL OR json_valid(tool_choice)),
    parallel_tool_calls INTEGER CHECK (parallel_tool_calls IN (0,1)),
    model_provider TEXT CHECK (model_provider IN ('openai_codex','openrouter')),
    model_name TEXT CHECK (length(trim(model_name)) > 0 AND trim(model_name) = model_name),
    model_effort TEXT CHECK (model_effort IN ('off','minimal','low','medium','high','xhigh','max'))
        CHECK ((model_provider IS NULL AND model_name IS NULL AND model_effort IS NULL)
            OR (model_provider IS NOT NULL AND model_name IS NOT NULL AND model_effort IS NOT NULL)),
    reasoning TEXT CHECK (reasoning IS NULL OR json_valid(reasoning)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);
INSERT INTO event_inference_start_efforts
    (event_pk,kind,inference_id,max_output_tokens,tool_choice,parallel_tool_calls,model_provider,model_name,model_effort)
SELECT event_pk,kind,inference_id,max_output_tokens,tool_choice,parallel_tool_calls,model_provider,model_name,model_effort
FROM event_inference_result_start;
DROP TABLE event_inference_result_start;
ALTER TABLE event_inference_start_efforts RENAME TO event_inference_result_start;
