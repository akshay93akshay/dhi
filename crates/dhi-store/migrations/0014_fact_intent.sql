ALTER TABLE facts
    ADD COLUMN IF NOT EXISTS intent_id UUID REFERENCES intents(id) ON DELETE SET NULL;

CREATE UNIQUE INDEX IF NOT EXISTS facts_one_per_intent
    ON facts (intent_id)
    WHERE intent_id IS NOT NULL;
