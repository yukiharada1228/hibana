#!/usr/bin/env bash
# rls-lint — M3b テナント分離の静的ガード (§3.2 / §15)。
#
# 2 つの grep ベースのトリップワイヤ（fail-closed: マッチ＝非ゼロ終了で CI を落とす）。
# CI（.github/workflows/ci.yml）とローカル（make rls-lint）で同一スクリプトを実行する。
#
#   (1) GUC を生の `SET app.tenant_id` で設定するコードは禁止。
#       正は `SELECT set_config('app.tenant_id', $1, true)`（パラメータ化・transaction-local）。
#       SET ステートメントは文字列結合 injection の温床であり、LOCAL 欠落は
#       接続プールでテナントコンテキストが残存＝クロステナント漏洩になる。
#       → app.tenant_id を対象とする SET（LOCAL の有無を問わず）を全て検出する。
#
#   (2) ヒューリスティック: テナントスコープの db:: 書き込み/読み取りを `state.pool()` に
#       直接渡すコードを検出する。これらは set_tenant_guc を設定した tx（&mut *tx）に
#       通さねばならない（FORCE RLS 下では GUC 未設定で ERROR=fail-closed）。
#       認証前参照 find_token_by_hash/find_user_by_email/find_tenant_id_by_slug は
#       SECURITY DEFINER 関数経由で GUC 不要のため、意図的に対象から除外する。
#
# 注意: (2) は単一行の `db::fn(state.pool()` のみを捕捉するヒューリスティック（tripwire）で
# あり、保証ではない（`let p = state.pool(); db::fn(p` 等は捕捉しない）。
#
# コメント行の誤検出を避けるため、行頭が // / -- / * のコメント行と、行内コメント以降に
# 現れる文字列は除外してから判定する。

set -euo pipefail

cd "$(dirname "$0")/.."

fail=0

# --- (1) SET app.tenant_id ハザード ---------------------------------------
# 行内コメント（// もしくは --）以降を削ってから、生の SET app.tenant_id を探す。
# sed で「//」または「--」以降を除去（URL の // を巻き込まないよう、SQL/Rust の
# 行コメント前提で素朴に削る。RLS 用途では十分）。
set1=$(
  grep -rnE "SET[[:space:]]+(LOCAL[[:space:]]+)?app\.tenant_id" \
    --include=*.rs --include=*.sql crates/ migrations/ 2>/dev/null \
    | sed -E 's#(//|--).*$##' \
    | grep -E "SET[[:space:]]+(LOCAL[[:space:]]+)?app\.tenant_id" \
  || true
)
if [ -n "$set1" ]; then
  echo "ERROR(rls-lint 1): raw 'SET app.tenant_id' statement detected."
  echo "  Use SELECT set_config('app.tenant_id', \$1, true) (parameterized, transaction-local) instead."
  echo "$set1"
  fail=1
fi

# --- (2) tenant-scoped db:: call passed the raw pool -----------------------
fns="create_component|insert_version|set_active_version|insert_pending_execution|get_execution"
fns="$fns|find_component_by_id|find_component_by_name|active_version_storage|list_components|list_versions"
fns="$fns|soft_delete_component|soft_delete_version|find_version_id"
fns="$fns|has_active_executions_for_component|has_active_executions_for_version|finalize_execution"
fns="$fns|create_user|find_user_role|create_token|revoke_token|token_exists|bootstrap_tenant"
# M6b Cron CRUD + due-scan（cron_due_tenant_jobs は SECURITY DEFINER で GUC 不要のため除外）。
fns="$fns|insert_cron_job|list_cron_jobs|delete_cron_job|lock_due_cron_job|advance_cron_next_fire"
# M6c トリガー CRUD + 配送台帳 + chain/event 解決（すべて set_tenant_guc 済み tx で呼ぶ）。
fns="$fns|insert_trigger|list_triggers|delete_trigger|list_enabled_triggers_by_type|record_trigger_delivery"

set2=$(
  grep -rnE "db::($fns)\([[:space:]]*state\.pool\(\)" \
    --include=*.rs crates/ 2>/dev/null \
  || true
)
if [ -n "$set2" ]; then
  echo "ERROR(rls-lint 2): tenant-scoped db:: call passed state.pool() directly."
  echo "  Must thread &mut *tx after set_tenant_guc (FORCE RLS requires the GUC on the same tx)."
  echo "$set2"
  fail=1
fi

# --- (3) ランタイム DATABASE_URL が特権ロールを指していないか ---------------
# M3b(§3.2): ランタイムは必ず非特権 faas_app で接続する。FORCE RLS は SUPERUSER /
# BYPASSRLS ロールに対し無条件にバイパスされ、RLS 層全体が「飾り」になる。アプリ起動時の
# self-check（current_user の rolsuper/rolbypassrls）が一次防御だが、設定ファイル上でも
# 退行を検出する静的トリップワイヤを置く。
#
# DATABASE_URL= が postgres://faas_app 以外（典型的には所有者 postgres://faas:）を指す行を
# 検出する。MIGRATION_DATABASE_URL は特権ロールでよいので対象から除外する。
dburl=$(
  grep -rnE "^[[:space:]]*DATABASE_URL[[:space:]]*[?:]?=[[:space:]]*postgres://" \
    .env.example Makefile 2>/dev/null \
    | grep -vE "postgres://faas_app[:@]" \
  || true
)
if [ -n "$dburl" ]; then
  echo "ERROR(rls-lint 3): runtime DATABASE_URL does not connect as the non-privileged 'faas_app' role."
  echo "  Point DATABASE_URL at postgres://faas_app:...; keep the privileged role only in MIGRATION_DATABASE_URL."
  echo "$dburl"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "rls-lint: FAILED"
  exit 1
fi

echo "rls-lint: OK (no SET-without-LOCAL hazard; no raw-pool tenant db:: calls; runtime DATABASE_URL is faas_app)"
