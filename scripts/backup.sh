#!/usr/bin/env bash
# Create a compressed logical backup of the live DHI database with pg_dump.
# Requires prebuilt PostgreSQL 16 client tools on PATH.
#   export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app
#   # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries
#   export PATH="$PG_BIN:$PATH"
#
# Usage: scripts/backup.sh [output-dir]
# Env:   MIGRATION_DATABASE_URL  owner connection string used by pg_dump
#                               (default postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi)
set -euo pipefail
cd "$(dirname "$0")/.."

umask 077

MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL:-postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi}"

usage() {
  echo "usage: $0 [output-dir]" >&2
  exit 2
}

[ "$#" -le 1 ] || usage

if ! command -v pg_dump >/dev/null 2>&1; then
  echo "error: pg_dump not found on PATH" >&2
  echo "install prebuilt PostgreSQL 16 (Postgres.app or EDB binaries) and add its bin" >&2
  echo "directory to PATH, for example:" >&2
  echo '  export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"   # Postgres.app' >&2
  echo '  # or: export PG_BIN="/path/to/pgsql/bin"                              # EDB binaries' >&2
  echo '  export PATH="$PG_BIN:$PATH"' >&2
  exit 1
fi

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

if ! pg_dump --format=custom --no-owner --no-privileges \
  -d "$MIGRATION_DATABASE_URL" >"$tmp"; then
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
