//! Validate uploaded WASI HTTP components in a resource-limited subprocess.
//!
//! The child checks the Component Model, the HTTP export and supported host
//! imports, then returns the digest, size and resolved imports to persist.
//! Custom application interfaces must be composed into the uploaded component.
//! Environment bindings and outbound grants are enforced separately at runtime.
//!
//! A semaphore bounds concurrent children; a wall-clock timeout and Linux
//! `RLIMIT_AS` bound each validation process.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use wasmparser::{ComponentTypeRef, Parser, Payload, Validator, WasmFeatures};

use hibana_shared::{subprocess, FaasError};

/// 同時に走る検証の上限（DoS 緩和, §6.2）。
const MAX_CONCURRENT_VALIDATIONS: usize = 4;

/// Supported WASI host imports. Socket/HTTP access still requires an outbound
/// grant; filesystem imports receive no preopened directories. Type-only
/// imports do not grant capabilities and are skipped by `match_capabilities`.
const SUPPORTED_IMPORT_PREFIXES: &[&str] = &[
    "wasi:io/",
    "wasi:cli/",
    "wasi:clocks/",
    "wasi:random/",
    "wasi:sockets/",
    "wasi:filesystem/",
    "wasi:http/types",
    "wasi:http/outgoing-handler",
];

fn is_supported_import(name: &str) -> bool {
    SUPPORTED_IMPORT_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
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
    /// §4.4: 承認集合との strict matching を通過した capability import 集合（型のみは除く）。
    /// 呼び出し側はこれ（クライアント宣言値ではなく）を `component_versions.capabilities` に保存する。
    pub approved_imports: Vec<String>,
    /// 本体の sha256（16進・小文字）。
    pub sha256: String,
    /// 本体サイズ（bytes）。
    pub size_bytes: u64,
    pub build_metadata: Option<crate::build_metadata::BuildMetadata>,
}

/// 検証子プロセスの wall-clock timeout（秒）。超過で SIGKILL する。
const DEFAULT_VALIDATION_TIMEOUT_SECS: u64 = 5;
/// 検証子プロセスのメモリ上限（MiB, Linux の `RLIMIT_AS`）。
/// macOS では `RLIMIT_AS` を張らず、時間・出力上限で縛る。
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
    /// wasm 自体が不正 / 未承認。子は exit 0 でこれを返す。
    Rejected { message: String },
}

