-- Allow rules for the five persistent terminal tools.
--
-- Migrations run before `seed_defaults`. On a fresh database `guard_rules` is
-- empty, so the `EXISTS` guard makes every statement below a no-op and the
-- seed adds these rules together with the existing defaults. On an existing
-- database (rules already present) each statement adds only the rule whose
-- name is missing, leaving operator edits untouched.

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-shell_open', 'tool_name', 'exact', 'shell_open', 'allow', 'open a persistent terminal', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-shell_open');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-shell_send', 'tool_name', 'exact', 'shell_send', 'allow', 'send text to a persistent terminal', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-shell_send');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-shell_read', 'tool_name', 'exact', 'shell_read', 'allow', 'read new output from a persistent terminal', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-shell_read');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-shell_close', 'tool_name', 'exact', 'shell_close', 'allow', 'close a persistent terminal', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-shell_close');

INSERT INTO guard_rules (name, match_target, match_type, pattern, action, reason, priority, enabled, timeout_secs)
SELECT 'allow-shell_list', 'tool_name', 'exact', 'shell_list', 'allow', 'list persistent terminals', 0, true, 0
WHERE EXISTS (SELECT 1 FROM guard_rules)
  AND NOT EXISTS (SELECT 1 FROM guard_rules WHERE name = 'allow-shell_list');
