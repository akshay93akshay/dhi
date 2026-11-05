-- Side-question sessions and their frozen snapshots.
-- A session belongs to one task and carries the JSON snapshot taken at
-- creation; requests are the question/answer rows appended to it. Neither
-- table is part of the work graph, and nothing here is a fact, intent, or
-- goal.

CREATE TABLE IF NOT EXISTS side_question_sessions (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    -- Frozen at creation: later facts/goals/activity never enter this value.
    snapshot JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS side_question_requests (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES side_question_sessions(id) ON DELETE CASCADE,
    question TEXT NOT NULL,
    answer TEXT,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'ok', 'error')),
    error TEXT,
    model TEXT,
    tokens_in INT,
    tokens_out INT,
    latency_ms BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_side_question_sessions_task
    ON side_question_sessions (task_id, created_at);
CREATE INDEX IF NOT EXISTS idx_side_question_requests_session
    ON side_question_requests (session_id, created_at);
