//! Verified Wasm artifacts and trusted, locally compiled Component caches.
use crate::{
    metrics,
    runtime::{ExecError, PreparedComponent},
};
use anyhow::Context as _;
use lru::LruCache;
use sha2::{Digest, Sha256};
use std::{
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{info, warn};
use wasmtime::{component::Component, Engine};
const COMPONENT_CACHE_CAP: usize = 64;
pub(crate) struct ArtifactCache {
    engine: Engine,
    http: reqwest::Client,
    wasm_cache_dir: PathBuf,
    cache: Mutex<LruCache<String, (Arc<PreparedComponent>, usize)>>,
    // Fixed stripes avoid retaining an unbounded set of failed artifact hashes.
    inflight_precompile: [tokio::sync::Mutex<()>; 32],
    compilation_slots: Arc<tokio::sync::Semaphore>,
    metrics: Arc<metrics::Metrics>,
    compiler_limits: crate::compiler::Limits,
    disk_writer: Mutex<()>,
}
impl ArtifactCache {
    pub(crate) fn new(
        engine: Engine,
        http: reqwest::Client,
        wasm_cache_dir: PathBuf,
        max_compilations: usize,
        compiler_limits: crate::compiler::Limits,
        metrics: Arc<metrics::Metrics>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&wasm_cache_dir).context("failed to create WASM_CACHE_DIR")?;
        crate::cache_storage::prune(&wasm_cache_dir, 0, crate::cache_storage::DISK_BUDGET)?;
        Ok(Self {
            disk_writer: Mutex::new(()),
            engine,
            compiler_limits,
            http,
            wasm_cache_dir,
            metrics,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(COMPONENT_CACHE_CAP).unwrap(),
            )),
            inflight_precompile: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
            compilation_slots: Arc::new(tokio::sync::Semaphore::new(max_compilations)),
        })
    }
    /// HTTP admission can only load code already compiled by this Worker.
    /// Preparation probes do not increment HTTP cache-hit counters.
    pub(crate) fn cached_component(
        &self,
        sha: &str,
        observe_hit: bool,
    ) -> std::result::Result<Option<Arc<PreparedComponent>>, ExecError> {
        if !hibana_shared::preparation::valid_digest(sha) {
            return Err(ExecError::Failed("Invalid artifact digest".into()));
        }
        if let Some(component) = self.cache_get(sha) {
            if observe_hit {
                self.metrics
                    .wasmtime_component_cache_hits_total
                    .with_label_values(&["lru"])
                    .inc();
            }
            return Ok(Some(component));
        }
        let cwasm_path = self.wasm_cache_dir.join(format!("{sha}.cwasm"));
        if let Ok(metadata) = cwasm_path.symlink_metadata() {
            if !metadata.is_file() || metadata.len() > crate::compiler::MAX_OUTPUT as u64 {
                return Err(ExecError::Failed("Invalid compiled cache file".into()));
            }
            // SAFETY: deserialize_file は信頼できない入力に対して未定義動作になりうる。
            // ここで読むのは「自分が precompile_component で生成した cwasm」のみであり、
            // 他テナント由来の cwasm は決して読み込まない (§3.6 MUST NOT)。
            // キーは sha256 で、cwasm 自体も自プラットフォーム生成物に限定している。
            match unsafe { Component::deserialize_file(&self.engine, &cwasm_path) } {
                Ok(component) => {
                    let component =
                        Arc::new(PreparedComponent::new(component).map_err(|e| {
                            ExecError::Failed(format!("Component preparation: {e}"))
                        })?);
                    self.cache_put(
                        sha.to_string(),
                        Arc::clone(&component),
                        metadata.len() as usize,
                    );
                    if observe_hit {
                        self.metrics
                            .wasmtime_component_cache_hits_total
                            .with_label_values(&["cwasm"])
                            .inc();
                    }
                    return Ok(Some(component));
                }
                Err(e) => {
                    // バージョン不一致・破損など。c へフォールバックする。
                    warn!(%sha, error = %e, "cwasm deserialize failed; background preparation required");
                }
            }
        }
        Ok(None)
    }

    /// Only the authenticated preparation path may download and compile.
    pub(crate) async fn prepare(
        &self,
        sha: &str,
        url: &str,
    ) -> std::result::Result<Arc<PreparedComponent>, ExecError> {
        if let Some(component) = self.cached_component(sha, false)? {
            return Ok(component);
        }
        let stripe = usize::from(u8::from_str_radix(&sha[..2], 16).unwrap())
            % self.inflight_precompile.len();
        let _flight = tokio::time::timeout(
            Duration::from_secs(75),
            self.inflight_precompile[stripe].lock(),
        )
        .await
        .map_err(|_| ExecError::Failed("Artifact preparation capacity timeout".into()))?;
        if let Some(component) = self.cached_component(sha, false)? {
            return Ok(component);
        }
        let cwasm_path = self.wasm_cache_dir.join(format!("{sha}.cwasm"));

        // c. ダウンロード → sha256 照合 → precompile → cwasm 書き込み → deserialize。
        // M4a (§3.8): miss を計上（download + precompile に進む経路）。
        self.metrics.wasmtime_component_cache_misses_total.inc();
        info!(%sha, "component cache: miss; downloading and precompiling");
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.compilation_slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| ExecError::Failed("Compilation capacity timeout".into()))?
        .map_err(|_| ExecError::Failed("Compiler unavailable".into()))?;
        let bytes = self.download_wasm(url).await?;
        self.verify_sha256(&bytes, sha)?;

        let cwasm = crate::compiler::compile(
            bytes,
            self.compiler_limits,
            permit,
            self.metrics.active_compilations.clone(),
        )
        .await
        .map_err(|e| ExecError::Failed(format!("Compilation failed: {e}")))?;

        // アトミックに書き込む (tmp -> rename)。失敗しても実行自体は続行する。
        let persisted = {
            let _writer = self.disk_writer.lock().expect("cache disk mutex poisoned");
            crate::cache_storage::write(&self.wasm_cache_dir, &cwasm_path, &cwasm)
        };
        if let Err(e) = persisted {
            warn!(%sha, error = %e, "failed to persist cwasm cache (continuing)");
        }

        let engine = self.engine.clone();
        let compiled_bytes = cwasm.len();
        // Memory-image creation can copy megabytes on Linux. Keep that work off
        // the async executor that also serves active invocations.
        let component = tokio::task::spawn_blocking(move || {
            // SAFETY: only our compiler's output, with the same Engine settings.
            let component = unsafe { Component::deserialize(&engine, &cwasm) }
                .map_err(|e| ExecError::Failed(format!("Component::deserialize failed: {e}")))?;
            PreparedComponent::new(component)
                .map(Arc::new)
                .map_err(|e| ExecError::Failed(format!("Component preparation: {e}")))
        })
        .await
        .map_err(|e| ExecError::Failed(format!("Component preparation task: {e}")))??;

        // d. in-memory LRU に格納する。
        self.cache_put(sha.to_string(), Arc::clone(&component), compiled_bytes);
        Ok(component)
    }

    /// in-memory LRU から取得する (hit で参照順を更新)。
    fn cache_get(&self, sha: &str) -> Option<Arc<PreparedComponent>> {
        let mut cache = self.cache.lock().expect("component cache mutex poisoned");
        cache.get(sha).map(|(component, _)| Arc::clone(component))
    }

    /// in-memory LRU へ格納する。
    fn cache_put(&self, sha: String, component: Arc<PreparedComponent>, bytes: usize) {
        let mut cache = self.cache.lock().expect("component cache mutex poisoned");
        if bytes > crate::cache_storage::MEMORY_BUDGET {
            return;
        }
        cache.pop(&sha);
        while cache.iter().map(|(_, (_, size))| size).sum::<usize>() + bytes
            > crate::cache_storage::MEMORY_BUDGET
        {
            cache.pop_lru();
        }
        cache.put(sha, (component, bytes));
    }

    /// presigned GET URL から wasm 本体を取得する (§3.4)。
    async fn download_wasm(&self, url: &str) -> std::result::Result<Vec<u8>, ExecError> {
        let resp = self
            .http
            .get(url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| ExecError::Failed("Failed to GET wasm".into()))?;
        let mut resp = resp
            .error_for_status()
            .map_err(|_| ExecError::Failed("Wasm GET returned error status".into()))?;
        const MAX_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;
        let mut bytes = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|_| ExecError::Failed("Failed to read wasm body".into()))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_ARTIFACT_BYTES {
                return Err(ExecError::Failed(
                    "Wasm artifact exceeds Worker limit (32 MiB)".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_uses_only_prepared_code_and_keeps_it_alive_after_eviction() {
        let dir = std::env::temp_dir().join(format!(
            "hibana-prepared-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = crate::runtime::build_engine().unwrap();
        let metrics = crate::metrics::Metrics::init();
        let cache = ArtifactCache::new(
            engine.clone(),
            reqwest::Client::new(),
            dir.clone(),
            1,
            crate::compiler::Limits {
                memory_mib: 1024,
                timeout_secs: 60,
            },
            metrics.clone(),
        )
        .unwrap();
        let sha = "a".repeat(64);
        assert!(cache
            .cached_component(&sha, true)
            .is_ok_and(|v| v.is_none()));
        assert_eq!(
            metrics.wasmtime_component_cache_misses_total.get(),
            0,
            "admission never compiles"
        );
        let compiled = engine
            .precompile_component(&[0, 97, 115, 109, 13, 0, 1, 0])
            .unwrap();
        std::fs::write(dir.join(format!("{sha}.cwasm")), compiled).unwrap();
        assert!(cache
            .cached_component(&sha, false)
            .is_ok_and(|v| v.is_some()));
        assert_eq!(
            metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["lru"])
                .get(),
            0,
            "preparation probes must not count as HTTP cache hits"
        );
        let accepted = cache
            .cached_component(&sha, true)
            .ok()
            .flatten()
            .expect("prepared code");
        cache.cache.lock().unwrap().clear();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(cache
            .cached_component(&sha, true)
            .is_ok_and(|v| v.is_none()));
        assert_eq!(metrics.wasmtime_component_cache_misses_total.get(), 0);
        // An invocation admitted before eviction owns its own Component reference.
        assert!(accepted.component().serialize().is_ok());
    }
}
