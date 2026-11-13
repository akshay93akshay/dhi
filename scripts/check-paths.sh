#!/usr/bin/env bash
# Read-only check for non-macOS machine-specific paths in product files.
# `/Users/` is valid on this macOS-only branch; `/home/` and `C:\` are not.
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
  matches="$(rg "${RG_ARGS[@]}" -e '/home/' -e 'C:\' 2>/dev/null || true)"
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
    -e '/home/' -e 'C:\' . 2>/dev/null || true)"
fi

matches="$(printf '%s\n' "$matches" | grep -E '(/home/|C:\\)' || true)"

if [ -n "$matches" ]; then
  printf '%s\n' "$matches" | awk -F: 'NF >= 2 { print $1 ":" $2 }'
  exit 1
fi

echo "path check ok"
