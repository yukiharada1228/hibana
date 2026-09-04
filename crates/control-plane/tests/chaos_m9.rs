//! M9 サンドボックス強化のレッドチームテスト（仕様書 §15 M9 完了条件）。
//!
//! §15 M9 完了条件:「脅威モデルに基づくレッドチームテストで、悪意ある wasm が
//! 隣接テナント / ホスト / 未許可 outbound に到達できないことを確認する」。
//!
//! M9 は M5〜M8 と性質が違う。これまでは「機能を作って chaos で測る」だったが、M9 は
//! **攻撃側を書き、それが全部弾かれることを示す**。したがって各シナリオは
//! `chaos_v{n}_` 接頭辞（v = validation/vulnerability。M4=a/M5=e/M6=s/M7=t/M8=u の慣習を継ぐ）。
//!
//! ## スライスと現状
//!
//! - **v6（本ファイルで land 済み）**: 検証 DoS が control-plane を落とさない（M9b, §6.2）。
//! - v3/v4/v5（egress allowlist, M9c）/ v7（署名, M9a）/ v1/v2/v8（回帰ガード）は後続スライスで追加。
//!
//! ## v6 が検証すること（信頼境界の明示）
//!
//! M9b で検証は **別プロセス**に隔離された（`crates/control-plane/src/validation.rs` の
//! `spawn_validation_child`）。悪意ある wasm が wasmparser のメモリ / CPU を食い潰しても、
//! 被害は子プロセス 1 個に限局し control-plane 本体は生き続ける、というのが守るべき性質である。
//!
//! - **survival**: 不正な wasm を大量に投げても CP の `/readyz` が 200 を保つ。
//! - **isolation**: 各不正アップロードは 4xx（クライアントエラー）で返る。500 でも hang でもない。
//!   500 は「CP 側が壊れた」、hang は「検証が詰まった」を意味し、どちらも隔離の失敗である。
//! - **負の対照（重要）**: DoS を浴びせた**後**に、正常な wasm のアップロードが成功する。
//!   これが無いと「全部 4xx」を「安全」と誤読する（CP が半死で全部弾いていても緑になる）。
//!
//! **実効的なメモリ隔離（RLIMIT_AS）は Linux で効く**（macOS は RLIMIT_AS を無視するので
//! wall-clock timeout で縛る, §4.2）。本テストはプラットフォーム非依存な survival / isolation /
//! 負の対照を見る。rlimit の値そのものの検証は設計書 §4 とコード側で担保する。
//!
//! ## 実行方法
//! ```sh
//! docker compose up -d && make migrate
//! make run-cp > /tmp/cp.log 2>&1 &
//! make bootstrap && export CHAOS_TOKEN=$(make -s login)
//! make build-component && make deploy      # 正常 component（echo）を 1 つ用意
//! cargo test -p faas-control-plane --test chaos_m9 -- --ignored --test-threads=1
//! ```
//!
//! env: `CHAOS_BASE_URL`(既定 http://127.0.0.1:8080) / `CHAOS_TOKEN`(必須) /
//!      `CHAOS_ECHO`(既定 echo) / `CHAOS_ECHO_WASM`(既定 <workspace>/components-dist/echo.wasm) /
//!      `CHAOS_M9_DOS_COUNT`(既定 12: 同時に投げる不正アップロード数。検証セマフォ 4 を超える値)。
//!
//! **`--test-threads=1` で実行すること**: 検証セマフォ / 子プロセス数という**プロセス外の
//! 共有資源**を飽和させるため、並行実行すると互いの前提を壊す（chaos_m7/m8 と同型）。

#![allow(clippy::needless_return)]

use std::time::Duration;

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

