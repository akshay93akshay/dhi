#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

docker compose up -d postgres
scripts/migrate.sh

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

DATABASE_URL="${DATABASE_URL:-postgres://dhi_app:dhi_app@127.0.0.1:5432/dhi}" \
MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL:-postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi}" \
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
