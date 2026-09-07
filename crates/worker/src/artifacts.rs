//! Verified Wasm artifacts and trusted, locally compiled Component caches.
use crate::{metrics, runtime::ExecError};
use anyhow::Context as _;
use faas_shared::JobMessage;
use lru::LruCache;
use sha2::{Digest, Sha256};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tracing::{info, warn};
use wasmtime::{component::Component, Engine};
const COMPONENT_CACHE_CAP: usize = 64;
pub(crate) struct ArtifactCache {
    engine: Engine,
    http: reqwest::Client,
    wasm_cache_dir: PathBuf,
    cache: Mutex<LruCache<String, Arc<Component>>>,
    inflight_precompile: dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    metrics: Arc<metrics::Metrics>,
}
impl ArtifactCache {
    pub(crate) fn new(
        engine: Engine,
        http: reqwest::Client,
        wasm_cache_dir: PathBuf,
        metrics: Arc<metrics::Metrics>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&wasm_cache_dir).context("failed to create WASM_CACHE_DIR")?;
        Ok(Self {
            engine,
            http,
            wasm_cache_dir,
            metrics,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(COMPONENT_CACHE_CAP).unwrap(),
            )),
            inflight_precompile: dashmap::DashMap::new(),
        })
    }
    /// `job.wasm_sha256` をキーに Component を解決する (§3.6)。
    ///
    /// キャッシュ階層:
    ///   a. in-memory LRU が hit すれば即返す (coldstart 短縮の主経路)。
    ///   b. miss 時、ローカル cwasm `{WASM_CACHE_DIR}/{sha256}.cwasm` が在れば、
    ///      自プラットフォームが生成した信頼できる成果物として deserialize する。
    ///      バージョン不一致等で失敗したら c へフォールバックする。
    ///   c. cwasm が無い/壊れている場合、`job.wasm_url` から本体を取得し sha256 を
    ///      照合 → `precompile_component` で cwasm を生成 → アトミックに書き込み →
    ///      deserialize する。
    ///   d. 解決した Component を in-memory LRU に格納する。
    pub(crate) async fn resolve_component(
        &self,
        job: &JobMessage,
    ) -> std::result::Result<Arc<Component>, ExecError> {
        let sha = &job.wasm_sha256;

        // a. in-memory LRU hit（ロック外の高速経路。hit では single-flight の競合を一切踏まない）。
        if let Some(component) = self.cache_get(sha) {
            // M4a (§3.8): LRU hit を計上。
            self.metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["lru"])
                .inc();
            return Ok(component);
        }

        // M11-6 (§6.6): single-flight。miss した sha について per-sha ロックを取り、同一 sha の
        // 同時 cold invoke が **precompile を二重に走らせない**ようにする（N×23s の stampede 防止）。
        // 待たされた側はロック取得後の double-check で cache hit を引いて即返る。
        let lock = self
            .inflight_precompile
            .entry(sha.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _flight = lock.lock().await;

        // double-check: 待っている間に別の holder が cache を埋めたかもしれない。
        if let Some(component) = self.cache_get(sha) {
            self.inflight_precompile.remove(sha);
            self.metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["lru"])
                .inc();
            return Ok(component);
        }

        // b. ローカル cwasm を試す。
        let cwasm_path = self.wasm_cache_dir.join(format!("{sha}.cwasm"));
        if cwasm_path.exists() {
            // SAFETY: deserialize_file は信頼できない入力に対して未定義動作になりうる。
            // ここで読むのは「自分が precompile_component で生成した cwasm」のみであり、
            // 他テナント由来の cwasm は決して読み込まない (§3.6 MUST NOT)。
            // キーは sha256 で、cwasm 自体も自プラットフォーム生成物に限定している。
            match unsafe { Component::deserialize_file(&self.engine, &cwasm_path) } {
                Ok(component) => {
                    let component = Arc::new(component);
                    self.cache_put(sha.clone(), Arc::clone(&component));
                    self.inflight_precompile.remove(sha);
                    // M4a (§3.8): cwasm hit を計上。
                    self.metrics
                        .wasmtime_component_cache_hits_total
                        .with_label_values(&["cwasm"])
                        .inc();
                    info!(%sha, "component cache: cwasm hit");
                    return Ok(component);
                }
                Err(e) => {
                    // バージョン不一致・破損など。c へフォールバックする。
                    warn!(%sha, error = %e, "cwasm deserialize failed; recompiling");
                }
            }
        }

        // c. ダウンロード → sha256 照合 → precompile → cwasm 書き込み → deserialize。
        // M4a (§3.8): miss を計上（download + precompile に進む経路）。
        self.metrics.wasmtime_component_cache_misses_total.inc();
        info!(%sha, "component cache: miss; downloading and precompiling");
        let bytes = self.download_wasm(&job.wasm_url).await?;
        self.verify_sha256(&bytes, sha)?;

        let engine = self.engine.clone();
        let cwasm = tokio::task::spawn_blocking(move || engine.precompile_component(&bytes))
            .await
            .map_err(|e| ExecError::Failed(format!("precompile join error: {e}")))?
            .map_err(|e| ExecError::Failed(format!("precompile_component failed: {e}")))?;

        // アトミックに書き込む (tmp -> rename)。失敗しても実行自体は続行する。
        if let Err(e) = write_atomic(&cwasm_path, &cwasm) {
            warn!(%sha, error = %e, "failed to persist cwasm cache (continuing)");
        }

        // SAFETY: 直前に同一 Engine で生成した cwasm を読み込む。信頼できる自前生成物。
        let component = unsafe { Component::deserialize(&self.engine, &cwasm) }
            .map_err(|e| ExecError::Failed(format!("Component::deserialize failed: {e}")))?;
        let component = Arc::new(component);

        // d. in-memory LRU に格納する。
        self.cache_put(sha.clone(), Arc::clone(&component));
        // single-flight エントリを解放（待機側は double-check で cache hit を引く）。
        self.inflight_precompile.remove(sha);
        Ok(component)
    }

    /// in-memory LRU から取得する (hit で参照順を更新)。
    fn cache_get(&self, sha: &str) -> Option<Arc<Component>> {
        let mut cache = self.cache.lock().expect("component cache mutex poisoned");
        cache.get(sha).map(Arc::clone)
    }

    /// in-memory LRU へ格納する。
    fn cache_put(&self, sha: String, component: Arc<Component>) {
        let mut cache = self.cache.lock().expect("component cache mutex poisoned");
        cache.put(sha, component);
    }

    /// presigned GET URL から wasm 本体を取得する (§3.4)。
    async fn download_wasm(&self, url: &str) -> std::result::Result<Vec<u8>, ExecError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| ExecError::Failed(format!("failed to GET wasm: {e}")))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| ExecError::Failed(format!("wasm GET returned error status: {e}")))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ExecError::Failed(format!("failed to read wasm body: {e}")))?;
        Ok(bytes.to_vec())
    }

    /// ダウンロード本体の sha256 (16進) を期待値と照合する。不一致は Failed (§3.6)。
    fn verify_sha256(&self, bytes: &[u8], expected: &str) -> std::result::Result<(), ExecError> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let actual = hex_encode(&hasher.finalize());
        if actual.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(ExecError::Failed(format!(
                "wasm sha256 mismatch: expected {expected}, got {actual}"
            )))
        }
    }
}
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `path` へアトミックに書き込む (同一ディレクトリの tmp に書いてから rename)。
///
/// 複数 worker / 同一 worker の並行ダウンロードが同じ cwasm を書いても、
/// 最終的な可視ファイルが部分書き込みにならないようにする (§3.6)。
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // tmp 名は衝突を避けるため pid + nanos を含める。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("cwasm"),
        std::process::id(),
        nanos
    ));
    std::fs::write(&tmp, data)?;
    // rename は同一ファイルシステム内でアトミック。既存があっても置き換える。
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 失敗時は tmp を掃除しておく。
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}
