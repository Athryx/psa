CREATE TABLE event_message_content (
    event_pk INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'message' CHECK (kind = 'message'),
    message_type TEXT NOT NULL CHECK (message_type IN ('human','agent','harness','system')),
    delivery TEXT CHECK (delivery IN ('queue','steer')),
    content TEXT NOT NULL CHECK (json_valid(content) AND json_type(content) = 'array'),
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    FOREIGN KEY (event_pk,kind) REFERENCES events(event_pk,kind)
);

-- Preserve the content previously used for replay, including an explicitly empty
-- rich message. Only plain messages fall back to the former text field.
INSERT INTO event_message_content (event_pk,kind,message_type,delivery,content,metadata)
SELECT event_pk,kind,message_type,delivery,
       COALESCE(replay_content,
                json_array(json_object('Text', json_object('text', contents, 'annotations', json_array())))),
       metadata
FROM event_message;

DROP TABLE event_message;
ALTER TABLE event_message_content RENAME TO event_message;
