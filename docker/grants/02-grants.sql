-- Runtime grants. Run after migrations, as dhi_owner.

GRANT USAGE ON SCHEMA public TO dhi_app;

GRANT SELECT, INSERT, UPDATE, DELETE
    ON ALL TABLES IN SCHEMA public
    TO dhi_app;

GRANT USAGE, SELECT
    ON ALL SEQUENCES IN SCHEMA public
    TO dhi_app;

-- Keep future migrations usable by the runtime role too. The initdb script
-- sets the same defaults on a fresh volume; this pass fixes existing ones.
ALTER DEFAULT PRIVILEGES FOR ROLE dhi_owner IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO dhi_app;

ALTER DEFAULT PRIVILEGES FOR ROLE dhi_owner IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO dhi_app;

-- The activity table is append-only for the application role.
REVOKE UPDATE, DELETE ON activity FROM dhi_app;
GRANT SELECT, INSERT ON activity TO dhi_app;
