# Native PostgreSQL setup

DHI runs against a normal PostgreSQL 16 server on this machine. There is no
container, no image, and no daemon socket. This directory holds the only SQL
that runs outside the server.

| File | Who runs it | What it does |
| --- | --- | --- |
| `01-roles.sql` | you, once, as a superuser | creates `dhi_app` and `dhi_test`, grants database access, sets default privileges for `dhi_owner`, and installs the `activity` append-only event trigger |
| `02-grants.sql` | you, before or after the server migrations | grants runtime privileges on existing tables and sequences and applies the `activity` lock when that table exists |

Application schema migrations are owned by `dhi-server`. It applies
`crates/dhi-store/migrations/*.sql` as `dhi_owner` at startup. Do not run them
by hand.

## Prerequisites

Use a prebuilt PostgreSQL 16. The two recommended options need no compiler:

- **Postgres.app**: https://postgresapp.com downloads a signed app bundle with
  PostgreSQL 16 inside. Its binaries live under
  `/Applications/Postgres.app/Contents/Versions/16/bin`.
- **EDB PostgreSQL 16 binaries**: https://www.enterprisedb.com/downloads/postgres-postgresql-downloads
  ships an installer with a `bin` directory (typically
  `/Library/PostgreSQL/16/bin`). If you use the installer, keep the data
  directory it creates, for example `/Library/PostgreSQL/16/data`.

Homebrew is optional and only on a macOS version where it has prebuilt
bottles. On Intel macOS 14 it has no bottles for `postgresql@16` and compiles
from source, so do not rely on it unless you accept that build.

```sh
# Postgres.app: point PG_BIN at the 16 binaries and put them on PATH.
export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app
# or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries
export PATH="$PG_BIN:$PATH"
psql --version

# Start the server. Postgres.app also starts it when the app runs.
pg_ctl -D <data-dir> start          # e.g. ~/Library/Application Support/Postgres/var-16
```

To start the server automatically at login without Homebrew, run it from
Postgres.app's menu, or install a per-user LaunchAgent that runs
`$PG_BIN/pg_ctl -D <data-dir> start`. A LaunchAgent runs as your own user, so
it needs no `sudo` and no system daemon.

## One-time setup

Run as a superuser. With Postgres.app and a `pg_ctl`-initialized cluster that
is your own macOS user, the default superuser of the cluster.

```sh
# 1. Create the owner role with a password and make it a database creator.
psql -d postgres -v ON_ERROR_STOP=1 <<'SQL'
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dhi_owner') THEN
        CREATE ROLE dhi_owner LOGIN CREATEDB PASSWORD 'dhi_owner';
    END IF;
END
$$;
SQL

# 2. Create the database owned by dhi_owner.
createdb -O dhi_owner dhi

# 3. Create the runtime and test roles and their grants.
psql "postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi" \
  -v ON_ERROR_STOP=1 -f ops/postgres/01-roles.sql

# 4. Let dhi-server apply the app migrations at startup.
#    Nothing to run here: start the server once so it migrates.
DATABASE_URL=postgres://dhi_app:dhi_app@127.0.0.1:5432/dhi \
MIGRATION_DATABASE_URL=postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi \
  cargo run -p dhi-server

# 5. Apply the runtime grants. Safe here (before migrations) and again after.
psql "postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi" \
  -v ON_ERROR_STOP=1 -f ops/postgres/02-grants.sql
```

`scripts/migrate.sh` performs steps 3, a readiness check, and step 5 in one
command. It never applies app migrations; the server does that.

`02-grants.sql` is safe before or after the server migrations. On a fresh
database the `activity` table does not exist yet, so it skips the `activity`
lock with a notice instead of failing. The event trigger installed by
`01-roles.sql` applies the lock when the server creates the table. Re-run
`scripts/migrate.sh` (or `02-grants.sql`) after the server has migrated to
verify the lock: `activity` should allow `SELECT` and `INSERT` for `dhi_app`
and deny `UPDATE` and `DELETE`.

## Connection strings

`.env.example` and the scripts share these defaults:

| Variable | Role | URL |
| --- | --- | --- |
| `DATABASE_URL` | `dhi_app` runtime | `postgres://dhi_app:dhi_app@127.0.0.1:5432/dhi` |
| `MIGRATION_DATABASE_URL` | `dhi_owner` migrations and grants | `postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi` |
| `TEST_DATABASE_URL` | `dhi_test` test runs | `postgres://dhi_test:dhi_test@127.0.0.1:5432/dhi` |

These passwords are local development defaults. Change them for any host that
is reachable from the network.

## Where to go next

- `scripts/migrate.sh`: create/verify roles and grants.
- `scripts/backup.sh`: `pg_dump` a compressed backup.
- `scripts/restore-test.sh`: rehearse a restore into a throwaway database.
- `scripts/dhi-health.sh`: check API, UI, and PostgreSQL readiness.
