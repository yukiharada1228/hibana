#!/usr/bin/env bash
# Disposable services only; no existing Hibana or identity-provider data is used.
set -euo pipefail
cd "$(dirname "$0")/.."
pg="hibana-oidc-pg-$$"
redis="hibana-oidc-redis-$$"
keycloak="hibana-oidc-keycloak-$$"
certs=""
cleanup() {
  docker stop "$keycloak" "$redis" "$pg" >/dev/null 2>&1 || true
  if [[ -n "$certs" ]]; then rm -rf "$certs"; fi
}
trap cleanup EXIT
docker run --rm -d --name "$pg" -e POSTGRES_DB=hibana_oidc -e POSTGRES_HOST_AUTH_METHOD=trust -p 127.0.0.1::5432 postgres:16 >/dev/null
docker run --rm -d --name "$redis" -p 127.0.0.1::6379 redis:7-alpine >/dev/null
keycloak_args=(-p 127.0.0.1::8080)
if [[ "${HIBANA_TEST_HTTPS:-0}" == 1 ]]; then
  certs="$(mktemp -d "${TMPDIR:-/tmp}/hibana-oidc-tls.XXXXXX")"
  openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 2 \
    -subj '/CN=Hibana OIDC fixture CA' -addext 'basicConstraints=critical,CA:TRUE' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -keyout "$certs/ca.key" -out "$certs/ca.crt" >/dev/null 2>&1
  openssl req -newkey rsa:2048 -nodes -sha256 -subj /CN=localhost \
    -keyout "$certs/server.key" -out "$certs/server.csr" >/dev/null 2>&1
  cat > "$certs/server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1
EOF
  openssl x509 -req -sha256 -days 2 -in "$certs/server.csr" \
    -CA "$certs/ca.crt" -CAkey "$certs/ca.key" -CAcreateserial \
    -extfile "$certs/server.ext" -out "$certs/server.crt" >/dev/null 2>&1
  # Only the disposable server key is mounted; the CA key stays host-private.
  chmod 644 "$certs/server.key"
  keycloak_args=(-p 127.0.0.1::8443
    -v "$certs/server.crt:/tls/server.crt:ro" -v "$certs/server.key:/tls/server.key:ro"
    -e KC_HTTPS_CERTIFICATE_FILE=/tls/server.crt -e KC_HTTPS_CERTIFICATE_KEY_FILE=/tls/server.key)
  export OIDC_TEST_TLS_CERT="$certs/server.crt" OIDC_TEST_TLS_KEY="$certs/server.key"
  export OIDC_TEST_CA="$certs/ca.crt" NODE_EXTRA_CA_CERTS="$certs/ca.crt"
fi
docker run --rm -d --name "$keycloak" "${keycloak_args[@]}" \
  -e KC_BOOTSTRAP_ADMIN_USERNAME=fixture-admin -e KC_BOOTSTRAP_ADMIN_PASSWORD=fixture-admin-password \
  quay.io/keycloak/keycloak:26.7.4 start-dev >/dev/null
export OIDC_TEST_PG="$pg" OIDC_TEST_REDIS="$redis" OIDC_TEST_KEYCLOAK="$keycloak"
cargo build --locked -p hibana-control-plane
npm run build --prefix console
node scripts/test-oidc.mjs
