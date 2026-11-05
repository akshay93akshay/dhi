-- Allow rule for the provider-backed web_research tool.
--
-- Migrations run before `seed_defaults`. On a fresh database `guard_rules` is
-- empty, so the `EXISTS` guard makes this statement a no-op and the seed adds
-- the rule together with the existing defaults. On an existing database it
-- inserts only the missing rule, leaving operator edits untouched.

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-web_research', 'tool_name', 'exact', 'web_research', 'allow', 'research a question through Parallel and return cited sources', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-web_research');
