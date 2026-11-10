-- `swarm_scale` approvals: an Auto swarm asking to exceed the default cap.
-- The lead is not counted; `requested_workers` counts teammates only.
CREATE TABLE IF NOT EXISTS scale_approvals (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    -- The team row is owned by the team runtime. It is nullable so a request
    -- can be opened before the team persists, and it is deliberately not a
    -- foreign key while the team schema lands separately.
    team_id UUID,
    requested_workers INTEGER NOT NULL,
    current_cap INTEGER NOT NULL,
    reason TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    decided_at TIMESTAMPTZ,
    CONSTRAINT scale_approvals_requested_workers_range
        CHECK (requested_workers BETWEEN 2 AND 64),
    CONSTRAINT scale_approvals_current_cap_range
        CHECK (current_cap BETWEEN 1 AND 64),
    CONSTRAINT scale_approvals_status
        CHECK (status IN ('pending', 'granted', 'denied', 'expired'))
);

CREATE INDEX IF NOT EXISTS idx_scale_approvals_pending
    ON scale_approvals (status, created_at, id);

CREATE INDEX IF NOT EXISTS idx_scale_approvals_task
    ON scale_approvals (task_id, created_at);
