#!/usr/bin/env bash
# M8 (§5.5): worker プロセス群を slot 番号で管理するための共通関数。
#
# `run-workers.sh` / `worker-autoscale.sh` / `stop-workers.sh` から source して使う。
#
# # なぜ docker compose の worker サービスにしないのか（§5.5.2）
#
# 1. このリポジトリには **Dockerfile が 1 つも無い**。追加すると内側ループが
#    「編集 → イメージ再ビルド → compose up」になり、実測サイクルが著しく遅くなる。
# 2. `docker compose up --scale worker=N` は **per-replica の固定ホストポートを公開できない**。
#    これは chaos が「生きている worker 台数」を観測する唯一の非 shell-out 手段を壊す。
# 3. compose 化しても第 3 層の一実装に過ぎず、第 1-2 層（lane / permit）の設計は 1 ミリも変わらない。
#
# # 全 worker に同一 env を渡すこと (MUST)
#
# ここで `.env` を読み込んで export し、supervisor の環境をそのまま子へ継承させる。
# 台ごとに env が割れると挙動が割れ、原因の切り分けが不可能になる。

set -euo pipefail

# .env を読み込む。
#
# **`set -a; . ./.env` を使ってはならない。** この repo の `.env` は Makefile の `include` で
# 読まれる前提で書かれており、値をクォートしない Make 記法が混ざる
# （例: `.env.example:33` の `SMOKE_TENANT_NAME=Smoke Tenant`）。shell の `.` で読むと
# `Tenant: command not found` になり、`set -e` の下では **supervisor が黙って即死する**。
# 実際にそれで「3 台起動したはずが 0 台」になったので、Make と同じ意味論で自前に解釈する:
# 先頭の `=` で分割し、残り全体を値とする（surrounding quote は剥がす）。
load_dotenv() {
  local file=${1:-./.env} line key value
  [ -f "$file" ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ''|'#'*) continue ;;
      *'='*) ;;
      *) continue ;;
    esac
    key=${line%%=*}
    value=${line#*=}
    # `export FOO=bar` 形式も許す。キーが識別子でなければ無視する。
    key=${key#export }
    case "$key" in
      *[!A-Za-z0-9_]*|'') continue ;;
    esac
    # 前後のクォートを剥がす（両端が対になっているときだけ）。
    case "$value" in
      \"*\") value=${value#\"}; value=${value%\"} ;;
      \'*\') value=${value#\'}; value=${value%\'} ;;
    esac
    # 既に環境にある値を上書きしない（呼び出し側の明示指定を優先する）。
    [ -n "${!key+x}" ] || export "$key=$value"
  done < "$file"
}
load_dotenv ./.env

WORKER_BIN=${WORKER_BIN:-target/debug/faas-worker}
WORKER_RUNDIR=${WORKER_RUNDIR:-.workers}
# 既定 9101。**9090 と衝突させない**のが要点である。
# worker のメトリクスサーバは bind に失敗しても warn して task が return するだけで、
# プロセスは無言で稼働を続ける。9090 を既定にすると
# 「`make run-worker` で手動起動した worker が 9090 を掴んだまま supervisor を回す」→
# 「supervisor は 1 台も起動していないのに live 判定が 1 になる」という、
# 最も気づきにくい形の混入が起きる。
WORKER_METRICS_PORT_BASE=${WORKER_METRICS_PORT_BASE:-9101}
# 起動後 /readyz を待つ上限（秒）。
WORKER_READY_TIMEOUT_SECS=${WORKER_READY_TIMEOUT_SECS:-10}
# SIGTERM 後に諦めて SIGKILL するまでの猶予。ドレイン上限 + 余裕。
WORKER_STOP_GRACE_SECS=${WORKER_STOP_GRACE_SECS:-$(( ${WORKER_DRAIN_TIMEOUT_SECS:-0} + 5 ))}

mkdir -p "$WORKER_RUNDIR"

worker_port() { echo $(( WORKER_METRICS_PORT_BASE + $1 )); }
worker_pidfile() { echo "$WORKER_RUNDIR/worker-$1.pid"; }
worker_logfile() { echo "$WORKER_RUNDIR/worker-$1.log"; }

# slot i のプロセスが生きているか。
worker_alive() {
  local pidfile; pidfile=$(worker_pidfile "$1")
  [ -f "$pidfile" ] || return 1
  local pid; pid=$(cat "$pidfile" 2>/dev/null || echo "")
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null
}

