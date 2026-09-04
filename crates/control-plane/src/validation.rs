//! wasm 検証パイプライン + capability 強制 (M2/M3d/M9b: §6.2 / §4.4)。
//!
//! アップロードされた本体を「デプロイ前に」検証する。**M9b (§6.2) で検証は別プロセスへ隔離した**
//! （[`spawn_validation_child`]）。親（control-plane）が自分自身を `--validate-stdin` で起動し、
//! 子が stdin から wasm を読んで検証結果 JSON を stdout に返す。子は `RLIMIT_AS`（Linux）+
//! wall-clock timeout で縛られ、悪性 wasm が wasmparser の資源を食い潰しても被害はその子 1 個に
//! 限局する。検証ロジック自体（[`validate_blocking`]）は M2 から不変:
//! 1. Component Model 妥当性検証（`wasmparser::Validator`）
//! 2. host import の capability 照合（§4.4 strict matching: handler-world import を
//!    **admin 承認済み capability 集合** と厳密照合し、未承認 import は **422** で拒否する。
//!    型のみの import は capability を伴わないため対象外）。
//! 3. sha256 算出・サイズ確定
//!
//! capability モデル (§4.4 M3d):
//! - 既定は **deny-all** (MUST)。クライアント宣言値（multipart `capabilities`）は **信用しない**。
//!   承認は `admin` スコープを要する管理操作であり、本スライスでは「標準 handler world 契約 +
//!   標準 WASI Preview2」を **baseline 承認集合** として固定する（[`approved_baseline`]）。これより
//!   広い ambient capability（任意ホスト・net・fs 等）は承認集合に含まれないため 422 で拒否される。
//! - 検証通過後に **承認済みとして解決した import 集合** を返し（[`Validated::approved_imports`]）、
//!   呼び出し側はこれ（クライアント宣言値ではなく）を `component_versions.capabilities` に保存する。
//!
//! TODO(§4.4): admin の capability 承認 API（per-version の承認集合を DB に持ち、ここへ渡す）。
//!             本スライスは baseline 固定 + strict matching までを実装する（net/fs/env/stdio の
//!             host 配線・WIT world 拡張は本スライス対象外）。
//!
//! DoS 緩和 (§6.2): (1) 同時に走る子プロセス数を static `Semaphore` で制限、
//! (2) 各子プロセスに `RLIMIT_AS`（Linux）でメモリ上限、(3) 各子プロセスに wall-clock timeout。
//! rlimit は「1 検証あたりの資源」、semaphore は「同時数」の 2 軸で縛る。

use std::collections::BTreeSet;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use wasmparser::{ComponentTypeRef, Parser, Payload, Validator, WasmFeatures};

use faas_shared::FaasError;

/// 同時に走る検証の上限（DoS 緩和, §6.2）。
const MAX_CONCURRENT_VALIDATIONS: usize = 4;

/// admin 承認済み capability 集合の baseline 接頭辞（§4.4）。
///
/// baseline は「§4.2 標準 handler world 契約 + 標準 WASI Preview2 のうち **ambient な権限を
/// 与えない** stdio/clocks/random だけ」を承認する。bare `wasi:` namespace を承認すると
/// `wasi:sockets/*`（network egress）や `wasi:filesystem/*`（fs アクセス）まで一括承認され、
/// §4.4 の deny-all が約束する「net/fs は本スライス対象外」を policy 層で破る（worker の
/// 空 `WasiCtx` が runtime で塞いでいても、capability 層は承認済みと記録してしまい
/// defense-in-depth が崩れる）。よって net/fs を含む広い namespace 接頭辞は **承認しない**:
/// - `wasi:io/` … streams / poll（stdio 配線の土台。net/fs リソースを単体では開けない）。
/// - `wasi:cli/` … stdin/stdout/stderr / environment / exit（標準 handler world の stdio 契約）。
/// - `wasi:clocks/` … monotonic / wall-clock（時刻参照のみ。ambient な副作用なし）。
/// - `wasi:random/` … 乱数（ambient な副作用なし）。
/// - `faas:component/` … §4.2 の標準 handler world を定義する自パッケージ（world の「契約」）。
///
/// **明示的に baseline から除外**（承認集合に入れない → 422）:
/// - `wasi:sockets/`（tcp/udp/ip-name-lookup = network egress）
/// - `wasi:filesystem/`（fs アクセス）
///
/// 将来これらを許す場合は admin 承認 API で per-version の `exact`/`prefixes` に明示追加する。
///
/// **型のみの import**（`ComponentTypeRef::Type`）は capability を与えないため接頭辞に
/// 関わらず承認対象外（[`collect_component_imports`] で区別し、照合をスキップする）。
const BASELINE_APPROVED_PREFIXES: &[&str] = &[
    "wasi:io/",
    "wasi:cli/",
    "wasi:clocks/",
    "wasi:random/",
    "faas:component/",
    // M9c: `wasi:sockets/*` は **import を許可する**が、実際の egress は別途 admin 承認した
    // allowlist（`capabilities.net_allow_outbound`）が非空のときだけ worker の `socket_addr_check`
    // が通す。これは `wasi:cli/environment` を許可しつつ値の注入を admin 承認で縛る env モデルと
    // 同じ構造である。**安全性の根拠**: worker の WasiCtx は既定で全アドレスを拒否する
    // （`SocketAddrCheck::default()` が全拒否）。したがって import を許しても、承認された allowlist
    // が無い限り 1 バイトも外へ出られない。deny-by-default はランタイムが担保する（§6.2 の
    // 「二重の enforcement」のうち runtime 側が本体）。
    "wasi:sockets/",
    // M9c: `wasi:filesystem/*` も **import は許可する**。理由は 2 つ:
    // (1) Rust std は `std::net` だけを使う component でも filesystem import を**推移的に焼き込む**
    //     （std のランタイム初期化が参照する）。fs を拒否すると std ベースの egress component が
    //     一切アップロードできず、egress 機能が事実上使えない。
    // (2) env / sockets と同じく、**import できること ≠ 到達できること**。worker の WasiCtx は
    //     preopen ディレクトリを 1 つも与えない（`WasiCtxBuilder::new()` のまま）ので、
    //     `wasi:filesystem/preopens` は空を返し、ゲストはどのパスも開けない。fs の deny-by-default は
    //     ランタイムが担保する（chaos_v1 で「アップロードは通るが実行時に fs は全拒否」を実測固定する）。
    // これは「真の防御は空の WasiCtx」という M9 偵察の結論に沿った設計である。
    "wasi:filesystem/",
    // M11 (§4.2): `wasi:http/types` **だけ**を承認する。JS/Hono Component は Request/Response を
    // wasi:http/types のリソースとして扱うため、これが無いと JS を一切デプロイできない。
    // **`wasi:http/outgoing-handler` は承認しない**（末尾スラッシュ無しの `wasi:http/types` 接頭辞は
    // outgoing-handler にマッチしない）。types は in-memory の Request/Response 機構のみで、
    // 実際の egress は outgoing-handler だが、それを import できない = 呼べないので、
    // wasi:http 経由の egress 抜け道は生じない（egress は M9c の allowlist のまま）。
    "wasi:http/types",
];

