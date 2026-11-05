#!/usr/bin/env bash
# Create a compressed logical backup of the live DHI database.
# Usage: scripts/backup.sh [output-dir]
set -euo pipefail
cd "$(dirname "$0")/.."

umask 077

CONTAINER="dhi-postgres-1"
DB="dhi"
DB_OWNER="dhi_owner"

usage() {
  echo "usage: $0 [output-dir]" >&2
  exit 2
}

[ "$#" -le 1 ] || usage

out_dir="${1:-backups}"

if ! mkdir -p "$out_dir"; then
  echo "error: cannot create backup directory: $out_dir" >&2
  exit 1
fi
if [ ! -d "$out_dir" ]; then
  echo "error: backup path is not a directory: $out_dir" >&2
  exit 1
fi
chmod 700 "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"

timestamp="$(date +%Y%m%d-%H%M%S)"
dump="$out_dir/dhi-$timestamp.dump"
tmp="$out_dir/.dhi-$timestamp.dump.tmp.$$"

cleanup() {
  rm -f "$tmp"
}
trap cleanup EXIT

if [ -e "$dump" ]; then
  echo "error: backup already exists: $dump" >&2
  exit 1
fi

run_pg_dump() {
  if docker compose version >/dev/null 2>&1; then
    docker compose exec -T postgres pg_dump -U "$DB_OWNER" -d "$DB" -Fc
  else
    docker exec "$CONTAINER" pg_dump -U "$DB_OWNER" -d "$DB" -Fc
  fi
}

if ! run_pg_dump >"$tmp"; then
  echo "error: pg_dump failed; no backup written" >&2
  exit 1
fi
if [ ! -s "$tmp" ]; then
  echo "error: pg_dump produced an empty file; no backup written" >&2
  exit 1
fi

chmod 600 "$tmp"
mv -f "$tmp" "$dump"
trap - EXIT
chmod 600 "$dump"

size="$(du -h "$dump" | awk '{print $1}')"
printf 'backup written: %s\n' "$dump"
printf 'size: %s\n' "$size"
