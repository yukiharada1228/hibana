#!/usr/bin/env bash
# Disposable services only; no existing Hibana or identity-provider data is used.
set -euo pipefail
cd "$(dirname "$0")/.."
pg="hibana-oidc-pg-$$"
redis="hibana-oidc-redis-$$"
keycloak="hibana-oidc-keycloak-$$"
cleanup() { docker stop "$keycloak" "$redis" "$pg" >/dev/null 2>&1 || true; }
trap cleanup EXIT
docker run --rm -d --name "$pg" -e POSTGRES_DB=hibana_oidc -e POSTGRES_HOST_AUTH_METHOD=trust -p 127.0.0.1::5432 postgres:16 >/dev/null
docker run --rm -d --name "$redis" -p 127.0.0.1::6379 redis:7-alpine >/dev/null
docker run --rm -d --name "$keycloak" -p 127.0.0.1::8080 \
  -e KC_BOOTSTRAP_ADMIN_USERNAME=fixture-admin -e KC_BOOTSTRAP_ADMIN_PASSWORD=fixture-admin-password \
  quay.io/keycloak/keycloak:26.7.4 start-dev >/dev/null
export OIDC_TEST_PG="$pg" OIDC_TEST_REDIS="$redis" OIDC_TEST_KEYCLOAK="$keycloak"
cargo build --locked -p hibana-control-plane
npm run build --prefix console
node scripts/test-oidc.mjs
