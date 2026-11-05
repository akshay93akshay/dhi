#!/usr/bin/env bash
# Rehearse a restore into a throwaway database and compare row counts.
# Usage: scripts/restore-test.sh [dump-file]
set -euo pipefail
cd "$(dirname "$0")/.."

CONTAINER="dhi-postgres-1"
DB="dhi"
DB_OWNER="dhi_owner"
TMP_PREFIX="dhi_restore_test_"

usage() {
  echo "usage: $0 [dump-file]" >&2
  exit 2
}

[ "$#" -le 1 ] || usage

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
if [ "$TMPDB" = "$DB" ]; then
  echo "error: refusing to use the live database as a restore target" >&2
  exit 1
fi

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
          if docker exec "$CONTAINER" dropdb -U "$DB_OWNER" --if-exists "$TMPDB" >/dev/null 2>&1; then
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

docker exec "$CONTAINER" createdb -U "$DB_OWNER" "$TMPDB"
TMPDB_CREATED=1

tmp_log="$(mktemp "${TMPDIR:-/tmp}/dhi-restore-test.XXXXXX")"
if ! docker exec -i "$CONTAINER" pg_restore -U "$DB_OWNER" -d "$TMPDB" \
  --single-transaction --exit-on-error --no-owner --no-privileges \
  <"$dump" 2>"$tmp_log"; then
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
  docker exec "$CONTAINER" psql -U "$DB_OWNER" -d "$DB" -Atc "$counts_sql"
)
read -r test_tasks test_facts test_intents test_results < <(
  docker exec "$CONTAINER" psql -U "$DB_OWNER" -d "$TMPDB" -Atc "$counts_sql"
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
