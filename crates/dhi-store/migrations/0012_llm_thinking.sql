-- LLM thinking controls and capture.
--
-- Profiles gain two independent knobs, both defaulting to the empty string so
-- existing rows keep sending no thinking settings at all:
--   thinking_type:    '' | 'disabled' | 'enabled'
--   reasoning_effort: '' | 'low' | 'medium' | 'high' | 'xhigh' | 'max'
--
-- Agent runs keep the provider's final reasoning text in `thinking`; NULL
-- means the model returned no reasoning (an empty string is never stored).

ALTER TABLE llm_profiles
    ADD COLUMN IF NOT EXISTS thinking_type TEXT NOT NULL DEFAULT '';
ALTER TABLE llm_profiles
    ADD COLUMN IF NOT EXISTS reasoning_effort TEXT NOT NULL DEFAULT '';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'llm_profiles_thinking_type_check'
           AND conrelid = 'llm_profiles'::regclass
    ) THEN
        ALTER TABLE llm_profiles
            ADD CONSTRAINT llm_profiles_thinking_type_check
            CHECK (thinking_type IN ('', 'disabled', 'enabled'));
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'llm_profiles_reasoning_effort_check'
           AND conrelid = 'llm_profiles'::regclass
    ) THEN
        ALTER TABLE llm_profiles
            ADD CONSTRAINT llm_profiles_reasoning_effort_check
            CHECK (reasoning_effort IN ('', 'low', 'medium', 'high', 'xhigh', 'max'));
    END IF;
END
$$;

ALTER TABLE agent_runs
    ADD COLUMN IF NOT EXISTS thinking TEXT NULL;
