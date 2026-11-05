-- Tool catalog, role visibility, and skill usage.
--
-- All statements are idempotent. Existing databases gain the three tables;
-- fresh databases create them through the normal migration run.

CREATE TABLE IF NOT EXISTS tool_catalog (
    key TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    input_schema JSONB NOT NULL DEFAULT '{}'::jsonb,
    default_roles TEXT[] NOT NULL DEFAULT '{}',
    enabled BOOLEAN NOT NULL DEFAULT true,
    deferred BOOLEAN NOT NULL DEFAULT false,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS tool_visibility (
    tool_key TEXT NOT NULL REFERENCES tool_catalog(key) ON DELETE CASCADE,
    role TEXT NOT NULL,
    PRIMARY KEY (tool_key, role)
);

CREATE TABLE IF NOT EXISTS skill_usage (
    id BIGSERIAL PRIMARY KEY,
    skill TEXT NOT NULL,
    role TEXT NOT NULL,
    task_id UUID,
    intent_id UUID,
    found BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_skill_usage_skill_created
    ON skill_usage (skill, created_at DESC);
