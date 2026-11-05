#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

docker compose up -d postgres
scripts/migrate.sh

export DATABASE_URL="${DATABASE_URL:-postgres://dhi_app:dhi_app@127.0.0.1:5432/dhi}"
export MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL:-postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi}"
export DHI_BIND="${DHI_BIND:-127.0.0.1:8080}"
export RUST_LOG="${RUST_LOG:-dhi=info,tower_http=info}"

cargo run -p dhi-server &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

cd web
if [ ! -d node_modules ]; then
  npm install
fi
npm run dev -- --hostname 0.0.0.0
