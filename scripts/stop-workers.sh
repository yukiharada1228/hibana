#!/usr/bin/env bash
# M8 (§5.5): supervisor 管理下の worker を**全台ドレイン停止**する。
#
# SIGTERM → `WORKER_DRAIN_TIMEOUT_SECS + 5` 秒待つ → まだ生きていれば SIGKILL + warn。
# SIGKILL に至った場合、そのプロセスが抱えていたジョブは再配送されゲストが 2 回走るので、
# スクリプトは黙らず警告する。

set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/worker-lib.sh

reap_dead_pids
slots=$(live_slots)
if [ -z "$slots" ]; then
  echo "OK: 稼働中の worker はありません"
  exit 0
fi

# 逆順に落とす（slot 番号の大きい方から。run-workers.sh の縮小と同じ順序）。
for slot in $(echo "$slots" | sort -rn); do
  stop_worker "$slot"
done

rm -f "$WORKER_RUNDIR/heartbeat"
echo "OK: 全 worker を停止しました"
