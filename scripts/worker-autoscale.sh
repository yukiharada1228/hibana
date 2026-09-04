#!/usr/bin/env bash
# M8 (§5.5): 参照アクチュエータ。`GET /internal/scale` をポーリングして worker を増減させる。
#
#   ./scripts/worker-autoscale.sh          # フォアグラウンドで回り続ける（Ctrl-C で停止）
#
# 停止しても worker は残る。落とすには `scripts/stop-workers.sh` を使うこと
# （アクチュエータの停止と worker の停止を分けているのは、
#   「監視だけ止めて現状の台数で観察したい」場面が実際にあるため）。
#
# # 設計上の要点
#
# - **HTTP 200 のときだけ desired を読む**。503（未観測 / stale / ポーラ無効）や
#   取得失敗は「現状維持」として扱う。control-plane 側が `desired` を出さないのは
#   「値が取れない」という意味であり、`desired: 0` として扱うと全台停止になる。
# - **ポーリング周期は応答から取る**。`poll_interval_secs` をここにハードコードすると、
#   env を変えた瞬間に「スクリプトは動いているが実際には追随できていない」という
#   静かなズレが生まれる。
# - 増減は 1 周期に何台でも行う（scale-out は即時が正しい。§5.3 R4）。

set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/worker-lib.sh

INTERNAL_URL=${CONTROL_PLANE_INTERNAL_URL:-http://127.0.0.1:8081}
FALLBACK_POLL=${SCALE_POLL_INTERVAL_SECS:-5}

echo "==> autoscaler started (internal=$INTERNAL_URL rundir=$WORKER_RUNDIR port_base=$WORKER_METRICS_PORT_BASE)"
echo "    停止は Ctrl-C。worker を落とすには ./scripts/stop-workers.sh"

# chaos の前提 assert 用。アクチュエータが**実際に回っている**ことを外から確認できるようにする
# （回っていないのに「スケールしなかった」と判定するのを防ぐ）。
date +%s > "$WORKER_RUNDIR/heartbeat"

while :; do
  poll=$FALLBACK_POLL

  body=$(curl -sS --max-time 2 -w '\n%{http_code}' "$INTERNAL_URL/internal/scale" 2>/dev/null || true)
  code=$(printf '%s' "$body" | tail -n1)

  if [ "$code" = "200" ]; then
    json=$(printf '%s' "$body" | sed '$d')
    desired=$(printf '%s' "$json" | sed -n 's/.*"desired":[[:space:]]*\([0-9]*\).*/\1/p')
    p=$(printf '%s' "$json" | sed -n 's/.*"poll_interval_secs":[[:space:]]*\([0-9]*\).*/\1/p')
    [ -n "$p" ] && poll=$p

    if [ -n "$desired" ]; then
      reap_dead_pids
      before=$(live_count)
      while [ "$(live_count)" -lt "$desired" ]; do
        start_worker "$(next_free_slot)" || break
      done
      while [ "$(live_count)" -gt "$desired" ]; do
        stop_worker "$(highest_slot)"
      done
      after=$(live_count)
      [ "$before" != "$after" ] && echo "==> scaled: $before -> $after (desired=$desired)"
    fi
  else
    # 503 / 到達不能。**現状維持**する（これがアクチュエータ側の安全既定）。
    reap_dead_pids
  fi

  date +%s > "$WORKER_RUNDIR/heartbeat"
  sleep "$poll"
done
