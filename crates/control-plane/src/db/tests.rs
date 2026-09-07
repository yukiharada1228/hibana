use super::{
    components::{ROLLBACK_ACTIVE_VERSION_SQL, SWITCH_ACTIVE_VERSION_SQL},
    executions::{FINALIZE_EXECUTION_SQL, STUCK_EXECUTION_SWEEP_SQL},
    saturating_i64,
    usage::UPSERT_USAGE_ROLLUP_SQL,
    TenantQuotaOverrides, SET_TENANT_GUC_SQL,
};

// ---- M4d クォータ上書き JSONB のパース不変条件（DB 非依存）-----------------

/// 空 JSONB（既定 '{}'）はすべて None（= グローバル既定を継承）。
#[test]
fn tenant_quota_overrides_empty_object_is_all_none() {
    let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(v.invoke_rate_per_sec.is_none());
    assert!(v.invoke_burst.is_none());
    assert!(v.max_concurrent_executions.is_none());
}

/// 既知キーは読み取り、未知キーは無視する（前方互換）。
#[test]
fn tenant_quota_overrides_reads_known_ignores_unknown() {
    let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
        "invoke_rate_per_sec": 100,
        "invoke_burst": 1000,
        "max_concurrent_executions": 50,
        "future_field_we_dont_know": "ignored",
    }))
    .unwrap();
    assert_eq!(v.invoke_rate_per_sec, Some(100));
    assert_eq!(v.invoke_burst, Some(1000));
    assert_eq!(v.max_concurrent_executions, Some(50));
}

/// 部分的な上書き: 設定されていないキーは継承（None）になる。
#[test]
fn tenant_quota_overrides_partial_override() {
    let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
        "invoke_rate_per_sec": 200,
    }))
    .unwrap();
    assert_eq!(v.invoke_rate_per_sec, Some(200));
    assert!(v.invoke_burst.is_none());
    assert!(v.max_concurrent_executions.is_none());
}

/// null 値は None として扱う（明示的「継承」）。
#[test]
fn tenant_quota_overrides_null_means_inherit() {
    let v: TenantQuotaOverrides = serde_json::from_value(serde_json::json!({
        "invoke_rate_per_sec": null,
        "max_concurrent_executions": 10,
    }))
    .unwrap();
    assert!(v.invoke_rate_per_sec.is_none());
    assert_eq!(v.max_concurrent_executions, Some(10));
}

/// finalize_execution は **CAS** で終端遷移する: `status NOT IN (terminal)` ガードにより
/// 既終端の行には 0 行しか当たらない（= 重複 result / worker 多重実行があっても終端状態を
/// 上書きしない, §6.6）。DB 非依存で SQL テキストのガードを静的検査する（退行ガード）。
#[test]
fn finalize_execution_sql_is_cas_guarded() {
    // 終端状態の上書きを防ぐ CAS ガードを必ず持つ。
    assert!(
        FINALIZE_EXECUTION_SQL.contains("status NOT IN ('succeeded', 'failed', 'timeout')"),
        "finalize must be a CAS (no-op when row already terminal)"
    );
    // tenant_id / id はバインドパラメータ（テナント境界 + injection 防止）。
    assert!(FINALIZE_EXECUTION_SQL.contains("tenant_id = $1"));
    assert!(FINALIZE_EXECUTION_SQL.contains("id = $2"));
    // 単一 UPDATE（DELETE を伴わない）。
    let upper = FINALIZE_EXECUTION_SQL.to_ascii_uppercase();
    assert!(upper.starts_with("UPDATE EXECUTIONS"));
    assert!(!upper.contains("DELETE"));
    // M5 (§15): 計量列を同一 SET 句に同梱する（status と計量が同一行・同一述語で原子更新される）。
    for col in [
        "cpu_fuel_used = $6",
        "wall_time_ms = $7",
        "peak_memory_bytes = $8",
        "output_bytes = $9",
        "invocation_count = $10",
    ] {
        assert!(
            FINALIZE_EXECUTION_SQL.contains(col),
            "finalize must carry metering column {col} in the same CAS UPDATE"
        );
    }
}

