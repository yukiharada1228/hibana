#!/usr/bin/env bash
# 全 Workers 互換バインディング(env/KV/R2/D1/DO/Queues) を 1 コンポーネントで end-to-end 検証する。
#
# 前提: control-plane / worker / Postgres / NATS / MinIO が起動済みで、ingress gateway が
#       ${GATEWAY} で待ち受けていること（deploy はローカルの hibana CLI を使う）。
#
# 使い方:
#   ./scripts/smoke.sh
# 環境変数で上書き可:
#   GATEWAY               ingress gateway の URL           (default http://localhost:8080)
#   HIBANA_INGRESS_DOMAIN 公開サブドメインのベースドメイン  (default hibana.local)
#   TENANT                テナント slug                    (default smoke)
#   QUEUE_TIMEOUT         queue consumer 反映待ちの秒数     (default 20)
set -euo pipefail

GATEWAY="${GATEWAY:-http://localhost:8080}"
HIBANA_INGRESS_DOMAIN="${HIBANA_INGRESS_DOMAIN:-hibana.local}"
TENANT="${TENANT:-smoke}"
QUEUE_TIMEOUT="${QUEUE_TIMEOUT:-20}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$ROOT/sdk/examples/all-bindings"
CLI="$ROOT/sdk/src/cli.mjs"
HOST_HEADER="Host: all-bindings.${TENANT}.${HIBANA_INGRESS_DOMAIN}"

pass=0
fail=0
red() { printf '\033[31m%s\033[0m\n' "$1"; }
green() { printf '\033[32m%s\033[0m\n' "$1"; }

# get PATH -> stdout body
get() { curl -sS "${GATEWAY}$1" -H "$HOST_HEADER"; }

# assert PATH EXPECTED
assert() {
  local path="$1" want="$2" got
  got="$(get "$path")"
  if [ "$got" = "$want" ]; then
    green "PASS  $path -> $got"
    pass=$((pass + 1))
  else
    red "FAIL  $path -> got '$got', want '$want'"
    fail=$((fail + 1))
  fi
}

echo "==> deploy all-bindings ($GATEWAY, *.${TENANT}.${HIBANA_INGRESS_DOMAIN})"
( cd "$APP_DIR" && HIBANA_INGRESS_DOMAIN="$HIBANA_INGRESS_DOMAIN" node "$CLI" deploy )

# 冷起動（初回 precompile）と single-shot アサートのレースを避けるため、"/" が応答するまで待つ。
echo "==> wait for app readiness"
for _ in $(seq 1 30); do
  [ "$(get /)" = "all-bindings hello" ] && { green "ready"; break; }
  sleep 1
done

echo "==> assert synchronous bindings"
assert /       "all-bindings hello"
assert /kv     "kv-ok"
assert /r2     "r2-ok"
assert /d1     "d1-ok"

# DO は呼ぶたびに増える。1 回目の値を読んで数値であることだけ確認する。
do1="$(get /do)"
if [[ "$do1" =~ ^[0-9]+$ ]]; then
  green "PASS  /do -> $do1 (numeric, persisted)"
  pass=$((pass + 1))
else
  red "FAIL  /do -> '$do1' (want numeric)"
  fail=$((fail + 1))
fi

echo "==> assert queue producer -> consumer -> KV"
assert /q "queued"
last="(none)"
for _ in $(seq 1 "$QUEUE_TIMEOUT"); do
  last="$(get /qlast)"
  [ "$last" != "(none)" ] && break
  sleep 1
done
if [ "$last" = "processed:q-ok" ]; then
  green "PASS  /qlast -> $last"
  pass=$((pass + 1))
else
  red "FAIL  /qlast -> '$last' (want processed:q-ok within ${QUEUE_TIMEOUT}s)"
  fail=$((fail + 1))
fi

echo "==> assert Durable Object alarm (setAlarm -> scheduler fires alarm())"
assert /alarm-arm "armed"
fired="0"
for _ in $(seq 1 "$QUEUE_TIMEOUT"); do
  fired="$(get /alarm-fired)"
  [ "$fired" = "1" ] && break
  sleep 1
done
if [ "$fired" = "1" ]; then
  green "PASS  /alarm-fired -> $fired"
  pass=$((pass + 1))
else
  red "FAIL  /alarm-fired -> '$fired' (want 1 within ${QUEUE_TIMEOUT}s)"
  fail=$((fail + 1))
fi

echo
echo "==> $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
