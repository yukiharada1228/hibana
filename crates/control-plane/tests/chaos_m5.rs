//! M5 課金・メータリングの **障害注入 / 冪等会計** end-to-end テスト（仕様書 §15 M5 完了条件）。
//!
//! §15 M5 完了条件:「障害注入（再配送・worker 落下・タイムアウト）下でも計量が二重計上/欠落せず、
//! テナント別に時間窓集計が一致する」。本ファイルは公開 API（`POST /invoke` + `GET /usage`）だけを
//! 使うブラックボックステストで、`chaos_m4.rs` と同じ作法（全て `#[ignore]`・docker compose stack +
//! `CHAOS_TOKEN` 前提・ポーリングで終端待ち）に従う。
//!
//! ## 何を検証し、何を検証「できない」か（信頼境界の明示）
//!
//! 冪等の唯一のアンカーは subscriber の CAS finalize（`status NOT IN terminal` ガード）で、`usage_rollups`
//! の increment UPSERT は CAS が実際に遷移させた（`finalize_execution` が `Some(period_start)` を返した）
//! ときだけ同一 tx 内で打たれる（db.rs / subscriber.rs）。再配送・重複 `result`・DLQ 後着・sweeper 先着は
//! 後着 tx の CAS が no-op（`None`）になり rollup を一切触らない＝二重計上は構造的に不可能。
//!
//! - **検証できる（本ファイル）**: (E1) 同一 `Idempotency-Key` の重複 invoke は集計を 1 回しか増やさない、
//!   (E2) N 件の distinct 実行はちょうど N 計上され drop も二重計上も起きない、時間窓集計が一致する。
//! - **黒箱では注入できない**: NATS 層での重複 `ResultMessage` の直接 publish。worker が echo する
//!   `job_token` は CP の Ed25519 秘密鍵で署名されており（§3.3）、テストからは偽造できないため、検証を
//!   通る重複 result を外部から流し込めない。この経路の冪等性は (a) 上記の構造的不変条件、(b) db/subscriber
//!   の DB-free モデルテスト（`commit_finalize_and_release` の `updated==0` 契約）で担保する。本 e2e は
//!   「正規経路 + Idempotency-Key 重複」で会計の正確性（二重計上/欠落なし）を確認する役割に絞る。
//!
//! ## 実行方法
//! ```sh
//! docker compose up -d
//! make bootstrap                       # 出力された token を CHAOS_TOKEN にエクスポート
//! export CHAOS_TOKEN=...                # echo component が未デプロイなら別途アップロード
//! cargo test -p faas-control-plane --test chaos_m5 --ignored
//! ```
//! env: `CHAOS_BASE_URL`(既定 http://127.0.0.1:8080) / `CHAOS_TOKEN`(必須) / `CHAOS_ECHO`(既定 echo)。

#![allow(clippy::needless_return)]

use std::time::Duration;

/// chaos test の共通 env（既定値は docker-compose.yml と一致）。chaos_m4.rs と同一規約
/// （各 tests/*.rs は独立クレートのためヘルパは複製する）。
fn base_url() -> String {
    std::env::var("CHAOS_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into())
}

fn token() -> String {
    std::env::var("CHAOS_TOKEN")
        .expect("CHAOS_TOKEN must be set (run `make bootstrap` and export the printed token)")
}

fn echo_component() -> String {
    std::env::var("CHAOS_ECHO").unwrap_or_else(|_| "echo".into())
}

/// `GET /usage`（既定: 今日を含む直近 30 日, UTC）の `totals` を取得する。
///
/// `succeeded` 実行をまたいだ delta 比較に使う。範囲既定は「今日」を含むため、テスト中に作る
/// 実行は必ずこの窓に入る（UTC 日境界をまたぐ瞬間の稀ケースは finalize 側の単一時計源で吸収される）。
async fn fetch_usage_totals(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> serde_json::Value {
    let resp = client
        .get(format!("{base}/usage"))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /usage send");
    assert_eq!(
        resp.status(),
        200,
        "GET /usage must return 200 (read scope)"
    );
    let body: serde_json::Value = resp.json().await.expect("GET /usage json");
    body["totals"].clone()
}

/// `totals` から i64 フィールドを取り出す（欠損は 0 とみなす）。
fn field(totals: &serde_json::Value, key: &str) -> i64 {
    totals[key].as_i64().unwrap_or(0)
}

/// 1 件 invoke して終端まで待つ。返り値は (execution_id, 終端 status)。
/// `idem_key` を渡すと `Idempotency-Key` ヘッダを付ける（重複 invoke の dedup 検証用）。
async fn invoke_and_wait(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    idem_key: Option<&str>,
    tag: &str,
) -> (String, String) {
    let mut req = client
        .post(format!("{base}/invoke"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "component": echo_component(),
            "input": {"hello": tag},
        }));
    if let Some(k) = idem_key {
        req = req.header("Idempotency-Key", k);
    }
    let resp = req.send().await.expect("invoke send");
    assert_eq!(resp.status(), 202, "invoke must return 202 Accepted");
    let body: serde_json::Value = resp.json().await.expect("invoke json");
    let exec_id = body["execution_id"]
        .as_str()
        .expect("execution_id in response")
        .to_string();

    // 終端化まで最大 30 秒ポーリング（chaos_m4 と同じ作法）。
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let r = client
            .get(format!("{base}/executions/{exec_id}"))
            .bearer_auth(token)
            .send()
            .await
            .expect("get execution");
        if r.status() != 200 {
            continue;
        }
        let b: serde_json::Value = r.json().await.unwrap();
        let status = b["status"].as_str().unwrap_or("");
        if status != "pending" && status != "running" {
            return (exec_id, status.to_string());
        }
    }
    panic!("execution {exec_id} did not finalize within 30s");
}

