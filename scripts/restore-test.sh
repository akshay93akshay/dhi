#!/usr/bin/env bash
# Rehearse a restore into a throwaway database and compare row counts.
# Requires prebuilt PostgreSQL 16 client tools on PATH.
#   export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app
#   # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries
#   export PATH="$PG_BIN:$PATH"
#
# Usage: scripts/restore-test.sh [dump-file]
# Env:   MIGRATION_DATABASE_URL  owner connection string used to reach the
#                               server (default postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi)
set -euo pipefail
cd "$(dirname "$0")/.."

MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL:-postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi}"
TMP_PREFIX="dhi_restore_test_"

usage() {
  echo "usage: $0 [dump-file]" >&2
  exit 2
}

[ "$#" -le 1 ] || usage

missing=""
for tool in psql createdb dropdb pg_restore; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "error: PostgreSQL client tools not found:$missing" >&2
  echo "install prebuilt PostgreSQL 16 (Postgres.app or EDB binaries) and add its bin" >&2
  echo "directory to PATH, for example:" >&2
  echo '  export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app' >&2
  echo '  # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries' >&2
  echo '  export PATH="$PG_BIN:$PATH"' >&2
  exit 1
fi

# Connections to the server, then to the live and temporary databases.
server_url="${MIGRATION_DATABASE_URL%%\?*}"
case "$server_url" in
  postgres://*/*)
    server_base="${server_url%/*}"
    ;;
  postgres://*:*)
    echo "error: MIGRATION_DATABASE_URL has no database name: $MIGRATION_DATABASE_URL" >&2
    exit 1
    ;;
  *)
    echo "error: unsupported MIGRATION_DATABASE_URL: $MIGRATION_DATABASE_URL" >&2
    exit 1
    ;;
esac
live_url="$server_url"

if [ "$#" -eq 1 ]; then
  dump="$1"
else
  dump=""
  if [ -d backups ]; then
    dump="$(ls -1t backups/dhi-*.dump 2>/dev/null | head -n 1 || true)"
  fi
  if [ -z "$dump" ]; then
    echo "error: no backup found at backups/dhi-*.dump" >&2
    echo "run: scripts/backup.sh" >&2
    exit 1
  fi
fi

if [ ! -f "$dump" ]; then
  echo "error: dump file not found: $dump" >&2
  exit 1
fi
if [ ! -s "$dump" ]; then
  echo "error: dump file is empty: $dump" >&2
  exit 1
fi
dump="$(cd "$(dirname "$dump")" && pwd)/$(basename "$dump")"

TMPDB="${TMP_PREFIX}$(date +%Y%m%d%H%M%S)"
case "$TMPDB" in
  "$TMP_PREFIX"*) ;;
  *)
    echo "error: refusing unsafe temporary database name: $TMPDB" >&2
    exit 1
    ;;
esac
if [ "$server_base/$TMPDB" = "$live_url" ]; then
  echo "error: refusing to use the live database as a restore target" >&2
  exit 1
fi
tmp_url="$server_base/$TMPDB"

tmp_log=""
TMPDB_CREATED=0
KEEP_TMPDB=0

cleanup() {
  local rc=$?
  trap - EXIT
  set +e

  if [ "$TMPDB_CREATED" = "1" ]; then
    if [ "$KEEP_TMPDB" = "1" ]; then
      echo "temporary database kept for investigation: $TMPDB" >&2
    else
      case "$TMPDB" in
        "$TMP_PREFIX"*)
          if dropdb --if-exists --maintenance-db="$server_url" "$TMPDB" >/dev/null 2>&1; then
            echo "temporary database dropped: $TMPDB"
          else
            echo "warning: failed to drop temporary database: $TMPDB" >&2
            [ "$rc" -ne 0 ] || rc=1
          fi
          ;;
        *)
          echo "warning: refusing to drop unsafe database name: $TMPDB" >&2
          [ "$rc" -ne 0 ] || rc=1
          ;;
      esac
    fi
  fi

  if [ -n "$tmp_log" ]; then
    rm -f "$tmp_log"
  fi
  exit "$rc"
}
trap cleanup EXIT

echo "dump: $dump"
echo "temporary database: $TMPDB"

createdb --maintenance-db="$server_url" "$TMPDB"
TMPDB_CREATED=1

tmp_log="$(mktemp "${TMPDIR:-/tmp}/dhi-restore-test.XXXXXX")"
if ! pg_restore -d "$tmp_url" \
  --single-transaction --exit-on-error --no-owner --no-privileges \
  "$dump" 2>"$tmp_log"; then
  echo "error: pg_restore failed" >&2
  sed 's/^/  /' "$tmp_log" >&2 || true
  exit 1
fi
rm -f "$tmp_log"
tmp_log=""

counts_sql="select
  (select count(*) from tasks)::text || ' ' ||
  (select count(*) from facts)::text || ' ' ||
  (select count(*) from intents)::text || ' ' ||
  (select count(*) from results)::text"

read -r live_tasks live_facts live_intents live_results < <(
  psql "$live_url" -Atc "$counts_sql"
)
read -r test_tasks test_facts test_intents test_results < <(
  psql "$tmp_url" -Atc "$counts_sql"
)

printf '%-8s %10s %10s\n' "table" "live" "restored"
printf '%-8s %10s %10s\n' "tasks" "$live_tasks" "$test_tasks"
printf '%-8s %10s %10s\n' "facts" "$live_facts" "$test_facts"
printf '%-8s %10s %10s\n' "intents" "$live_intents" "$test_intents"
printf '%-8s %10s %10s\n' "results" "$live_results" "$test_results"

mismatch=0
[ "$live_tasks" = "$test_tasks" ] || mismatch=1
[ "$live_facts" = "$test_facts" ] || mismatch=1
[ "$live_intents" = "$test_intents" ] || mismatch=1
[ "$live_results" = "$test_results" ] || mismatch=1

if [ "$mismatch" -ne 0 ]; then
  KEEP_TMPDB=1
  echo "error: restored row counts differ from the live database" >&2
  exit 1
fi

echo "restore test passed: row counts match"