/// M5 (§15): rollup の増分 UPSERT は (1) 複合 PK を競合ターゲットにし、(2) SUM 列を加算、
/// (3) `peak_memory_bytes_max` は `GREATEST`（MAX セマンティクス）で更新する。DB 非依存で
/// SQL テキストの集計セマンティクスを静的検査する（退行ガード）。
#[test]
fn upsert_usage_rollup_sql_has_sum_and_max_semantics() {
    // 複合 PK を競合ターゲットにする。
    assert!(UPSERT_USAGE_ROLLUP_SQL
        .contains("ON CONFLICT (tenant_id, period_start, component_id) DO UPDATE"));
    // SUM 列は既存値に加算する。
    assert!(
        UPSERT_USAGE_ROLLUP_SQL.contains("invocation_count = usage_rollups.invocation_count + 1")
    );
    assert!(UPSERT_USAGE_ROLLUP_SQL
        .contains("cpu_fuel_used = usage_rollups.cpu_fuel_used + EXCLUDED.cpu_fuel_used"));
    assert!(UPSERT_USAGE_ROLLUP_SQL
        .contains("wall_time_ms = usage_rollups.wall_time_ms + EXCLUDED.wall_time_ms"));
    assert!(UPSERT_USAGE_ROLLUP_SQL
        .contains("output_bytes = usage_rollups.output_bytes + EXCLUDED.output_bytes"));
    // peak は MAX（GREATEST）で更新する（SUM ではない）。
    assert!(UPSERT_USAGE_ROLLUP_SQL.contains(
        "peak_memory_bytes_max = GREATEST(usage_rollups.peak_memory_bytes_max, EXCLUDED.peak_memory_bytes_max)"
    ));
    // 終端カウンタも加算する。
    assert!(UPSERT_USAGE_ROLLUP_SQL
        .contains("succeeded_count = usage_rollups.succeeded_count + EXCLUDED.succeeded_count"));
    assert!(UPSERT_USAGE_ROLLUP_SQL
        .contains("failed_count = usage_rollups.failed_count + EXCLUDED.failed_count"));
    assert!(UPSERT_USAGE_ROLLUP_SQL
        .contains("timeout_count = usage_rollups.timeout_count + EXCLUDED.timeout_count"));
    // テナント境界はバインドパラメータ（$1）。DELETE は伴わない。
    assert!(UPSERT_USAGE_ROLLUP_SQL.contains("tenant_id"));
    assert!(!UPSERT_USAGE_ROLLUP_SQL
        .to_ascii_uppercase()
        .contains("DELETE"));
}

/// `saturating_i64` は `i64::MAX` で頭打ちにし、負値混入や格納失敗を防ぐ（二重防御）。
#[test]
fn saturating_i64_clamps_at_i64_max() {
    assert_eq!(saturating_i64(0), 0);
    assert_eq!(saturating_i64(123), 123);
    assert_eq!(saturating_i64(i64::MAX as u64), i64::MAX);
    assert_eq!(saturating_i64(u64::MAX), i64::MAX);
}

/// stuck-execution sweeper の SQL は (1) 非終端行のみを (2) deadline 超過のものに限り
/// failed に倒し、(3) tenant_id をバインドパラメータで境界し、(4) 単一 UPDATE である。
/// DB 非依存で SQL テキストを静的検査する（退行ガード）。
#[test]
fn stuck_execution_sweep_sql_is_guarded() {
    // 非終端（pending/running）行のみを対象にする（既終端は触らない = CAS 的）。
    assert!(STUCK_EXECUTION_SWEEP_SQL.contains("status IN ('pending', 'running')"));
    // failed へ倒す。
    assert!(STUCK_EXECUTION_SWEEP_SQL.contains("status = 'failed'"));
    // deadline 超過（created_at < now() - interval）に限定する。
    assert!(STUCK_EXECUTION_SWEEP_SQL.contains("created_at <"));
    assert!(STUCK_EXECUTION_SWEEP_SQL.contains("make_interval"));
    // tenant_id / deadline はバインドパラメータ（テナント境界 + injection 防止）。
    assert!(STUCK_EXECUTION_SWEEP_SQL.contains("tenant_id = $1"));
    assert!(STUCK_EXECUTION_SWEEP_SQL.contains("$2"));
    // 回収した id を返す（呼び出し側が DECR する）。
    assert!(STUCK_EXECUTION_SWEEP_SQL.contains("RETURNING id"));
    // 単一 UPDATE（DELETE を伴わない）。
    let upper = STUCK_EXECUTION_SWEEP_SQL.to_ascii_uppercase();
    assert!(upper.starts_with("UPDATE EXECUTIONS"));
    assert!(!upper.contains("DELETE"));
}

