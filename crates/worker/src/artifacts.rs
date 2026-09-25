//! Verified Wasm artifacts and authenticated platform-generated Component caches.
use crate::{
    metrics,
    runtime::{ExecError, PreparedComponent},
};
use anyhow::Context as _;
use lru::LruCache;
use sha2::{Digest, Sha256};
use std::{
    num::NonZeroUsize,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{info, warn};
use wasmtime::{component::Component, Engine};
const COMPONENT_CACHE_CAP: usize = 64;

// Cache files are private and atomically replaced. Remember validation only for
// the same file; deletion, replacement or modification requires validation again.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

pub(crate) struct ArtifactCache {
    engine: Engine,
    http: reqwest::Client,
    wasm_cache_dir: PathBuf,
    cache: Mutex<LruCache<String, (Arc<PreparedComponent>, usize)>>,
    validated_files: Mutex<LruCache<String, FileStamp>>,
    // Fixed stripes avoid retaining an unbounded set of failed artifact hashes.
    inflight_precompile: [tokio::sync::Mutex<()>; 32],
    compilation_slots: Arc<tokio::sync::Semaphore>,
    metrics: Arc<metrics::Metrics>,
    compiler_limits: crate::compiler::Limits,
    disk_writer: Mutex<()>,
    disk_budget: u64,
    shared: Option<crate::shared_cache::SharedCache>,
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
            disk_budget: crate::cache_storage::DISK_BUDGET,
            shared: None,
            engine,
            compiler_limits,
            http,
            wasm_cache_dir,
            metrics,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(COMPONENT_CACHE_CAP).unwrap(),
            )),
            validated_files: Mutex::new(LruCache::new(
                NonZeroUsize::new(crate::cache_storage::DISK_ENTRIES).unwrap(),
            )),
            inflight_precompile: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
            compilation_slots: Arc::new(tokio::sync::Semaphore::new(max_compilations)),
        })
    }
    pub(crate) fn with_disk_budget(mut self, bytes: u64) -> anyhow::Result<Self> {
        crate::cache_storage::prune(&self.wasm_cache_dir, 0, bytes)?;
        self.disk_budget = bytes;
        Ok(self)
    }
    pub(crate) fn with_shared_cache(
        mut self,
        shared: Option<crate::shared_cache::SharedCache>,
    ) -> Self {
        self.shared = shared;
        self
    }
    pub(crate) fn runtime_id(&self) -> Option<&str> {
        self.shared.as_ref().map(|cache| cache.runtime())
    }
    /// Acquire a reference that remains valid even if its cache file is evicted.
    pub(crate) fn cached_component(
        &self,
        sha: &str,
    ) -> std::result::Result<Option<Arc<PreparedComponent>>, ExecError> {
        if !hibana_shared::preparation::valid_digest(sha) {
            return Err(ExecError::Failed("Invalid artifact digest".into()));
        }
        let _disk = self.disk_writer.lock().unwrap();
        if let Some(component) = self.cache_get(sha) {
            self.touch(sha);
            self.metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["lru"])
                .inc();
            return Ok(Some(component));
        }
        let Some(file) = self.disk_file(sha)? else {
            return Ok(None);
        };
        let component = self.load_component(sha, file)?;
        if let Some(component) = &component {
            self.touch(sha);
            self.cache_put(sha.to_string(), Arc::clone(component), file.len as usize);
            self.metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["cwasm"])
                .inc();
        }
        Ok(component)
    }

    /// Check retained, executable code without populating or touching the HTTP LRU.
    /// An unchanged file is validated once, even when there are more apps than fit
    /// in memory. A memory-only hit must never certify a version for publication.
    pub(crate) fn is_prepared(&self, sha: &str) -> std::result::Result<bool, ExecError> {
        let _disk = self.disk_writer.lock().unwrap();
        let Some(file) = self.disk_file(sha)? else {
            self.validated_files.lock().unwrap().pop(sha);
            return Ok(false);
        };
        if self.validated_files.lock().unwrap().get(sha) == Some(&file) {
            return Ok(true);
        }
        self.load_component(sha, file)?;
        Ok(self.validated_files.lock().unwrap().peek(sha) == Some(&file))
    }

    fn disk_file(&self, sha: &str) -> std::result::Result<Option<FileStamp>, ExecError> {
        if !hibana_shared::preparation::valid_digest(sha) {
            return Err(ExecError::Failed("Invalid artifact digest".into()));
        }
        let path = self.wasm_cache_dir.join(format!("{sha}.cwasm"));
        match path.symlink_metadata() {
            Ok(meta) if meta.is_file() && meta.len() <= crate::compiler::MAX_OUTPUT as u64 => {
                Ok(Some(FileStamp {
                    device: meta.dev(),
                    inode: meta.ino(),
                    len: meta.len(),
                    modified: (meta.mtime(), meta.mtime_nsec()),
                    changed: (meta.ctime(), meta.ctime_nsec()),
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            _ => Err(ExecError::Failed("Invalid compiled cache file".into())),
        }
    }

    fn load_component(
        &self,
        sha: &str,
        file: FileStamp,
    ) -> std::result::Result<Option<Arc<PreparedComponent>>, ExecError> {
        self.validated_files.lock().unwrap().pop(sha);
        let cwasm_path = self.wasm_cache_dir.join(format!("{sha}.cwasm"));
        // SAFETY: the private directory contains only our compiler's output or
        // shared artifacts authenticated with the platform key before writing.
        // Guest-uploaded native code can never enter this directory.
        match unsafe { Component::deserialize_file(&self.engine, &cwasm_path) } {
            Ok(component) => {
                let component = Arc::new(
                    PreparedComponent::new(component)
                        .map_err(|e| ExecError::Failed(format!("Component preparation: {e}")))?,
                );
                if self.disk_file(sha)? == Some(file) {
                    self.validated_files
                        .lock()
                        .unwrap()
                        .put(sha.to_string(), file);
                }
                return Ok(Some(component));
            }
            Err(e) => {
                // バージョン不一致・破損など。c へフォールバックする。
                warn!(%sha, error = %e, "cwasm deserialize failed; on-demand preparation required");
            }
        }
        Ok(None)
    }

    // Coarsen recency writes to avoid an fsync/metadata write on every hot request.
    // Called under disk_writer; update the memo only for a previously validated file.
    fn touch(&self, sha: &str) {
        let path = self.wasm_cache_dir.join(format!("{sha}.cwasm"));
        if let Ok(file) = std::fs::File::open(path) {
            let stale = file
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() >= 30);
            if stale {
                let previous = self.disk_file(sha).ok().flatten();
                if file.set_modified(std::time::SystemTime::now()).is_ok() {
                    let mut validated = self.validated_files.lock().unwrap();
                    if validated.peek(sha).copied() == previous {
                        if let Ok(Some(stamp)) = self.disk_file(sha) {
                            validated.put(sha.to_string(), stamp);
                        }
                    }
                }
            }
        }
    }

    /// Publication verifies executable code without populating the HTTP LRU.
    pub(crate) async fn prepare(
        &self,
        sha: &str,
        url: &str,
        shared_request: Option<(Option<&str>, &axum::http::HeaderValue)>,
    ) -> Result<(), ExecError> {
        self.prepare_inner(sha, url, shared_request, false)
            .await
            .map(|_| ())
    }

    /// Only server-authorized immutable artifacts can be restored for invocation.
    pub(crate) async fn restore(
        &self,
        sha: &str,
        url: &str,
        shared_request: Option<(Option<&str>, &axum::http::HeaderValue)>,
    ) -> Result<Arc<PreparedComponent>, ExecError> {
        self.prepare_inner(sha, url, shared_request, true)
            .await?
            .ok_or_else(|| ExecError::Failed("Prepared component missing".into()))
    }

    // Outer Some means a cache hit. Publication needs no Component reference
    // and must not populate the HTTP LRU, so its inner value stays None.
    fn existing(
        &self,
        sha: &str,
        invocation: bool,
    ) -> Result<Option<Option<Arc<PreparedComponent>>>, ExecError> {
        if invocation {
            Ok(self.cached_component(sha)?.map(Some))
        } else {
            Ok(self.is_prepared(sha)?.then_some(None))
        }
    }

    async fn prepare_inner(
        &self,
        sha: &str,
        url: &str,
        shared_request: Option<(Option<&str>, &axum::http::HeaderValue)>,
        invocation: bool,
    ) -> Result<Option<Arc<PreparedComponent>>, ExecError> {
        // A historical version may have lost its shared copy while inactive.
        // Explicit publication must republish a local hit under the new DB pin;
        // otherwise a warm deploy can leave the next cold Pod recompiling it.
        let republish = !invocation && self.shared.is_some() && shared_request.is_some();
        if !republish {
            if let Some(component) = self.existing(sha, invocation)? {
                return Ok(component);
            }
        }
        if !hibana_shared::preparation::valid_digest(sha) {
            return Err(ExecError::Failed("Invalid artifact digest".into()));
        }
        let stripe = usize::from(u8::from_str_radix(&sha[..2], 16).unwrap())
            % self.inflight_precompile.len();
        let _flight = tokio::time::timeout(
            Duration::from_secs(75),
            self.inflight_precompile[stripe].lock(),
        )
        .await
        .map_err(|_| ExecError::Failed("Artifact preparation capacity timeout".into()))?;
        if !republish {
            if let Some(component) = self.existing(sha, invocation)? {
                return Ok(component);
            }
        }
        let cwasm_path = self.wasm_cache_dir.join(format!("{sha}.cwasm"));

        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.compilation_slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| ExecError::Failed("Compilation capacity timeout".into()))?
        .map_err(|_| ExecError::Failed("Compiler unavailable".into()))?;
        // Hold the capacity permit through transfer, validation and persistence.
        // The compiler retains its own reference until the child is reaped.
        let permit = Arc::new(permit);
        let local = if republish {
            let _writer = self.disk_writer.lock().unwrap();
            // Private, bounded regular files; the writer guard prevents eviction
            // or replacement between metadata validation and this read.
            self.disk_file(sha)?
                .and_then(|_| std::fs::read(&cwasm_path).ok())
        } else {
            None
        };
        let local = match local {
            Some(bytes) => self.validate_native(bytes, permit.clone()).await.ok(),
            None => None,
        };
        let remote = match (&self.shared, shared_request, local.is_none()) {
            (Some(cache), Some((Some(url), _)), true) => cache.fetch(sha, url).await,
            _ => None,
        };
        let remote = if let Some(bytes) = remote {
            match self.validate_native(bytes, permit.clone()).await {
                Ok(bytes) => Some(bytes),
                Err(_) => {
                    warn!(%sha, "shared compiled cache incompatible; recompiling source");
                    None
                }
            }
        } else {
            None
        };
        let from_shared = remote.is_some();
        let (cwasm, component) = if let Some(bytes) = local.or(remote) {
            bytes
        } else {
            self.metrics.wasmtime_component_cache_misses_total.inc();
            info!(%sha, "component cache: miss; downloading and precompiling");
            let bytes = self.download_wasm(url).await?;
            self.verify_sha256(&bytes, sha)?;
            let bytes = crate::compiler::compile(
                bytes,
                self.compiler_limits,
                permit.clone(),
                self.metrics.active_compilations.clone(),
            )
            .await
            .map_err(|e| ExecError::Failed(format!("Compilation failed: {e}")))?;
            self.validate_native(bytes, permit.clone()).await?
        };

        {
            let _writer = self.disk_writer.lock().unwrap();
            // Cache persistence is optional: disk pressure must not invalidate
            // verified code or the durable source object. Never modify mapped files.
            if crate::cache_storage::write(
                &self.wasm_cache_dir,
                &cwasm_path,
                &cwasm,
                self.disk_budget,
            )
            .is_ok()
            {
                if let Some(file) = self.disk_file(sha)? {
                    self.validated_files
                        .lock()
                        .unwrap()
                        .put(sha.to_string(), file);
                }
            } else {
                warn!(%sha, "compiled cache persistence failed; using verified code in memory");
            }
            if invocation {
                self.cache_put(sha.to_string(), component.clone(), cwasm.len());
            }
        }
        if !from_shared {
            if let (Some(cache), Some((_, token))) = (&self.shared, shared_request) {
                cache.publish(sha, token, cwasm).await;
            }
        }
        Ok(invocation.then_some(component))
    }

    async fn validate_native(
        &self,
        cwasm: Vec<u8>,
        permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<(Vec<u8>, Arc<PreparedComponent>), ExecError> {
        let engine = self.engine.clone();
        // Validate executable code before publication, without filling the HTTP
        // LRU. Only actual invocations decide which applications stay in memory.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // SAFETY: our compiler output, or platform-authenticated compatible output.
            let component = unsafe { Component::deserialize(&engine, &cwasm) }
                .map_err(|e| ExecError::Failed(format!("Component::deserialize failed: {e}")))?;
            let prepared = PreparedComponent::new(component)
                .map_err(|e| ExecError::Failed(format!("Component preparation: {e}")))?;
            Ok::<_, ExecError>((cwasm, Arc::new(prepared)))
        })
        .await
        .map_err(|e| ExecError::Failed(format!("Component preparation task: {e}")))?
    }

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
        let actual = hex::encode(hasher.finalize());
        if actual.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(ExecError::Failed(format!(
                "wasm sha256 mismatch: expected {expected}, got {actual}"
            )))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admission_uses_only_prepared_code_and_keeps_it_alive_after_eviction() {
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
        assert!(cache.cached_component(&sha).is_ok_and(|v| v.is_none()));
        assert_eq!(
            metrics.wasmtime_component_cache_misses_total.get(),
            0,
            "admission never compiles"
        );
        let compiled = engine
            .precompile_component(&[0, 97, 115, 109, 13, 0, 1, 0])
            .unwrap();
        std::fs::write(dir.join(format!("{sha}.cwasm")), compiled).unwrap();
        assert!(matches!(cache.is_prepared(&sha), Ok(true)));
        assert!(cache.cache.lock().unwrap().is_empty());
        assert_eq!(
            metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["lru"])
                .get(),
            0,
            "preparation probes must not count as HTTP cache hits"
        );
        let accepted = cache
            .cached_component(&sha)
            .ok()
            .flatten()
            .expect("prepared code");
        std::fs::remove_file(dir.join(format!("{sha}.cwasm"))).unwrap();
        assert!(cache.cached_component(&sha).is_ok_and(|v| v.is_some()));
        assert!(matches!(cache.is_prepared(&sha), Ok(false)));
        assert!(
            cache.prepare(&sha, "invalid:artifact", None).await.is_err(),
            "a memory-only hit must not certify a version for publication"
        );
        cache.cache.lock().unwrap().clear();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(cache.cached_component(&sha).is_ok_and(|v| v.is_none()));
        assert_eq!(metrics.wasmtime_component_cache_misses_total.get(), 1);
        // An invocation admitted before eviction owns its own Component reference.
        assert!(accepted.component().serialize().is_ok());
    }

    #[tokio::test]
    async fn bounded_disk_restores_evicted_code_and_preserves_live_references() {
        use axum::{routing::get, Router};
        let (_dir, mut cache) = fixture();
        let auth = hibana_shared::compiled_cache::Auth::from_hex(&"bc".repeat(32)).unwrap();
        let shared = crate::shared_cache::SharedCache::new(
            auth.clone(),
            &cache.engine,
            reqwest::Client::new(),
            "http://127.0.0.1:1",
            cache.metrics.clone(),
        );
        let mut routes = Router::new();
        let mut hashes = Vec::new();
        let mut largest = 0;
        for n in 0..3 {
            let source = format!(
                "(component (core module (func (export \"v\") (result i32) i32.const {n})))"
            );
            let sha = hex::encode(Sha256::digest(source.as_bytes()));
            let mut compiled = cache
                .engine
                .precompile_component(source.as_bytes())
                .unwrap();
            largest = largest.max(compiled.len() as u64);
            auth.seal(shared.runtime(), &sha, &mut compiled).unwrap();
            routes = routes.route(
                &format!("/{sha}"),
                get(move || {
                    let bytes = compiled.clone();
                    async move { bytes }
                }),
            );
            hashes.push(sha);
        }
        cache = cache
            .with_disk_budget(largest)
            .unwrap()
            .with_shared_cache(Some(shared));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
        let token = axum::http::HeaderValue::from_static("fixture");
        let mut accepted = Vec::new();
        for sha in &hashes {
            let url = format!("{base}/{sha}");
            accepted.push(
                cache
                    .restore(sha, "invalid:source", Some((Some(&url), &token)))
                    .await
                    .ok()
                    .unwrap(),
            );
            assert_eq!(std::fs::read_dir(&cache.wasm_cache_dir).unwrap().count(), 1);
        }
        assert!(!cache
            .wasm_cache_dir
            .join(format!("{}.cwasm", hashes[0]))
            .exists());
        cache.cache.lock().unwrap().clear();
        for component in accepted {
            assert!(component.component().serialize().is_ok());
        }
        let url = format!("{base}/{}", hashes[0]);
        let (first, second) = tokio::join!(
            cache.restore(&hashes[0], "invalid:source", Some((Some(&url), &token))),
            cache.restore(&hashes[0], "invalid:source", Some((Some(&url), &token))),
        );
        assert!(Arc::ptr_eq(&first.ok().unwrap(), &second.ok().unwrap()));
        assert_eq!(
            cache
                .metrics
                .shared_cache_total
                .with_label_values(&["hit"])
                .get(),
            4
        );
        assert_eq!(cache.metrics.wasmtime_component_cache_misses_total.get(), 0);
        // Deterministic no-space case: verified code still runs and is reused in memory.
        cache.cache.lock().unwrap().clear();
        cache.disk_budget = 1;
        crate::cache_storage::prune(&cache.wasm_cache_dir, 0, 1).unwrap();
        let component = cache
            .restore(&hashes[0], "invalid:source", Some((Some(&url), &token)))
            .await
            .ok()
            .unwrap();
        assert_eq!(std::fs::read_dir(&cache.wasm_cache_dir).unwrap().count(), 0);
        assert!(Arc::ptr_eq(
            &component,
            &cache.cached_component(&hashes[0]).ok().unwrap().unwrap()
        ));
        server.abort();
    }

    #[test]
    fn disk_eviction_preserves_recently_used_code() {
        let (_dir, cache) = fixture();
        let a = store_component(&cache, "(component)");
        let b = store_component(&cache, "(component (core module))");
        for (sha, secs) in [(&a, 0), (&b, 1)] {
            std::fs::File::open(cache.wasm_cache_dir.join(format!("{sha}.cwasm")))
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        }
        assert!(cache.cached_component(&a).ok().unwrap().is_some());
        let size = cache.disk_file(&a).ok().unwrap().unwrap().len;
        crate::cache_storage::prune(&cache.wasm_cache_dir, 0, size).unwrap();
        assert!(cache.disk_file(&a).ok().unwrap().is_some());
        assert!(cache.disk_file(&b).ok().unwrap().is_none());
    }

    fn fixture() -> (tempfile::TempDir, ArtifactCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = ArtifactCache::new(
            crate::runtime::build_engine().unwrap(),
            reqwest::Client::new(),
            dir.path().to_path_buf(),
            1,
            crate::compiler::Limits {
                memory_mib: 1024,
                timeout_secs: 60,
            },
            crate::metrics::Metrics::init(),
        )
        .unwrap();
        (dir, cache)
    }

    fn store_component(cache: &ArtifactCache, source: &str) -> String {
        let sha = hex::encode(Sha256::digest(source.as_bytes()));
        let compiled = cache
            .engine
            .precompile_component(source.as_bytes())
            .unwrap();
        std::fs::write(cache.wasm_cache_dir.join(format!("{sha}.cwasm")), compiled).unwrap();
        sha
    }

    #[tokio::test]
    async fn preparation_of_more_apps_than_fit_in_memory_preserves_hot_code() {
        let (_dir, cache) = fixture();
        // Distinct compiled components, one more than the in-memory entry limit.
        let hashes: Vec<_> = (0..=COMPONENT_CACHE_CAP)
            .map(|n| {
                store_component(
                    &cache,
                    &format!(
                "(component (core module (func (export \"value\") (result i32) i32.const {n})))"
            ),
                )
            })
            .collect();
        for sha in &hashes[..COMPONENT_CACHE_CAP] {
            assert!(cache.cached_component(sha).is_ok_and(|v| v.is_some()));
        }
        let order = || {
            cache
                .cache
                .lock()
                .unwrap()
                .iter()
                .map(|(sha, _)| sha.clone())
                .collect::<Vec<_>>()
        };
        let hot = order();
        for _ in 0..3 {
            for sha in &hashes {
                assert!(matches!(cache.is_prepared(sha), Ok(true)));
                // Any network fetch or compilation here would fail the test.
                assert!(cache.prepare(sha, "invalid:artifact", None).await.is_ok());
            }
            assert_eq!(
                order(),
                hot,
                "background checks must not evict or reorder hot apps"
            );
        }
        assert_eq!(cache.metrics.wasmtime_component_cache_misses_total.get(), 0);
        assert_eq!(
            cache
                .metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["lru"])
                .get(),
            0
        );
        assert_eq!(
            cache
                .metrics
                .wasmtime_component_cache_hits_total
                .with_label_values(&["cwasm"])
                .get(),
            COMPONENT_CACHE_CAP as u64
        );

        // Only a real request changes the working set. An evicted app can return
        // from its compiled file without fetching or compiling the source again.
        assert!(cache
            .cached_component(&hashes[COMPONENT_CACHE_CAP])
            .is_ok_and(|v| v.is_some()));
        assert!(!cache.cache.lock().unwrap().contains(&hashes[0]));
        assert!(cache
            .cached_component(&hashes[0])
            .is_ok_and(|v| v.is_some()));
        assert_eq!(cache.metrics.wasmtime_component_cache_misses_total.get(), 0);
    }

    #[tokio::test]
    async fn preparation_revalidates_changed_files_even_when_code_is_in_memory() {
        let (_dir, cache) = fixture();
        let sha = store_component(&cache, "(component)");
        let path = cache.wasm_cache_dir.join(format!("{sha}.cwasm"));
        assert!(matches!(cache.is_prepared(&sha), Ok(true)));
        assert!(cache.cached_component(&sha).is_ok_and(|v| v.is_some()));

        // Replace the file atomically, as the cache writer does. A hot memory
        // entry must not mask an incompatible/missing retained artifact during deploy.
        let replacement = cache.wasm_cache_dir.join("replacement");
        let incompatible = Engine::default()
            .precompile_component(b"(component)")
            .unwrap();
        std::fs::write(&replacement, incompatible).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(matches!(cache.is_prepared(&sha), Ok(false)));
        assert!(cache.cached_component(&sha).is_ok_and(|v| v.is_some()));

        store_component(&cache, "(component)");
        assert!(matches!(cache.is_prepared(&sha), Ok(true)));
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(cache.is_prepared(&sha), Ok(false)));
        assert!(cache.is_prepared("../../outside").is_err());

        // Existing code must still link against the Worker's supported imports.
        let unsupported =
            store_component(&cache, "(component (import \"unsupported-host\" (func)))");
        assert!(cache.is_prepared(&unsupported).is_err());
        std::os::unix::fs::symlink(
            cache.wasm_cache_dir.join(format!("{unsupported}.cwasm")),
            &path,
        )
        .unwrap();
        assert!(cache.is_prepared(&sha).is_err());
    }
}
