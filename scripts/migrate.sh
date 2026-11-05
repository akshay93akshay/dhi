#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

docker compose up -d postgres

for _ in {1..30}; do
  if docker compose exec -T postgres pg_isready -U dhi_owner -d dhi >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

echo "[1/3] applying migrations twice"
for pass in 1 2; do
  echo "  pass $pass"
  for migration in crates/dhi-store/migrations/*.sql; do
    echo "    $migration"
    docker compose exec -T postgres \
      psql -U dhi_owner -d dhi -v ON_ERROR_STOP=1 < "$migration"
  done
done

echo "[2/3] applying runtime grants"
docker compose exec -T postgres \
  psql -U dhi_owner -d dhi -v ON_ERROR_STOP=1 < docker/grants/02-grants.sql

echo "[3/3] verifying activity lock"
docker compose exec -T postgres \
  psql -U dhi_owner -d dhi -v ON_ERROR_STOP=1 <<'SQL'
DO $$
BEGIN
  IF has_table_privilege('dhi_app', 'activity', 'UPDATE')
     OR has_table_privilege('dhi_app', 'activity', 'DELETE') THEN
    RAISE EXCEPTION 'activity lock failed: dhi_app can update or delete activity';
  END IF;
END
$$;
SQL

echo "migrate ok"
