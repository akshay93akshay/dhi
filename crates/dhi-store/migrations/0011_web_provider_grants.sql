-- Runtime grants for web provider settings.
--
-- Migrations run as the owner role. The application role needs access to the
-- new settings table; without this grant every provider read and write fails
-- with "permission denied for table web_provider_settings".

GRANT SELECT, INSERT, UPDATE, DELETE
    ON web_provider_settings
    TO dhi_app;
