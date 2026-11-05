-- ARTEX-parity management schema: LLM profiles, guard rules, app settings.
-- Three new tables (llm_profiles, guard_rules, app_settings) and two nullable
-- task pin columns. The seed (PostgreSQL becomes the source of truth after the
-- first boot) is data, applied by `Store::seed_defaults` in dhi-store, not SQL:
-- it needs the environment-resolved default profiles and must be a no-op when
-- the process runs without LLM configuration (mock mode).

CREATE TABLE IF NOT EXISTS llm_profiles (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    format TEXT NOT NULL CHECK (format IN ('openai', 'anthropic')),
    base_url TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL,
    -- Write-only: no read route ever selects or returns this column.
    api_key TEXT NOT NULL DEFAULT '',
    -- Last four characters of the key at save time (ARTEX `…xxxx` style);
    -- empty when the key is shorter than four characters or absent.
    api_key_hint TEXT NOT NULL DEFAULT '',
    -- Output cap for one reply; 0 = no cap is sent.
    max_tokens INTEGER NOT NULL DEFAULT 0,
    context_window_k INTEGER NOT NULL DEFAULT 0,
    streaming BOOLEAN NOT NULL DEFAULT TRUE,
    -- Higher goes first in the failover chain, after the role default.
    priority INTEGER NOT NULL DEFAULT 0,
    -- True keeps the profile out of every failover chain; an explicit task pin
    -- still reaches it.
    pool_exclude BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS guard_rules (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    match_target TEXT NOT NULL CHECK (match_target IN ('tool_name', 'tool_input')),
    match_type TEXT NOT NULL CHECK (match_type IN ('exact', 'regex')),
    pattern TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('allow', 'deny', 'ask')),
    reason TEXT NOT NULL DEFAULT '',
    -- Rules evaluate by priority DESC; the first enabled match wins.
    priority INTEGER NOT NULL DEFAULT 0,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    -- Per-rule ask-approval budget in seconds; 0 = use
    -- app_settings.approval_timeout_secs.
    timeout_secs INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_guard_rules_enabled
    ON guard_rules (priority DESC)
    WHERE enabled;

CREATE INDEX IF NOT EXISTS idx_llm_profiles_priority
    ON llm_profiles (priority DESC);

-- Singleton application settings row. Referenced profiles are seeded defaults;
-- every read of live guard/pool state goes through the in-memory snapshot
-- built from this row, so the write path only persists here.
CREATE TABLE IF NOT EXISTS app_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    planner_profile_id BIGINT NOT NULL REFERENCES llm_profiles(id),
    worker_profile_id BIGINT NOT NULL REFERENCES llm_profiles(id),
    -- Seeded with the engine's approval-expiry default (900s = 15 minutes).
    approval_timeout_secs INTEGER NOT NULL,
    -- Applied when no guard rule matches; seeded with the compiled policy's
    -- historical default (`deny`).
    guard_default_action TEXT NOT NULL CHECK (guard_default_action IN ('allow', 'deny', 'ask')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Optional per-task role pins. A pin always resolves against `llm_profiles`;
-- deleting a referenced profile is refused while any pin or default points at
-- it. The engine resolves the pin to a failover chain head per round.
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS pinned_planner_profile_id BIGINT REFERENCES llm_profiles(id);
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS pinned_worker_profile_id BIGINT REFERENCES llm_profiles(id);
