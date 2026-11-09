-- Planner-only instructions sent by a human while a task is live.
-- `delivered_at` is set once a planner call has seen the text, so a restart
-- never re-injects it. Instructions never reach worker prompts.
CREATE TABLE IF NOT EXISTS task_instructions (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    text TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_task_instructions_task
    ON task_instructions (task_id, created_at);
CREATE INDEX IF NOT EXISTS idx_task_instructions_pending
    ON task_instructions (task_id, created_at)
    WHERE delivered_at IS NULL;
