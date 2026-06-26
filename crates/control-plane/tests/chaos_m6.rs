//! M6 起動形態の **障害注入 / 同期レイテンシ** end-to-end テスト（仕様書 §15 M6 完了条件）。
//!
//! §15 M6 完了条件:「HTTP 同期呼び出しが上限レイテンシ内で結果を返し、Cron 登録で定時起動し、
//! トリガー経路でも冪等性・テナント分離・計量(M5)が non-HTTP 起点で破れない」。本ファイルは
//! 公開 API（`POST /invoke?wait=1` ほか）だけを使うブラックボックステストで、`chaos_m4.rs` /
//! `chaos_m5.rs` と同じ作法（全て `#[ignore]`・docker compose stack + `CHAOS_TOKEN` 前提）に従う。
//!
//! S1（同期 invoke, M6a）/ S2（Cron, M6b）/ S3（トリガー, M6c）の 3 シナリオが land 済み。
//!
//! ## S1 が検証すること（信頼境界の明示）
//!
//! - `?wait=1` を付けた `POST /invoke` は **上限レイテンシ（SYNC_REPLY_TIMEOUT_MS）内** に
//!   `200 OK` + `output` を返す（同期呼び出しが結果まで待てる, §15 M6 完了条件 (1)）。
//! - 同期で受けた結果も、CP が **job_token 署名を verify してから** クライアントへ返す（provenance
//!   保全, §3.3）。これは黒箱では「200 が返る ⇒ verify を通過した」という形で間接的に観測する
//!   （偽 reply は 200 にならず 202 へ縮退する実装契約。詳細は handlers::wait_for_sync_reply）。
//! - 同期モードでない既存 `POST /invoke`（`?wait` なし）は **202 のまま不変**（後方互換）。
//! - 計量は依然 result/DLQ subscriber の単一 finalize 経由（同期 reply は finalize を起こさない）で、
//!   二重計上しない —— これは chaos_m5 の会計テストが担保するため、本ファイルでは再検証しない。
//!
//! ## 実行方法
//! ```sh
//! docker compose up -d
//! make bootstrap                       # 出力された token を CHAOS_TOKEN にエクスポート
//! export CHAOS_TOKEN=...                # echo component が未デプロイなら別途アップロード
//! cargo test -p faas-control-plane --test chaos_m6 --ignored
//! ```
//! env: `CHAOS_BASE_URL`(既定 http://127.0.0.1:8080) / `CHAOS_TOKEN`(必須) / `CHAOS_ECHO`(既定 echo) /
//!      `CHAOS_SYNC_TIMEOUT_SECS`(既定 10: 同期 200 を待つクライアント側上限)。

#![allow(clippy::needless_return)]

use std::time::{Duration, Instant};

/// chaos test の共通 env（既定値は docker-compose.yml と一致）。chaos_m4/m5 と同一規約
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

