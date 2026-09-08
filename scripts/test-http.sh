#!/usr/bin/env bash
# Only disposable containers, loopback ports and an empty database are used.
set -euo pipefail
cd "$(dirname "$0")/.."
pg="hibana-http-pg-$$"
redis="hibana-http-redis-$$"
cleanup() {
  docker stop "$pg" >/dev/null 2>&1 || true
  docker stop "$redis" >/dev/null 2>&1 || true
}
trap cleanup EXIT
docker run --rm -d --name "$pg" -e POSTGRES_DB=hibana_http -e POSTGRES_HOST_AUTH_METHOD=trust -p 127.0.0.1::5432 postgres:16 >/dev/null
docker run --rm -d --name "$redis" -p 127.0.0.1::6379 redis:7-alpine >/dev/null
for _ in $(seq 1 30); do
  if docker exec "$pg" pg_isready -U postgres >/dev/null 2>&1; then break; fi
  sleep 1
done
pg_port="$(docker port "$pg" 5432/tcp | sed 's/.*://')"
redis_port="$(docker port "$redis" 6379/tcp | sed 's/.*://')"
export REDIS_URL="redis://127.0.0.1:$redis_port"
export HTTP_TEST_DATABASE_URL="postgres://postgres@127.0.0.1:$pg_port/hibana_http"
export DATABASE_URL="postgres://faas_app:faas_app@127.0.0.1:$pg_port/hibana_http"
export S3_ENDPOINT=http://127.0.0.1:19000 S3_REGION=us-east-1 S3_BUCKET=test-components S3_ACCESS_KEY=test-only S3_SECRET_KEY=test-only
export BOOTSTRAP_ADMIN_TOKEN=test-only JOB_SIGNING_KEY=0001020304050607080900010203040506070809000102030405060708090001 JOB_SIGNING_KID=test
export SECRETS_MASTER_KEY=0001020304050607080900010203040506070809000102030405060708090001 SECRETS_MASTER_KID=test
export WORKER_HTTP_URL=http://127.0.0.1:1
export HTTP_TEST_PG_CONTAINER="$pg"
cargo test --locked -p hibana-control-plane http_mvp_regression -- --ignored --nocapture
cargo test --locked -p hibana-worker execution_configuration_survives_reused_names -- --ignored --nocapture
cargo build --locked -p hibana-control-plane -p hibana-worker
node scripts/test-upload-lifecycle.mjs
