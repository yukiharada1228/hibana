//! M7 デプロイ運用（canary / rollback / env / secret）の end-to-end テスト（仕様書 §15 M7 完了条件）。
//!
//! §15 M7 完了条件:「active-version を 10%→100% へ段階移行でき、ワンクリック rollback が効き、
//! secret はログ・監査・他テナントへ漏れない」。本ファイルは公開 API だけを使うブラックボックス
//! テストで、`chaos_m4.rs` / `chaos_m5.rs` / `chaos_m6.rs` と同じ作法（全て `#[ignore]`・
//! docker compose stack + `CHAOS_TOKEN` 前提）に従う。
//!
//! S1（canary 段階移行 + ワンクリック rollback, M7a）が land 済み。
//! S2（env / secret 注入, M7b/M7c）と S3（secret 非漏洩, M7c）は後続ステップで追加する。
//!
//! ## S1 が検証すること（信頼境界の明示）
//!
//! - **端点は決定的**に検証する: `weight=0` なら全 key が stable、`weight=100` なら全 key が canary。
//!   ここに統計は入らない（バケット導出が決定的なので、比率ではなく端点を突く）。
//! - **中間は単調性**で検証する: 同じ key 集合に対し weight を 10 → 50 → 100 と上げたとき、
//!   canary 側へ落ちた key 集合が包含関係（S10 ⊆ S50 ⊆ S100 = K）を保つこと。比率そのものは
//!   assert しない（`routing.rs` の全列挙ユニットテストが分配の数学を既に証明しているため、
//!   ここでは「その数学が実際に配線されている」ことだけを見る）。
//! - **ハッシュ式をテスト側に複製しない**（複製すると実装との乖離が静かに発生する）。
//! - rollback は **新規 enqueue のみ**を変える。publish 済みのジョブは canary 版で完走する
//!   —— これは弱点ではなく設計上の保証境界であり、(7) で実測して固定する。
//!
//! 黒箱で検証できないのは `audit_logs` の内容（参照 API が無い）のみ。`routing_reason` は
//! `GET /components/{id}/traffic` の `version_stats[].canary_routed` として露出しているので
//! 検証できる。他テナントへ漏れないことの構造的保証は RLS + `migration_tests` + `rls-lint` に委譲する。
//!
//! ## 実行方法
//! ```sh
//! docker compose up -d
//! make bootstrap                        # 出力された token を CHAOS_TOKEN にエクスポート
//! export CHAOS_TOKEN=...
//! make build-component && make deploy    # echo 0.1.0 を active にしておく
//! cargo test -p faas-control-plane --test chaos_m7 --ignored chaos_t1_
//! ```
//! env: `CHAOS_BASE_URL`(既定 http://127.0.0.1:8080) / `CHAOS_TOKEN`(必須) / `CHAOS_ECHO`(既定 echo) /
//!      `CHAOS_POLL_SECS`(既定 30: 終端待ちのポーリング上限)。

#![allow(clippy::needless_return)]

use std::collections::BTreeSet;
use std::time::Duration;

/// chaos test の共通 env（既定値は docker-compose.yml と一致）。chaos_m4/m5/m6 と同一規約
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

