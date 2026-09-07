use super::health::build_readyz_response;
use super::health::check_db_ready;
use super::tenants::bootstrap_token_matches;
use super::usage::fold_usage_totals;
use super::usage::resolve_usage_range;
use super::usage::UsageByComponent;
use super::usage::UsageTotals;
use super::*;
use crate::authz::resolve_token_scopes;
use axum::http::StatusCode;
use faas_shared::{FaasError, Role, Scope};
use serde_json::Value;

// --- GET /usage 純関数 ---

/// from/to 未指定の既定: to=today / from=today-30d。
#[test]
fn usage_range_defaults_to_last_30_days() {
    let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
    let (from, to) = resolve_usage_range(None, None, today).unwrap();
    assert_eq!(to, today);
    assert_eq!(from, chrono::NaiveDate::from_ymd_opt(2026, 5, 24).unwrap());
}

/// from/to 明示指定はパースされ既定を上書きする。
#[test]
fn usage_range_parses_explicit_bounds() {
    let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
    let (from, to) = resolve_usage_range(Some("2026-01-01"), Some("2026-01-31"), today).unwrap();
    assert_eq!(from, chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
    assert_eq!(to, chrono::NaiveDate::from_ymd_opt(2026, 1, 31).unwrap());
}

/// 不正な日付フォーマットは 400（InvalidRequest）。
#[test]
fn usage_range_rejects_malformed_date() {
    let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
    assert!(matches!(
        resolve_usage_range(Some("2026/01/01"), None, today),
        Err(FaasError::InvalidRequest(_))
    ));
    assert!(matches!(
        resolve_usage_range(None, Some("not-a-date"), today),
        Err(FaasError::InvalidRequest(_))
    ));
}

/// from > to は空でない範囲を保証するため 400。
#[test]
fn usage_range_rejects_inverted_bounds() {
    let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
    assert!(matches!(
        resolve_usage_range(Some("2026-02-01"), Some("2026-01-01"), today),
        Err(FaasError::InvalidRequest(_))
    ));
}

fn by_component(component_id: &str, inv: i64, cpu: i64, peak: i64) -> UsageByComponent {
    UsageByComponent {
        component_id: component_id.to_string(),
        invocation_count: inv,
        cpu_fuel_used: cpu,
        wall_time_ms: 0,
        peak_memory_bytes_max: peak,
        output_bytes: 0,
        succeeded_count: inv,
        failed_count: 0,
        timeout_count: 0,
    }
}

/// 畳み込み: SUM 列は和、peak は最大。
#[test]
fn fold_totals_sums_and_takes_max_peak() {
    let rows = vec![
        by_component("c1", 2, 100, 4096),
        by_component("c2", 3, 50, 8192),
    ];
    let totals = fold_usage_totals(&rows);
    assert_eq!(totals.invocation_count, 5);
    assert_eq!(totals.cpu_fuel_used, 150);
    assert_eq!(totals.peak_memory_bytes_max, 8192);
    assert_eq!(totals.succeeded_count, 5);
}

/// 空入力は全 0 の totals。
#[test]
fn fold_totals_empty_is_zero() {
    let totals = fold_usage_totals(&[]);
    assert_eq!(
        totals,
        UsageTotals {
            invocation_count: 0,
            cpu_fuel_used: 0,
            wall_time_ms: 0,
            peak_memory_bytes_max: 0,
            output_bytes: 0,
            succeeded_count: 0,
            failed_count: 0,
            timeout_count: 0,
        }
    );
}

#[test]
fn bootstrap_token_matches_only_on_exact_value() {
    assert!(bootstrap_token_matches("s3cret", "s3cret"));
    assert!(!bootstrap_token_matches("s3cret", "s3cre"));
    assert!(!bootstrap_token_matches("s3cre", "s3cret"));
    assert!(!bootstrap_token_matches("wrong", "s3cret"));
}

#[test]
fn bootstrap_empty_expected_never_matches() {
    // 未設定（空）の期待値は誤って全許可しない。
    assert!(!bootstrap_token_matches("", ""));
    assert!(!bootstrap_token_matches("anything", ""));
}

/// create_token のスコープ ceiling: caller=member相当が admin 要求すると 403。
#[test]
fn create_token_scope_ceiling_rejects_escalation() {
    let caller = vec![Scope::Read, Scope::Invoke, Scope::Deploy];
    let err = resolve_token_scopes(&[Scope::Admin], &caller, Role::Admin).unwrap_err();
    assert!(matches!(err, FaasError::Forbidden));
}

/// 対象ユーザが member なら admin スコープは付与不能（caller が admin でも）。
#[test]
fn create_token_target_role_ceiling_enforced() {
    let caller = vec![Scope::Read, Scope::Invoke, Scope::Deploy, Scope::Admin];
    let err = resolve_token_scopes(&[Scope::Admin], &caller, Role::Member).unwrap_err();
    assert!(matches!(err, FaasError::Forbidden));
}

// ---- M3c 冪等性: Idempotency-Key 形式検証 (§6.6 decision #4) -----------------

// ---- M3c 冪等性: 正準 body hash (§6.6 layer 1) ------------------------------

// ---- M3d 大入力: input_ref 完全一致検証 (§3.4) -----------------------------

/// is_unique_violation は 23505 のみ true。
#[test]
fn unique_violation_detection() {
    // RowNotFound は unique violation ではない。
    assert!(!is_unique_violation(&sqlx::Error::RowNotFound));
}

/// `build_readyz_response`: 全 hop が Ok のとき 200 + ボディに "ok"。
#[tokio::test]
async fn readyz_returns_200_when_all_hops_ok() {
    use axum::body::to_bytes;
    let resp = build_readyz_response(Ok(()), Ok(()));
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["db"], "ok");
    assert_eq!(v["store"], "ok");
}