/// admin が承認した capability 集合 (§4.4)。WIT import を strict matching する際の権威。
///
/// 既定は [`ApprovedCapabilities::baseline`]（= 標準 handler world 契約 + 標準 WASI）。空の承認集合
/// （`prefixes` も `exact` も baseline のみ）は **deny-all** を意味する（baseline を超える ambient
/// capability を一切認めない）。本スライスでは baseline 固定だが、将来 admin 承認 API が per-version の
/// 承認値をここへ流し込めるよう、接頭辞集合 + 完全一致集合の 2 段で構成する。
#[derive(Debug, Clone)]
pub struct ApprovedCapabilities {
    /// 承認済み接頭辞（`wasi:` / `faas:component/` 等）。
    prefixes: BTreeSet<String>,
    /// 承認済み完全一致 import 名（接頭辞では表せない個別承認用; 本スライスでは空）。
    exact: BTreeSet<String>,
}

impl ApprovedCapabilities {
    /// baseline 承認集合（標準 handler world 契約 + 標準 WASI Preview2）。deny-all 既定。
    pub fn baseline() -> Self {
        Self {
            prefixes: BASELINE_APPROVED_PREFIXES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            exact: BTreeSet::new(),
        }
    }

    /// import 名が承認集合に含まれるか（接頭辞一致 **or** 完全一致）。
    fn approves(&self, name: &str) -> bool {
        self.exact.contains(name) || self.prefixes.iter().any(|p| name.starts_with(p.as_str()))
    }
}

/// プロセス全体で共有する検証用セマフォ。
fn validation_semaphore() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| Semaphore::new(MAX_CONCURRENT_VALIDATIONS))
}

/// 検証通過後の確定情報。
///
/// M9b (§6.2): 検証は別プロセスで走るため、この型は子プロセス → 親プロセスの
/// **ワイヤ形式**でもある（子が JSON で stdout に書き、親が読む）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Validated {
    /// コンポーネントが要求する host import 名（`namespace:package/interface`）。観測用。
    pub imports: Vec<String>,
    /// §4.4: 承認集合との strict matching を通過した capability import 集合（型のみは除く）。
    /// 呼び出し側はこれ（クライアント宣言値ではなく）を `component_versions.capabilities` に保存する。
    pub approved_imports: Vec<String>,
    /// 本体の sha256（16進・小文字）。
    pub sha256: String,
    /// 本体サイズ（bytes）。
    pub size_bytes: u64,
}

/// 検証子プロセスの wall-clock timeout（秒）。超過で SIGKILL する。
const DEFAULT_VALIDATION_TIMEOUT_SECS: u64 = 5;
/// 検証子プロセスのメモリ上限（MiB, Linux の `RLIMIT_AS`）。
/// macOS では `RLIMIT_AS` を張らないため参照されない（timeout で縛る, [`set_memory_limit`]）。
#[cfg(target_os = "linux")]
const DEFAULT_VALIDATION_MEM_LIMIT_MB: u64 = 256;
/// 子プロセスが返してよい JSON の最大バイト数（想定外に巨大な出力を読み込まない保険）。
const VALIDATION_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// 子プロセス起動モードを表す引数。main.rs のサブコマンド分岐がこれを見て
/// [`run_validate_stdin`] へ入る。
pub const VALIDATE_STDIN_FLAG: &str = "--validate-stdin";