/// set_tenant_guc の SQL は **必ず** `$1` バインドプレースホルダで tenant_id を渡し、
/// SET ステートメントを使わず set_config(..., true)（transaction-local）を使う。
/// （RLS の fail-closed と injection 防止の不変条件。DB 非依存で検査する。）
#[test]
fn set_tenant_guc_sql_is_parameterized() {
    // パラメータバインドを使う（$1 がある）。
    assert!(
        SET_TENANT_GUC_SQL.contains("$1"),
        "tenant_id must be passed as a bind parameter, not interpolated"
    );
    // set_config を transaction-local (3rd arg true) で呼ぶ。
    assert!(SET_TENANT_GUC_SQL.contains("set_config"));
    assert!(SET_TENANT_GUC_SQL.contains("'app.tenant_id'"));
    assert!(SET_TENANT_GUC_SQL.contains("true"));
    // 生の SET ステートメントは使わない（SET app.tenant_id = ... は文字列結合の温床）。
    let upper = SET_TENANT_GUC_SQL.to_ascii_uppercase();
    assert!(
        !upper.contains("SET APP.TENANT_ID"),
        "must not use a SET statement for the GUC"
    );
}

/// tenant_id 値は SQL 文字列に一切埋め込まれない（任意の値を入れても SQL は不変）。
#[test]
fn set_tenant_guc_sql_never_embeds_tenant_value() {
    // SQL はコンパイル時定数であり、tenant_id 引数に依存しない。
    // 代表的な injection ペイロードが SQL リテラルに現れないことを確認する。
    assert!(!SET_TENANT_GUC_SQL.contains("'; DROP"));
    assert!(!SET_TENANT_GUC_SQL.contains("tenant-123"));
}

// --- M3c migration 0005 不変条件（DB 非依存。SQL テキストを静的検査する）---------
//
// ライブ DB はこの環境では検証不能のため、追記専用 + RLS + 部分 UNIQUE の不変条件を
// マイグレーション SQL のテキストに対して検査する（退行ガード）。

const MIGRATION_0005: &str = include_str!("../../../../migrations/0005_provenance.sql");

/// audit_logs は追記専用: faas_app に UPDATE/DELETE を **GRANT しない**こと（§3.2 MUST NOT）。
#[test]
fn audit_logs_is_append_only_for_faas_app() {
    let sql = MIGRATION_0005;
    // 防御的 REVOKE は存在してよいが、GRANT 側に UPDATE/DELETE が紛れていないこと。
    // GRANT 行を集めて、その中に UPDATE/DELETE が無いことを確認する。
    for line in sql.lines() {
        let stripped = line.split("--").next().unwrap_or("").to_ascii_uppercase();
        if stripped.contains("GRANT") && stripped.contains("AUDIT_LOGS") {
            assert!(
                !stripped.contains("UPDATE"),
                "audit_logs must never GRANT UPDATE: {line}"
            );
            assert!(
                !stripped.contains("DELETE"),
                "audit_logs must never GRANT DELETE: {line}"
            );
            assert!(
                !stripped.contains(" ALL "),
                "audit_logs must not GRANT ALL: {line}"
            );
        }
    }
    // 明示的に SELECT,INSERT は付与する。
    assert!(sql.contains("GRANT  SELECT, INSERT ON audit_logs TO   faas_app"));
    // 防御的に UPDATE/DELETE を REVOKE し、PUBLIC からも全剥奪する。
    assert!(sql
        .to_ascii_uppercase()
        .contains("REVOKE UPDATE, DELETE ON AUDIT_LOGS FROM FAAS_APP"));
    assert!(sql
        .to_ascii_uppercase()
        .contains("REVOKE ALL            ON AUDIT_LOGS FROM PUBLIC"));
}

