-- Session-level decision model, as a JSON `Model`; NULL means unset.
ALTER TABLE sessions ADD COLUMN decision_model TEXT CHECK (decision_model IS NULL OR json_valid(decision_model));
