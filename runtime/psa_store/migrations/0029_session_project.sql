-- Client-chosen project grouping sessions; NULL marks a global session.
ALTER TABLE sessions ADD COLUMN project_id TEXT CHECK (project_id IS NULL OR length(project_id) > 0);
CREATE INDEX sessions_project ON sessions(project_id);