/// 終端待ちのポーリング上限（秒）。chaos_m5 と同じ 1 秒間隔ポーリング。
fn poll_secs() -> u64 {
    std::env::var("CHAOS_POLL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30)
}

/// S1 で使う canary 側の version（`activate=false` で upload する）。
const CANARY_VERSION: &str = "0.2.0";

/// 単調性検証に使う key 数。
///
/// 40 では足りない: バケットは `routing_bucket(component_id, key)` で component_id を含むため、
/// 固定 component の固定キー集合はその stack で決定的に固まる。40 キーだと
/// P(全キーが bucket >= 10) = 0.9^40 ≈ 1.5% で、当たりの悪い component_id を持つ環境では
/// **毎回落ちる**（しかも「重みが効いていない」という誤った症状に見える）。
/// 200 キーなら 0.9^200 ≈ 7e-10。
const MONOTONIC_KEYS: usize = 200;

/// 端点（weight=0 / 100）の検証に使う key 数。端点は決定的なので少なくてよい。
const ENDPOINT_KEYS: usize = 40;

// ============================================================================
// ヘルパ
// ============================================================================

/// `GET /components` から対象 component の id を引く。
async fn resolve_component_id(client: &reqwest::Client, base: &str, token: &str) -> String {
    let comps: serde_json::Value = client
        .get(format!("{base}/components"))
        .bearer_auth(token)
        .send()
        .await
        .expect("list components send")
        .json()
        .await
        .expect("list components json");
    let name = echo_component();
    // GET /components は素の配列を返す（エンベロープ無し）。
    comps
        .as_array()
        .unwrap_or_else(|| panic!("GET /components must return an array; body={comps}"))
        .iter()
        .find(|c| c["name"].as_str() == Some(name.as_str()))
        .and_then(|c| c["component_id"].as_str().or_else(|| c["id"].as_str()))
        .unwrap_or_else(|| {
            panic!("component '{name}' must exist (run `make deploy`); body={comps}")
        })
        .to_string()
}

/// `POST /invoke` を投げ、execution_id だけを返す（終端は待たない）。
async fn invoke_async(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    routing_key: &str,
) -> String {
    let body: serde_json::Value = client
        .post(format!("{base}/invoke"))
        .bearer_auth(token)
        .header("X-Faas-Routing-Key", routing_key)
        .json(&serde_json::json!({
            "component": echo_component(),
            "input": {"chaos": "t1"},
        }))
        .send()
        .await
        .expect("invoke send")
        .json()
        .await
        .expect("invoke json");
    body["execution_id"]
        .as_str()
        .unwrap_or_else(|| panic!("execution_id in invoke response; body={body}"))
        .to_string()
}

/// execution が終端するまで 1 秒間隔でポーリングし、`version_id` を返す（chaos_m5 の作法）。
async fn wait_version_id(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    execution_id: &str,
) -> String {
    for _ in 0..poll_secs() {
        let body: serde_json::Value = client
            .get(format!("{base}/executions/{execution_id}"))
            .bearer_auth(token)
            .send()
            .await
            .expect("get execution send")
            .json()
            .await
            .expect("get execution json");
        let status = body["status"].as_str().unwrap_or("");
        if matches!(status, "succeeded" | "failed" | "timeout") {
            return body["version_id"]
                .as_str()
                .unwrap_or_else(|| panic!("version_id in execution; body={body}"))
                .to_string();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!(
        "execution {execution_id} did not reach a terminal state within {}s",
        poll_secs()
    );
}

/// invoke → 終端待ち → 実際に起動した version_id。
async fn invoke_and_version(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    routing_key: &str,
) -> String {
    let id = invoke_async(client, base, token, routing_key).await;
    wait_version_id(client, base, token, &id).await
}

/// `PUT /components/{id}/traffic`。
async fn set_traffic(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    component_id: &str,
    version: &str,
    weight: u32,
) {
    let resp = client
        .put(format!("{base}/components/{component_id}/traffic"))
        .bearer_auth(token)
        .json(&serde_json::json!({"canary_version": version, "weight": weight}))
        .send()
        .await
        .expect("set traffic send");
    assert_eq!(
        resp.status(),
        200,
        "PUT /traffic (weight={weight}) must return 200; body={}",
        resp.text().await.unwrap_or_default()
    );
}

/// `GET /components/{id}/traffic`。
async fn get_traffic(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    component_id: &str,
) -> serde_json::Value {
    client
        .get(format!("{base}/components/{component_id}/traffic"))
        .bearer_auth(token)
        .send()
        .await
        .expect("get traffic send")
        .json()
        .await
        .expect("get traffic json")
}

/// `GET /traffic` の version_stats から当該 version の canary_routed を引く（未出現は 0）。
fn canary_routed_of(traffic: &serde_json::Value, version_id: &str) -> i64 {
    traffic["version_stats"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .find(|r| r["version_id"].as_str() == Some(version_id))
                .and_then(|r| r["canary_routed"].as_i64())
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// 指定 weight で key 集合を invoke し、canary 版へ落ちた key の集合を返す。
async fn canary_key_set(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    keys: &[String],
    canary_version_id: &str,
) -> BTreeSet<String> {
    let mut hit = BTreeSet::new();
    for key in keys {
        let vid = invoke_and_version(client, base, token, key).await;
        if vid == canary_version_id {
            hit.insert(key.clone());
        }
    }
    hit
}

// ============================================================================
// Scenario S1 — canary 段階移行と ワンクリック rollback
// ============================================================================

/// **Scenario S1**: `active-version` を 10%→100% へ段階移行でき、ワンクリック rollback が効く
/// （§15 M7 完了条件の前半 2 点）。
///
/// 端点（weight=0 / 100）は決定的に、中間は集合の単調性で検証する。比率そのものは assert しない
/// （分配の数学は `routing.rs` の全列挙ユニットテストが証明済み。ここでは配線を見る）。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: docker compose up -d && cargo test -p faas-control-plane --test chaos_m7 --ignored chaos_t1_"]
async fn chaos_t1_canary_stepwise_shift_and_one_click_rollback() {
    let base = base_url();
    let token = token();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client");

    let cid = resolve_component_id(&client, &base, &token).await;

    // --- 準備: canary 用 version を activate=false で upload（既存なら 400 で冪等スキップ）---
    // 段階移行の出発点。activate 既定 true のままだと保存と同時に 100% になり段階移行が形骸化する。
    // 統合テストの cwd はクレートディレクトリなので、既定はワークスペース root から解決する
    // （Makefile の COMPONENTS_DIR は workspace 相対の ./components-dist）。
    let wasm_path = std::env::var("CHAOS_ECHO_WASM").unwrap_or_else(|_| {
        format!(
            "{}/../../components-dist/echo.wasm",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    let wasm = std::fs::read(&wasm_path)
        .unwrap_or_else(|e| panic!("cannot read {wasm_path} (run `make build-component`): {e}"));
    let form = reqwest::multipart::Form::new()
        .text("version", CANARY_VERSION)
        .text("activate", "false")
        .part(
            "wasm",
            reqwest::multipart::Part::bytes(wasm).file_name("echo.wasm"),
        );
    let up = client
        .post(format!("{base}/components/{cid}/versions"))
        .bearer_auth(&token)
        .multipart(form)
        .send()
        .await
        .expect("upload canary version send");
    let up_status = up.status();
    let up_body = up.text().await.unwrap_or_default();
    assert!(
        up_status == 201 || up_body.contains("version already exists"),
        "uploading {CANARY_VERSION} must succeed or be an idempotent duplicate; \
         status={up_status} body={up_body}"
    );

    // upload 直後に stable が動いていないこと（activate=false の検証）。
    let t0 = get_traffic(&client, &base, &token, &cid).await;
    let stable_version_id = t0["stable"]["version_id"]
        .as_str()
        .unwrap_or_else(|| panic!("component must have an active version; body={t0}"))
        .to_string();

    // canary の version_id を引く（PUT /traffic の応答が権威）。
    set_traffic(&client, &base, &token, &cid, CANARY_VERSION, 0).await;
    let t1 = get_traffic(&client, &base, &token, &cid).await;
    let canary_version_id = t1["canary"]["version_id"]
        .as_str()
        .expect("canary version_id after PUT /traffic")
        .to_string();
    assert_ne!(
        stable_version_id, canary_version_id,
        "the canary must be a different version than stable"
    );

    let endpoint_keys: Vec<String> = (0..ENDPOINT_KEYS).map(|i| format!("t1-e-{i}")).collect();
    let mono_keys: Vec<String> = (0..MONOTONIC_KEYS).map(|i| format!("t1-m-{i}")).collect();

    // --- (1) weight=0 は端点: 全て stable（統計ではないのでフレークしない） ---
    for key in &endpoint_keys {
        let vid = invoke_and_version(&client, &base, &token, key).await;
        assert_eq!(
            vid, stable_version_id,
            "weight=0 must route every key to stable (key={key})"
        );
    }

    // --- (2) weight=100 は端点: 全て canary ---
    set_traffic(&client, &base, &token, &cid, CANARY_VERSION, 100).await;
    for key in &endpoint_keys {
        let vid = invoke_and_version(&client, &base, &token, key).await;
        assert_eq!(
            vid, canary_version_id,
            "weight=100 must route every key to canary (key={key})"
        );
    }

    // --- (3) 単調性: S10 ⊆ S50 ⊆ S100 = K ---
    set_traffic(&client, &base, &token, &cid, CANARY_VERSION, 10).await;
    let traffic_before_10 = get_traffic(&client, &base, &token, &cid).await;
    let routed_before_10 = canary_routed_of(&traffic_before_10, &canary_version_id);
    let s10 = canary_key_set(&client, &base, &token, &mono_keys, &canary_version_id).await;

    set_traffic(&client, &base, &token, &cid, CANARY_VERSION, 50).await;
    let traffic_before_50 = get_traffic(&client, &base, &token, &cid).await;
    let routed_before_50 = canary_routed_of(&traffic_before_50, &canary_version_id);
    let s50 = canary_key_set(&client, &base, &token, &mono_keys, &canary_version_id).await;

    set_traffic(&client, &base, &token, &cid, CANARY_VERSION, 100).await;
    let s100 = canary_key_set(&client, &base, &token, &mono_keys, &canary_version_id).await;
    let traffic_after_100 = get_traffic(&client, &base, &token, &cid).await;
    let routed_after_100 = canary_routed_of(&traffic_after_100, &canary_version_id);

    assert!(
        s10.is_subset(&s50),
        "raising the weight must never move a key back to stable: |S10|={} |S50|={}",
        s10.len(),
        s50.len()
    );
    assert!(
        s50.is_subset(&s100),
        "raising the weight must never move a key back to stable: |S50|={} |S100|={}",
        s50.len(),
        s100.len()
    );
    assert_eq!(
        s100.len(),
        mono_keys.len(),
        "weight=100 must route every key to canary"
    );
    assert!(
        !s10.is_empty(),
        "weight=10 must route at least one key to canary"
    );
    assert!(
        s50.len() > s10.len(),
        "weight=50 must route strictly more keys than weight=10: |S10|={} |S50|={}",
        s10.len(),
        s50.len()
    );

    // --- (4) canary_routed（executions.routing_reason 由来の一次情報）が単調増加する ---
    assert!(
        routed_before_50 > routed_before_10,
        "canary_routed must grow while the canary is receiving traffic: {routed_before_10} -> {routed_before_50}"
    );
    assert!(
        routed_after_100 > routed_before_50,
        "canary_routed must grow while the canary is receiving traffic: {routed_before_50} -> {routed_after_100}"
    );

    // --- (5) sticky: 同じ key は何度呼んでも同じ版（決定性の黒箱証明） ---
    set_traffic(&client, &base, &token, &cid, CANARY_VERSION, 50).await;
    let sticky_key = "t1-sticky";
    let first = invoke_and_version(&client, &base, &token, sticky_key).await;
    for _ in 0..2 {
        let again = invoke_and_version(&client, &base, &token, sticky_key).await;
        assert_eq!(
            again, first,
            "the same routing key must always resolve to the same version"
        );
    }

    // --- (6) 削除保護と fail-safe ---
    let del_while_canary = client
        .delete(format!("{base}/components/{cid}/versions/{CANARY_VERSION}"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("delete canary version send");
    assert_eq!(
        del_while_canary.status(),
        409,
        "deleting the canary target must be refused while a split is configured"
    );

    // --- (7) rollback の実効範囲: publish 済みジョブは canary 版で完走する（保証の境界） ---
    set_traffic(&client, &base, &token, &cid, CANARY_VERSION, 100).await;
    let in_flight = invoke_async(&client, &base, &token, "t1-inflight").await;
    let _ = client
        .post(format!("{base}/components/{cid}/rollback"))
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await;
    let in_flight_version = wait_version_id(&client, &base, &token, &in_flight).await;
    assert_eq!(
        in_flight_version, canary_version_id,
        "rollback must only affect NEW enqueues; already-published jobs keep running the canary \
         version (this is the documented guarantee boundary, not a bug)"
    );

    // --- (8) 宣言的 no-op 切替のあとでも rollback が効く（CASE ガードの回帰） ---
    for v in ["0.1.0", CANARY_VERSION, CANARY_VERSION] {
        let r = client
            .put(format!("{base}/components/{cid}/active-version"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"version": v}))
            .send()
            .await
            .expect("set active-version send");
        assert_eq!(r.status(), 200, "PUT /active-version {v} must return 200");
    }
    let rb: serde_json::Value = client
        .post(format!("{base}/components/{cid}/rollback"))
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("rollback after no-op send")
        .json()
        .await
        .expect("rollback json");
    assert_eq!(
        rb["active_version_id"].as_str(),
        Some(stable_version_id.as_str()),
        "a repeated no-op PUT /active-version must not destroy the rollback target; body={rb}"
    );

    // --- (9) ワンクリック rollback（promote 後に body {} の 1 リクエストだけで戻る） ---
    set_traffic(&client, &base, &token, &cid, CANARY_VERSION, 100).await;
    let promoted: serde_json::Value = client
        .post(format!("{base}/components/{cid}/promote"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"version": CANARY_VERSION}))
        .send()
        .await
        .expect("promote send")
        .json()
        .await
        .expect("promote json");
    assert_eq!(
        promoted["active_version_id"].as_str(),
        Some(canary_version_id.as_str()),
        "promote must make the canary the new stable; body={promoted}"
    );

    // ここが「ワンクリック」の実測: **引数なしの 1 リクエスト**だけを送る。
    let rolled: serde_json::Value = client
        .post(format!("{base}/components/{cid}/rollback"))
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("one-click rollback send")
        .json()
        .await
        .expect("one-click rollback json");

    let after = get_traffic(&client, &base, &token, &cid).await;
    let post_rollback_versions: Vec<String> = {
        let mut v = Vec::new();
        for i in 0..10 {
            v.push(invoke_and_version(&client, &base, &token, &format!("t1-after-{i}")).await);
        }
        v
    };

    // --- (10) 後始末は assert より前に（chaos_m6 の作法: 失敗しても stack を汚さない） ---
    let _ = client
        .delete(format!("{base}/components/{cid}/traffic"))
        .bearer_auth(&token)
        .send()
        .await;
    let _ = client
        .put(format!("{base}/components/{cid}/active-version"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"version": "0.1.0"}))
        .send()
        .await;

    assert_eq!(
        rolled["active_version_id"].as_str(),
        Some(stable_version_id.as_str()),
        "one-click rollback must restore the previous stable; body={rolled}"
    );
    assert_eq!(
        after["weight"].as_i64(),
        Some(0),
        "rollback must clear the traffic split; body={after}"
    );
    assert!(
        after["canary"].is_null(),
        "rollback must clear the canary pointer; body={after}"
    );
    for (i, vid) in post_rollback_versions.iter().enumerate() {
        assert_eq!(
            vid, &stable_version_id,
            "every invoke after the rollback must run the restored stable version (i={i})"
        );
    }
}
