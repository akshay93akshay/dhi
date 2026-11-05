#!/usr/bin/env bash
# Read-only health check for a running DHI deployment.
# Never restarts services or writes files.
#
# Usage: scripts/dhi-health.sh [--api-only]
set -euo pipefail
cd "$(dirname "$0")/.."

DHI_API_URL="${DHI_API_URL:-http://127.0.0.1:8080}"
DHI_CHESS_URL="${DHI_CHESS_URL:-http://127.0.0.1:8090}"
DHI_MIN_FREE_GB="${DHI_MIN_FREE_GB:-5}"
DHI_PG_SERVICE="${DHI_PG_SERVICE:-postgres}"
DHI_PG_USER="${DHI_PG_USER:-dhi_owner}"
DHI_PG_DB="${DHI_PG_DB:-dhi}"

ok_count=0
warn_count=0
fail_count=0
skip_count=0

usage() {
  echo "usage: $0 [--api-only]" >&2
  exit 2
}

ok() {
  printf '[ok]   %-8s %s\n' "$1" "$2"
  ok_count=$((ok_count + 1))
}

warn() {
  printf '[warn] %-8s %s\n' "$1" "$2"
  warn_count=$((warn_count + 1))
}

fail() {
  printf '[fail] %-8s %s\n' "$1" "$2"
  fail_count=$((fail_count + 1))
}

skip() {
  printf '[skip] %-8s %s\n' "$1" "$2"
  skip_count=$((skip_count + 1))
}

api_only=0
for arg in "$@"; do
  case "$arg" in
    --api-only)
      api_only=1
      ;;
    *)
      usage
      ;;
  esac
done

# 1. API health (required).
health_url="$DHI_API_URL/api/health"
api_response="$(curl -fsS --max-time 3 -w '\n%{http_code}' "$health_url" 2>/dev/null || true)"
api_body="${api_response%$'\n'*}"
api_code="${api_response##*$'\n'}"

api_ok=0
if [ "$api_code" = "200" ] && printf '%s' "$api_body" | python3 -c '
import json
import sys

try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(1)
sys.exit(0 if isinstance(data, dict) and data.get("status") == "ok" else 1)
' 2>/dev/null; then
  api_ok=1
fi

if [ "$api_ok" -eq 1 ]; then
  ok "api" "GET $health_url -> 200"
elif [ "$api_code" = "200" ]; then
  fail "api" "GET $health_url -> 200 but JSON status != ok"
else
  fail "api" "GET $health_url -> ${api_code:-no response} (expected 200 status=ok)"
fi

# 2. UI (required unless --api-only).
ui_response="$(curl -fsS --max-time 3 -o /dev/null -w '%{http_code} %{content_type}' "$DHI_API_URL/" 2>/dev/null || true)"
ui_code="${ui_response%% *}"
ui_type="${ui_response#* }"

ui_ok=0
if [ "$ui_code" = "200" ]; then
  case "$ui_type" in
    *text/html*)
      ui_ok=1
      ;;
  esac
fi

ui_detail="GET $DHI_API_URL/ -> ${ui_code:-no response}${ui_type:+ $ui_type} (expected 200 text/html)"
if [ "$ui_ok" -eq 1 ]; then
  ok "ui" "GET $DHI_API_URL/ -> 200 text/html"
elif [ "$api_only" -eq 1 ]; then
  warn "ui" "$ui_detail"
else
  fail "ui" "$ui_detail"
fi

# 3. Postgres (required).
container=""
if compose_out="$(docker compose ps -q "$DHI_PG_SERVICE" 2>/dev/null)"; then
  container="$(printf '%s\n' "$compose_out" | head -n 1)"
fi
[ -n "$container" ] || container="dhi-postgres-1"

inspect="$(docker inspect --format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$container" 2>/dev/null || true)"
pg_state="${inspect%% *}"
pg_health="${inspect#* }"
[ -n "$pg_health" ] || pg_health="none"

if [ "$pg_state" = "running" ] && { [ "$pg_health" = "healthy" ] || [ "$pg_health" = "none" ]; }; then
  pg_ready=0
  if docker compose exec -T "$DHI_PG_SERVICE" pg_isready -U "$DHI_PG_USER" -d "$DHI_PG_DB" >/dev/null 2>&1; then
    pg_ready=1
  elif docker exec "$container" pg_isready -U "$DHI_PG_USER" -d "$DHI_PG_DB" >/dev/null 2>&1; then
    pg_ready=1
  fi

  if [ "$pg_ready" -eq 1 ]; then
    ok "postgres" "$container running $pg_health"
  else
    fail "postgres" "$container running $pg_health but pg_isready failed"
  fi
else
  fail "postgres" "${inspect:-$container not found}"
fi

# 4. Disk (required).
free_kb="$(df -Pk / | awk 'NR == 2 { print $4 }')"
if [ -n "$free_kb" ]; then
  free_gib="$(awk -v kb="$free_kb" 'BEGIN { printf "%.1f", kb / 1048576 }')"
  min_gib="$(awk -v gb="$DHI_MIN_FREE_GB" 'BEGIN { printf "%.1f", gb }')"

  if awk -v free="$free_kb" -v min="$DHI_MIN_FREE_GB" 'BEGIN { exit !(free / 1048576 >= min) }'; then
    ok "disk" "/ $free_gib GiB free (min $min_gib GiB)"
  else
    fail "disk" "/ $free_gib GiB free (min $min_gib GiB)"
  fi
else
  fail "disk" "could not read free space for /"
fi

# 5. Chess board (optional).
if lsof -nP -iTCP:8090 -sTCP:LISTEN >/dev/null 2>&1; then
  chess_code="$(curl -fsS --max-time 3 -o /dev/null -w '%{http_code}' "$DHI_CHESS_URL/" 2>/dev/null || true)"
  case "$chess_code" in
    2??|3??)
      ok "chess" "GET $DHI_CHESS_URL/ -> $chess_code"
      ;;
    *)
      warn "chess" "GET $DHI_CHESS_URL/ -> ${chess_code:-no response} (optional)"
      ;;
  esac
else
  skip "chess" "not listening"
fi

if [ "$fail_count" -gt 0 ]; then
  echo "dhi-health: FAIL ($ok_count ok, $warn_count warn, $fail_count fail, $skip_count skip)"
  exit 1
fi

echo "dhi-health: PASS ($ok_count ok, $warn_count warn, 0 fail, $skip_count skip)"
