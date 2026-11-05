-- Local and CI roles.
-- dhi_owner owns the schema and runs migrations.
-- dhi_app is the runtime role.
-- dhi_test is the test role. It can create databases, but is not a superuser.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dhi_app') THEN
        CREATE ROLE dhi_app LOGIN PASSWORD 'dhi_app';
    END IF;

    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dhi_test') THEN
        CREATE ROLE dhi_test LOGIN CREATEDB PASSWORD 'dhi_test';
    END IF;
END
$$;

GRANT CONNECT ON DATABASE dhi TO dhi_app;
GRANT CONNECT ON DATABASE dhi TO dhi_test;
GRANT CREATE ON DATABASE dhi TO dhi_test;
GRANT USAGE ON SCHEMA public TO dhi_app;

-- dhi-server applies migrations as dhi_owner at startup. These defaults make
-- every table and sequence it creates in public usable by dhi_app without a
-- second grants pass. The runtime grants file repeats them for existing
-- databases, and keeps activity append-only.
ALTER DEFAULT PRIVILEGES FOR ROLE dhi_owner IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO dhi_app;

ALTER DEFAULT PRIVILEGES FOR ROLE dhi_owner IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO dhi_app;

-- Startup migrations create activity after this script has run, so a plain
-- REVOKE here would fail. Revoke the write privileges as the table is created
-- instead; the grants file repeats the lock for existing databases.
CREATE OR REPLACE FUNCTION dhi_keep_activity_append_only()
RETURNS event_trigger
LANGUAGE plpgsql
AS $$
DECLARE
    command record;
BEGIN
    FOR command IN SELECT * FROM pg_event_trigger_ddl_commands() LOOP
        IF command.object_type = 'table'
           AND command.schema_name = 'public'
           AND command.object_identity IN ('activity', 'public.activity') THEN
            EXECUTE 'REVOKE UPDATE, DELETE ON public.activity FROM dhi_app';
            EXECUTE 'GRANT SELECT, INSERT ON public.activity TO dhi_app';
        END IF;
    END LOOP;
END;
$$;

DROP EVENT TRIGGER IF EXISTS dhi_keep_activity_append_only;
CREATE EVENT TRIGGER dhi_keep_activity_append_only
    ON ddl_command_end
    WHEN TAG IN ('CREATE TABLE', 'CREATE TABLE AS', 'SELECT INTO')
    EXECUTE FUNCTION dhi_keep_activity_append_only();
