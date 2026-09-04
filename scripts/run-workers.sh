#!/usr/bin/env bash
# M8 (§5.5): worker を固定 N 台起動する（負荷試験・手動デバッグ用）。
#
#   ./scripts/run-workers.sh 3
#
# オートスケールしたい場合は `scripts/worker-autoscale.sh` を使うこと。
# 停止は `scripts/stop-workers.sh`（ドレインを待つ）。

set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/worker-lib.sh

N=${1:-1}
if ! [ "$N" -ge 0 ] 2>/dev/null; then
  echo "usage: $0 <台数>" >&2; exit 2
fi

reap_dead_pids
current=$(live_count)
echo "==> 現在 $current 台 → 目標 $N 台 (rundir=$WORKER_RUNDIR port_base=$WORKER_METRICS_PORT_BASE)"

while [ "$(live_count)" -lt "$N" ]; do
  start_worker "$(next_free_slot)"
done
while [ "$(live_count)" -gt "$N" ]; do
  stop_worker "$(highest_slot)"
done

echo "OK: $(live_count) 台稼働中 (slots: $(live_slots | tr '\n' ' '))"
