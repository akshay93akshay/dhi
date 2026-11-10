#!/usr/bin/env bash
# Read-only check for machine-specific paths in product files.
# Prints path:line only; it never prints file contents.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  echo "usage: $0" >&2
  exit 2
}

[ "$#" -eq 0 ] || usage

# This checker necessarily contains the search strings, so it skips itself.
RG_ARGS=(
  --hidden
  --line-number
  --no-heading
  --color never
  --fixed-strings
  --glob '!.git/**'
  --glob '!target/**'
  --glob '!web/node_modules/**'
  --glob '!web/.next/**'
  --glob '!web/out/**'
  --glob '!backups/**'
  --glob '!workspace/**'
  --glob '!scripts/check-paths.sh'
)

matches=""
if command -v rg >/dev/null 2>&1; then
  matches="$(rg "${RG_ARGS[@]}" -e '/Users/' -e '/home/' -e 'C:\' 2>/dev/null || true)"
else
  matches="$(grep -rInF \
    --exclude-dir=.git \
    --exclude-dir=target \
    --exclude-dir=node_modules \
    --exclude-dir=.next \
    --exclude-dir=out \
    --exclude-dir=backups \
    --exclude-dir=workspace \
    --exclude=check-paths.sh \
    -e '/Users/' -e '/home/' -e 'C:\' . 2>/dev/null || true)"
fi

# `/home/linuxbrew` is a path inside the task image, not a host path. The
# task container hides it with a read-only tmpfs. Remove only that token;
# a line that also contains another `/home/`, `/Users/`, or `C:\` path still
# fails.
matches="$(printf '%s\n' "$matches" \
  | sed 's#/home/linuxbrew##g' \
  | grep -E '(/home/|/Users/|C:\\)' || true)"

if [ -n "$matches" ]; then
  printf '%s\n' "$matches" | awk -F: 'NF >= 2 { print $1 ":" $2 }'
  exit 1
fi

echo "path check ok"
