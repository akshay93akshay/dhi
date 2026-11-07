CREATE TABLE IF NOT EXISTS swarm_messages (
    id BIGSERIAL PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    intent_id UUID NOT NULL REFERENCES intents(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_swarm_messages_task
    ON swarm_messages (task_id, id);