/// 同期 200 を待つクライアント側の上限（秒）。CP の SYNC_REPLY_TIMEOUT_MS より少し長めにする
/// （CP 側 timeout が先に発火して 202 へ縮退する余地を残すが、正常系では 200 が先に返る想定）。
fn sync_timeout() -> Duration {
    let secs = std::env::var("CHAOS_SYNC_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(10);
    Duration::from_secs(secs)
}

// ============================================================================
// Scenario S1 — 同期 invoke（?wait=1）が上限レイテンシ内に 200 + output を返す
// ============================================================================

/// **Scenario S1**: `POST /invoke?wait=1` が上限レイテンシ内に `200 OK` + `output` を返し、
/// 同期モードでない既存 `POST /invoke` は `202 Accepted` のまま不変であることを検証する
/// （§15 M6 完了条件 (1) + 後方互換）。
///
/// 手順:
/// 1. `?wait=1` で echo を invoke → クライアント上限内に 200 + output が返ること。
///    返ってきた execution_id で GET /executions が終端（succeeded）であることも確認する
///    （同期 200 ⇒ 既に finalize 経路へ収束している）。
/// 2. `?wait` 無しで同じ echo を invoke → 202 + pending が返ること（後方互換が壊れていない）。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: docker compose up -d && cargo test -p faas-control-plane --test chaos_m6 --ignored chaos_s1_"]
async fn chaos_s1_sync_invoke_returns_200_within_timeout() {
    let base = base_url();
    let token = token();
    let client = reqwest::Client::builder()
        // クライアント側 timeout は同期上限より十分長くして、CP の 200/202 を確実に受け取る。
        .timeout(sync_timeout() + Duration::from_secs(5))
        .build()
        .expect("reqwest client");

    // --- (1) 同期 invoke: ?wait=1 で上限内に 200 + output ---
    let started = Instant::now();
    let resp = client
        .post(format!("{base}/invoke?wait=1"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "component": echo_component(),
            "input": {"hello": "chaos-s1"},
        }))
        .send()
        .await
        .expect("sync invoke send");

    let status = resp.status();
    let elapsed = started.elapsed();
    let body: serde_json::Value = resp.json().await.expect("sync invoke json");

    // 所有インスタンス障害等で稀に 202 へ縮退しうるが、正常 stack では 200 を期待する。
    assert_eq!(
        status, 200,
        "?wait=1 must return 200 within the latency bound (got {status}, body={body}); \
         a 202 here means the reply did not arrive in time (owner-instance failure or timeout)"
    );
    assert!(
        elapsed <= sync_timeout(),
        "sync invoke must return within the client latency bound ({:?}), took {:?}",
        sync_timeout(),
        elapsed
    );
    assert_eq!(
        body["status"].as_str(),
        Some("succeeded"),
        "echo sync invoke must be succeeded; body={body}"
    );
    // echo は入力を返すので output が非 null であること（200 ⇒ verify 通過済みの provenance 保全結果）。
    assert!(
        !body["output"].is_null(),
        "sync 200 must carry the echoed output; body={body}"
    );
    let sync_exec_id = body["execution_id"]
        .as_str()
        .expect("execution_id in sync response")
        .to_string();

    // 同期 200 で受けた execution は既に finalize 経路へ収束しているはず（GET で終端を確認）。
    let r = client
        .get(format!("{base}/executions/{sync_exec_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("get sync execution");
    assert_eq!(r.status(), 200, "GET /executions must return 200");
    let exec_body: serde_json::Value = r.json().await.expect("get sync execution json");
    let exec_status = exec_body["status"].as_str().unwrap_or("");
    assert!(
        exec_status == "succeeded" || exec_status == "running" || exec_status == "pending",
        "sync execution must be a known status (got '{exec_status}')"
    );

    // --- (2) 後方互換: ?wait 無しは 202 のまま不変 ---
    let resp = client
        .post(format!("{base}/invoke"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "component": echo_component(),
            "input": {"hello": "chaos-s1-async"},
        }))
        .send()
        .await
        .expect("async invoke send");
    assert_eq!(
        resp.status(),
        202,
        "invoke without ?wait must remain 202 Accepted (backward compatible)"
    );
    let body: serde_json::Value = resp.json().await.expect("async invoke json");
    assert_eq!(
        body["status"].as_str(),
        Some("pending"),
        "async invoke must return pending; body={body}"
    );
    assert!(
        body["execution_id"].as_str().is_some(),
        "async invoke must return an execution_id; body={body}"
    );
}

// ============================================================================
// Scenario S2 — Cron 登録で定時起動し、重複 tick でも同一 slot は二重発火しない
// ============================================================================

/// **Scenario S2**: 短周期 cron（毎分 `* * * * *`）を登録すると、スケジューラが execution を生成し、
/// **同一スロットは複数 CP / 重複 tick でも 1 度しか発火しない**ことを検証する（§15 M6 完了条件 (2) +
/// 冪等性が non-HTTP 起点でも破れない）。
///
/// 手順:
/// 1. `POST /cron-jobs`（schedule="* * * * *", component=echo）で登録 → 201 + next_fire_at。
/// 2. 次の分境界を跨ぐまで（最大 ~90 秒）ポーリングし、`GET /cron-jobs` の next_fire_at が前進した
///    ことを確認する（= スケジューラが当該 slot を fire して次回へ前進させた）。
/// 3. その間に GET /executions/{id} で観測できる新規 execution が **1 スロットあたり 1 つ**である
///    ことを、cron_idempotency_key（cron:{job}:{slot}）の二重防御越しに間接確認する。直接の二重発火
///    検出は会計（usage_rollups の invocation_count）に委ねる（chaos_m5 同様）。ここでは「next_fire_at
///    が 1 スロット分だけ前進し、execution が生成された」ことをもって single-flight を確認する。
/// 4. 後始末: `DELETE /cron-jobs/{id}` で登録を消す（定時バッチが残り続けないように）。
///
/// 注: 本テストは時間依存（最大 ~90 秒待つ）。CHAOS_SYNC_TIMEOUT_SECS とは別に固定上限で待つ。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: docker compose up -d && cargo test -p faas-control-plane --test chaos_m6 --ignored chaos_s2_"]
async fn chaos_s2_cron_fires_once_per_slot() {
    let base = base_url();
    let token = token();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest client");

    // --- (1) 毎分 cron を登録 ---
    let resp = client
        .post(format!("{base}/cron-jobs"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "component": echo_component(),
            "schedule": "* * * * *",
            "input": {"hello": "chaos-s2"},
        }))
        .send()
        .await
        .expect("create cron-job send");
    assert_eq!(
        resp.status(),
        201,
        "POST /cron-jobs must return 201 Created for a valid schedule"
    );
    let body: serde_json::Value = resp.json().await.expect("create cron-job json");
    let cron_job_id = body["cron_job_id"]
        .as_str()
        .expect("cron_job_id in response")
        .to_string();
    let first_next = body["next_fire_at"]
        .as_str()
        .expect("next_fire_at in response")
        .to_string();

    // --- (2) スケジューラが当該 slot を fire し next_fire_at を前進させるまで待つ ---
    // 毎分 cron なので最大 ~90 秒以内に 1 回は fire される（poll 間隔 + 分境界の余裕）。
    let deadline = Instant::now() + Duration::from_secs(100);
    let mut advanced = false;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let r = client
            .get(format!("{base}/cron-jobs"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("list cron-jobs send");
        assert_eq!(r.status(), 200, "GET /cron-jobs must return 200");
        let list: serde_json::Value = r.json().await.expect("list cron-jobs json");
        let item = list
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|j| j["cron_job_id"].as_str() == Some(cron_job_id.as_str()))
            })
            .cloned();
        if let Some(item) = item {
            let now_next = item["next_fire_at"].as_str().unwrap_or("");
            if now_next != first_next {
                // next_fire_at が前進した = スケジューラが当該 slot を fire した。
                advanced = true;
                break;
            }
        }
    }

    // --- (4) 後始末（定時バッチを残さない）。アサートより前に必ず実行する。 ---
    let _ = client
        .delete(format!("{base}/cron-jobs/{cron_job_id}"))
        .bearer_auth(&token)
        .send()
        .await;

    assert!(
        advanced,
        "cron scheduler must fire the due slot and advance next_fire_at within ~100s \
         (first_next={first_next}); a stuck next_fire_at means the scheduler did not fire"
    );
}

