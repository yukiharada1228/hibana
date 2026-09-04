//! M8 弾力スケールとテナント間アイソレーションの end-to-end テスト（仕様書 §15 M8 完了条件）。
//!
//! §15 M8 完了条件（改訂版の逐語）:「あるテナントのバーストが他テナントの
//! **サーバ時計で測ったキュー待ちの絶対上限**・**429 件数**・**実行順序の件数**を劣化させず、
//! 負荷に応じて worker の**実プロセス数**が増減すること」。
//!
//! 当初の完了条件は「p99 レイテンシを劣化させない」だったが、このリポジトリには
//! **分布を測る手段が無い**（Prometheus histogram は公開 API から読めず、chaos は黒箱テストである）。
//! 測れない条件は「通ったことにする」以外の使い道が無いので、同じ性質を**整数と絶対上限**で
//! 表現し直したのが上記である。
//!
//! U1（バーストが他テナントのクォータを劣化させない, C2）/ U2（キュー待ちが backlog に比例しない, C3/C4）/
//! U3（admission バイパス経路でも分離が効く + M6 回帰, H2）/ U4（worker が自動増減する, C1/C5）。
//!
//! ## Un が検証すること（信頼境界の明示）
//!
//! - **時間の assert は「サーバ時計の絶対上限」だけ**にする。`started_at - created_at` は
//!   すべて control-plane / worker が書いたサーバ時刻から算出し、クライアントの `Instant` は
//!   測定に使わない。相対比較（「B は A より速い」）もしない —— マシン速度に依存して
//!   flaky になるからである。これは chaos_m6 が確立した「唯一許容される時間 assert の型」に従う。
//! - **順序は件数で見る**。「B の最後の実行より前に始まった A の実行が何件か」は整数であり、
//!   FIFO 単一レーンなら構造的に BURST 件、lane が分かれていれば数十のオーダーになる。
//!   分子・分母が同じマシン速度でスケールするので機種依存が消える。
//! - **負の対照を必ず置く**。「A が実際に滞留していたこと」を先に整数で確認してから
//!   「B は滞留していない」を見る。これが無いと、単に負荷が軽くて何も起きなかった実行が
//!   緑になり、テストが vacuous に通る。
//! - **`/metrics` の scrape を限定的に解禁する**。chaos_m4 以来 M7 まで、どのテストも
//!   `/metrics` を読んでいない（「内部ネット越し前提」として回避してきた）。M8 はこれを破る。
//!   破ってよい根拠は、読む対象が `wasmtime_worker_slot` / `executions_total{outcome}` /
//!   `wasmtime_redelivered_total` / `wasmtime_component_cache_hits_total{tier}` という
//!   **整数カウンタ / gauge の差分**であって分布ではないこと、そして
//!   「worker が何台生きているか」「ゲストが何回実行されたか」「§3.6 のキャッシュ階層がどう効いたか」を
//!   黒箱で観測する手段が他に存在しないことである。プロセス introspection も
//!   `docker compose` の shell-out も使わない（chaos_m7 が環境依存として却下済み）。
//!
//! ## 実行方法
//! ```sh
//! docker compose up -d
//! # QUOTA_MAX_CONCURRENT_EXECUTIONS は **CHAOS_M8_BURST より大きく**すること（下の「前提」参照）
//! QUOTA_MAX_CONCURRENT_EXECUTIONS=400 TENANT_LANES_ENABLED=true SCALE_POLL_INTERVAL_SECS=5 make run-cp
//! make bootstrap                                    # A テナント → CHAOS_TOKEN
//! make bootstrap SMOKE_TENANT_SLUG=chaos-b SMOKE_EMAIL=b@example.com   # B → CHAOS_TOKEN_B
//! make deploy-chaos-components                      # burn を両テナントへ
//! make run-workers N=1                              # U1〜U3 用（U4 は make autoscale）
//! cargo test -p faas-control-plane --test chaos_m8 -- --ignored --test-threads=1
//! ```
//!
//! env: `CHAOS_BASE_URL`(既定 http://127.0.0.1:8080) / `CHAOS_TOKEN`(必須) /
//!      `CHAOS_TOKEN_B`(必須: 2 テナント目) / `CHAOS_TOKEN_C`(U3 のみ: 3 テナント目) /
//!      `CHAOS_INTERNAL_URL`(既定 http://127.0.0.1:8081) / `CHAOS_BURN`(既定 burn) /
//!      `CHAOS_POLL_SECS`(既定 120) / `CHAOS_M8_BURST`(既定 300) / `CHAOS_M8_VICTIM`(既定 20) /
//!      `CHAOS_M8_BURN_MS`(既定 300) / `CHAOS_M8_B_QUEUE_WAIT_MAX_MS`(既定 5000) /
//!      `CHAOS_M8_SCALE_TIMEOUT_SECS`(既定 180) / `CHAOS_M8_COLDSTART_BUDGET_SECS`(既定 60) /
//!      `WORKER_METRICS_PORT_BASE`(既定 9101) / `WORKER_RUNDIR`(既定 .workers)。
//!
//! ## 前提（満たさなければ skip ではなく panic する）
//!
//! 最も見落としやすい前提は **admission ゲート 2** である。A の in-flight は
//! `max_concurrent_executions`（既定 20）で頭打ちになるので、既定のまま BURST=300 を投げても
//! **A の実 execution は 20〜40 件にしかならない**。その状態では「A の 300 件の後ろに B が並ぶ」
//! という検証したい状況が再現せず、C4 の閾値は修正前でも自明に成立する ——
//! **つまりテストが vacuous に通る**。
//!
//! 上限は `CHAOS_M8_BURST` より**大きく**なければならない。BURST 件を同時に投げるので、
//! 上限が BURST 以下だと超過分がその場で 429 になるからである（実測: 上限 200 / BURST 300 で
//! ちょうど 200 件受理・100 件 429）。既定 BURST=300 に対して 400 を推奨する。
//! テストは「A が実際に積めた件数」を数え、満たさなければ必要な値を添えて panic する。
//!
//! ## `--test-threads=1` で実行すること
//!
//! U1〜U4 は共有 JetStream stream の depth、worker プロセス全体の実行スロット、worker プロセス台数
//! という**プロセス外の共有状態**を飽和させる。並行実行すると互いの backlog を測ってしまい、
//! どの assert も意味を失う（chaos_m5 / chaos_m7 が直列実行を要求するのと同型の理由）。
//!
//! ## 「修正前は red、修正後は green」の対比（実測結果）
//!
//! **修正前から緑になる assert は完了条件の証拠にならない**ので、`TENANT_LANES_ENABLED=false`
//! （= M8-4 適用前と同じ共有 consumer 1 本のトポロジ）で実際に回して確かめた。結果:
//!
//! | assert | lanes=true | lanes=false | 判定 |
//! | --- | --- | --- | --- |
//! | **U2 / C3**（B のキュー待ち上限） | green | **red** | **これが M8 の証拠** |
//! | U1 / C2-a（B の 429 が 0） | green | green | 2 テナントでは識別力なし（下記） |
//!
//! lanes=false での U2 の実測値:
//!
//! ```text
//! B のキュー待ち最大値が 40689ms で、上限 5000ms を超えました
//! （同時刻の A の最大値は 40010ms）。
//! ```
//!
//! B の待ち時間が A のそれ（40010ms）と**ほぼ同一**になっている。これは
//! 「B が A の backlog の後ろに丸ごと並んだ」ことの直接の観測であり、M8 が解こうとしている
//! 問題そのものである。lane を有効にすると B は上限内に収まる。
//!
//! **設計時の想定と違った点を明記しておく**: 設計書は「U1 の C2-a も lanes=false で落ちるはず」
//! としていたが、**2 テナントでは落ちない**。C2-a が発火するには legacy 共有 consumer の
//! 合算 `max_ack_pending`（1000）を溢れさせる必要があり、それには
//! `Σ(テナント数 × max_concurrent_executions) > 1000` が要る。本テストの規模
//! （2 テナント × 400）では 800 で届かない。既定クォータ 20 なら 50 テナント必要である。
//! したがって **C2-a は「劣化していないことの回帰ガード」であって、分離の証明ではない**。
//! 分離を証明しているのは U2 の C3 である。