/// `build_readyz_response`: いずれか 1 hop が Err なら 503 にフェイル（fail-closed）。
/// 残りの hop の "ok" 判定はそのまま JSON ボディに残す（運用者が原因 hop を判別できる）。
#[tokio::test]
async fn readyz_returns_503_when_db_check_fails() {
    use axum::body::to_bytes;
    let resp = build_readyz_response(Err("db: connection closed".to_string()), Ok(()));
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v["db"]
            .as_str()
            .map(|s| s.starts_with("db:"))
            .unwrap_or(false),
        "db hop should report its error message: got {:?}",
        v["db"]
    );
    // 他の hop の判定はそのまま残す（503 でも per-hop 状態を運用者に見せる）。
    assert_eq!(v["store"], "ok");
}

#[tokio::test]
async fn readyz_returns_503_when_store_fails() {
    assert_eq!(
        build_readyz_response(Ok(()), Err("store: down".into())).status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        build_readyz_response(Err("db: down".into()), Err("store: down".into())).status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

/// `check_db_ready`: 閉じた / 到達不能な PgPool に対しては `Err` を返す（hang しない）。
///
/// `connect_lazy` で実接続を作らない pool に対して `SELECT 1` を投げると即 fail
/// する（unreachable host + 短いタイムアウト）。これにより「DB が落ちたとき /readyz は
/// 503 を返す」スライス完了条件が live DB 無しで検証できる。
#[tokio::test]
async fn check_db_ready_errors_when_pool_unreachable() {
    // unreachable な URL（接続不可ポート）。connect_lazy なので構築自体は成功する。
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(100))
        .connect_lazy("postgres://faas:faas@127.0.0.1:1/faas")
        .expect("connect_lazy never fails on parse-valid URLs");
    let res = check_db_ready(&pool).await;
    assert!(
        res.is_err(),
        "check_db_ready against an unreachable pool must return Err; got {res:?}"
    );
}

// ---- M4a (§3.8) /metrics: Prometheus exposition の最低限の構造 -----------
//
// 専用パーサ（prometheus-parse）に依存せず、Prometheus text format の不変条件:
// HELP / TYPE 行、サンプル行、改行終端、Content-Type をテストする。
// exposition の形が壊れると、後段の Prometheus / Grafana が静かに読み飛ばすため
// 退行ガードとして必要。

/// `Metrics::render` は `# HELP` / `# TYPE` 行とサンプル行を持つ Prometheus 形式を返す。
#[test]
fn metrics_render_produces_parseable_exposition() {
    let m = crate::metrics::Metrics::init();
    // 既定 0 のままだとサンプル行が出ないメトリクスがあるため、各種を 1 度 inc しておく。
    m.executions_total.with_label_values(&["succeeded"]).inc();
    m.executions_total.with_label_values(&["failed"]).inc();
    m.admission_rejections_total
        .with_label_values(&["rate_limited"])
        .inc();
    m.tenant_invoke_total.with_label_values(&["ten_a"]).inc();

    let (headers, body) = m.render();
    // Content-Type は prometheus 標準（"text/plain; version=0.0.4"）。
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .expect("Content-Type must be set");
    let ct = ct.to_str().expect("ascii");
    assert!(
        ct.starts_with("text/plain"),
        "Prometheus exposition must be text/plain; got {ct:?}"
    );

    // HELP / TYPE / サンプル行が全て揃っている。
    assert!(body.contains("# HELP faas_executions_total"));
    assert!(body.contains("# TYPE faas_executions_total counter"));
    assert!(body.contains("faas_executions_total{status=\"succeeded\"} 1"));
    assert!(body.contains("faas_executions_total{status=\"failed\"} 1"));

    // admission 429 の kind ラベル付きサンプル（M4d）。
    assert!(body.contains("faas_admission_rejections_total{kind=\"rate_limited\"} 1"));

    // テナント別 invoke カウンタ（M4a per-tenant 観測）。
    assert!(body.contains("faas_tenant_invoke_total{tenant_id=\"ten_a\"} 1"));

    // 形式の最低保証: 各行が `\n` で終わり、空でない・"# HELP" の数 == "# TYPE" の数。
    assert!(
        body.ends_with('\n'),
        "Prometheus exposition must end with newline"
    );
    let help_lines = body.lines().filter(|l| l.starts_with("# HELP ")).count();
    let type_lines = body.lines().filter(|l| l.starts_with("# TYPE ")).count();
    assert_eq!(
        help_lines, type_lines,
        "every metric must have matching HELP and TYPE lines"
    );
    assert!(
        help_lines > 0,
        "exposition must contain at least one metric"
    );
}
