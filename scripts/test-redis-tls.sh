#!/usr/bin/env bash
# Isolated Redis TCP/TLS tests. Never connect to the platform's configured Redis.
set -euo pipefail
cd "$(dirname "$0")/.."
# Fetch/build before replacing the process trust roots with our private test CA.
cargo test --locked -p hibana-control-plane redis_transport_ --no-run
cargo build --locked -p hibana-control-plane
certs="$(mktemp -d "${TMPDIR:-/tmp}/hibana-redis-tls.XXXXXX")"
trusted="hibana-redis-tls-$$"
untrusted="hibana-redis-untrusted-$$"
cleanup() {
  docker stop "$trusted" "$untrusted" >/dev/null 2>&1 || true
  rm -rf "$certs"
}
trap cleanup EXIT

for issuer in trusted untrusted; do
  directory="$certs/$issuer"
  mkdir "$directory"
  openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 2 \
    -subj "/CN=Hibana Redis $issuer test CA" \
    -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' \
    -keyout "$directory/ca.key" -out "$directory/ca.crt" >/dev/null 2>&1
  openssl req -newkey rsa:2048 -nodes -sha256 -subj /CN=localhost \
    -keyout "$directory/server.key" -out "$directory/server.csr" >/dev/null 2>&1
  cat > "$directory/server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
EOF
  openssl x509 -req -sha256 -days 2 -in "$directory/server.csr" \
    -CA "$directory/ca.crt" -CAkey "$directory/ca.key" -CAcreateserial \
    -extfile "$directory/server.ext" -out "$directory/server.crt" >/dev/null 2>&1
done

start_redis() {
  local name="$1" issuer="$2"
  docker run --rm -d --name "$name" --user "$(id -u):$(id -g)" \
    -v "$certs/$issuer:/tls:ro" -p 127.0.0.1::6379 -p 127.0.0.1::6380 \
    redis:7-alpine redis-server --save '' --appendonly no --requirepass test-only \
    --tls-port 6380 --tls-cert-file /tls/server.crt --tls-key-file /tls/server.key \
    --tls-ca-cert-file /tls/ca.crt --tls-auth-clients no >/dev/null
  for _ in $(seq 1 30); do
    if docker exec -e REDISCLI_AUTH=test-only "$name" redis-cli \
      --tls -h localhost -p 6380 --cacert /tls/ca.crt PING 2>/dev/null | grep -q '^PONG'; then
      return
    fi
    sleep 1
  done
  docker logs "$name"
  return 1
}
start_redis "$trusted" trusted
start_redis "$untrusted" untrusted

tcp_port="$(docker port "$trusted" 6379/tcp | sed 's/.*://')"
tls_port="$(docker port "$trusted" 6380/tcp | sed 's/.*://')"
untrusted_port="$(docker port "$untrusted" 6380/tcp | sed 's/.*://')"
export SSL_CERT_FILE="$certs/trusted/ca.crt"
export SSL_CERT_DIR="$certs/empty"
mkdir "$SSL_CERT_DIR"
export HIBANA_TEST_REDIS_TCP_URL="redis://:test-only@127.0.0.1:$tcp_port/0"
export HIBANA_TEST_REDIS_TLS_URL="rediss://:test-only@localhost:$tls_port/0"
export HIBANA_TEST_REDIS_UNTRUSTED_URL="rediss://:test-only@localhost:$untrusted_port/0"
cargo test --offline --locked -p hibana-control-plane redis_transport_ -- --include-ignored

# Exercise the exact dependency probe used by platform install as well as Store.
# Other probes receive closed loopback endpoints, never inherited infrastructure URLs.
export DATABASE_URL=postgres://test-only@127.0.0.1:1/test
export S3_ENDPOINT=http://127.0.0.1:1 S3_BUCKET=test-only WORKER_HTTP_URL=http://127.0.0.1:1
export REDIS_URL="$HIBANA_TEST_REDIS_TLS_URL"
cargo run --offline --quiet --locked -p hibana-control-plane -- --maintenance check control-plane 127.0.0.1 \
  | python3 -c 'import json,sys; assert json.load(sys.stdin)["checks"]["redis"] is True'
export REDIS_URL="$HIBANA_TEST_REDIS_UNTRUSTED_URL"
cargo run --offline --quiet --locked -p hibana-control-plane -- --maintenance check control-plane 127.0.0.1 \
  | python3 -c 'import json,sys; assert json.load(sys.stdin)["checks"]["redis"] is False'
echo 'PASS Redis TCP/TLS operations, certificate verification and installation probes'