#![allow(clippy::needless_return)]

use std::time::Duration;

// ============================================================================
// ヘルパ（各 tests/*.rs は独立クレートのため chaos_m4〜m7 と同様に複製する）
// ============================================================================

fn base_url() -> String {
    std::env::var("CHAOS_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into())
}

fn internal_url() -> String {
    std::env::var("CHAOS_INTERNAL_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".into())
}

fn token_a() -> String {
    std::env::var("CHAOS_TOKEN")
        .expect("CHAOS_TOKEN must be set (run `make bootstrap` and export the printed token)")
}

/// 2 テナント目。**未設定なら skip ではなく panic** する。
///
/// skip にすると「2 テナント目を用意し忘れた実行」が緑になり、M8 の完了条件そのもの
/// （テナント間アイソレーション）を一度も検証しないまま通ってしまう。
fn token_b() -> String {
    std::env::var("CHAOS_TOKEN_B").expect(
        "CHAOS_TOKEN_B must be set: M8 はテナント間アイソレーションの検証なので 2 テナント目が必須。\n\
         make bootstrap SMOKE_TENANT_SLUG=chaos-b SMOKE_EMAIL=b@example.com \\\n\
         を実行し、出力された token を CHAOS_TOKEN_B にエクスポートすること",
    )
}

fn burn_component() -> String {
    std::env::var("CHAOS_BURN").unwrap_or_else(|_| "burn".into())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn poll_secs() -> u64 {
    env_u64("CHAOS_POLL_SECS", 120)
}

fn burst() -> usize {
    env_u64("CHAOS_M8_BURST", 300) as usize
}

fn victim_count() -> usize {
    env_u64("CHAOS_M8_VICTIM", 20) as usize
}

fn burn_ms() -> u64 {
    env_u64("CHAOS_M8_BURN_MS", 300)
}

fn b_queue_wait_max_ms() -> i64 {
    env_u64("CHAOS_M8_B_QUEUE_WAIT_MAX_MS", 5000) as i64
}

fn metrics_port_base() -> u16 {
    env_u64("WORKER_METRICS_PORT_BASE", 9101) as u16
}

/// supervisor の作業ディレクトリ。
///
/// **統合テストの cwd はクレートディレクトリ**（`crates/control-plane`）であって
/// ワークスペース root ではない。`scripts/*.sh` は root 相対の `.workers` を使うので、
/// 既定値は root から解決しないと「heartbeat が存在しません」と誤診断する
/// （chaos_m7 が `CHAOS_ECHO_WASM` で同じ罠を踏んでいる）。
fn worker_rundir() -> String {
    std::env::var("WORKER_RUNDIR")
        .unwrap_or_else(|_| format!("{}/../../.workers", env!("CARGO_MANIFEST_DIR")))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build reqwest client")
}

/// `POST /invoke`（非同期）。**429 を握り潰さず** status code と body を返す。
///
/// 429 の件数そのものが C2-a の assert 対象なので、ここでエラーにしてはならない。
async fn invoke_async(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    component: &str,
    burn_ms: u64,
) -> (u16, serde_json::Value) {
    let resp = client
        .post(format!("{base}/invoke"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "component": component,
            "input": { "burn_ms": burn_ms },
        }))
        .send()
        .await
        .expect("POST /invoke");
    let status = resp.status().as_u16();
    let body = resp
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// `GET /executions/{id}` を 1 回読む。
async fn get_execution(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    id: &str,
) -> serde_json::Value {
    client
        .get(format!("{base}/executions/{id}"))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /executions/{id}")
        .json::<serde_json::Value>()
        .await
        .expect("execution json")
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "timeout")
}

/// 全 execution が終端するまで poll する。上限を超えたら未終端の件数を添えて返す。
async fn wait_all_terminal(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    ids: &[String],
    limit_secs: u64,
) -> Vec<serde_json::Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(limit_secs);
    loop {
        let mut out = Vec::with_capacity(ids.len());
        let mut pending = 0usize;
        for id in ids {
            let e = get_execution(client, base, token, id).await;
            let st = e.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if !is_terminal(st) {
                pending += 1;
            }
            out.push(e);
        }
        if pending == 0 || std::time::Instant::now() >= deadline {
            if pending > 0 {
                eprintln!(
                    "WARN: {pending}/{} executions still non-terminal after {limit_secs}s",
                    ids.len()
                );
            }
            return out;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// 未終端の件数を数える（負の対照用）。
async fn count_unterminated(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    ids: &[String],
) -> usize {
    let mut n = 0;
    for id in ids {
        let e = get_execution(client, base, token, id).await;
        let st = e.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if !is_terminal(st) {
            n += 1;
        }
    }
    n
}

fn parse_ts(v: Option<&str>) -> Option<i64> {
    v.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
}

/// `started_at - created_at`（ミリ秒）。**すべてサーバ時計**である
/// （`created_at` は CP、`started_at` は worker の `mark_running` が書く）。
/// どちらかが欠けている execution は `None`（未着手 = 測定対象外）。
fn queue_wait_ms(e: &serde_json::Value) -> Option<i64> {
    let created = parse_ts(e.get("created_at").and_then(|v| v.as_str()))?;
    let started = parse_ts(e.get("started_at").and_then(|v| v.as_str()))?;
    Some(started - created)
}

fn started_at_ms(e: &serde_json::Value) -> Option<i64> {
    parse_ts(e.get("started_at").and_then(|v| v.as_str()))
}

/// `GET /usage` の合計を読む（chaos_m5 と同じ形）。
async fn usage_total(client: &reqwest::Client, base: &str, token: &str, field: &str) -> i64 {
    let v: serde_json::Value = client
        .get(format!("{base}/usage"))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /usage")
        .json()
        .await
        .expect("usage json");
    v.get("totals")
        .and_then(|t| t.get(field))
        .and_then(|x| x.as_i64())
        .unwrap_or(0)
}

/// `GET /internal/scale`。**タイムアウト予算はこの応答から算術で導出する**（値をハードコードしない）。
async fn scale_signal(client: &reqwest::Client) -> Option<serde_json::Value> {
    let resp = client
        .get(format!("{}/internal/scale", internal_url()))
        .send()
        .await
        .ok()?;
    let ok = resp.status().is_success();
    let body = resp.json::<serde_json::Value>().await.ok()?;
    if ok {
        Some(body)
    } else {
        // 503（未観測 / stale / ポーラ無効）。desired は含まれない。
        None
    }
}

/// Prometheus text から 1 系列の値を読む。
///
/// **契約: 該当系列が存在しなければ 0 を返す。** `IntCounterVec` は一度も inc されていない
/// ラベル組み合わせがテキストに現れないため、この縮退規約が無いとベースライン取得が
/// 「系列が無い」でパニックする。`wasmtime_component_cache_hits_total` は Vec なのに
/// `..._misses_total` は素の `IntCounter` という非対称が実在するので、両方をこの 1 本で扱う。
fn scrape_value(text: &str, name: &str, labels: Option<&str>) -> u64 {
    let prefix = match labels {
        Some(l) => format!("{name}{{{l}}} "),
        None => format!("{name} "),
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(prefix.as_str()) {
            return rest.trim().parse::<f64>().unwrap_or(0.0) as u64;
        }
    }
    0
}

async fn scrape(client: &reqwest::Client, port: u16) -> Option<String> {
    client
        .get(format!("http://127.0.0.1:{port}/metrics"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()
}

/// 生存中の worker の slot 番号を返す。
///
/// `/healthz` ではなく `/metrics` を使い、**`wasmtime_worker_slot` の値が期待 slot と一致する**
/// ものだけを数える。200 が返るだけでは「別プロセスが同じポートを掴んでいる」場合と区別が
/// つかないからである（手動 `make run-worker` の混入は実際に起きる）。
async fn live_workers(client: &reqwest::Client, max_slots: usize) -> Vec<usize> {
    let base = metrics_port_base();
    let mut out = Vec::new();
    for i in 0..max_slots {
        let port = base + i as u16;
        if let Some(text) = scrape(client, port).await {
            if scrape_value(&text, "wasmtime_worker_slot", None) == i as u64 {
                out.push(i);
            }
        }
    }
    out
}

/// 全 slot のカウンタを合算する（**停止済み slot の分も含める**）。
///
/// プロセスが死ぬとカウンタも消えるため、supervisor は停止時に最終値を
/// `$WORKER_RUNDIR/slot-{i}.{persist_key}` へ累計として落としている（`worker-lib.sh`）。
///
/// **生存 slot でも累計ファイルを必ず足す。** 同じ slot が「起動 → 停止 → 再起動」した場合、
/// 過去の寿命ぶんは累計ファイルに、現在の寿命ぶんは `/metrics` にあり、**両方が必要**である。
/// 生存時にファイルを無視すると、再起動をまたいだ実行が丸ごと欠落する。
async fn sum_over_slots(
    client: &reqwest::Client,
    max_slots: usize,
    name: &str,
    labels: Option<&str>,
    persist_key: &str,
) -> u64 {
    let base = metrics_port_base();
    let mut total = 0u64;
    for i in 0..max_slots {
        // (1) 過去の寿命ぶん（停止時に supervisor が確定させた累計）。
        let path = format!("{}/slot-{i}.{persist_key}", worker_rundir());
        if let Ok(s) = std::fs::read_to_string(&path) {
            total += s.trim().parse::<u64>().unwrap_or(0);
        }
        // (2) 現在の寿命ぶん（生きていれば）。
        let port = base + i as u16;
        if let Some(text) = scrape(client, port).await {
            if scrape_value(&text, "wasmtime_worker_slot", None) == i as u64 {
                total += scrape_value(&text, name, labels);
            }
        }
    }
    total
}

// ============================================================================
// 前提チェック（満たさなければ明確なメッセージで panic する）
// ============================================================================

/// burn component が両テナントへデプロイされていること。
async fn require_burn(client: &reqwest::Client, base: &str, token: &str, who: &str) {
    let comps: serde_json::Value = client
        .get(format!("{base}/components"))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /components")
        .json()
        .await
        .expect("components json");
    let name = burn_component();
    // `GET /components` は **裸の配列**を返す（`{"components": [...]}` ではない）。
    let found = comps
        .as_array()
        .map(|a| {
            a.iter()
                .any(|c| c.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
        })
        .unwrap_or(false);
    assert!(
        found,
        "テナント {who} に component '{name}' がデプロイされていません。\n\
         `make deploy-chaos-components` を各テナントのトークンで実行してください。\n\
         burn は「N ミリ秒かかって succeeded で終端する」ノブであり、\n\
         M8 の負荷は『件数 × 1 件あたり実行時間』を制御できることが前提です"
    );
}

/// A が実際に BURST の 9 割以上を積めたこと（§10.3.1 の最重要前提）。
fn require_enough_enqueued(accepted: usize, rejected_429: usize) {
    let n = burst();
    let need = (n * 9) / 10;
    // 上限は BURST より大きくないと、同時投入した超過分がその場で 429 になる。
    let suggested = n + n / 4;
    assert!(
        accepted >= need,
        "A が enqueue できたのは {accepted} 件（429: {rejected_429} 件）で、必要な {need} 件に届きません。\n\
         admission ゲート 2 が A の in-flight を max_concurrent_executions で頭打ちにしています。\n\
         control-plane を **QUOTA_MAX_CONCURRENT_EXECUTIONS={suggested} 以上**（= CHAOS_M8_BURST({n}) より大きく）\n\
         で起動するか、A テナントの quotas.max_concurrent_executions を上書きしてください。\n\
         あるいは CHAOS_M8_BURST を下げてください。\n\
         この前提を満たさないと『A の後ろに B が並ぶ』状況が再現せず、\n\
         C4 の閾値は修正前でも自明に成立してしまいます（テストが vacuous に通る）"
    );
}

// ============================================================================
// Scenario U1 — バーストテナント A が被害テナント B のクォータを劣化させない（C2）
// ============================================================================

/// **Scenario U1**: A が BURST 件をバーストしている最中に、B が自分のクォータの 1/5 のレートで
/// 少数を invoke する。B には **429 が 1 件も出ず**、**全件 succeeded** し、**会計もちょうど一致**する。
///
/// B の送信レートを B 自身のクォータの 1/5 に抑えてあるので、B に起因する rate_limited /
/// concurrency_limit は**構造的に 0** である。したがって 429 が 1 件でも出れば
/// 「A が B の何かを消費した」ことの証明になる。これは完全に決定的な整数 assert であり、
/// マシン速度に一切依存しない。
///
/// `TENANT_LANES_ENABLED=false` で回すと C2-a が落ちる（M7 までの共有 consumer では
/// A の負荷が B の配送枠を食い潰す）。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + CHAOS_TOKEN_B + burn; run: cargo test -p faas-control-plane --test chaos_m8 -- --ignored --test-threads=1 chaos_u1_"]
async fn chaos_u1_burst_tenant_does_not_degrade_victim_quota() {
    let base = base_url();
    let (ta, tb) = (token_a(), token_b());
    let client = client();

    require_burn(&client, &base, &ta, "A").await;
    require_burn(&client, &base, &tb, "B").await;

    let comp = burn_component();
    let ms = burn_ms();
    let usage_before = usage_total(&client, &base, &tb, "invocation_count").await;

    // --- A: BURST 件を並行 invoke。429 も成功も全件記録する ---
    let n = burst();
    let mut a_handles = Vec::with_capacity(n);
    for _ in 0..n {
        let (c, b, t, comp) = (client.clone(), base.clone(), ta.clone(), comp.clone());
        a_handles.push(tokio::spawn(async move {
            invoke_async(&c, &b, &t, &comp, ms).await
        }));
    }

    // --- B: A のバースト中に少数を、B のクォータの 1/5 のレートで invoke ---
    // 既定クォータ 50 req/s の 1/5 = 10 req/s → 100ms 間隔。
    let m = victim_count();
    let mut b_ids = Vec::with_capacity(m);
    let mut b_status = Vec::with_capacity(m);
    for _ in 0..m {
        let (status, body) = invoke_async(&client, &base, &tb, &comp, ms).await;
        b_status.push(status);
        if let Some(id) = body.get("execution_id").and_then(|v| v.as_str()) {
            b_ids.push(id.to_string());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A の結果を回収。
    let mut a_ids = Vec::new();
    let mut a_429 = 0usize;
    for h in a_handles {
        let (status, body) = h.await.expect("join A invoke");
        if status == 429 {
            a_429 += 1;
        } else if let Some(id) = body.get("execution_id").and_then(|v| v.as_str()) {
            a_ids.push(id.to_string());
        }
    }

    // --- 後始末は assert より前（chaos_m7 の作法）: A の残ジョブを終端させる ---
    let _ = wait_all_terminal(&client, &base, &ta, &a_ids, poll_secs()).await;
    let b_execs = wait_all_terminal(&client, &base, &tb, &b_ids, poll_secs()).await;
    // rollup 反映の取りこぼしを避ける。
    tokio::time::sleep(Duration::from_secs(3)).await;
    let usage_after = usage_total(&client, &base, &tb, "invocation_count").await;

    // --- 前提 ---
    require_enough_enqueued(a_ids.len(), a_429);

    // --- C2-a: B に 429 が 1 件も無い ---
    let b_429 = b_status.iter().filter(|&&s| s == 429).count();
    assert_eq!(
        b_429, 0,
        "B に 429 が {b_429} 件出ました（A は {} 件 enqueue、429 は {a_429} 件）。\n\
         B の送信レートは B 自身のクォータの 1/5 なので、B に起因する 429 は構造的に 0 です。\n\
         1 件でも出たということは、A のバーストが B のクォータを消費したことを意味します（§15 M8 違反）",
        a_ids.len()
    );

    // --- C2-b: B の全 execution が succeeded ---
    let b_succeeded = b_execs
        .iter()
        .filter(|e| e.get("status").and_then(|v| v.as_str()) == Some("succeeded"))
        .count();
    assert_eq!(
        b_succeeded,
        b_ids.len(),
        "B の execution は全件 succeeded であること（{b_succeeded}/{} 件）。\n\
         failed / timeout は A のバーストが B の実行を巻き込んだ証拠になります",
        b_ids.len()
    );

    // --- C2-c: 会計がちょうど一致（M5 の不変条件が M8 の負荷下でも保たれる）---
    let dinv = usage_after - usage_before;
    assert_eq!(
        dinv,
        b_ids.len() as i64,
        "B の invocation_count は正確に +{} であること（実際は +{dinv}）。\n\
         少なければ欠落、多ければ二重計上で、どちらも §15 M5 の会計不変条件を破ります",
        b_ids.len()
    );
}

// ============================================================================
// Scenario U2 — B のキュー待ちがバーストの backlog に比例しない（C3 / C4）
// ============================================================================

/// **Scenario U2**: U1 と同じ負荷の下で、B のキュー待ち（サーバ時計）が絶対上限を超えないこと、
/// および「B より先に始まった A の実行」の件数が BURST の半分未満であること。
///
/// **負の対照を 2 つ置く**のが本シナリオの要点である:
/// (i) B の最終 invoke 時点で A が実際に滞留していたこと（未終端 >= BURST/2）
/// (ii) A 自身のキュー待ちの最大値が B の閾値を超えていたこと
/// これらを先に確認しないと、「単に負荷が軽くて誰も待たなかった」実行が緑になり、
/// テストは何も証明しない。
///
/// C4 は FIFO 単一レーンなら構造的に BURST 件になる（B は stream 上で A の後ろに並ぶ）。
/// lane が分かれていれば数十のオーダーに落ちる。分子・分母が同じマシン速度でスケールするので、
/// 機種依存が消える。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + CHAOS_TOKEN_B + burn; run: cargo test -p faas-control-plane --test chaos_m8 -- --ignored --test-threads=1 chaos_u2_"]
async fn chaos_u2_victim_queue_wait_does_not_track_burst_backlog() {
    let base = base_url();
    let (ta, tb) = (token_a(), token_b());
    let client = client();

    require_burn(&client, &base, &ta, "A").await;
    require_burn(&client, &base, &tb, "B").await;

    let comp = burn_component();
    let ms = burn_ms();
    let n = burst();

    let mut a_handles = Vec::with_capacity(n);
    for _ in 0..n {
        let (c, b, t, comp) = (client.clone(), base.clone(), ta.clone(), comp.clone());
        a_handles.push(tokio::spawn(async move {
            invoke_async(&c, &b, &t, &comp, ms).await
        }));
    }

    let m = victim_count();
    let mut b_ids = Vec::with_capacity(m);
    for _ in 0..m {
        let (_status, body) = invoke_async(&client, &base, &tb, &comp, ms).await;
        if let Some(id) = body.get("execution_id").and_then(|v| v.as_str()) {
            b_ids.push(id.to_string());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut a_ids = Vec::new();
    let mut a_429 = 0usize;
    for h in a_handles {
        let (status, body) = h.await.expect("join A invoke");
        if status == 429 {
            a_429 += 1;
        } else if let Some(id) = body.get("execution_id").and_then(|v| v.as_str()) {
            a_ids.push(id.to_string());
        }
    }

    // B の最終 invoke 直後に A の未終端件数を数える（負の対照 (i) の測定点）。
    let a_unterminated = count_unterminated(&client, &base, &ta, &a_ids).await;

    // --- 後始末は assert より前 ---
    let a_execs = wait_all_terminal(&client, &base, &ta, &a_ids, poll_secs()).await;
    let b_execs = wait_all_terminal(&client, &base, &tb, &b_ids, poll_secs()).await;

    // --- 前提 ---
    require_enough_enqueued(a_ids.len(), a_429);

    // --- C3 負の対照 (i): A が実際に滞留していた ---
    assert!(
        a_unterminated >= n / 2,
        "B の最終 invoke 時点で A の未終端は {a_unterminated} 件しかありません（必要: {} 件以上）。\n\
         A が滞留していないなら『B は A の影響を受けない』を確認したことになりません。\n\
         CHAOS_M8_BURST か CHAOS_M8_BURN_MS を上げてください",
        n / 2
    );

    let a_waits: Vec<i64> = a_execs.iter().filter_map(queue_wait_ms).collect();
    let b_waits: Vec<i64> = b_execs.iter().filter_map(queue_wait_ms).collect();
    assert!(
        !b_waits.is_empty(),
        "B の execution が 1 件も started_at を持ちません"
    );

    let a_max = a_waits.iter().copied().max().unwrap_or(0);
    let b_max = b_waits.iter().copied().max().unwrap_or(0);
    let limit = b_queue_wait_max_ms();

    // --- C3 負の対照 (ii): A 自身は閾値を超えて待たされていた ---
    assert!(
        a_max >= limit,
        "A のキュー待ち最大値が {a_max}ms で、閾値 {limit}ms を超えていません。\n\
         A ですら待っていないのなら、B が待たないのは当たり前で何も証明できません。\n\
         CHAOS_M8_BURST か CHAOS_M8_BURN_MS を上げてください"
    );

    // --- C3: B のキュー待ちは絶対上限以内（サーバ時計・絶対値・env 上書き可）---
    assert!(
        b_max <= limit,
        "B のキュー待ち最大値が {b_max}ms で、上限 {limit}ms を超えました\n\
         （同時刻の A の最大値は {a_max}ms）。\n\
         B のキュー待ちが A の backlog に引きずられており、§15 M8 の完了条件を満たしません"
    );

    // --- C4: 「B より先に始まった A の実行」の件数 ---
    let b_last_start = b_execs.iter().filter_map(started_at_ms).max().unwrap_or(0);
    let a_before_b = a_execs
        .iter()
        .filter_map(started_at_ms)
        .filter(|&t| t < b_last_start)
        .count();
    assert!(
        a_before_b < n / 2,
        "B の最後の実行より前に始まった A の実行が {a_before_b} 件あります（上限 {} 件）。\n\
         FIFO の単一レーンならこれは構造的に BURST({n}) 件に近づきます。\n\
         lane が分かれていれば数十のオーダーに収まるはずで、この値は\n\
         『配送層で分離が効いているか』の直接の指標です",
        n / 2
    );
}

// ============================================================================
// Scenario U3 — admission をバイパスする経路でも分離が効き、M6 が壊れていない
// ============================================================================

/// **Scenario U3**: A が Cron 経由（= HTTP admission を通らない経路）で発火し続けている間も、
/// B の 429 が 0 でキュー待ちが上限以内であること。加えて **M6 の回帰ガード**として、
/// 作りたてのテナント C の初回 `POST /invoke?wait=1` が 200 を返す（202 への縮退でない）こと。
///
/// M6 回帰ガードが重要なのは、lane 作成の即時通知（`faas.lane.changed`）が効いていないと
/// 「lane はあるが worker がまだ購読していない」窓が最大 1 discovery 周期残り、
/// **新規テナントの初回同期 invoke が必ず 202 に縮退する**からである。これは M6 の完了条件の
/// 回帰であり、M8 の変更（consumer 作成責務の CP 移管）が直接踏みうる地雷である。
///
/// `CHAOS_TOKEN_C` が無い場合、M6 回帰ガードだけを skip する（分離の検証は行う）。
/// 3 テナント目は `make bootstrap SMOKE_TENANT_SLUG=chaos-c SMOKE_EMAIL=c@example.com` で作る。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + CHAOS_TOKEN_B + burn; run: cargo test -p faas-control-plane --test chaos_m8 -- --ignored --test-threads=1 chaos_u3_"]
async fn chaos_u3_isolation_holds_for_admission_bypassing_paths() {
    let base = base_url();
    let (ta, tb) = (token_a(), token_b());
    let client = client();

    require_burn(&client, &base, &ta, "A").await;
    require_burn(&client, &base, &tb, "B").await;

    let comp = burn_component();
    let ms = burn_ms();

    // --- A: cron を 1 分粒度で登録する（M6 の chain 暴走ガードの範囲内）---
    // cron 経路は HTTP admission を通らないので、分離が「受付層」ではなく
    // 「配送層」で効いていることの検証になる。
    let cron_resp = client
        .post(format!("{base}/cron-jobs"))
        .bearer_auth(&ta)
        .json(&serde_json::json!({
            "component": comp,
            "schedule": "* * * * *",
            "input": { "burn_ms": ms },
        }))
        .send()
        .await
        .expect("POST /cron-jobs");
    let cron_status = cron_resp.status().as_u16();
    let cron_body: serde_json::Value = cron_resp.json().await.unwrap_or(serde_json::Value::Null);
    let cron_id = cron_body
        .get("cron_job_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // --- A: 同時に HTTP からもバーストして backlog を作る ---
    let n = burst();
    let mut a_handles = Vec::with_capacity(n);
    for _ in 0..n {
        let (c, b, t, comp2) = (client.clone(), base.clone(), ta.clone(), comp.clone());
        a_handles.push(tokio::spawn(async move {
            invoke_async(&c, &b, &t, &comp2, ms).await
        }));
    }

    // --- B: 少数を低レートで invoke ---
    let m = victim_count();
    let mut b_ids = Vec::with_capacity(m);
    let mut b_status = Vec::with_capacity(m);
    for _ in 0..m {
        let (status, body) = invoke_async(&client, &base, &tb, &comp, ms).await;
        b_status.push(status);
        if let Some(id) = body.get("execution_id").and_then(|v| v.as_str()) {
            b_ids.push(id.to_string());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // --- M6 回帰ガード: 作りたてのテナント C の初回同期 invoke ---
    let c_sync_status = match std::env::var("CHAOS_TOKEN_C").ok() {
        Some(tc) => {
            let resp = client
                .post(format!("{base}/invoke?wait=1"))
                .bearer_auth(&tc)
                .json(&serde_json::json!({
                    "component": comp,
                    "input": { "burn_ms": 0 },
                }))
                .send()
                .await
                .expect("POST /invoke?wait=1 (tenant C)");
            Some(resp.status().as_u16())
        }
        None => {
            eprintln!(
                "NOTE: CHAOS_TOKEN_C 未設定のため M6 回帰ガード（新規テナントの初回同期 invoke）を skip します。\n\
                 有効化するには: make bootstrap SMOKE_TENANT_SLUG=chaos-c SMOKE_EMAIL=c@example.com"
            );
            None
        }
    };

    let mut a_ids = Vec::new();
    let mut a_429 = 0usize;
    for h in a_handles {
        let (status, body) = h.await.expect("join A invoke");
        if status == 429 {
            a_429 += 1;
        } else if let Some(id) = body.get("execution_id").and_then(|v| v.as_str()) {
            a_ids.push(id.to_string());
        }
    }

    // --- 後始末は assert より前（chaos_m6 の作法）---
    if let Some(id) = &cron_id {
        let _ = client
            .delete(format!("{base}/cron-jobs/{id}"))
            .bearer_auth(&ta)
            .send()
            .await;
    }
    let _ = wait_all_terminal(&client, &base, &ta, &a_ids, poll_secs()).await;
    let b_execs = wait_all_terminal(&client, &base, &tb, &b_ids, poll_secs()).await;

    // --- 前提 ---
    assert_eq!(
        cron_status, 201,
        "cron の登録に失敗しました（HTTP {cron_status}）。U3 は admission をバイパスする経路の\n\
         検証なので、cron が登録できないと何も検証できません: {cron_body}"
    );
    require_enough_enqueued(a_ids.len(), a_429);

    // --- 分離（C2 / C3 と同型）---
    let b_429 = b_status.iter().filter(|&&s| s == 429).count();
    assert_eq!(
        b_429, 0,
        "cron を含む負荷の下でも B に 429 が出てはなりません（{b_429} 件）"
    );

    let b_waits: Vec<i64> = b_execs.iter().filter_map(queue_wait_ms).collect();
    let b_max = b_waits.iter().copied().max().unwrap_or(0);
    let limit = b_queue_wait_max_ms();
    assert!(
        b_max <= limit,
        "cron 経路を含む負荷下で B のキュー待ちが {b_max}ms（上限 {limit}ms）を超えました。\n\
         admission をバイパスする経路では分離が効いていないことを意味します"
    );

    // --- M6 回帰ガード ---
    if let Some(code) = c_sync_status {
        assert_eq!(
            code, 200,
            "新規テナント C の初回 `POST /invoke?wait=1` が {code} を返しました（期待 200）。\n\
             202 への縮退は、lane 作成の即時通知（faas.lane.changed）が効かず\n\
             『lane はあるが worker がまだ購読していない』窓に落ちたことを意味します。\n\
             これは M6 の完了条件（同期 invoke）の回帰です"
        );
    }
}

// ============================================================================
// Scenario U4 — 負荷に応じ worker が自動増減する（C1 / C5）
// ============================================================================

/// **Scenario U4**: `make autoscale` が走っている状態で負荷をかけ、worker の
/// **実プロセス数**が増え、負荷が引くと減ることを確認する。同時に
/// 「増減の過程でジョブを 1 件も再実行 / 失敗させていない」ことを整数で確認する。
///
/// 当初案の「ある瞬間の backlog == K」はレースする（アクチュエータが publish 完了前に
/// worker を立てるため、テストが読む backlog は `[0, K]` のどの値にもなりうる）。
/// したがって**等値を捨て、「ポーリング中に観測した最大値」の整数 assert に置き換える**。
///
/// C5 の 2 本（再実行ゼロ / 再配送ゼロ）が**ドレインが効いていることの直接証拠**である。
/// ドレインが無いと scale-in のたびに抱えていたジョブが再配送され、ゲストが 2 回走る。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + `make autoscale` running + CHAOS_TOKEN + burn; run: cargo test -p faas-control-plane --test chaos_m8 -- --ignored --test-threads=1 chaos_u4_"]
async fn chaos_u4_workers_scale_with_load_without_reexecution() {
    let base = base_url();
    let ta = token_a();
    let client = client();

    require_burn(&client, &base, &ta, "A").await;

    // --- 前提: アクチュエータが実際に回っていること ---
    // heartbeat が無い / 古いのに「スケールしなかった」と判定するのは誤診断なので、
    // タイムアウトを待たずに即 panic する。
    let hb_path = format!("{}/heartbeat", worker_rundir());
    let hb = std::fs::metadata(&hb_path).and_then(|m| m.modified()).ok();
    let hb_age = hb
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX);
    assert!(
        hb_age < 120,
        "アクチュエータの heartbeat ({hb_path}) が {} です。\n\
         U4 は `make autoscale` が別ターミナルで走っていることを前提とします。\n\
         起動せずに実行すると『スケールしなかった』という誤った診断になります",
        if hb.is_none() {
            "存在しません".to_string()
        } else {
            format!("{hb_age} 秒前で古すぎます")
        }
    );

    // --- 前提: supervisor 管理外の worker が居ないこと ---
    // 9090 を掴んだ手動 worker が居ると live_workers() の数え方が壊れる。
    let stray = client
        .get("http://127.0.0.1:9090/healthz")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    assert!(
        !stray,
        "127.0.0.1:9090 が応答しています = supervisor 管理外の worker が居ます。\n\
         `make run-worker` で手動起動した worker を停止してから U4 を実行してください\n\
         （混入すると『何台生きているか』の観測が信用できなくなります）"
    );

    // --- スケール方針を /internal/scale から読む（値をハードコードしない）---
    let sig = scale_signal(&client)
        .await
        .expect("GET /internal/scale が 200 を返しません。SCALE_POLL_INTERVAL_SECS>0 で CP を起動してください");
    let max_workers = sig.get("max_workers").and_then(|v| v.as_u64()).unwrap_or(4) as usize;
    let poll_interval = sig
        .get("poll_interval_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(5);
    let cooldown = sig
        .get("scale_in_cooldown_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let scale_timeout = env_u64("CHAOS_M8_SCALE_TIMEOUT_SECS", 180);

    let comp = burn_component();
    let ms = burn_ms();

    // --- 0. warm-up: cwasm を確実に生成しておく ---
    // これが無いと 10 の cache assert が「クリーンなマシンで落ち、汚れたマシンで通る」テストになる。
    let (_s, warm) = invoke_async(&client, &base, &ta, &comp, 0).await;
    if let Some(id) = warm.get("execution_id").and_then(|v| v.as_str()) {
        let _ = wait_all_terminal(&client, &base, &ta, &[id.to_string()], poll_secs()).await;
    }

    // --- 1. ベースライン退避 ---
    let exec_before = sum_over_slots(
        &client,
        max_workers,
        "executions_total",
        Some(r#"outcome="succeeded""#),
        "executions_succeeded",
    )
    .await;
    let redeliver_before = sum_over_slots(
        &client,
        max_workers,
        "wasmtime_redelivered_total",
        None,
        "redelivered",
    )
    .await;

    // --- 2. 負荷をかけつつ desired / live を最大値でサンプリングする ---
    let k = burst();
    let mut handles = Vec::with_capacity(k);
    for _ in 0..k {
        let (c, b, t, comp2) = (client.clone(), base.clone(), ta.clone(), comp.clone());
        handles.push(tokio::spawn(async move {
            invoke_async(&c, &b, &t, &comp2, ms).await
        }));
    }

    let sampler_client = client.clone();
    let sampler = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(scale_timeout);
        let (mut max_desired, mut max_live) = (0u64, 0usize);
        while std::time::Instant::now() < deadline {
            if let Some(s) = scale_signal(&sampler_client).await {
                if let Some(d) = s.get("desired").and_then(|v| v.as_u64()) {
                    max_desired = max_desired.max(d);
                }
            }
            max_live = max_live.max(live_workers(&sampler_client, max_workers).await.len());
            if max_desired >= max_workers as u64 && max_live >= max_workers {
                break;
            }
            tokio::time::sleep(Duration::from_secs(poll_interval.max(1))).await;
        }
        (max_desired, max_live)
    });

    let mut ids = Vec::new();
    let mut n429 = 0usize;
    for h in handles {
        let (status, body) = h.await.expect("join invoke");
        if status == 429 {
            n429 += 1;
        } else if let Some(id) = body.get("execution_id").and_then(|v| v.as_str()) {
            ids.push(id.to_string());
        }
    }
    let (max_desired, max_live) = sampler.await.expect("join sampler");

    // --- 後始末は assert より前: 全件終端させる ---
    let execs = wait_all_terminal(&client, &base, &ta, &ids, poll_secs().max(scale_timeout)).await;

    // --- 前提 ---
    require_enough_enqueued(ids.len(), n429);

    // --- C1-out: desired も実プロセス数も上限まで届いた ---
    assert_eq!(
        max_desired, max_workers as u64,
        "負荷中に観測した desired の最大値が {max_desired} で、max_workers({max_workers}) に届きません。\n\
         backlog が上限に対して小さすぎる可能性があります（CHAOS_M8_BURST を上げるか\n\
         QUOTA_MAX_CONCURRENT_EXECUTIONS を上げてください）"
    );
    assert_eq!(
        max_live, max_workers,
        "負荷中に観測した実プロセス数の最大値が {max_live} で、max_workers({max_workers}) に届きません。\n\
         desired は出ているのにプロセスが増えていないなら、アクチュエータ側の問題です\n\
         （$WORKER_RUNDIR/worker-*.log を確認してください）"
    );

    // --- 全件 succeeded ---
    let succeeded = execs
        .iter()
        .filter(|e| e.get("status").and_then(|v| v.as_str()) == Some("succeeded"))
        .count();
    assert_eq!(
        succeeded,
        ids.len(),
        "スケールの過程で {} 件中 {succeeded} 件しか succeeded していません。\n\
         オートスケーラが正常なジョブを失敗させています",
        ids.len()
    );

    // --- C5-a: 再実行ゼロ ---
    let exec_after = sum_over_slots(
        &client,
        max_workers,
        "executions_total",
        Some(r#"outcome="succeeded""#),
        "executions_succeeded",
    )
    .await;
    let dexec = exec_after.saturating_sub(exec_before);
    assert_eq!(
        dexec,
        ids.len() as u64,
        "worker 側の succeeded 実行数の delta が {dexec} で、投入した {} 件と一致しません。\n\
         多い場合はゲストが複数回実行されています（= ドレインが効いていない）",
        ids.len()
    );

    // --- C5-b: 再配送ゼロ（ドレインが効いていることの直接証拠）---
    let redeliver_after = sum_over_slots(
        &client,
        max_workers,
        "wasmtime_redelivered_total",
        None,
        "redelivered",
    )
    .await;
    let dredeliver = redeliver_after.saturating_sub(redeliver_before);
    assert_eq!(
        dredeliver, 0,
        "スケールの過程で {dredeliver} 件の再配送が起きました（期待 0）。\n\
         scale-in で落とした worker がドレインせずに死に、抱えていたジョブが\n\
         再配送されています。WORKER_DRAIN_TIMEOUT_SECS を確認してください\n\
         （0 だとハンドラが入らず、SIGTERM で即死します）"
    );

    // --- C1-in: 負荷が引いたらプロセスが減る ---
    let deadline = std::time::Instant::now() + Duration::from_secs(cooldown + scale_timeout);
    let mut final_live = max_live;
    while std::time::Instant::now() < deadline {
        final_live = live_workers(&client, max_workers).await.len();
        if final_live < max_live {
            break;
        }
        tokio::time::sleep(Duration::from_secs(poll_interval.max(1))).await;
    }
    assert!(
        final_live < max_live,
        "負荷が引いてから {}秒（cooldown {cooldown}s + {scale_timeout}s）待っても\n\
         プロセス数が {max_live} 台のまま減りません。scale-in が効いていません",
        cooldown + scale_timeout
    );
}
