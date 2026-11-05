-- Web provider settings (TinyFish, Parallel, Firecrawl).
--
-- Rows are optional: absence means "use environment/file fallback". Once a row
-- exists it is authoritative for that provider, including an absent API key
-- (clearing a stored key must not silently resurrect an environment key).
-- Nothing is seeded here: a database that never saw a web-provider save keeps
-- resolving entirely from the environment and DHI config file.
--
-- `api_key` is write-only. `api_key_hint` is stored at save time and is the
-- only key-derived value any read path returns.

CREATE TABLE IF NOT EXISTS web_provider_settings (
    provider TEXT PRIMARY KEY
        CHECK (provider IN ('tinyfish', 'parallel', 'firecrawl')),
    api_key TEXT NULL,
    api_key_hint TEXT NULL,
    base_url TEXT NULL,
    timeout_secs BIGINT NULL CHECK (timeout_secs IS NULL OR timeout_secs > 0),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    -- Documented router order: 1 tinyfish, 2 parallel, 3 firecrawl. Stored for
    -- future use; the API does not expose an editable priority.
    priority INTEGER NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_web_provider_settings_priority
    ON web_provider_settings (priority);
