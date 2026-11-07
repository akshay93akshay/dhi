ALTER TABLE agent_runs
    ADD COLUMN IF NOT EXISTS worker_key TEXT;

CREATE INDEX IF NOT EXISTS idx_agent_runs_worker
    ON agent_runs (intent_id, worker_key);