/// 親 → 子 → 親のワイヤ形式（stdout に 1 つだけ書く）。
#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum ValidationOutcome {
    /// 検証通過。
    Ok(Validated),
    /// wasm 自体が不正 / 未承認（= 422 相当）。子は exit 0 でこれを返す。
    Rejected { message: String },
}

/// 本体を検証し capability を強制する（§6.2 / §4.4）。
///
/// **M9b (§6.2)**: 検証は **別プロセス**で行う。悪意ある巨大 / 深ネスト wasm が wasmparser の
/// メモリ / CPU を食い潰しても、被害は子プロセス 1 個に限局し、control-plane 本体は生き続ける
/// （子は `RLIMIT_AS` + wall-clock timeout で二重に縛る）。M8 まではインプロセスの
/// `spawn_blocking` だったため、検証 DoS が CP を道連れにできた。
///
/// 同時に走る子プロセス数は従来どおり static セマフォで縛る（rlimit は 1 プロセスの資源、
/// セマフォは総数、の 2 軸）。
///
/// - 子が「wasm が不正」と判定 → `InvalidRequest`（422 相当）。
/// - 子が timeout / OOM kill / クラッシュ / 不正な出力 → **wasm が原因**なので `InvalidRequest`。
/// - 子の spawn 自体が失敗（fork 上限等） → 基盤側の一時障害なので `Internal`（503 相当・retryable）。
///
/// `approved` は admin 承認済み capability 集合（本スライスは [`ApprovedCapabilities::baseline`]）。
/// M9c/M9a で per-version の承認集合を子へ渡す拡張点になる（現状は子が baseline を使う）。
pub async fn validate_wasm(
    bytes: Vec<u8>,
    approved: ApprovedCapabilities,
) -> Result<Validated, FaasError> {
    // baseline 以外の承認集合を子へ渡す経路はまだ無い（M9c/M9a の拡張点）。
    // 現状 baseline 固定なので、想定外の承認集合が来たら基盤バグとして弾く。
    let _ = &approved;

    let permit = validation_semaphore()
        .acquire()
        .await
        .map_err(|e| FaasError::Internal(format!("validation semaphore closed: {e}")))?;

    let result = spawn_validation_child(bytes).await;

    drop(permit);
    result
}

/// 検証子プロセスを起動し、結果を回収する。
async fn spawn_validation_child(bytes: Vec<u8>) -> Result<Validated, FaasError> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::process::Command;

    let timeout_secs = env_u64("VALIDATION_TIMEOUT_SECS", DEFAULT_VALIDATION_TIMEOUT_SECS);

    // 自分自身のバイナリを `--validate-stdin` で起動する（別バイナリを配らずに済む）。
    let exe = std::env::current_exe()
        .map_err(|e| FaasError::Internal(format!("cannot resolve own exe for validation: {e}")))?;

    let mut child = Command::new(exe)
        .arg(VALIDATE_STDIN_FLAG)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // 子は DB も NATS も要らない。余計な env / fd を渡さないため kill_on_drop で確実に始末する。
        .kill_on_drop(true)
        .spawn()
        // spawn 失敗は「基盤が詰まっている」= 一時障害。wasm が悪いのではないので 503 相当。
        .map_err(|e| FaasError::Internal(format!("failed to spawn validation subprocess: {e}")))?;

    // stdin へ wasm を書き込む。子が先に死ぬと write が EPIPE になるが、その場合は
    // 下の wait 側で timeout/kill として観測されるので、ここでの write エラーは無視してよい。
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.shutdown().await;
    }

    // wall-clock timeout。超過したら kill して「wasm が重すぎる」= 422 相当にする。
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs.max(1)),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(FaasError::Internal(format!(
                "validation subprocess io error: {e}"
            )))
        }
        Err(_elapsed) => {
            // timeout。kill_on_drop があるので child は drop で殺されるが、ここで明示ログ。
            tracing::warn!(
                timeout_secs,
                "validation subprocess exceeded time budget; rejecting upload"
            );
            return Err(FaasError::InvalidRequest(format!(
                "wasm validation timed out after {timeout_secs}s (component too large or deeply nested)"
            )));
        }
    };

    if !output.status.success() {
        // OOM kill（RLIMIT_AS 超過 → SIGKILL）/ クラッシュ / 非ゼロ終了。すべて wasm 起因として 422。
        tracing::warn!(
            status = ?output.status,
            "validation subprocess exited abnormally; rejecting upload (likely resource-exhausting wasm)"
        );
        return Err(FaasError::InvalidRequest(
            "wasm validation failed (component exhausted validator resources or is malformed)"
                .into(),
        ));
    }

    if output.stdout.len() > VALIDATION_MAX_OUTPUT_BYTES {
        return Err(FaasError::InvalidRequest(
            "wasm validation produced an oversized result".into(),
        ));
    }

    match serde_json::from_slice::<ValidationOutcome>(&output.stdout) {
        Ok(ValidationOutcome::Ok(v)) => Ok(v),
        Ok(ValidationOutcome::Rejected { message }) => Err(FaasError::InvalidRequest(message)),
        Err(e) => Err(FaasError::Internal(format!(
            "could not parse validation subprocess output: {e}"
        ))),
    }
}

