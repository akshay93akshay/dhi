#!/usr/bin/env bash
# Start the native dhi-server and the web dev server.
#
# PostgreSQL must already be running. Start it with:
#   export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app
#   # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries
#   export PATH="$PG_BIN:$PATH"
#   pg_ctl -D <data-dir> start    # or start it from Postgres.app; a user LaunchAgent can run it at login
#
# Usage: scripts/dev.sh
set -euo pipefail
cd "$(dirname "$0")/.."

DATABASE_URL="${DATABASE_URL:-postgres://dhi_app:dhi_app@127.0.0.1:5432/dhi}"
MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL:-postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi}"

if ! command -v pg_isready >/dev/null 2>&1; then
  echo "error: pg_isready not found on PATH" >&2
  echo "install prebuilt PostgreSQL 16 (Postgres.app or EDB binaries) and add its bin" >&2
  echo "directory to PATH, for example:" >&2
  echo '  export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app' >&2
  echo '  # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries' >&2
  echo '  export PATH="$PG_BIN:$PATH"' >&2
  exit 1
fi

ready=""
for _ in $(seq 1 30); do
  if pg_isready -q -d "$MIGRATION_DATABASE_URL" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "error: PostgreSQL is not ready ($MIGRATION_DATABASE_URL)" >&2
  echo "  pg_ctl -D <data-dir> start" >&2
  echo "  (or start it from Postgres.app; a user LaunchAgent can run it at login)" >&2
  echo "  scripts/migrate.sh   # first run: create roles and grants" >&2
  exit 1
fi

export DATABASE_URL MIGRATION_DATABASE_URL
export DHI_BIND="${DHI_BIND:-127.0.0.1:8080}"
export RUST_LOG="${RUST_LOG:-dhi=info,tower_http=info}"

cargo run -p dhi-server &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

cd web
if [ ! -d node_modules ]; then
  npm install
fi
npm run dev -- --hostname 0.0.0.0
