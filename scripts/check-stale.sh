#!/usr/bin/env bash
# Read-only check for stale development-language markers in product files.
# Prints path:line only; it never prints file contents.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  echo "usage: $0" >&2
  exit 2
}

[ "$#" -eq 0 ] || usage

# Phase/Wave/Pre-Phase/6E are case-sensitive: lowercase uses such as the SSE
# `phase` variable are legitimate. The remaining markers are matched
# case-insensitively.
CASE_SENSITIVE='\bPhase\b|\bWave\b|Pre-Phase|\b6E\b'
CASE_INSENSITIVE='later block|in this block|deliberately empty|\bTODO\b|\bFIXME\b|\bHACK\b|DHI_BUILD_PLAN|CHANGELOG|web/AGENTS|/testdata/'

if command -v rg >/dev/null 2>&1; then
  matches="$(
    rg --hidden --line-number --no-heading --color never \
      --glob '!.git/**' \
      --glob '!target/**' \
      --glob '!web/node_modules/**' \
      --glob '!web/.next/**' \
      --glob '!web/out/**' \
      --glob '!Cargo.lock' \
      --glob '!web/package-lock.json' \
      --glob '!scripts/check-stale.sh' \
      -e "$CASE_SENSITIVE" \
      -e "(?i)$CASE_INSENSITIVE" \
      . 2>/dev/null || true
  )"
else
  matches="$(
    grep -rInE \
      --exclude-dir=.git \
      --exclude-dir=target \
      --exclude-dir=node_modules \
      --exclude-dir=.next \
      --exclude-dir=out \
      --exclude=Cargo.lock \
      --exclude=package-lock.json \
      --exclude=check-stale.sh \
      -e "$CASE_SENSITIVE" . 2>/dev/null || true
    grep -rIniE \
      --exclude-dir=.git \
      --exclude-dir=target \
      --exclude-dir=node_modules \
      --exclude-dir=.next \
      --exclude-dir=out \
      --exclude=Cargo.lock \
      --exclude=package-lock.json \
      --exclude=check-stale.sh \
      -e "$CASE_INSENSITIVE" . 2>/dev/null || true
  )"
fi

matches="$(printf '%s\n' "$matches" | awk -F: 'NF >= 2 { print $1 ":" $2 }' | sort -u)"

if [ -n "$matches" ]; then
  printf '%s\n' "$matches"
  exit 1
fi

echo "stale check ok"