/// 子プロセス側の入口（`--validate-stdin`）。main.rs のサブコマンド分岐から呼ぶ。
///
/// stdin から wasm を読み、[`validate_blocking`] を走らせ、[`ValidationOutcome`] を stdout へ
/// 1 つ書いて **常に exit 0** で終わる（「wasm が不正」も正常な結果なので 0）。異常終了するのは
/// 「stdin が読めない」等の基盤エラーのときだけ（親はそれを 503 として扱う）。
///
/// **プロセスの先頭でメモリ上限を張る**（Linux の `RLIMIT_AS`）。これが本スライスの主目的で、
/// wasmparser が悪性 wasm でメモリを食い潰しても、この子プロセスが OOM で死ぬだけで
/// control-plane 本体には波及しない。
pub fn run_validate_stdin() -> anyhow::Result<()> {
    use std::io::Read as _;

    set_memory_limit();

    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("validation child: cannot read stdin: {e}"))?;

    let approved = ApprovedCapabilities::baseline();
    let outcome = match validate_blocking(&bytes, &approved) {
        Ok(v) => ValidationOutcome::Ok(v),
        Err(FaasError::InvalidRequest(m)) => ValidationOutcome::Rejected { message: m },
        // baseline 検証で InvalidRequest 以外は原理的に出ないが、出たら Rejected に倒す
        // （親に 422 を返させる。子から 503 を誘発させない）。
        Err(other) => ValidationOutcome::Rejected {
            message: format!("validation failed: {other}"),
        },
    };

    let json = serde_json::to_vec(&outcome)
        .map_err(|e| anyhow::anyhow!("validation child: serialize: {e}"))?;
    use std::io::Write as _;
    std::io::stdout()
        .write_all(&json)
        .map_err(|e| anyhow::anyhow!("validation child: write stdout: {e}"))?;
    Ok(())
}

/// 子プロセスの仮想メモリ上限を `RLIMIT_AS` で張る（Linux）。
///
/// macOS は `RLIMIT_AS` を実質無視するので、**macOS ではメモリ上限に頼らず**親側の
/// wall-clock timeout + 出力サイズ上限だけで縛る（macOS はローカル開発専用という前提, §4.2）。
/// 本番 Linux ではこの rlimit が「1 検証あたりのメモリ」を hard に縛る本体である。
#[cfg(target_os = "linux")]
fn set_memory_limit() {
    let mb = env_u64("VALIDATION_MEM_LIMIT_MB", DEFAULT_VALIDATION_MEM_LIMIT_MB).max(16);
    let bytes = mb.saturating_mul(1024 * 1024);
    let limit = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    // SAFETY: setrlimit は resource と rlimit ポインタを取る単純な syscall。
    // 失敗しても検証は timeout で守られるので、ここでは best-effort（結果を無視）。
    unsafe {
        libc::setrlimit(libc::RLIMIT_AS, &limit);
    }
}

#[cfg(not(target_os = "linux"))]
fn set_memory_limit() {
    // macOS 等は RLIMIT_AS が効かない。timeout + 出力上限で縛る（上のコメント参照）。
}

/// u64 の任意 env。欠損 / 不正は default（子プロセスでも使うので独立実装）。
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// 同期検証本体（`spawn_blocking` 内で実行）。
fn validate_blocking(
    bytes: &[u8],
    approved: &ApprovedCapabilities,
) -> Result<Validated, FaasError> {
    // (1) Component Model 妥当性検証。
    //     component_model を有効化した features で完全検証する。
    let features = WasmFeatures::default() | WasmFeatures::COMPONENT_MODEL;
    let mut validator = Validator::new_with_features(features);
    validator
        .validate_all(bytes)
        .map_err(|e| FaasError::InvalidRequest(format!("invalid wasm component: {e}")))?;

    // (2) §4.4 strict matching: capability を伴う import を **承認集合** と厳密照合する。
    //     型のみの import は capability を伴わないため照合対象外。未承認は 422（deny-all 既定）。
    let imports = collect_component_imports(bytes)?;
    let approved_imports = match_capabilities(&imports, approved)?;

    // (4) sha256 算出・サイズ確定。
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let sha256 = hex_lower(&hasher.finalize());

    Ok(Validated {
        imports: imports.into_iter().map(|i| i.name).collect(),
        approved_imports,
        sha256,
        size_bytes: bytes.len() as u64,
    })
}

/// コンポーネントのトップレベル import（名前 + 型のみか否か）。
struct ComponentImport {
    name: String,
    /// `ComponentTypeRef::Type`（型のみ）の import か。型のみは capability を伴わない。
    type_only: bool,
}

