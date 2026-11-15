-- Cache writes and cost are typed usage fields rather than keys read out of
-- the raw provider usage JSON. Backfill both from the stored raw usage.
ALTER TABLE event_inference_result_end ADD COLUMN cache_write_tokens INTEGER CHECK (cache_write_tokens >= 0);
ALTER TABLE event_inference_result_end ADD COLUMN cost REAL;
ALTER TABLE event_compaction ADD COLUMN cache_write_tokens INTEGER CHECK (cache_write_tokens >= 0);
ALTER TABLE event_compaction ADD COLUMN cost REAL;

UPDATE event_inference_result_end SET
    cache_write_tokens = CASE
        WHEN json_type(usage_details, '$.prompt_tokens_details.cache_write_tokens') = 'integer'
          AND json_extract(usage_details, '$.prompt_tokens_details.cache_write_tokens') >= 0
        THEN json_extract(usage_details, '$.prompt_tokens_details.cache_write_tokens')
    END,
    cost = CASE
        WHEN json_type(usage_details, '$.cost') IN ('integer', 'real')
        THEN json_extract(usage_details, '$.cost')
    END;

UPDATE event_compaction SET
    cache_write_tokens = CASE
        WHEN json_type(usage_details, '$.prompt_tokens_details.cache_write_tokens') = 'integer'
          AND json_extract(usage_details, '$.prompt_tokens_details.cache_write_tokens') >= 0
        THEN json_extract(usage_details, '$.prompt_tokens_details.cache_write_tokens')
    END,
    cost = CASE
        WHEN json_type(usage_details, '$.cost') IN ('integer', 'real')
        THEN json_extract(usage_details, '$.cost')
    END;
