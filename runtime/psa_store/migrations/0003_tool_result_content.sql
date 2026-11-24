CREATE TABLE event_tool_result_content (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'tool_result' CHECK (kind = 'tool_result'),
    call_id TEXT NOT NULL,
    content TEXT NOT NULL CHECK (json_valid(content) AND json_type(content) = 'array'),
    error INTEGER NOT NULL CHECK (error IN (0,1)),
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);

-- Preserve the content previously used for replay. Plain results become a single
-- text block; an explicitly empty rich result stays empty.
INSERT INTO event_tool_result_content (event_pk,kind,call_id,content,error,metadata)
SELECT event_pk,kind,call_id,
       COALESCE(replay_content,
                json_array(json_object('Text', json_object('text', contents, 'annotations', json_array())))),
       error,metadata
FROM event_tool_result;

DROP TABLE event_tool_result;
ALTER TABLE event_tool_result_content RENAME TO event_tool_result;