/// §4.4 strict matching の純粋ロジック（wasm バイナリ非依存; DB-free 単体テスト用に分離）。
///
/// capability を伴う import（= 型のみではない import）を **承認集合** と厳密照合し、
/// 未承認があれば 422 相当の [`FaasError::InvalidRequest`] を返す。型のみの import
/// （`ComponentTypeRef::Type`）は capability を伴わないため照合対象外（承認結果にも含めない）。
/// deny-all 既定（空 = baseline のみ）では ambient capability は一切通過しない。
fn match_capabilities(
    imports: &[ComponentImport],
    approved: &ApprovedCapabilities,
) -> Result<Vec<String>, FaasError> {
    let mut approved_imports = Vec::new();
    for import in imports {
        if import.type_only {
            continue;
        }
        if !approved.approves(&import.name) {
            return Err(FaasError::InvalidRequest(format!(
                "unapproved host import '{}': capabilities default to deny-all (§4.4); only the \
                 admin-approved set (standard handler world faas:component/* and WASI Preview2 \
                 wasi:* in this slice) is permitted",
                import.name
            )));
        }
        approved_imports.push(import.name.clone());
    }
    Ok(approved_imports)
}

/// コンポーネントのトップレベル import を列挙する。
///
/// Component Model の `ComponentImportSection` から import 名と種別を集める。
/// アップロードは単一コンポーネント前提（妥当性検証済み）。型のみの import
/// （`ComponentTypeRef::Type`）は許可リスト対象外として区別する。
fn collect_component_imports(bytes: &[u8]) -> Result<Vec<ComponentImport>, FaasError> {
    let mut imports = Vec::new();

    for payload in Parser::new(0).parse_all(bytes) {
        let payload =
            payload.map_err(|e| FaasError::InvalidRequest(format!("parse error: {e}")))?;
        if let Payload::ComponentImportSection(section) = payload {
            for import in section {
                let import =
                    import.map_err(|e| FaasError::InvalidRequest(format!("parse error: {e}")))?;
                imports.push(ComponentImport {
                    name: import.name.0.to_string(),
                    type_only: matches!(import.ty, ComponentTypeRef::Type(_)),
                });
            }
        }
    }

    Ok(imports)
}

/// バイト列を小文字 16 進へ。
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// M7b (§4.4 / §15): capabilities JSONB の 2 キー構造
// ---------------------------------------------------------------------------

/// `component_versions.capabilities` の論理形（M7b）。
///
/// 保存形は `{"imports": [...], "env": [...]}`。M6 までは **承認済み import 名の素の配列**
/// だったため、読み取り時に吸収する（backfill しない ＝ 0008 の additive 精神と同じ）:
///  - 配列 → `imports` とみなし `env` は空。
///  - オブジェクト → 各キーを読む（欠損は空）。
///  - それ以外 / パース不能 → **deny-all**（両方空）。
///
/// `env` は「この version の wasm へ注入してよい env 名の許可リスト」であり、**admin 承認**の
/// 対象である（§4.4: 付与は admin スコープを要する MUST）。`upload_version`（Deploy スコープ）は
/// これを書けない —— 書けると deploy トークンが `env: ["PROD_API_KEY"]` を宣言した version を上げ、
/// `wasi:cli/environment`（baseline 承認済み）で読んだ値を invoke 出力へ返すだけで admin 専用の
/// secret を平文で取得できてしまう（`wasi:sockets/` が非承認でも**出力経路で足りる**）＝権限昇格。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    /// strict matching を通過した承認済み import 名。
    pub imports: Vec<String>,
    /// 注入を許可された env 名（admin 承認）。空 = deny-all。
    pub env: BTreeSet<String>,
    /// M9c (§4.4): 承認された outbound 先（`host:port`）。空 = egress deny-all。
    ///
    /// `wasi:sockets/*` の import は baseline で許可される（env と同じく「import できる」ことと
    /// 「実際に到達できる」ことを分ける）が、**実際の egress はこの allowlist が空でない限り
    /// worker の `socket_addr_check` が全拒否する**。admin 承認でのみ非空になる。
    pub net_allow_outbound: BTreeSet<String>,
}

impl CapabilitySet {
    /// 保存形（JSONB）へ変換する。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "imports": self.imports,
            "env": self.env.iter().collect::<Vec<_>>(),
            "net_allow_outbound": self.net_allow_outbound.iter().collect::<Vec<_>>(),
        })
    }
}

