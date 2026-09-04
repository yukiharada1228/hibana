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
//! - **v6（M9b）**: 検証 DoS が control-plane を落とさない（§6.2）。
//! - **v1（M9c）**: filesystem は import できても実行時に全拒否される（deny-by-default の実測）。
//! - **v3/v4/v5（M9c）**: egress allowlist の実効強制（未承認拒否 / 承認のみ到達 / 内部 hard-deny）。
//! - **v7（M9a）**: 署名必須ポリシー下で、署名なし / 不正署名の wasm を拒否する（供給網検証, T6）。
//! - v2/v8（回帰ガード）は後続スライスで追加。
//!
//! ## v1/v3/v4/v5 が検証すること（信頼境界の明示）
//!
//! M9c で `wasi:sockets/*` / `wasi:filesystem/*` の **import は許可**するようになった（Rust std は
//! net-only の component でも filesystem を推移的に import するため）。**実際の到達可否はランタイムの
//! WasiCtx が決める**（M9 偵察の結論「真の防御は空の WasiCtx」）:
//! - fs: preopen を 1 つも与えない → どのパスも開けない（v1）。
//! - egress: admin 承認した allowlist を worker が解決した IP:port と、接続時の IP を照合。
//!   allowlist に無い宛先は拒否（v3/v4）。**SSRF ハードデニー（プライベート/メタデータ IP）が
//!   allowlist より優先**するので、承認名が内部 IP を指しても到達できない（v5）。
//!
//! **v4 は外向きネットワークを要する**（承認ホストへ実際に接続して「到達できる」ことを見る負の対照）。
//! `CHAOS_M9_EGRESS_TARGET`（既定 example.com:443）へ TCP 443 が通らない環境ではこの 1 本が落ちる。
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
//!      `CHAOS_M9_DOS_COUNT`(既定 12: 同時に投げる不正アップロード数。検証セマフォ 4 を超える値) /
//!      `CHAOS_NETPROBE`(既定 netprobe) / `CHAOS_NETPROBE_WASM`(既定 <workspace>/target/wasm32-wasip2/release/netprobe.wasm) /
//!      `CHAOS_M9_EGRESS_TARGET`(既定 example.com:443: v4 の到達確認先。外向き TCP を要する) /
//!      `CHAOS_M9_BLOCKED_TARGET`(既定 github.com:443: allowlist 外の宛先) /
//!      `CHAOS_M9_INTERNAL_TARGET`(既定 localhost:9000: 内部 IP に解決される承認候補。hard-deny 検証用)。
//!      egress テストは worker 1 台が起動している前提（`make run-workers N=1`）。
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
    // 「version 既存」は 400 で返る（409 ではない）ので、ステータスで冪等判定できない。
    // 未使用の version を選び、それでも失敗したら「既に在るか」を GET で確認する。
    let ok = upload_bytes(&client, &base, &token, &cid, "6.6.900", wasm).await;
    let uploaded_ok =
        ok == 201 || ok == 200 || version_exists(&client, &base, &token, &cid, "6.6.900").await;
    assert!(
        uploaded_ok,
        "DoS の後で正常な echo.wasm のアップロードが失敗した（{ok}）。\n\
         検証パイプラインが DoS で壊れて正規の操作まで巻き添えにしている"
    );
}

// ============================================================================
// M9c: egress allowlist + fs deny-by-default（v1 / v3 / v4 / v5）
// ============================================================================

fn netprobe_component() -> String {
    std::env::var("CHAOS_NETPROBE").unwrap_or_else(|_| "netprobe".into())
}

fn netprobe_wasm_path() -> String {
    std::env::var("CHAOS_NETPROBE_WASM").unwrap_or_else(|_| {
        format!(
            "{}/../../target/wasm32-wasip2/release/netprobe.wasm",
            env!("CARGO_MANIFEST_DIR")
        )
    })
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}