fn dos_count() -> usize {
    std::env::var("CHAOS_M9_DOS_COUNT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(12)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        // 検証が hang したら「詰まった」ことをテストが検出できるよう、上限を短めに置く。
        // 子プロセスの timeout（既定 5s）+ 余裕。ここを無限にすると hang を見逃す。
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build reqwest client")
}

/// `GET /components` から対象 component の id を引く（素の配列）。
async fn resolve_component_id(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
) -> String {
    let comps: serde_json::Value = client
        .get(format!("{base}/components"))
        .bearer_auth(token)
        .send()
        .await
        .expect("list components send")
        .json()
        .await
        .expect("list components json");
    comps
        .as_array()
        .unwrap_or_else(|| panic!("GET /components must return an array; body={comps}"))
        .iter()
        .find(|c| c["name"].as_str() == Some(name))
        .and_then(|c| c["component_id"].as_str().or_else(|| c["id"].as_str()))
        .unwrap_or_else(|| {
            panic!("component '{name}' must exist (run `make deploy`); body={comps}")
        })
        .to_string()
}

async fn readyz(client: &reqwest::Client, base: &str) -> u16 {
    client
        .get(format!("{base}/readyz"))
        .send()
        .await
        .map(|r| r.status().as_u16())
        .unwrap_or(0)
}

/// 1 件の不正 wasm アップロードを投げ、HTTP ステータスを返す（本体は捨てる）。
async fn upload_bytes(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    cid: &str,
    version: &str,
    bytes: Vec<u8>,
) -> u16 {
    let form = reqwest::multipart::Form::new()
        .text("version", version.to_string())
        .part(
            "wasm",
            reqwest::multipart::Part::bytes(bytes).file_name("x.wasm"),
        );
    match client
        .post(format!("{base}/components/{cid}/versions"))
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
    {
        Ok(r) => r.status().as_u16(),
        // タイムアウト = 検証が hang した = 隔離の失敗。0 を返して assert 側で落とす。
        Err(_) => 0,
    }
}

// ============================================================================
// Scenario V6 — 検証 DoS が control-plane を落とさない（M9b, §6.2）
// ============================================================================

/// **Scenario V6**: 不正 wasm を検証セマフォ数を超えて同時に投げても、
/// (1) CP は生き続け（`/readyz` = 200）、(2) 各アップロードは 4xx で返り（500 でも hang でもない）、
/// (3) その後に正常な wasm のアップロードが成功する。
///
/// M9b 前（インプロセス検証）は、悪性 wasm が wasmparser のメモリを食い潰すと CP プロセスごと
/// 落ちる経路があった。M9b で検証を別プロセスへ隔離したので、被害は子プロセスに限局する。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: cargo test -p faas-control-plane --test chaos_m9 -- --ignored --test-threads=1 chaos_v6_"]
async fn chaos_v6_validation_dos_does_not_kill_cp() {
    let base = base_url();
    let token = token();
    let client = client();
    let echo = echo_component();

    // 前提: CP が生きていて、正常 component が 1 つある。
    assert_eq!(
        readyz(&client, &base).await,
        200,
        "前提: 開始時点で CP が ready であること（make run-cp を確認）"
    );
    let cid = resolve_component_id(&client, &base, &token, &echo).await;

    // --- (1) 不正 wasm を「検証セマフォ数を超えて」同時に投げる ---
    // 種類を混ぜる: (a) ただのゴミ (b) 正しい magic だが壊れた本体 (c) 巨大め（ただし
    // アップロード上限内）。どれも検証子プロセスで弾かれるはずで、CP は無傷であること。
    let n = dos_count();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let (c, b, t, cid) = (client.clone(), base.clone(), token.clone(), cid.clone());
        handles.push(tokio::spawn(async move {
            let bytes = match i % 3 {
                0 => vec![0xABu8; 4096], // ゴミ
                1 => {
                    // wasm magic + version だけ正しく、あとは壊れたバイト列。
                    let mut v = vec![0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];
                    v.extend(std::iter::repeat_n(0xFFu8, 8192));
                    v
                }
                _ => vec![0x00u8; 65536], // 大きめのゼロ埋め
            };
            upload_bytes(&c, &b, &t, &cid, &format!("6.6.{i}"), bytes).await
        }));
    }

    let mut statuses = Vec::with_capacity(n);
    for h in handles {
        statuses.push(h.await.expect("join upload"));
    }

    // --- (2) isolation: すべて 4xx。0（timeout=hang）も 5xx（CP 破損）もあってはならない ---
    for (i, &s) in statuses.iter().enumerate() {
        assert!(
            (400..500).contains(&s),
            "不正アップロード #{i} が {s} を返した（期待 4xx）。\n\
             0 = 検証が hang（隔離が詰まった） / 5xx = CP 側が壊れた、のいずれかで、\n\
             どちらも検証隔離（§6.2）の失敗を意味する。全ステータス: {statuses:?}"
        );
    }

    // --- (1 の確認) survival: DoS の最中〜直後も CP は ready ---
    assert_eq!(
        readyz(&client, &base).await,
        200,
        "検証 DoS の後で CP が ready でない。検証が本体プロセスを巻き込んで落ちた可能性がある\n\
         （M9b の別プロセス隔離が効いていない）"
    );

    // --- (3) 負の対照: DoS の後で正常な wasm のアップロードが成功する ---
    // これが無いと「全部 4xx」を「安全」と誤読する（CP が半死で全部弾いていても (2) は緑になる）。
    let wasm_path = std::env::var("CHAOS_ECHO_WASM").unwrap_or_else(|_| {
        format!(
            "{}/../../components-dist/echo.wasm",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    let wasm = std::fs::read(&wasm_path)
        .unwrap_or_else(|e| panic!("cannot read {wasm_path} (run `make build-component`): {e}"));
    let ok = upload_bytes(
        &client, &base, &token, &cid,
        // 既存と衝突しない version（衝突すると 400 になり負の対照にならない）。
        "6.6.900", wasm,
    )
    .await;
    assert!(
        ok == 201 || ok == 200 || ok == 409,
        "DoS の後で正常な echo.wasm のアップロードが失敗した（{ok}）。\n\
         検証パイプラインが DoS で壊れて正規の操作まで巻き添えにしている\n\
         （409 は version 既存＝別実行で作成済みで、これは許容）"
    );
}
