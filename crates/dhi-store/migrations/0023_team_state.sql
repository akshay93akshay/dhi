-- Swarm (Cline Teams) shared state. A team is scoped to exactly one task; the
-- team id is the task id and every row cascades with that task.
CREATE TABLE IF NOT EXISTS team_members (
    team_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('lead', 'teammate')),
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'idle', 'stopped')),
    description TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (team_id, agent_id)
);

CREATE INDEX IF NOT EXISTS idx_team_members_team
    ON team_members (team_id, role);

CREATE TABLE IF NOT EXISTS team_tasks (
    id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'in_progress', 'blocked', 'completed')),
    created_by TEXT NOT NULL,
    assignee TEXT,
    depends_on UUID[] NOT NULL DEFAULT '{}'::UUID[],
    summary TEXT NOT NULL DEFAULT '',
    blocked_reason TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_team_tasks_team_status
    ON team_tasks (team_id, status, created_at);

CREATE TABLE IF NOT EXISTS team_messages (
    id BIGSERIAL PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    from_agent_id TEXT NOT NULL,
    -- NULL is a team-wide broadcast.
    to_agent_id TEXT,
    subject TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL,
    task_id UUID REFERENCES team_tasks(id) ON DELETE SET NULL,
    sent_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    read_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_team_messages_inbox
    ON team_messages (team_id, to_agent_id, sent_at);
CREATE INDEX IF NOT EXISTS idx_team_messages_sent
    ON team_messages (team_id, from_agent_id, sent_at);

CREATE TABLE IF NOT EXISTS team_mission_log (
    id BIGSERIAL PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL,
    task_id UUID REFERENCES team_tasks(id) ON DELETE SET NULL,
    kind TEXT NOT NULL
        CHECK (kind IN ('progress', 'handoff', 'blocked', 'decision', 'done', 'error')),
    summary TEXT NOT NULL,
    evidence TEXT NOT NULL DEFAULT '',
    next_action TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_team_mission_log_team
    ON team_mission_log (team_id, created_at, id);

CREATE TABLE IF NOT EXISTS team_runs (
    id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL,
    task_id UUID REFERENCES team_tasks(id) ON DELETE SET NULL,
    status TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'completed', 'failed', 'cancelled', 'interrupted')),
    message TEXT NOT NULL DEFAULT '',
    priority INTEGER NOT NULL DEFAULT 0,
    retry_count INTEGER NOT NULL DEFAULT 0,
    max_retries INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ,
    continue_conversation BOOLEAN NOT NULL DEFAULT FALSE,
    started_at TIMESTAMPTZ,
    ended_at TIMESTAMPTZ,
    lease_owner TEXT,
    heartbeat_at TIMESTAMPTZ,
    last_progress_at TIMESTAMPTZ,
    last_progress_message TEXT NOT NULL DEFAULT '',
    current_activity TEXT NOT NULL DEFAULT '',
    result TEXT,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_team_runs_team_status
    ON team_runs (team_id, status, created_at);
CREATE INDEX IF NOT EXISTS idx_team_runs_lease
    ON team_runs (team_id, lease_owner, heartbeat_at)
    WHERE status = 'running';

CREATE TABLE IF NOT EXISTS team_outcomes (
    id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'in_review', 'finalized')),
    required_sections TEXT[] NOT NULL DEFAULT '{}'::TEXT[],
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finalized_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_team_outcomes_team
    ON team_outcomes (team_id, created_at);

CREATE TABLE IF NOT EXISTS team_outcome_fragments (
    id UUID PRIMARY KEY,
    team_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    outcome_id UUID NOT NULL REFERENCES team_outcomes(id) ON DELETE CASCADE,
    section TEXT NOT NULL,
    source_agent_id TEXT NOT NULL,
    source_run_id UUID REFERENCES team_runs(id) ON DELETE SET NULL,
    content TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'reviewed', 'rejected')),
    reviewed_by TEXT,
    reviewed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_team_outcome_fragments_outcome
    ON team_outcome_fragments (team_id, outcome_id, created_at);
CREATE INDEX IF NOT EXISTS idx_team_outcome_fragments_review
    ON team_outcome_fragments (team_id, outcome_id, status);
