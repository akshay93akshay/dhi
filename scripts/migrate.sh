#!/usr/bin/env bash
# Apply the native PostgreSQL role setup and runtime grants.
#
# This never applies application migrations: dhi-server applies
# crates/dhi-store/migrations/*.sql as dhi_owner at startup.
#
# Requires prebuilt PostgreSQL 16 client tools (psql, pg_isready) on PATH.
#   export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app
#   # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries
#   export PATH="$PG_BIN:$PATH"
#
# Usage: scripts/migrate.sh
# Env:   MIGRATION_DATABASE_URL  owner connection string
#                               (default postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi)
set -euo pipefail
cd "$(dirname "$0")/.."

MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL:-postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi}"
ROLES_SQL="ops/postgres/01-roles.sql"
GRANTS_SQL="ops/postgres/02-grants.sql"

missing=""
command -v psql >/dev/null 2>&1 || missing="$missing psql"
command -v pg_isready >/dev/null 2>&1 || missing="$missing pg_isready"
if [ -n "$missing" ]; then
  echo "error: PostgreSQL client tools not found:$missing" >&2
  echo "install prebuilt PostgreSQL 16 (Postgres.app or EDB binaries) and add its bin" >&2
  echo "directory to PATH, for example:" >&2
  echo '  export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app' >&2
  echo '  # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries' >&2
  echo '  export PATH="$PG_BIN:$PATH"' >&2
  exit 1
fi

for file in "$ROLES_SQL" "$GRANTS_SQL"; do
  if [ ! -r "$file" ]; then
    echo "error: SQL file not found: $file" >&2
    exit 1
  fi
done

echo "[1/4] waiting for PostgreSQL"
ready=""
for _ in $(seq 1 30); do
  if pg_isready -d "$MIGRATION_DATABASE_URL" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "error: PostgreSQL is not ready; check that the server is running" >&2
  echo "  pg_ctl -D <data-dir> start" >&2
  echo "  (or start it from Postgres.app; a user LaunchAgent can run it at login)" >&2
  exit 1
fi

echo "[2/4] applying $ROLES_SQL"
psql "$MIGRATION_DATABASE_URL" -v ON_ERROR_STOP=1 -f "$ROLES_SQL"

echo "[3/4] applying $GRANTS_SQL"
psql "$MIGRATION_DATABASE_URL" -v ON_ERROR_STOP=1 -f "$GRANTS_SQL"

echo "[4/4] verifying activity lock"
psql "$MIGRATION_DATABASE_URL" -v ON_ERROR_STOP=1 <<'SQL'
DO $$
BEGIN
  IF to_regclass('public.activity') IS NULL THEN
    RAISE NOTICE 'activity table not created yet; dhi-server installs the lock at startup';
  ELSIF has_table_privilege('dhi_app', 'activity', 'UPDATE')
     OR has_table_privilege('dhi_app', 'activity', 'DELETE') THEN
    RAISE EXCEPTION 'activity lock failed: dhi_app can update or delete activity';
  END IF;
END
$$;
SQL

echo "migrate ok"
echo "start dhi-server next: it applies application migrations as dhi_owner"
