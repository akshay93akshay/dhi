-- Browser-folder web projects: one uploaded snapshot per row.
-- The absolute root is stored for audit; task rows keep a nullable reference
-- so legacy and host-folder tasks remain unchanged.
CREATE TABLE IF NOT EXISTS web_projects (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    root_path TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_web_projects_created
    ON web_projects (created_at DESC, id);

ALTER TABLE tasks ADD COLUMN IF NOT EXISTS web_project_id UUID;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS web_project_name TEXT;

CREATE INDEX IF NOT EXISTS idx_tasks_web_project
    ON tasks (web_project_id);