/// netprobe を（無ければ作って）アップロードし、component_id を返す。version は固定 "1.0.0"。
/// 既に存在すれば 409 を許容してそのまま使う。
async fn ensure_netprobe(client: &reqwest::Client, base: &str, token: &str) -> String {
    let name = netprobe_component();
    // component 行を用意（既存なら CP 側で冪等 / 409 になるが id は下で引く）。
    let _ = client
        .post(format!("{base}/components"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await;
    let cid = resolve_component_id(client, base, token, &name).await;

    // version 1.0.0 が既に在れば再アップロードしない（「version 既存」は 400 で返るため、
    // ステータスコードで冪等判定できない。先に存在を確認する）。
    if version_exists(client, base, token, &cid, "1.0.0").await {
        return cid;
    }

    let wasm_path = netprobe_wasm_path();
    let wasm = std::fs::read(&wasm_path).unwrap_or_else(|e| {
        panic!("cannot read {wasm_path} (run `cargo build -p netprobe --target wasm32-wasip2 --release`): {e}")
    });
    let status = upload_bytes(client, base, token, &cid, "1.0.0", wasm).await;
    // 別スレッド/別実行との競合で先に作られた場合も、存在すれば成功扱い。
    if status == 201 || version_exists(client, base, token, &cid, "1.0.0").await {
        return cid;
    }
    panic!(
        "netprobe のアップロードが失敗した（{status}）。sockets/filesystem の import が baseline で\n\
         許可されていない可能性がある（M9c で許可したはず）"
    );
}

/// component に指定 version が存在するか（`GET /components/{id}/versions`）。
async fn version_exists(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    cid: &str,
    version: &str,
) -> bool {
    let v: serde_json::Value = match client
        .get(format!("{base}/components/{cid}/versions"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(r) => r.json().await.unwrap_or(serde_json::Value::Null),
        Err(_) => return false,
    };
    v.as_array()
        .map(|a| {
            a.iter()
                .any(|e| e.get("version").and_then(|s| s.as_str()) == Some(version))
        })
        .unwrap_or(false)
}

/// egress allowlist を全置換で承認する（admin）。空配列は deny-all へ戻す。
async fn set_egress(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    cid: &str,
    allow: &[&str],
) -> u16 {
    client
        .put(format!(
            "{base}/components/{cid}/versions/1.0.0/capabilities/egress"
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({ "allow_outbound": allow }))
        .send()
        .await
        .map(|r| r.status().as_u16())
        .unwrap_or(0)
}

/// netprobe を同期 invoke し、`(net, fs)` の結果文字列を返す。
async fn probe(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    target: &str,
) -> (String, String) {
    let resp = client
        .post(format!("{base}/invoke?wait=1"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "component": netprobe_component(),
            "input": { "target": target },
        }))
        .send()
        .await
        .expect("invoke netprobe");
    let body = resp
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    assert_eq!(
        body.get("status").and_then(|s| s.as_str()),
        Some("succeeded"),
        "netprobe は succeeded で終端すること（connect の失敗はゲスト内で握って succeeded を返す設計）。\n\
         status != succeeded は worker 未起動 or sync timeout。body={body}"
    );
    let out = body
        .get("output")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let net = out
        .get("net")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let fs = out
        .get("fs")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (net, fs)
}

/// **Scenario V1**: filesystem は import できても実行時に全拒否される（deny-by-default の実測）。
///
/// M9c で `wasi:filesystem/*` の import は許可するようになった（std が推移的に焼き込むため）。
/// しかし worker の WasiCtx は preopen を 1 つも与えないので、ゲストはどのパスも開けない。
/// 「import は通るが実行時に fs は全拒否」を固定する（T1 の回帰ガード）。
#[tokio::test]
#[ignore = "chaos: requires stack + worker + netprobe; run: cargo test -p faas-control-plane --test chaos_m9 -- --ignored --test-threads=1 chaos_v1_"]
async fn chaos_v1_filesystem_denied_at_runtime() {
    let base = base_url();
    let token = token();
    let client = client();
    let cid = ensure_netprobe(&client, &base, &token).await;
    // egress は空（このテストは fs だけ見る）。
    set_egress(&client, &base, &token, &cid, &[]).await;

    let (_net, fs) = probe(&client, &base, &token, "").await;
    assert!(
        !fs.is_empty(),
        "netprobe が fs 結果を返していない（component が古い可能性）"
    );
    assert!(
        !fs.contains("OPENDIR_OK") && !fs.contains("READ_OK"),
        "filesystem がゲストから読めてしまっている: {fs}\n\
         WasiCtx が preopen を与えていない前提が崩れている（T1 の退行）"
    );
}

/// **Scenario V3**: egress 未承認（allowlist 空）の component は一切 outbound できない。
#[tokio::test]
#[ignore = "chaos: requires stack + worker + netprobe; run: cargo test -p faas-control-plane --test chaos_m9 -- --ignored --test-threads=1 chaos_v3_"]
async fn chaos_v3_unapproved_egress_denied() {
    let base = base_url();
    let token = token();
    let client = client();
    let cid = ensure_netprobe(&client, &base, &token).await;

    // allowlist を空へ（deny-all）。
    assert_eq!(
        set_egress(&client, &base, &token, &cid, &[]).await,
        200,
        "egress の空承認（deny-all へのリセット）が 200 を返すこと"
    );

    let target = env_or("CHAOS_M9_EGRESS_TARGET", "example.com:443");
    let (net, _fs) = probe(&client, &base, &token, &target).await;
    assert!(
        net.starts_with("denied"),
        "egress 未承認なのに {target} へ到達できた: net={net}（allowlist 空 = deny-all のはず）"
    );
}

/// **Scenario V4**: 承認 component は **allowlist の宛先にのみ**到達し、他は拒否される。
///
/// 到達できる側（負の対照）は外向きネットワークを要する。到達を殺していないことを示すため必須。
#[tokio::test]
#[ignore = "chaos: requires stack + worker + netprobe + outbound network; run: cargo test -p faas-control-plane --test chaos_m9 -- --ignored --test-threads=1 chaos_v4_"]
async fn chaos_v4_approved_egress_only_to_allowlist() {
    let base = base_url();
    let token = token();
    let client = client();
    let cid = ensure_netprobe(&client, &base, &token).await;

    let allowed = env_or("CHAOS_M9_EGRESS_TARGET", "example.com:443");
    let blocked = env_or("CHAOS_M9_BLOCKED_TARGET", "github.com:443");

    assert_eq!(
        set_egress(&client, &base, &token, &cid, &[allowed.as_str()]).await,
        200,
        "egress 承認が 200 を返すこと"
    );

    // 負の対照: 承認先には**到達できる**（egress 機構が全部殺していない証拠）。
    let (net_ok, _) = probe(&client, &base, &token, &allowed).await;
    assert_eq!(
        net_ok, "connected",
        "承認済み {allowed} へ到達できるべき（net={net_ok}）。\n\
         外向き TCP が通らない環境ではここが落ちる（CHAOS_M9_EGRESS_TARGET を到達可能な先に）"
    );

    // セキュリティ: allowlist 外は拒否される。
    let (net_denied, _) = probe(&client, &base, &token, &blocked).await;
    assert!(
        net_denied.starts_with("denied"),
        "allowlist 外の {blocked} へ到達できてしまった: net={net_denied}（§15 M9 違反）"
    );
}

/// **Scenario V5**: 承認名が内部 IP へ解決されても、SSRF ハードデニーが allowlist より優先して拒否する。
///
/// `localhost:9000`（= 127.0.0.1、MinIO が実際に居るポート）を**承認しても**到達できないことを見る。
/// これが DNS rebinding / SSRF に対する核の実測である。
#[tokio::test]
#[ignore = "chaos: requires stack + worker + netprobe; run: cargo test -p faas-control-plane --test chaos_m9 -- --ignored --test-threads=1 chaos_v5_"]
async fn chaos_v5_internal_target_denied_even_if_approved() {
    let base = base_url();
    let token = token();
    let client = client();
    let cid = ensure_netprobe(&client, &base, &token).await;

    let internal = env_or("CHAOS_M9_INTERNAL_TARGET", "localhost:9000");
    assert_eq!(
        set_egress(&client, &base, &token, &cid, &[internal.as_str()]).await,
        200,
        "内部ホストの承認自体は 200（承認はできるが到達はできない、が要点）"
    );

    // 承認名（localhost → 127.0.0.1）へは hard-deny で到達できない。
    let (net_name, _) = probe(&client, &base, &token, &internal).await;
    assert!(
        net_name.starts_with("denied") || net_name == "no-target",
        "承認された内部ホスト {internal} へ到達できてしまった: net={net_name}\n\
         SSRF ハードデニーが効いていない（クラウドメタデータ窃取の経路が開く）"
    );

    // IP リテラル直指定も拒否される（socket_addr_check の hard-deny）。
    let (net_ip, _) = probe(&client, &base, &token, "127.0.0.1:9000").await;
    assert!(
        net_ip.starts_with("denied"),
        "ループバック IP 直指定へ到達できてしまった: net={net_ip}"
    );
}

// ============================================================================
// M9a: 署名付き Component（v7）
// ============================================================================

/// echo.wasm のバイト列を読む（chaos_m7 と同じ既定パス解決）。
fn echo_wasm_bytes() -> Vec<u8> {
    let path = std::env::var("CHAOS_ECHO_WASM").unwrap_or_else(|_| {
        format!(
            "{}/../../components-dist/echo.wasm",
            env!("CARGO_MANIFEST_DIR")
        )
    });
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("cannot read {path} (run `make build-component`): {e}"))
}

/// wasm を（任意の署名付きで）アップロードし、HTTP ステータスを返す。
async fn upload_signed(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    cid: &str,
    version: &str,
    wasm: Vec<u8>,
    signature: Option<&str>,
) -> u16 {
    let mut form = reqwest::multipart::Form::new()
        .text("version", version.to_string())
        .part(
            "wasm",
            reqwest::multipart::Part::bytes(wasm).file_name("echo.wasm"),
        );
    if let Some(sig) = signature {
        form = form.text("signature", sig.to_string());
    }
    client
        .post(format!("{base}/components/{cid}/versions"))
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map(|r| r.status().as_u16())
        .unwrap_or(0)
}

/// **Scenario V7**: 署名必須ポリシー（require_signed_components=true）のテナントで、
/// (1) 署名なし → 拒否、(2) **正しい署名 → 成功（負の対照）**、(3) 不正署名 → 拒否。
///
/// 供給網汚染（T6）の防御: deploy トークンが漏れても、テナント登録鍵で署名された wasm でなければ
/// active にできない。負の対照（正しい署名は通る）が無いと「署名機能が全アップロードを殺している」
/// のを見逃す。
///
/// 後始末: ポリシーを false に戻し、登録鍵を retire する（他テストへの影響を残さない）。
#[tokio::test]
#[ignore = "chaos: requires docker compose stack + CHAOS_TOKEN + echo; run: cargo test -p faas-control-plane --test chaos_m9 -- --ignored --test-threads=1 chaos_v7_"]
async fn chaos_v7_unsigned_or_bad_signature_rejected() {
    use ed25519_dalek::{Signer as _, SigningKey};

    let base = base_url();
    let token = token();
    let client = client();
    let echo = echo_component();
    let cid = resolve_component_id(&client, &base, &token, &echo).await;

    // テスト専用の署名鍵（決定的 seed）。秘密鍵はテスト内だけに存在し、公開鍵を登録する。
    let sk = SigningKey::from_bytes(&[42u8; 32]);
    let pk_b64 = faas_shared::b64url_encode(sk.verifying_key().as_bytes());
    let key_id = "chaos-v7-key";

    // 署名鍵を登録。
    let reg = client
        .put(format!("{base}/admin/signing-keys/{key_id}"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "public_key": pk_b64 }))
        .send()
        .await
        .expect("register signing key");
    assert_eq!(reg.status().as_u16(), 200, "署名鍵の登録は 200 であること");

    // ポリシーを署名必須へ。
    let pol = client
        .put(format!("{base}/admin/signing-policy"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "require_signed_components": true }))
        .send()
        .await
        .expect("set signing policy");
    assert_eq!(pol.status().as_u16(), 200, "ポリシー設定は 200 であること");

    // 後始末は assert より前に必ず実行する（chaos の作法）。失敗しても他テストへ波及させない。
    // ここでは検証結果を貯めてから、末尾で後始末 → assert する。
    let wasm = echo_wasm_bytes();

    // 署名対象は wasm 本体の sha256（16 進文字列の UTF-8 バイト）。CP と同一規約。
    let sha_hex = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&wasm);
        h.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let good_sig = faas_shared::b64url_encode(&sk.sign(sha_hex.as_bytes()).to_bytes());
    // 別ダイジェストへの署名（本体に対しては不正）。
    let bad_sig = faas_shared::b64url_encode(&sk.sign(b"not the real digest").to_bytes());

    // 衝突しない version を選ぶ（署名検証の前に「version 既存」で 400 になると誤判定するため）。
    let base_ver = format!(
        "7.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );

    let unsigned = upload_signed(
        &client,
        &base,
        &token,
        &cid,
        &format!("{base_ver}.0"),
        wasm.clone(),
        None,
    )
    .await;
    let signed_ok = upload_signed(
        &client,
        &base,
        &token,
        &cid,
        &format!("{base_ver}.1"),
        wasm.clone(),
        Some(&good_sig),
    )
    .await;
    let bad_signed = upload_signed(
        &client,
        &base,
        &token,
        &cid,
        &format!("{base_ver}.2"),
        wasm.clone(),
        Some(&bad_sig),
    )
    .await;

    // --- 後始末（assert より前）: ポリシーを戻し鍵を retire する ---
    let _ = client
        .put(format!("{base}/admin/signing-policy"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "require_signed_components": false }))
        .send()
        .await;
    let _ = client
        .delete(format!("{base}/admin/signing-keys/{key_id}"))
        .bearer_auth(&token)
        .send()
        .await;

    // --- assert ---
    assert!(
        (400..500).contains(&unsigned),
        "署名必須なのに署名なしアップロードが {unsigned}（4xx 拒否を期待）。\n\
         deploy トークンだけで任意 wasm を active にできる = 供給網汚染（T6）を防げていない"
    );
    // 負の対照: 正しい署名は通る。これが無いと「全部拒否」を安全と誤読する。
    assert_eq!(
        signed_ok, 201,
        "正しい署名付きアップロードが {signed_ok}（201 を期待）。\n\
         署名検証が正規のデプロイまで殺している（負の対照が落ちた）"
    );
    assert!(
        (400..500).contains(&bad_signed),
        "不正署名のアップロードが {bad_signed}（4xx 拒否を期待）。\n\
         署名が付いていれば内容と一致するか必ず検証すること"
    );
}