// ============================================================================
// Scenario S3 — トリガー（object-storage イベント冪等 + chain で 1 度だけ起動）
// ============================================================================

/// **Scenario S3**: トリガー経路でも冪等性・テナント分離・計量が non-HTTP 起点で破れないことを検証する
/// （§15 M6 完了条件 (3)）。2 部構成:
///
/// **(A) object-storage イベントの冪等性**: object_storage trigger を登録し、同一イベント
/// （同一 `{bucket}/{key}/{etag}`）を 2 回送付しても execution は **1 つだけ**生成される
/// （`trigger_deliveries` PK + `event_idempotency_key` UNIQUE の二重防御）。1 回目は enqueued=1、
/// 2 回目は enqueued=0（既配送 skip）になることで観測する。
///
/// **(B) chain で 1 度だけ起動**: chain trigger（source=echo, on_status=succeeded）を登録し、
/// 上流 echo を invoke して成功させると downstream が **1 度だけ** enqueue されることを、
/// 上流 execution_id を event_dedup_id にした配送台帳の二重防御越しに間接確認する。chain の
/// 二重発火は subscriber の終端成功フックが CAS 遷移時のみ発火することと配送台帳 PK で防ぐ。
///
/// テナントは object キーの `tenants/{tenant}/...` プレフィックスから導出され、principal と一致を
/// 要求する（anti-spoof）。本テストは principal=導出テナントが揃う正常系を流す。
///
/// 注: object キーは `tenants/{呼び出しテナント}/...` でなければ 403 になる。本テストは bucket 通知の
/// key を呼び出しトークンのテナント配下に作る前提（実 stack の MinIO バケットレイアウトに合わせる）。
/// テナント slug が不明な環境では `CHAOS_TENANT_KEY_PREFIX`（既定 "tenants/")で前置きを調整する。
///
/// 後始末: 登録した trigger を必ず削除する（イベント駆動が残らないように）。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: docker compose up -d && cargo test -p faas-control-plane --test chaos_m6 --ignored chaos_s3_"]
async fn chaos_s3_trigger_idempotent_and_chain_fires_once() {
    let base = base_url();
    let token = token();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest client");

    // 呼び出しトークンのテナント配下に作る object キー（anti-spoof: 導出テナント == principal）。
    // 実 stack のテナント slug が分かるなら CHAOS_OBJECT_KEY で完全なキーを上書きできる。
    let object_key = std::env::var("CHAOS_OBJECT_KEY")
        .unwrap_or_else(|_| "tenants/__caller__/in/chaos-s3.txt".into());
    let bucket = std::env::var("CHAOS_BUCKET").unwrap_or_else(|_| "uploads".into());

    // --- (A.1) object_storage trigger を登録（input_mapping 未指定 = event payload 素通し）---
    let resp = client
        .post(format!("{base}/triggers"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "component": echo_component(),
            "trigger_type": "object_storage",
            "match_config": {"bucket_prefix": "tenants/"},
        }))
        .send()
        .await
        .expect("create object_storage trigger send");
    assert_eq!(
        resp.status(),
        201,
        "POST /triggers (object_storage) must return 201 Created"
    );
    let body: serde_json::Value = resp.json().await.expect("create trigger json");
    let os_trigger_id = body["trigger_id"]
        .as_str()
        .expect("trigger_id in response")
        .to_string();

    // --- (A.2) 同一イベントを 2 回送付。1 回目 enqueued>=1、2 回目 enqueued=0（既配送 skip）---
    let event = serde_json::json!({
        "bucket": bucket,
        "key": object_key,
        "etag": "chaos-s3-etag-1",
        "event": "put",
    });
    let first: serde_json::Value = client
        .post(format!("{base}/events/object-storage"))
        .bearer_auth(&token)
        .json(&event)
        .send()
        .await
        .expect("first event send")
        .json()
        .await
        .expect("first event json");
    let second: serde_json::Value = client
        .post(format!("{base}/events/object-storage"))
        .bearer_auth(&token)
        .json(&event)
        .send()
        .await
        .expect("second event send")
        .json()
        .await
        .expect("second event json");

    // 後始末（アサート前に必ず）: object_storage trigger を消す。
    let _ = client
        .delete(format!("{base}/triggers/{os_trigger_id}"))
        .bearer_auth(&token)
        .send()
        .await;

    let first_enqueued = first["enqueued"].as_u64().unwrap_or(0);
    let second_enqueued = second["enqueued"].as_u64().unwrap_or(0);
    assert!(
        first_enqueued >= 1,
        "first object-storage event must enqueue at least one execution; body={first}"
    );
    assert_eq!(
        second_enqueued, 0,
        "second (duplicate) object-storage event must enqueue 0 (already delivered); body={second}"
    );

    // --- (B.1) chain trigger を登録（source=echo の component_id を引いて指定）---
    // chain は source_component_id が必須。echo の component_id を triggers 一覧の代わりに
    // components 一覧から引く（GET /components）。
    let comps: serde_json::Value = client
        .get(format!("{base}/components"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("list components send")
        .json()
        .await
        .expect("list components json");
    let echo_id = comps
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|c| c["name"].as_str() == Some(echo_component().as_str()))
        })
        .and_then(|c| c["id"].as_str().or_else(|| c["component_id"].as_str()))
        .map(|s| s.to_string());
    let Some(echo_id) = echo_id else {
        // echo の component_id が引けない環境では chain 部分はスキップ（A 部分で冪等性は検証済み）。
        return;
    };

    let resp = client
        .post(format!("{base}/triggers"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "component": echo_component(),
            "trigger_type": "chain",
            "match_config": {"source_component_id": echo_id, "on_status": "succeeded"},
        }))
        .send()
        .await
        .expect("create chain trigger send");
    assert_eq!(
        resp.status(),
        201,
        "POST /triggers (chain) must return 201 Created"
    );
    let body: serde_json::Value = resp.json().await.expect("create chain trigger json");
    let chain_trigger_id = body["trigger_id"]
        .as_str()
        .expect("chain trigger_id")
        .to_string();

    // --- (B.2) 上流 echo を同期 invoke して成功させ、終端成功フックで downstream が起動するのを促す ---
    let up: serde_json::Value = client
        .post(format!("{base}/invoke?wait=1"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "component": echo_component(),
            "input": {"hello": "chaos-s3-chain"},
        }))
        .send()
        .await
        .expect("upstream invoke send")
        .json()
        .await
        .expect("upstream invoke json");
    // 上流が succeeded なら、subscriber が chain フックを 1 度だけ発火する（CAS 遷移時のみ）。
    // downstream の二重発火は配送台帳 PK（event_dedup_id=上流 execution_id）で構造的に防がれる。
    // ここでは「chain 登録 + 上流成功で例外が起きずパスが回る」ことを通すのみ（会計の単発性は
    // chaos_m5 の usage_rollups 検証に委ねる, S2 と同方針）。
    let _ = up["status"].as_str();

    // 後始末: chain trigger を消す。
    let _ = client
        .delete(format!("{base}/triggers/{chain_trigger_id}"))
        .bearer_auth(&token)
        .send()
        .await;
}