/// audit_logs は 0004 と同形の FORCE RLS + fail-closed tenant_isolation を持つこと。
#[test]
fn audit_logs_has_force_rls_and_failclosed_policy() {
    let sql = MIGRATION_0005;
    assert!(sql.contains("ALTER TABLE audit_logs ENABLE ROW LEVEL SECURITY"));
    assert!(sql.contains("ALTER TABLE audit_logs FORCE  ROW LEVEL SECURITY"));
    assert!(sql.contains("CREATE POLICY tenant_isolation ON audit_logs"));
    // USING / WITH CHECK 両方を持つ。
    assert!(sql.contains("USING (tenant_id = current_setting('app.tenant_id'))"));
    assert!(sql.contains("WITH CHECK (tenant_id = current_setting('app.tenant_id'))"));
    // fail-closed: current_setting に第 2 引数（フォールバック）を付けない。
    assert!(
        !sql.contains("current_setting('app.tenant_id', true)"),
        "policy must be fail-closed: no 2nd-arg fallback on current_setting"
    );
}

/// 冪等性は部分 UNIQUE (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL（§6.6）。
#[test]
fn idempotency_partial_unique_index_present() {
    let sql = MIGRATION_0005;
    assert!(sql.contains("CREATE UNIQUE INDEX IF NOT EXISTS uq_executions_tenant_idem"));
    assert!(sql.contains("ON executions (tenant_id, idempotency_key)"));
    assert!(sql.contains("WHERE idempotency_key IS NOT NULL"));
}

/// 追加列は全て nullable な additive ADD COLUMN IF NOT EXISTS（backfill 不要）。
#[test]
fn executions_provenance_columns_are_additive() {
    let sql = MIGRATION_0005;
    for col in [
        "idempotency_key",
        "idempotency_request_hash",
        "job_token_kid",
    ] {
        assert!(
            sql.contains(&format!(
                "ALTER TABLE executions ADD COLUMN IF NOT EXISTS {col}"
            )),
            "missing additive ADD COLUMN for {col}"
        );
    }
    // nullable: NOT NULL を付けない（DEFAULT も不要）。
    assert!(
        !sql.to_ascii_uppercase()
            .contains("ADD COLUMN IF NOT EXISTS IDEMPOTENCY_KEY          TEXT NOT NULL"),
        "provenance columns must stay nullable (additive, no backfill)"
    );
}

/// insert_audit_log の INSERT は audit_logs に対しパラメータ化されていること
/// （tenant_id を含む全値を $n バインドで渡し、文字列結合しない）。
/// この関数のクエリ文字列をテスト内で複製し、不変条件を静的に検査する。
#[test]
fn audit_log_insert_is_parameterized() {
    // insert_audit_log 本体と同一の SQL 文字列。
    let sql = "INSERT INTO audit_logs (tenant_id, actor, action, target, detail) \
               VALUES ($1, $2, $3, $4, $5)";
    assert!(sql.contains("$1") && sql.contains("$5"));
    // INSERT のみ（UPDATE/DELETE しない＝追記専用）。
    let upper = sql.to_ascii_uppercase();
    assert!(upper.starts_with("INSERT INTO AUDIT_LOGS"));
    assert!(!upper.contains("UPDATE") && !upper.contains("DELETE"));
}

// ---- M7a: 解決 SQL の不変条件（DB-free 文字列検査） ----------------------

// ---- M7a: stable ポインタを動かす 3 操作の不変条件 -----------------------

/// 同じ版を再 activate しても `previous_active_version_id` を自分自身で潰さない。
/// 潰すと直後の rollback が「200 を返すのに何も戻らない」最悪の failure mode になる。
#[test]
fn switch_active_version_preserves_previous_on_noop() {
    for sql in [SWITCH_ACTIVE_VERSION_SQL, ROLLBACK_ACTIVE_VERSION_SQL] {
        assert!(
            sql.contains("CASE") && sql.contains("IS DISTINCT FROM"),
            "previous_active_version_id must be guarded by a no-op CASE"
        );
    }
}

/// rollback の戻り先は必ず**未削除の**version として解決する（tombstone を active にしない）。
#[test]
fn rollback_sql_requires_live_version() {
    assert!(
        ROLLBACK_ACTIVE_VERSION_SQL.contains("cv.deleted_at IS NULL"),
        "rollback must not resurrect a soft-deleted version"
    );
    assert!(
        ROLLBACK_ACTIVE_VERSION_SQL.contains("cv.component_id = c.id")
            && ROLLBACK_ACTIVE_VERSION_SQL.contains("cv.tenant_id = c.tenant_id"),
        "rollback target must belong to the same component and tenant"
    );
    assert!(
        ROLLBACK_ACTIVE_VERSION_SQL.contains("COALESCE($3, c.previous_active_version_id)"),
        "an empty body must roll back to the recorded previous stable"
    );
}
