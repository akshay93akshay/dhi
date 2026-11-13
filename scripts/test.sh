#!/usr/bin/env bash
# Native test run: Rust workspace tests and the static web build.
#
# The rust tests need a running PostgreSQL. Start it yourself:
#   export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app
#   # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries
#   export PATH="$PG_BIN:$PATH"
#   pg_ctl -D <data-dir> start    # or start it from Postgres.app; a user LaunchAgent can run it at login
#
# Usage: scripts/test.sh
# Env:   SKIP_DB=1  skip the PostgreSQL readiness check
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "${SKIP_DB:-0}" = "1" ]; then
  echo "[db] SKIP_DB=1, skipping postgres readiness check"
else
  if ! command -v pg_isready >/dev/null 2>&1; then
    echo "error: pg_isready not found on PATH" >&2
    echo "install prebuilt PostgreSQL 16 (Postgres.app or EDB binaries) and add its bin" >&2
    echo "directory to PATH, for example:" >&2
    echo '  export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app' >&2
    echo '  # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries' >&2
    echo '  export PATH="$PG_BIN:$PATH"' >&2
    exit 1
  fi
  export DATABASE_URL="${DATABASE_URL:-postgres://dhi_test:dhi_test@127.0.0.1:5432/dhi}"
  echo "[db] waiting for postgres"
  ready=""
  for _ in $(seq 1 30); do
    if pg_isready -q -d "$DATABASE_URL" >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 1
  done
  if [ -z "$ready" ]; then
    echo "error: postgres not ready after 30 tries ($DATABASE_URL)" >&2
    echo "  pg_ctl -D <data-dir> start" >&2
    echo "  (or start it from Postgres.app; a user LaunchAgent can run it at login)" >&2
    exit 1
  fi
fi

echo "[1/2] rust tests"
cargo test --workspace

echo "[2/2] web build"
(cd web && npm run build:static)

echo "tests ok"
