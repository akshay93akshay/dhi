-- Allow rules for the six Docker-per-task coding tools.
--
-- Migrations run before `seed_defaults`. On a fresh database `guard_rules` is
-- empty, so the `EXISTS` guard makes every statement below a no-op and the
-- seed adds these rules together with the existing defaults. On an existing
-- database (rules already present) each statement adds only the rule whose
-- name is missing, leaving operator edits untouched.

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-read', 'tool_name', 'exact', 'read', 'allow', 'read a file in the task workspace', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-read');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-write', 'tool_name', 'exact', 'write', 'allow', 'write a file in the task workspace', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-write');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-edit', 'tool_name', 'exact', 'edit', 'allow', 'edit a file in the task workspace', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-edit');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-search', 'tool_name', 'exact', 'search', 'allow', 'search files in the task workspace', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-search');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-list', 'tool_name', 'exact', 'list', 'allow', 'list files in the task workspace', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-list');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-shell', 'tool_name', 'exact', 'shell', 'allow', 'run a command in the task container', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-shell');