# 死んだ PID ファイルを掃除する。
reap_dead_pids() {
  local f slot
  for f in "$WORKER_RUNDIR"/worker-*.pid; do
    [ -e "$f" ] || continue
    slot=$(basename "$f" .pid); slot=${slot#worker-}
    worker_alive "$slot" || rm -f "$f"
  done
}

live_slots() {
  local f slot
  for f in "$WORKER_RUNDIR"/worker-*.pid; do
    [ -e "$f" ] || continue
    slot=$(basename "$f" .pid); slot=${slot#worker-}
    worker_alive "$slot" && echo "$slot"
  done | sort -n
}

live_count() { live_slots | wc -l | tr -d ' '; }
highest_slot() { live_slots | tail -1; }

next_free_slot() {
  local i=0
  while worker_alive "$i"; do i=$((i + 1)); done
  echo "$i"
}

# slot i の worker が「本当にその slot として」応答しているかを確認する。
#
# `/readyz` の 200 だけでは足りない。**別プロセスが同じポートを先に掴んでいる**場合でも
# 200 は返るからである。`/metrics` の `wasmtime_worker_slot` が期待値と一致することまで
# 見て初めて「起動したのは自分が起動した worker だ」と言える。
worker_probe() {
  local slot=$1 port; port=$(worker_port "$slot")
  curl -sS --max-time 2 "http://127.0.0.1:$port/metrics" 2>/dev/null \
    | grep -q "^wasmtime_worker_slot $slot$"
}

start_worker() {
  local slot=$1 port; port=$(worker_port "$slot")

  if [ ! -x "$WORKER_BIN" ]; then
    echo "ERROR: worker バイナリがありません: $WORKER_BIN" >&2
    echo "       先に 'cargo build -p faas-worker' を実行してください" >&2
    echo "       （cargo run を N 回叩くと target/ のビルドロックで直列化するため、" >&2
    echo "         事前ビルド済みバイナリを直接起動する設計にしている）" >&2
    return 1
  fi

  echo "==> starting worker slot=$slot metrics=127.0.0.1:$port"
  WORKER_SLOT="$slot" METRICS_BIND_ADDR="127.0.0.1:$port" \
    nohup "$WORKER_BIN" > "$(worker_logfile "$slot")" 2>&1 &
  echo $! > "$(worker_pidfile "$slot")"

  # 起動確認。応答しなければ **loud に失敗させる**。
  # worker 側は bind 失敗を warn するだけでプロセスを止めない（互換のためその挙動は変えない）ので、
  # 「起動したつもりで実は観測できていない」を潰す責任は supervisor 側にある。
  local waited=0
  while [ "$waited" -lt "$WORKER_READY_TIMEOUT_SECS" ]; do
    if worker_probe "$slot"; then
      echo "    ok: slot=$slot is serving metrics on $port"
      return 0
    fi
    sleep 1; waited=$((waited + 1))
  done

  echo "ERROR: worker slot=$slot が ${WORKER_READY_TIMEOUT_SECS}s 以内に slot=$slot として応答しませんでした" >&2
  echo "       port=$port を別プロセスが掴んでいる可能性があります（手動起動の worker の混入）" >&2
  echo "       log: $(worker_logfile "$slot")" >&2
  return 1
}

stop_worker() {
  local slot=$1 pidfile pid
  pidfile=$(worker_pidfile "$slot")
  [ -f "$pidfile" ] || return 0
  pid=$(cat "$pidfile" 2>/dev/null || echo "")
  [ -n "$pid" ] || { rm -f "$pidfile"; return 0; }

  echo "==> stopping worker slot=$slot pid=$pid (drain up to ${WORKER_STOP_GRACE_SECS}s)"
  kill -TERM "$pid" 2>/dev/null || true

  local waited=0
  while [ "$waited" -lt "$WORKER_STOP_GRACE_SECS" ] && kill -0 "$pid" 2>/dev/null; do
    sleep 1; waited=$((waited + 1))
  done

  if kill -0 "$pid" 2>/dev/null; then
    # ここに来るのは異常。ドレインが完了していないので、このプロセスが抱えていたジョブは
    # 再配送 + 再実行される（ゲストが 2 回走る）。静かに済ませてはならない。
    echo "WARN: worker slot=$slot がドレイン期限内に終了しませんでした。SIGKILL します" >&2
    echo "      抱えていたジョブは再配送され、ゲストが 2 回実行されます" >&2
    echo "      (WORKER_DRAIN_TIMEOUT_SECS を実行時間に見合う値へ)" >&2
    kill -KILL "$pid" 2>/dev/null || true
  fi
  rm -f "$pidfile"
}
