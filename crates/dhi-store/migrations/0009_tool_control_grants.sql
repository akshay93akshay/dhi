-- Runtime grants for the tool control tables.
--
-- Migrations run as the owner role. The application role needs access to the
-- three tool control tables and the skill_usage sequence. The role exists in
-- the deployment init scripts and in the test database.

GRANT SELECT, INSERT, UPDATE, DELETE
    ON tool_catalog, tool_visibility, skill_usage
    TO dhi_app;

GRANT USAGE, SELECT
    ON SEQUENCE skill_usage_id_seq
    TO dhi_app;