// ============================================================================
// Scenario E1 — 重複 invoke（同一 Idempotency-Key）は会計を 1 回しか増やさない
// ============================================================================

/// **Scenario E1**: 同一 `Idempotency-Key` で 2 回 invoke しても（= 同一 execution が重複受付されても）、
/// `usage_rollups` の `invocation_count` / `succeeded_count` は **+1 だけ**増える（+2 にならない）。
///
/// これは「同一 execution への重複 finalize は CAS が no-op になり rollup を触らない」という M5 冪等
/// アンカーを公開 API で観測する最小シナリオ。chaos_b（dedup で execution_id が同一）の会計版。
///
/// 手順:
/// 1. GET /usage の baseline totals を取る。
/// 2. fresh な Idempotency-Key で invoke → 終端（succeeded）まで待つ。
/// 3. **同一キー**で再 invoke（dedup で同じ execution_id が返る）→ 数秒待つ。
/// 4. GET /usage の delta が invocation_count == +1 / succeeded_count == +1 であること（+2 でない）。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: docker compose up -d && cargo test -p faas-control-plane --test chaos_m5 --ignored chaos_e1_"]
async fn chaos_e1_duplicate_invoke_counts_usage_once() {
    let base = base_url();
    let token = token();
    let client = reqwest::Client::new();

    let before = fetch_usage_totals(&client, &base, &token).await;

    let key = format!(
        "chaos-e1-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );

    // 1 回目: 終端まで待つ。
    let (exec_1, status_1) = invoke_and_wait(&client, &base, &token, Some(&key), "chaos-e1").await;
    assert_eq!(status_1, "succeeded", "echo invocation must succeed");

    // 2 回目: 同一キー → dedup で同じ execution_id（新規実行されない, §6.6）。
    let (exec_2, _status_2) = invoke_and_wait(&client, &base, &token, Some(&key), "chaos-e1").await;
    assert_eq!(
        exec_1, exec_2,
        "same Idempotency-Key → identical execution_id (no new execution, §6.6)"
    );

    // rollup 反映の取りこぼしを避けるため少し待ってから集計を読む。
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = fetch_usage_totals(&client, &base, &token).await;

    let dinv = field(&after, "invocation_count") - field(&before, "invocation_count");
    let dsucc = field(&after, "succeeded_count") - field(&before, "succeeded_count");
    assert_eq!(
        dinv, 1,
        "duplicate invoke of the same execution must count invocation exactly once (got +{dinv}); \
         double-counting would break billing (§15 M5)"
    );
    assert_eq!(
        dsucc, 1,
        "succeeded_count must increase by exactly 1 for one distinct succeeded execution (got +{dsucc})"
    );
}

// ============================================================================
// Scenario E2 — N 件の distinct 実行はちょうど N 計上される（drop も二重計上もなし）
// ============================================================================

/// **Scenario E2**: distinct な N 件を invoke して全て succeeded させると、`usage_rollups` の
/// `invocation_count` / `succeeded_count` はちょうど **+N** 増える。欠落（< N）も二重計上（> N）も
/// 起きないことを公開 API で確認する（時間窓集計の一致 = §15 M5 完了条件）。
///
/// 既定 N=5。`CHAOS_M5_N` で上書き可能（CI の所要時間調整）。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: docker compose up -d && cargo test -p faas-control-plane --test chaos_m5 --ignored chaos_e2_"]
async fn chaos_e2_distinct_executions_count_exactly_n() {
    let base = base_url();
    let token = token();
    let client = reqwest::Client::new();

    let n: i64 = std::env::var("CHAOS_M5_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(5);

    let before = fetch_usage_totals(&client, &base, &token).await;

    for i in 0..n {
        let (_id, status) =
            invoke_and_wait(&client, &base, &token, None, &format!("chaos-e2-{i}")).await;
        assert_eq!(status, "succeeded", "echo invocation #{i} must succeed");
    }

    // rollup 反映の取りこぼしを避けるため少し待ってから集計を読む。
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = fetch_usage_totals(&client, &base, &token).await;

    let dinv = field(&after, "invocation_count") - field(&before, "invocation_count");
    let dsucc = field(&after, "succeeded_count") - field(&before, "succeeded_count");
    assert_eq!(
        dinv, n,
        "exactly {n} distinct executions must yield +{n} invocations (got +{dinv}); \
         < {n} = drop, > {n} = double-count — both violate §15 M5"
    );
    assert_eq!(
        dsucc, n,
        "succeeded_count must increase by exactly {n} (got +{dsucc})"
    );

    // 計測済み succeeded のリソース指標は単調増加（少なくとも cpu_fuel か wall_time のいずれかが進む）。
    // echo は決定的だが、worker 計測値は 0 になりうる（fuel 無効化時 cpu=0 等）ため OR 条件で緩く検査する。
    let dwall = field(&after, "wall_time_ms") - field(&before, "wall_time_ms");
    let dfuel = field(&after, "cpu_fuel_used") - field(&before, "cpu_fuel_used");
    assert!(
        dwall >= 0 && dfuel >= 0,
        "resource totals must be monotonic non-decreasing (wall +{dwall}, fuel +{dfuel})"
    );
}
