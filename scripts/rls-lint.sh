#!/usr/bin/env bash
# rls-lint — M3b テナント分離の静的ガード (§3.2 / §15)。
#
# 4 つの grep ベースのトリップワイヤ（マッチ＝非ゼロ終了で CI を落とす）。
# CI（.github/workflows/ci.yml）とローカル（make rls-lint）で同一スクリプトを実行する。
#
#   (1) GUC を生の `SET app.tenant_id` で設定するコードは禁止。
#       正は `SELECT set_config('app.tenant_id', $1, true)`（パラメータ化・transaction-local）。
#       SET ステートメントは文字列結合 injection の温床であり、LOCAL 欠落は
#       接続プールでテナントコンテキストが残存＝クロステナント漏洩になる。
#       → app.tenant_id を対象とする SET（LOCAL の有無を問わず）を全て検出する。
#
#   (2) ヒューリスティック: db:: / secrets:: 呼び出しに `state.pool()` を直接渡すコードを
#       検出する。認証前参照など、GUC 不要と確認済みの共通参照だけを除外する。
#       新しい DB 処理は既定で検査対象となり、テナントスコープの処理は
#       set_tenant_guc を設定した tx（&tx）に通さねばならない。
#
# 注意: (2) は単一行の `db::fn(state.pool()` のみを捕捉するヒューリスティック（tripwire）で
# あり、保証ではない（`let p = state.pool(); db::fn(p` 等は捕捉しない）。
#
# (1) と (4) はコメント中の例を検出しないよう、行内コメント以降を除外してから判定する。

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

# --- (2) unapproved db:: call passed the raw pool -------------------------
# Reviewed exceptions: SECURITY DEFINER lookups and reads of the non-RLS
# tenants table. Do not maintain a list of tenant queries: new helpers must
# receive the same check without needing another lint change.
pool_safe="find_token_by_hash|find_tenant_id_by_slug"
pool_safe="$pool_safe|secrets_stale_kek|secrets_kek_kid_counts_all"
pool_safe="$pool_safe|list_tenants_for_admin|load_tenant_status_and_quotas|tenant_is_active|session_tenant"

set2=$(
  grep -rnEo "(db|secrets)::[a-zA-Z_][a-zA-Z0-9_]*\([[:space:]]*state\.pool\(\)" \
    --include=*.rs crates/ 2>/dev/null \
    | grep -vE ":db::($pool_safe)\([[:space:]]*state\.pool\(\)$" \
  || true
)
if [ -n "$set2" ]; then
  echo "ERROR(rls-lint 2): unapproved db::/secrets:: call passed state.pool() directly."
  echo "  Must thread &tx after set_tenant_guc (FORCE RLS requires the GUC on the same tx)."
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

# --- (4) Redacted::expose() の allowlist（M7-0, §5.1 / §5.6）-----------------
# 秘密値は hibana_shared::Redacted<T> に包み、平文の取り出しは expose() の 1 経路に閉じる。
# 呼び出せるファイルを allowlist に限定することで「秘密がプロセス内のどこへ渡ったか」を
# grep で全数把握できる状態を維持する。
#
# allowlist の考え方:
#  - config.rs        : env から秘密が入ってくる唯一の入口（*_plain() アクセサ 3 本に閉じている）
#  - secrets.rs       : 封筒暗号の実装本体（M7c）
#  - handlers_secrets.rs : secret の write-only API と job-env 引き換え（M7c）
#  - crates/worker/src/env.rs : worker 側の env 組み立て（M7b/M7c）
# worker/main.rs のような巨大ファイルを allowlist に入れるとガードが実質無効になるため、
# env 組み立ては専用モジュールへ切り出すこと（設計 §5.6）。
expose_allow='crates/shared/src/redacted.rs|crates/control-plane/src/config.rs|crates/control-plane/src/oidc/config.rs|crates/control-plane/src/secrets.rs|crates/control-plane/src/handlers_secrets.rs|crates/worker/src/env.rs'
set4=$(
  grep -rnE "\.expose\(\)" --include=*.rs crates/ 2>/dev/null \
    | sed -E 's#(//).*$##' \
    | grep -E "\.expose\(\)" \
    | grep -vE "^($expose_allow):" \
  || true
)
if [ -n "$set4" ]; then
  echo "ERROR(rls-lint 4): Redacted::expose() called outside the allowlist."
  echo "  Secret plaintext may only be unwrapped in: $expose_allow"
  echo "  (Pass the Redacted<T> itself, or add a narrow accessor in config.rs.)"
  echo "$set4"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "rls-lint: FAILED"
  exit 1
fi

echo "rls-lint: OK (no SET-without-LOCAL hazard; no raw-pool tenant db:: calls; runtime DATABASE_URL is faas_app; Redacted::expose() is allowlisted)"
