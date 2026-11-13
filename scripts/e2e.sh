#!/usr/bin/env bash
# Native end-to-end smoke test: build the server, start it against a running
# PostgreSQL, and wait for /api/health.
#
# PostgreSQL must already be running as a native service. dhi-server applies
# the app migrations itself at startup, so no migration step is needed.
#
# Usage: scripts/e2e.sh
# Env:   DATABASE_URL, MIGRATION_DATABASE_URL, DHI_BIND, DHI_HEALTH_URL,
#        DHI_E2E_LOG, DHI_E2E_HEALTH
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
  exit 1
fi

cargo build -p dhi-server

BIND="${DHI_BIND:-127.0.0.1:8080}"
BIND_HOST="${BIND%:*}"
BIND_PORT="${BIND##*:}"
case "$BIND_HOST" in
  0.0.0.0|""|"::"|"[::]")
    HEALTH_HOST="127.0.0.1"
    ;;
  \[*\])
    HEALTH_HOST="$BIND_HOST"
    ;;
  *:*)
    # A bare IPv6 address needs brackets in a URL.
    HEALTH_HOST="[$BIND_HOST]"
    ;;
  *)
    HEALTH_HOST="$BIND_HOST"
    ;;
esac
HEALTH_URL="${DHI_HEALTH_URL:-http://$HEALTH_HOST:$BIND_PORT/api/health}"
LOG_FILE="${DHI_E2E_LOG:-${TMPDIR:-/tmp}/dhi-e2e.log}"
HEALTH_FILE="${DHI_E2E_HEALTH:-${TMPDIR:-/tmp}/dhi-e2e-health.json}"

DATABASE_URL="$DATABASE_URL" \
MIGRATION_DATABASE_URL="$MIGRATION_DATABASE_URL" \
DHI_BIND="$BIND" \
RUST_LOG="dhi=info" \
./target/debug/dhi-server >"$LOG_FILE" 2>&1 &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

for _ in {1..30}; do
  if curl -fsS "$HEALTH_URL" >"$HEALTH_FILE" 2>/dev/null; then
    cat "$HEALTH_FILE"
    echo
    echo "e2e ok"
    exit 0
  fi
  sleep 1
done

echo "e2e failed: health endpoint ($HEALTH_URL) did not respond"
tail -40 "$LOG_FILE"
exit 1
