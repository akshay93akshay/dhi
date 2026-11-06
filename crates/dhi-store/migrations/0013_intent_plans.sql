CREATE TABLE IF NOT EXISTS intent_plans (
    intent_id UUID PRIMARY KEY REFERENCES intents(id) ON DELETE CASCADE,
    plan JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_intent_plans_created ON intent_plans (created_at);