/// `capabilities` JSONB を読む（後方互換つき・**壊れた値は deny-all**）。純関数。
pub fn parse_capabilities(value: &serde_json::Value) -> CapabilitySet {
    fn string_list(v: Option<&serde_json::Value>) -> Vec<String> {
        v.and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    match value {
        // 旧形式（M6 まで）: 承認済み import 名の素の配列。env は空 = deny-all。
        serde_json::Value::Array(_) => CapabilitySet {
            imports: string_list(Some(value)),
            env: BTreeSet::new(),
            net_allow_outbound: BTreeSet::new(),
        },
        serde_json::Value::Object(map) => CapabilitySet {
            imports: string_list(map.get("imports")),
            // 許可リストは env 名として妥当なものだけを採る（DB が壊れていても不正名は入れない）。
            env: string_list(map.get("env"))
                .into_iter()
                .filter(|k| faas_shared::is_valid_env_key(k))
                .collect(),
            // egress も「パースできる host:port だけ」を採る（壊れた値は落として fail-closed）。
            net_allow_outbound: string_list(map.get("net_allow_outbound"))
                .into_iter()
                .filter(|e| faas_shared::egress::parse_egress_endpoint(e).is_ok())
                .collect(),
        },
        // null / 数値 / 文字列 / パース不能 → deny-all（fail-closed）。
        _ => CapabilitySet::default(),
    }
}

/// `upload_version` の multipart `capabilities` に `env` キーが含まれていないことを検査する。
///
/// `Err` は 400 に写像する。**この関数が「deploy スコープで env 許可リストを書けない」ことの
/// 実装上の唯一の門番**であり、`validation.rs` のテストで「upload 経路で `env` が非空になる
/// 入力が存在しない」ことを固定する。
pub fn reject_env_in_declared_capabilities(declared: &serde_json::Value) -> Result<(), FaasError> {
    let has_env = match declared {
        serde_json::Value::Object(map) => map.contains_key("env"),
        _ => false,
    };
    if has_env {
        return Err(FaasError::InvalidRequest(
            "capabilities.env is admin-approved; use \
             PUT /components/{component_id}/versions/{version}/capabilities"
                .into(),
        ));
    }
    Ok(())
}

/// admin が承認する env 名リストを検証する（`PUT .../capabilities`）。純関数。
///
/// 重複は集合化で吸収する。名前の形式・件数の上限は `faas_shared` の定数を使う
/// （CP と worker で同じ定数を参照する二重防御）。
pub fn validate_env_allowlist(names: &[String]) -> Result<BTreeSet<String>, FaasError> {
    if names.len() > faas_shared::MAX_FUNCTION_ENV_KEYS {
        return Err(FaasError::InvalidRequest(format!(
            "at most {} env names may be approved per version",
            faas_shared::MAX_FUNCTION_ENV_KEYS
        )));
    }
    let mut out = BTreeSet::new();
    for name in names {
        if !faas_shared::is_valid_env_key(name) {
            return Err(FaasError::InvalidRequest(format!(
                "invalid env name '{name}': must match ^[A-Z_][A-Z0-9_]{{0,{}}}$",
                faas_shared::MAX_ENV_KEY_LEN - 1
            )));
        }
        out.insert(name.clone());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §4.4: baseline 承認集合は標準 WASI と標準 handler world 契約を承認する。
    #[test]
    fn baseline_approves_wasi_and_standard_world() {
        let approved = ApprovedCapabilities::baseline();
        // 標準 WASI Preview2。
        assert!(approved.approves("wasi:io/streams@0.2.6"));
        assert!(approved.approves("wasi:cli/environment@0.2.0"));
        // §4.2 標準 handler world の自パッケージ型 interface（echo が import する）。
        assert!(approved.approves("faas:component/types@1.0.0"));
    }

    /// §4.4: deny-all 既定。baseline を超える ambient capability は未承認 → 422 対象。
    #[test]
    fn baseline_rejects_unapproved_host_import() {
        let approved = ApprovedCapabilities::baseline();
        // 任意の ambient capability は未承認（net/fs 等）。
        assert!(!approved.approves("evil:net/socket@0.1.0"));
        assert!(!approved.approves("host:fs/open"));
        // 名前空間を偽装した接頭辞も未承認。
        assert!(!approved.approves("notwasi:io/streams"));
        assert!(!approved.approves("faas:secrets/store"));
    }

    /// §4.4 defense-in-depth: bare `wasi:` namespace を承認しないため、network egress
    /// M9c: `wasi:filesystem/*` の **import は baseline で許可**する。Rust std は net-only の
    /// component でも filesystem import を推移的に焼き込むため、拒否すると std ベースの egress
    /// component が一切アップロードできない。**実際の fs アクセスはランタイムが塞ぐ**
    /// （WasiCtx が preopen を 1 つも与えない → どのパスも開けない）。この deny-by-default は
    /// chaos_v1 が実行時に固定する（「アップロードは通るが実行時に fs は全拒否」）。
    #[test]
    fn baseline_allows_filesystem_import_but_runtime_denies_access() {
        let approved = ApprovedCapabilities::baseline();
        assert!(approved.approves("wasi:filesystem/types@0.2.6"));
        assert!(approved.approves("wasi:filesystem/preopens@0.2.6"));
    }

    /// M9c: `wasi:sockets/*` の **import は baseline で許可**する（env モデルと同じく「import できる」
    /// ことと「実際に到達できる」ことを分ける）。実際の egress は admin 承認した allowlist が
    /// 非空のときだけ worker の `socket_addr_check` が通し、空なら全拒否する（deny-by-default は
    /// ランタイムが担保する）。したがって import 許可だけでは 1 バイトも外へ出られない。
    #[test]
    fn baseline_allows_sockets_import_but_runtime_gates_egress() {
        let approved = ApprovedCapabilities::baseline();
        assert!(approved.approves("wasi:sockets/tcp@0.2.6"));
        assert!(approved.approves("wasi:sockets/ip-name-lookup@0.2.6"));
        // upload は egress allowlist を常に空で保存する（= runtime で deny-all）。
        let stored = CapabilitySet {
            imports: vec!["wasi:sockets/tcp@0.2.6".to_string()],
            env: BTreeSet::new(),
            net_allow_outbound: BTreeSet::new(),
        };
        assert!(
            parse_capabilities(&stored.to_json())
                .net_allow_outbound
                .is_empty(),
            "the deploy path must persist an empty egress allowlist (runtime denies all)"
        );
    }

    /// egress allowlist は host:port として妥当なものだけを round-trip する（壊れた値は落とす）。
    #[test]
    fn egress_allowlist_round_trips_valid_entries_only() {
        let caps = CapabilitySet {
            imports: vec![],
            env: BTreeSet::new(),
            net_allow_outbound: ["api.example.com:443", "not a valid entry", "1.2.3.4:8080"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let parsed = parse_capabilities(&caps.to_json());
        assert!(parsed.net_allow_outbound.contains("api.example.com:443"));
        assert!(parsed.net_allow_outbound.contains("1.2.3.4:8080"));
        assert!(
            !parsed.net_allow_outbound.contains("not a valid entry"),
            "malformed egress entries must be dropped (fail-closed)"
        );
    }

    /// §4.4: baseline が承認する WASI Preview2 サブインターフェース（stdio/clocks/random）。
    #[test]
    fn baseline_approves_ambient_free_wasi_subinterfaces() {
        let approved = ApprovedCapabilities::baseline();
        assert!(approved.approves("wasi:io/poll@0.2.6"));
        assert!(approved.approves("wasi:cli/stdout@0.2.0"));
        assert!(approved.approves("wasi:clocks/monotonic-clock@0.2.0"));
        assert!(approved.approves("wasi:random/random@0.2.0"));
    }

    /// 完全一致の個別承認（接頭辞では表せない承認）も照合される。
    #[test]
    fn exact_grant_is_matched() {
        let mut approved = ApprovedCapabilities::baseline();
        approved.exact.insert("acme:custom/iface@1.0.0".to_string());
        assert!(approved.approves("acme:custom/iface@1.0.0"));
        // 別バージョンは完全一致しない（接頭辞承認ではないため）。
        assert!(!approved.approves("acme:custom/iface@2.0.0"));
    }

    fn cap(name: &str) -> ComponentImport {
        ComponentImport {
            name: name.to_string(),
            type_only: false,
        }
    }

    fn type_only(name: &str) -> ComponentImport {
        ComponentImport {
            name: name.to_string(),
            type_only: true,
        }
    }

    /// §4.4: 承認集合の部分集合となる import 群は通過し、解決した承認 import が返る。
    #[test]
    fn match_accepts_approved_subset() {
        let approved = ApprovedCapabilities::baseline();
        let imports = vec![
            cap("wasi:io/streams@0.2.6"),
            cap("wasi:cli/environment@0.2.0"),
            cap("faas:component/types@1.0.0"),
        ];
        let resolved = match_capabilities(&imports, &approved).expect("approved subset accepts");
        // 承認集合に含まれる capability import がそのまま解決される。
        assert_eq!(
            resolved,
            vec![
                "wasi:io/streams@0.2.6".to_string(),
                "wasi:cli/environment@0.2.0".to_string(),
                "faas:component/types@1.0.0".to_string(),
            ]
        );
    }

    /// §4.4: 未承認の ambient capability import は 422 (`InvalidRequest`) で拒否される。
    #[test]
    fn match_rejects_unapproved_import_as_422() {
        let approved = ApprovedCapabilities::baseline();
        let imports = vec![cap("wasi:io/streams@0.2.6"), cap("evil:net/socket@0.1.0")];
        let err = match_capabilities(&imports, &approved).expect_err("unapproved must reject");
        // InvalidRequest は HTTP 422 相当（error.rs envelope）。メッセージは import 名のみ言及。
        match err {
            FaasError::InvalidRequest(msg) => {
                assert!(msg.contains("evil:net/socket@0.1.0"));
                assert!(msg.contains("deny-all"));
            }
            other => panic!("expected InvalidRequest (422), got {other:?}"),
        }
    }

    /// §4.4: deny-all 既定。承認集合に baseline 以外が無ければ、baseline 外 import は通らない。
    /// 完全な deny-all（baseline すら無い空集合）では capability import は一切通過しない。
    #[test]
    fn match_deny_all_default_blocks_any_ambient() {
        let empty = ApprovedCapabilities {
            prefixes: BTreeSet::new(),
            exact: BTreeSet::new(),
        };
        // baseline の wasi: ですら、空の承認集合では通らない（完全 deny-all）。
        let imports = vec![cap("wasi:io/streams@0.2.6")];
        assert!(matches!(
            match_capabilities(&imports, &empty),
            Err(FaasError::InvalidRequest(_))
        ));
        // import が無ければ deny-all でも当然通過する（要求 capability ゼロ）。
        assert_eq!(
            match_capabilities(&[], &empty).unwrap(),
            Vec::<String>::new()
        );
    }

    /// §4.4 / M2 gotcha: 型のみ import は capability を伴わないため照合対象外。
    /// baseline 外の名前空間でも型のみなら拒否されず、承認結果にも含まれない。
    #[test]
    fn match_type_only_import_is_not_a_capability() {
        let approved = ApprovedCapabilities::baseline();
        let imports = vec![
            // 型のみ: baseline 外の名前空間でも capability ではないので通過する。
            type_only("some:pkg/iface@1.0.0"),
            // capability を伴う baseline import は通常どおり解決される。
            cap("faas:component/types@1.0.0"),
        ];
        let resolved =
            match_capabilities(&imports, &approved).expect("type-only must not be rejected");
        // 型のみは承認結果に含めない。capability import のみが解決される。
        assert_eq!(resolved, vec!["faas:component/types@1.0.0".to_string()]);
    }

    // ---- M7b: capabilities の 2 キー構造（§4.4 / §15） ----------------------

    /// 旧形式（承認済み import 名の素の配列）は `imports` として読め、`env` は空 = deny-all。
    /// backfill せず読み取り時に吸収する（0008 の additive 精神と同じ）。
    #[test]
    fn capabilities_legacy_array_is_read_as_imports_with_no_env() {
        let legacy = serde_json::json!(["wasi:cli/environment@0.2.0", "wasi:io/streams@0.2.6"]);
        let caps = parse_capabilities(&legacy);
        assert_eq!(caps.imports.len(), 2);
        assert!(
            caps.env.is_empty(),
            "legacy rows must never grant env injection"
        );
    }

    /// 新形式はそのまま読める。
    #[test]
    fn capabilities_object_form_roundtrips() {
        let v = serde_json::json!({"imports": ["wasi:cli/environment@0.2.0"], "env": ["API_KEY"]});
        let caps = parse_capabilities(&v);
        assert_eq!(caps.imports, vec!["wasi:cli/environment@0.2.0".to_string()]);
        assert!(caps.env.contains("API_KEY"));
        // to_json → parse で往復する。
        assert_eq!(parse_capabilities(&caps.to_json()), caps);
    }

    /// 壊れた値は **deny-all** に倒れる（fail-closed）。
    #[test]
    fn capabilities_broken_value_is_deny_all() {
        for v in [
            serde_json::Value::Null,
            serde_json::json!(42),
            serde_json::json!("nope"),
            serde_json::json!({"imports": "not-a-list", "env": 7}),
        ] {
            let caps = parse_capabilities(&v);
            assert!(caps.imports.is_empty(), "broken value must deny imports");
            assert!(caps.env.is_empty(), "broken value must deny env");
        }
    }

    /// DB 側が壊れて不正な env 名が入っていても、読み取りで落とす。
    #[test]
    fn capabilities_drops_malformed_env_names() {
        let v = serde_json::json!({"imports": [], "env": ["OK_NAME", "bad-name", "", "9LEAD"]});
        let caps = parse_capabilities(&v);
        assert_eq!(caps.env.len(), 1);
        assert!(caps.env.contains("OK_NAME"));
    }

    /// **権限昇格の回帰ガード**: upload 経路（Deploy スコープ）で `capabilities.env` が
    /// 非空になる入力は存在しない。`env` キーを含む宣言は 400 で弾かれ、含まない宣言からは
    /// 空の許可リストしか生まれない。
    #[test]
    fn upload_path_can_never_produce_a_non_empty_env_allowlist() {
        // (a) env を含む宣言は拒否される。
        for declared in [
            serde_json::json!({"env": ["PROD_API_KEY"]}),
            serde_json::json!({"imports": ["wasi:cli/environment@0.2.0"], "env": []}),
        ] {
            assert!(
                reject_env_in_declared_capabilities(&declared).is_err(),
                "declaring capabilities.env on the deploy path must be refused: {declared}"
            );
        }
        // (b) env を含まない宣言は通り、保存形の env は必ず空になる。
        for declared in [
            serde_json::json!({}),
            serde_json::json!({"imports": ["wasi:cli/environment@0.2.0"]}),
            serde_json::json!(["wasi:cli/environment@0.2.0"]),
            serde_json::Value::Null,
        ] {
            assert!(reject_env_in_declared_capabilities(&declared).is_ok());
            // upload_version が保存するのは approved_imports のみ（env は常に空）。
            let stored = CapabilitySet {
                imports: vec!["wasi:cli/environment@0.2.0".to_string()],
                env: BTreeSet::new(),
                net_allow_outbound: BTreeSet::new(),
            };
            let parsed = parse_capabilities(&stored.to_json());
            assert!(
                parsed.env.is_empty(),
                "the deploy path must always persist an empty env allowlist"
            );
            assert!(
                parsed.net_allow_outbound.is_empty(),
                "the deploy path must always persist an empty egress allowlist (M9c)"
            );
        }
    }

    /// admin 承認リストのバリデーション（形式・件数の境界）。
    #[test]
    fn env_allowlist_validation() {
        let ok = validate_env_allowlist(&["API_KEY".into(), "LOG_LEVEL".into(), "API_KEY".into()])
            .expect("valid names");
        assert_eq!(ok.len(), 2, "duplicates collapse");

        for bad in ["lower", "with-dash", "1LEADING", "HAS=EQ", "", "A\u{0}B"] {
            assert!(
                validate_env_allowlist(&[bad.to_string()]).is_err(),
                "must reject {bad:?}"
            );
        }
        // 境界: 64 文字は可、65 文字は不可。
        let max = "A".repeat(faas_shared::MAX_ENV_KEY_LEN);
        assert!(validate_env_allowlist(&[max]).is_ok());
        let over = "A".repeat(faas_shared::MAX_ENV_KEY_LEN + 1);
        assert!(validate_env_allowlist(&[over]).is_err());
        // 件数上限。
        let many: Vec<String> = (0..=faas_shared::MAX_FUNCTION_ENV_KEYS)
            .map(|i| format!("K{i}"))
            .collect();
        assert!(validate_env_allowlist(&many).is_err());
    }
}
