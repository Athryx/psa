-- Internal state only: the full output of a codemode call whose model-facing
-- result was truncated, read back by PRIOR_RESULTS. Never part of event payloads.
CREATE TABLE codemode_outputs (
    event_pk INTEGER PRIMARY KEY REFERENCES event_tool_result(event_pk),
    output TEXT NOT NULL
);