/// Validate in a bounded, credential-free subprocess. Keep the concurrency
/// permit until the child is reaped, including cancelled or timed-out uploads.
pub async fn validate_wasm(bytes: &[u8]) -> Result<Validated, FaasError> {
    let permit = validation_semaphore()
        .try_acquire()
        .map_err(|_| FaasError::Unavailable)?;

    let exe = std::env::current_exe()
        .map_err(|e| FaasError::Internal(format!("cannot resolve own exe for validation: {e}")))?;
    let mut command = subprocess::isolated_command(exe);
    command.arg(VALIDATE_STDIN_FLAG).env(
        "VALIDATION_MEM_LIMIT_MB",
        env_u64("VALIDATION_MEM_LIMIT_MB", DEFAULT_VALIDATION_MEM_LIMIT_MB)
            .max(16)
            .to_string(),
    );
    let output = subprocess::output(
        command,
        bytes,
        std::time::Duration::from_secs(
            env_u64("VALIDATION_TIMEOUT_SECS", DEFAULT_VALIDATION_TIMEOUT_SECS).max(1),
        ),
        VALIDATION_MAX_OUTPUT_BYTES,
        permit,
    )
    .await
    .map_err(|error| match error {
        subprocess::Error::Spawn(_) | subprocess::Error::Io(_) | subprocess::Error::Stopped => {
            FaasError::Internal(format!("validation {error}"))
        }
        _ => FaasError::InvalidRequest(format!("wasm validation failed: {error}")),
    })?;

    match serde_json::from_slice::<ValidationOutcome>(&output) {
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
/// 1 つ書いて exit 0 で終わる（「wasm が不正」も検証結果として返す）。
/// メモリ制限の設定失敗やstdin/stdoutのI/O失敗では異常終了する。
///
/// **プロセスの先頭でメモリ上限を張る**（Linux の `RLIMIT_AS`）。これが本スライスの主目的で、
/// wasmparser が悪性 wasm でメモリを食い潰しても、この子プロセスが OOM で死ぬだけで
/// control-plane 本体には波及しない。
pub fn run_validate_stdin() -> anyhow::Result<()> {
    use std::io::Read as _;

    set_memory_limit()?;

    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("validation child: cannot read stdin: {e}"))?;

    let outcome = match validate_blocking(&bytes) {
        Ok(v) => ValidationOutcome::Ok(v),
        Err(FaasError::InvalidRequest(m)) => ValidationOutcome::Rejected { message: m },
        // 未知の検証エラーも成功として扱わない。
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
fn set_memory_limit() -> anyhow::Result<()> {
    let mb = env_u64("VALIDATION_MEM_LIMIT_MB", DEFAULT_VALIDATION_MEM_LIMIT_MB).max(16);
    let bytes = mb.saturating_mul(1024 * 1024);
    let limit = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    // SAFETY: setrlimit は resource と rlimit ポインタを取る単純な syscall。
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_memory_limit() -> anyhow::Result<()> {
    // macOS 等は RLIMIT_AS が効かない。timeout + 出力上限で縛る（上のコメント参照）。
    Ok(())
}

/// u64 の任意 env。欠損 / 不正は default（子プロセスでも使うので独立実装）。
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// 検証子プロセスで実行する同期検証本体。
fn validate_blocking(bytes: &[u8]) -> Result<Validated, FaasError> {
    // (1) Component Model 妥当性検証。
    //     component_model を有効化した features で完全検証する。
    let features = WasmFeatures::default() | WasmFeatures::COMPONENT_MODEL;
    let mut validator = Validator::new_with_features(features);
    validator
        .validate_all(bytes)
        .map_err(|e| FaasError::InvalidRequest(format!("invalid wasm component: {e}")))?;

    // (2) §4.4 strict matching: capability を伴う import を **承認集合** と厳密照合する。
    //     型のみの import は capability を伴わないため照合対象外。未承認は 422（deny-all 既定）。
    require_http_export(bytes)?;
    let imports = collect_component_imports(bytes)?;
    let approved_imports = match_capabilities(&imports)?;

    // (4) sha256 算出・サイズ確定。
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let sha256 = hex::encode(hasher.finalize());

    Ok(Validated {
        approved_imports,
        sha256,
        size_bytes: bytes.len() as u64,
        build_metadata: crate::build_metadata::extract(bytes)?,
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
fn match_capabilities(imports: &[ComponentImport]) -> Result<Vec<String>, FaasError> {
    let mut approved_imports = Vec::new();
    for import in imports {
        if import.type_only {
            continue;
        }
        if !is_supported_import(&import.name) {
            return Err(FaasError::InvalidRequest(format!(
                "unapproved host import '{}': capabilities default to deny-all (§4.4); only the \
                 supported WASI imports are permitted",
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

    // **最外殻コンポーネントの import だけ**を集める（= host 境界）。`parse_all` は
    // ネストしたモジュール/コンポーネントのセクションも平坦に流すため、内部コンポーネントの
    // **内部 import**（親の instantiation で満たされ host からは配線されない）まで拾ってしまう。
    // それらは host capability ではないので許可リスト照合の対象にしてはならない。
    // 例: componentize-js 0.19.3（wasi:http proxy 経路）が生む component は、内部コンポーネントに
    // `handle` 関数 import を持つ —— これは incoming-handler の内部配線であって host import ではない。
    // nest 深さを ModuleSection/ComponentSection(+1) と End(-1) で数え、depth==0 のときだけ集める。
    let mut depth: i32 = 0;

    for payload in Parser::new(0).parse_all(bytes) {
        let payload =
            payload.map_err(|e| FaasError::InvalidRequest(format!("parse error: {e}")))?;
        match payload {
            // ネスト単位に入る（後続ペイロードは内部のもの）。
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => {
                depth += 1;
            }
            // 現在の単位の終わり。最外殻の End で depth は負になりうるが害はない。
            Payload::End(_) => {
                depth -= 1;
            }
            // host 境界（最外殻）の import section のみ照合対象にする。
            Payload::ComponentImportSection(section) if depth == 0 => {
                for import in section {
                    let import = import
                        .map_err(|e| FaasError::InvalidRequest(format!("parse error: {e}")))?;
                    imports.push(ComponentImport {
                        name: import.name.0.to_string(),
                        type_only: matches!(import.ty, ComponentTypeRef::Type(_)),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(imports)
}

/// バージョンへ注入する環境変数名の形式と件数を検証する純関数。
///
/// 重複は集合化で吸収する。名前の形式・件数の上限は `hibana_shared` の定数を使う
/// （CP と worker で同じ定数を参照する二重防御）。
pub fn validate_env_allowlist(names: &[String]) -> Result<BTreeSet<String>, FaasError> {
    if names.len() > hibana_shared::MAX_FUNCTION_ENV_KEYS {
        return Err(FaasError::InvalidRequest(format!(
            "at most {} env names may be approved per version",
            hibana_shared::MAX_FUNCTION_ENV_KEYS
        )));
    }
    let mut out = BTreeSet::new();
    for name in names {
        if !hibana_shared::is_valid_env_key(name) {
            return Err(FaasError::InvalidRequest(format!(
                "invalid env name '{name}': must match ^[A-Z_][A-Z0-9_]{{0,{}}}$",
                hibana_shared::MAX_ENV_KEY_LEN - 1
            )));
        }
        out.insert(name.clone());
    }
    Ok(out)
}

fn require_http_export(bytes: &[u8]) -> Result<(), FaasError> {
    let mut depth = 0i32;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| FaasError::InvalidRequest(format!("parse error: {e}")))? {
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) => depth -= 1,
            Payload::ComponentExportSection(section) if depth == 0 => {
                for export in section {
                    let export = export
                        .map_err(|e| FaasError::InvalidRequest(format!("parse error: {e}")))?;
                    if export.name.0 == "wasi:http/incoming-handler@0.2.3"
                        && export.kind == wasmparser::ComponentExternalKind::Instance
                    {
                        return Ok(());
                    }
                }
            }
            _ => {}
        }
    }
    Err(FaasError::InvalidRequest("Hibana requires a WASI HTTP Component exporting wasi:http/incoming-handler@0.2.3; bytes handlers and CLI modules are not supported".into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn deployment_rejects_components_without_http_export_and_core_modules() {
        for bytes in [
            b"\0asm\x0d\0\x01\0".as_slice(),
            b"\0asm\x01\0\0\0".as_slice(),
        ] {
            let err = super::validate_blocking(bytes).unwrap_err();
            assert!(err.to_string().contains("WASI HTTP Component"));
        }
    }

    use super::*;

    #[tokio::test]
    async fn full_validation_capacity_rejects_without_a_payload_wait_queue() {
        let permits = validation_semaphore().try_acquire_many(4).unwrap();
        assert!(matches!(
            validate_wasm(b"fixture").await,
            Err(FaasError::Unavailable)
        ));
        drop(permits);
        assert_eq!(validation_semaphore().available_permits(), 4);
    }

    /// §4.4: baseline 承認集合は標準 WASI と標準 handler world 契約を承認する。
    #[test]
    fn baseline_approves_wasi_http_contract() {
        // 標準 WASI Preview2。
        assert!(is_supported_import("wasi:io/streams@0.2.6"));
        assert!(is_supported_import("wasi:cli/environment@0.2.0"));
        // §4.2 標準 handler world の自パッケージ型 interface（echo が import する）。
        assert!(is_supported_import("wasi:http/types@0.2.3"));
    }

    /// §4.4: deny-all 既定。baseline を超える ambient capability は未承認 → 422 対象。
    #[test]
    fn baseline_rejects_unapproved_host_import() {
        // 任意の ambient capability は未承認（net/fs 等）。
        assert!(!is_supported_import("evil:net/socket@0.1.0"));
        assert!(!is_supported_import("host:fs/open"));
        // 名前空間を偽装した接頭辞も未承認。
        assert!(!is_supported_import("notwasi:io/streams"));
        assert!(!is_supported_import("faas:secrets/store"));
    }

    /// §4.4 defense-in-depth: bare `wasi:` namespace を承認しないため、network egress
    /// M9c: `wasi:filesystem/*` の **import は baseline で許可**する。Rust std は net-only の
    /// component でも filesystem import を推移的に焼き込むため、拒否すると std ベースの egress
    /// component が一切アップロードできない。**実際の fs アクセスはランタイムが塞ぐ**
    /// （WasiCtx が preopen を 1 つも与えない → どのパスも開けない）。この deny-by-default は
    /// chaos_v1 が実行時に固定する（「アップロードは通るが実行時に fs は全拒否」）。
    #[test]
    fn baseline_allows_filesystem_import_but_runtime_denies_access() {
        assert!(is_supported_import("wasi:filesystem/types@0.2.6"));
        assert!(is_supported_import("wasi:filesystem/preopens@0.2.6"));
    }

    /// M9c: `wasi:sockets/*` の **import は baseline で許可**する（env モデルと同じく「import できる」
    /// ことと「実際に到達できる」ことを分ける）。実際の egress は admin 承認した allowlist が
    /// 非空のときだけ worker の `socket_addr_check` が通し、空なら全拒否する（deny-by-default は
    /// ランタイムが担保する）。したがって import 許可だけでは 1 バイトも外へ出られない。
    #[test]
    fn baseline_allows_sockets_import_but_runtime_gates_egress() {
        assert!(is_supported_import("wasi:sockets/tcp@0.2.6"));
        assert!(is_supported_import("wasi:sockets/ip-name-lookup@0.2.6"));
    }

    /// §4.4: baseline が承認する WASI Preview2 サブインターフェース（stdio/clocks/random）。
    #[test]
    fn baseline_approves_ambient_free_wasi_subinterfaces() {
        assert!(is_supported_import("wasi:io/poll@0.2.6"));
        assert!(is_supported_import("wasi:cli/stdout@0.2.0"));
        assert!(is_supported_import("wasi:clocks/monotonic-clock@0.2.0"));
        assert!(is_supported_import("wasi:random/random@0.2.0"));
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
        let imports = vec![
            cap("wasi:io/streams@0.2.6"),
            cap("wasi:cli/environment@0.2.0"),
            cap("wasi:http/types@0.2.3"),
        ];
        let resolved = match_capabilities(&imports).expect("approved subset accepts");
        // 承認集合に含まれる capability import がそのまま解決される。
        assert_eq!(
            resolved,
            vec![
                "wasi:io/streams@0.2.6".to_string(),
                "wasi:cli/environment@0.2.0".to_string(),
                "wasi:http/types@0.2.3".to_string(),
            ]
        );
    }

    /// §4.4: 未承認の ambient capability import は 422 (`InvalidRequest`) で拒否される。
    #[test]
    fn match_rejects_unapproved_import_as_422() {
        let imports = vec![cap("wasi:io/streams@0.2.6"), cap("evil:net/socket@0.1.0")];
        let err = match_capabilities(&imports).expect_err("unapproved must reject");
        // InvalidRequest は HTTP 422 相当（error.rs envelope）。メッセージは import 名のみ言及。
        match err {
            FaasError::InvalidRequest(msg) => {
                assert!(msg.contains("evil:net/socket@0.1.0"));
                assert!(msg.contains("deny-all"));
            }
            other => panic!("expected InvalidRequest (422), got {other:?}"),
        }
    }

    #[test]
    fn match_accepts_no_imports() {
        assert_eq!(match_capabilities(&[]).unwrap(), Vec::<String>::new());
    }

    /// §4.4 / M2 gotcha: 型のみ import は capability を伴わないため照合対象外。
    /// baseline 外の名前空間でも型のみなら拒否されず、承認結果にも含まれない。
    #[test]
    fn match_type_only_import_is_not_a_capability() {
        let imports = vec![
            // 型のみ: baseline 外の名前空間でも capability ではないので通過する。
            type_only("some:pkg/iface@1.0.0"),
            // capability を伴う baseline import は通常どおり解決される。
            cap("wasi:http/types@0.2.3"),
        ];
        let resolved = match_capabilities(&imports).expect("type-only must not be rejected");
        // 型のみは承認結果に含めない。capability import のみが解決される。
        assert_eq!(resolved, vec!["wasi:http/types@0.2.3".to_string()]);
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
        let max = "A".repeat(hibana_shared::MAX_ENV_KEY_LEN);
        assert!(validate_env_allowlist(&[max]).is_ok());
        let over = "A".repeat(hibana_shared::MAX_ENV_KEY_LEN + 1);
        assert!(validate_env_allowlist(&[over]).is_err());
        // 件数上限。
        let many: Vec<String> = (0..=hibana_shared::MAX_FUNCTION_ENV_KEYS)
            .map(|i| format!("K{i}"))
            .collect();
        assert!(validate_env_allowlist(&many).is_err());
    }
}
