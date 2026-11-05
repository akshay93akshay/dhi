#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "${SKIP_DB:-0}" = "1" ]; then
  echo "[db] SKIP_DB=1, skipping postgres startup"
else
  echo "[db] starting postgres"
  docker compose up -d postgres
  ready=""
  for _ in $(seq 1 30); do
    if docker compose exec -T postgres pg_isready -U dhi_owner -d dhi >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 1
  done
  if [ -z "$ready" ]; then
    echo "error: postgres not ready after 30 tries" >&2
    exit 1
  fi
  export DATABASE_URL="${DATABASE_URL:-postgres://dhi_test:dhi_test@127.0.0.1:5432/dhi}"
fi

echo "[1/2] rust tests"
cargo test --workspace

echo "[2/2] web build"
(cd web && npm run build:static)

echo "tests ok"
