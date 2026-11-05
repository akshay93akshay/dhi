-- DHI core schema. 14 tables.
-- The work graph is per task.

CREATE TABLE IF NOT EXISTS tasks (
    id UUID PRIMARY KEY,
    goal TEXT NOT NULL,
    bound INTEGER NOT NULL DEFAULT 4,
    state TEXT NOT NULL DEFAULT 'created',
    exploration_id UUID NOT NULL UNIQUE,
    origin_fact_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS work_goals (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    description TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'open',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS facts (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    claim TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'confirmed',
    provenance JSONB NOT NULL DEFAULT '{}'::jsonb,
    origin_of UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (id, task_id)
);

CREATE TABLE IF NOT EXISTS intents (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    goal_id UUID REFERENCES work_goals(id) ON DELETE SET NULL,
    fact_id UUID NOT NULL,
    description TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'open',
    owner TEXT,
    priority INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    FOREIGN KEY (fact_id, task_id)
        REFERENCES facts(id, task_id)
        ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS results (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    intent_id UUID NOT NULL REFERENCES intents(id) ON DELETE CASCADE,
    claim TEXT NOT NULL,
    acceptance TEXT NOT NULL DEFAULT 'pending',
    evidence JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS hints (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    intent_id UUID REFERENCES intents(id) ON DELETE SET NULL,
    content TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS digests (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    content TEXT NOT NULL,
    token_count INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS activity (
    id BIGSERIAL PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS tool_calls (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    intent_id UUID NOT NULL REFERENCES intents(id) ON DELETE CASCADE,
    tool_name TEXT NOT NULL,
    input JSONB NOT NULL DEFAULT '{}'::jsonb,
    output JSONB,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS guard_decisions (
    id UUID PRIMARY KEY,
    tool_call_id UUID NOT NULL REFERENCES tool_calls(id) ON DELETE CASCADE,
    verdict TEXT NOT NULL,
    matched_rule TEXT,
    rule_version TEXT,
    reason TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS guard_policies (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    rules JSONB NOT NULL DEFAULT '{}'::jsonb,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (name, version)
);

CREATE TABLE IF NOT EXISTS approvals (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    tool_call_id UUID NOT NULL REFERENCES tool_calls(id) ON DELETE CASCADE,
    guard_decision_id UUID NOT NULL REFERENCES guard_decisions(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'pending',
    reason TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    decided_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS agent_runs (
    id UUID PRIMARY KEY,
    run_id UUID NOT NULL,
    call_index INTEGER NOT NULL DEFAULT 0,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    intent_id UUID REFERENCES intents(id) ON DELETE SET NULL,
    role TEXT NOT NULL,
    model TEXT NOT NULL,
    -- `<provider>/<model>/max_tokens=<n>/stream=<bool>`; never the API key.
    profile TEXT NOT NULL DEFAULT '',
    prompt TEXT NOT NULL,
    prompt_hash TEXT NOT NULL DEFAULT '',
    variables JSONB NOT NULL DEFAULT '{}'::jsonb,
    response TEXT,
    tokens_in BIGINT NOT NULL DEFAULT 0,
    tokens_out BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens BIGINT NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    reasoning_tokens BIGINT NOT NULL DEFAULT 0,
    -- Provider-reported USD only; 0 when the provider is silent.
    cost DOUBLE PRECISION NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'running',
    error TEXT,
    latency_ms BIGINT NOT NULL DEFAULT 0,
    checkpoint_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT agent_runs_status_check CHECK (
        status IN ('running','ok','error','exhausted','timed_out','truncated','no_output','aborted')
    )
);

CREATE TABLE IF NOT EXISTS checkpoints (
    id UUID PRIMARY KEY,
    agent_run_id UUID NOT NULL REFERENCES agent_runs(id) ON DELETE CASCADE,
    state JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A run outlives a pruned checkpoint: deleting the checkpoint nulls the
-- pointer, never the run row. Declared after `checkpoints` because it
-- targets that table. The `DO` block keeps a second pass idempotent, like
-- `CREATE TABLE IF NOT EXISTS` above.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'agent_runs_checkpoint_id_fkey'
          AND conrelid = 'agent_runs'::regclass
    ) THEN
        ALTER TABLE agent_runs
            ADD CONSTRAINT agent_runs_checkpoint_id_fkey
            FOREIGN KEY (checkpoint_id) REFERENCES checkpoints(id) ON DELETE SET NULL;
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS idx_intents_frontier
    ON intents (task_id, priority DESC, created_at)
    WHERE state = 'open';
CREATE INDEX IF NOT EXISTS idx_intents_task ON intents (task_id);
CREATE INDEX IF NOT EXISTS idx_facts_task ON facts (task_id);
CREATE INDEX IF NOT EXISTS idx_results_task ON results (task_id);
CREATE INDEX IF NOT EXISTS idx_activity_task_time ON activity (task_id, created_at);
CREATE INDEX IF NOT EXISTS idx_tool_calls_task ON tool_calls (task_id);
CREATE INDEX IF NOT EXISTS idx_tool_calls_intent ON tool_calls (intent_id);
CREATE INDEX IF NOT EXISTS idx_approvals_pending ON approvals (task_id)
    WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_agent_runs_task ON agent_runs (task_id);
CREATE INDEX IF NOT EXISTS idx_agent_runs_run ON agent_runs (run_id, call_index);
CREATE INDEX IF NOT EXISTS idx_agent_runs_task_time ON agent_runs (task_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_checkpoints_run ON checkpoints (agent_run_id);
