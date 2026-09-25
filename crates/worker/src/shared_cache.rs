//! Optional authenticated native-code reuse. A miss always falls back to source Wasm.
use axum::http::HeaderValue;
use hibana_shared::compiled_cache::{self as protocol, Auth};
use sha2::{Digest, Sha256};
use std::{
    hash::{Hash, Hasher},
    sync::Arc,
    time::Duration,
};

pub(crate) struct SharedCache {
    auth: Auth,
    runtime: String,
    http: reqwest::Client,
    upload_url: String,
    metrics: Arc<crate::metrics::Metrics>,
}

impl SharedCache {
    pub(crate) fn new(
        auth: Auth,
        engine: &wasmtime::Engine,
        http: reqwest::Client,
        control_plane: &str,
        metrics: Arc<crate::metrics::Metrics>,
    ) -> Self {
        Self {
            auth,
            runtime: runtime_id(engine),
            http,
            upload_url: format!(
                "{}/internal/compiled-artifact",
                control_plane.trim_end_matches('/')
            ),
            metrics,
        }
    }

    pub(crate) fn runtime(&self) -> &str {
        &self.runtime
    }

    pub(crate) async fn fetch(&self, sha: &str, url: &str) -> Option<Vec<u8>> {
        let result = self.fetch_inner(sha, url).await;
        self.record(match &result {
            Ok(_) => "hit",
            Err(reason) => reason,
        });
        result.ok()
    }

    async fn fetch_inner(&self, sha: &str, url: &str) -> Result<Vec<u8>, &'static str> {
        let mut response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| "unavailable")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err("miss");
        }
        if !response.status().is_success() {
            return Err("unavailable");
        }
        if response
            .content_length()
            .is_some_and(|n| n > protocol::MAX_OBJECT_BYTES as u64)
        {
            return Err("invalid");
        }
        let mut bytes = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        while let Some(chunk) = response.chunk().await.map_err(|_| "unavailable")? {
            if bytes.len().saturating_add(chunk.len()) > protocol::MAX_OBJECT_BYTES {
                return Err("invalid");
            }
            bytes.extend_from_slice(&chunk);
        }
        // Never pass unauthenticated bytes to Wasmtime's unsafe deserializer.
        self.auth
            .verify(&self.runtime, sha, &bytes)
            .map_err(|_| "invalid")?;
        bytes.truncate(bytes.len() - protocol::TAG_BYTES);
        Ok(bytes)
    }

    pub(crate) async fn publish(&self, sha: &str, token: &HeaderValue, mut bytes: Vec<u8>) {
        if self.auth.seal(&self.runtime, sha, &mut bytes).is_err() {
            return;
        }
        let response = self
            .http
            .post(&self.upload_url)
            .header(hibana_shared::preparation::TOKEN_HEADER, token)
            .header(protocol::RUNTIME_HEADER, &self.runtime)
            .timeout(Duration::from_secs(15))
            .body(bytes)
            .send()
            .await;
        self.record(if response.is_ok_and(|r| r.status().is_success()) {
            "stored"
        } else {
            "store_failed"
        });
    }

    fn record(&self, outcome: &'static str) {
        self.metrics
            .shared_cache_total
            .with_label_values(&[outcome])
            .inc();
        tracing::debug!(outcome, "shared compiled cache");
    }
}

fn runtime_id(engine: &wasmtime::Engine) -> String {
    // Wasmtime's compatibility hash includes its version, target and Engine config.
    struct CompatibilityHasher(Sha256);
    impl Hasher for CompatibilityHasher {
        fn write(&mut self, bytes: &[u8]) {
            self.0.update(bytes);
        }
        fn finish(&self) -> u64 {
            u64::from_le_bytes(self.0.clone().finalize()[..8].try_into().unwrap())
        }
    }
    let mut hash = CompatibilityHasher(Sha256::new());
    hash.write(b"hibana-native-compatibility-v1\0");
    engine.precompile_compatibility_hash().hash(&mut hash);
    hex::encode(hash.0.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_key_is_stable_and_changes_with_engine_settings() {
        let first = crate::runtime::build_engine().unwrap();
        let second = crate::runtime::build_engine().unwrap();
        assert_eq!(runtime_id(&first), runtime_id(&second));
        assert!(protocol::valid_id(&runtime_id(&first)));
        assert_ne!(runtime_id(&first), runtime_id(&wasmtime::Engine::default()));
    }

    #[tokio::test]
    async fn only_authenticated_compatible_bytes_are_accepted() {
        use axum::{routing::get, Router};
        let auth = Auth::from_hex(&"ab".repeat(32)).unwrap();
        let engine = crate::runtime::build_engine().unwrap();
        let source = b"(component)";
        let sha = hex::encode(Sha256::digest(source));
        let mut signed = engine.precompile_component(source).unwrap();
        auth.seal(&runtime_id(&engine), &sha, &mut signed).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let mut corrupt = signed.clone();
        corrupt[0] ^= 1;
        let app = Router::new()
            .route(
                "/valid",
                get(move || {
                    let bytes = signed.clone();
                    async { bytes }
                }),
            )
            .route(
                "/corrupt",
                get(move || {
                    let bytes = corrupt.clone();
                    async { bytes }
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cache = SharedCache::new(
            auth,
            &engine,
            reqwest::Client::builder().no_proxy().build().unwrap(),
            &url,
            crate::metrics::Metrics::init(),
        );
        let native = cache.fetch(&sha, &format!("{url}/valid")).await.unwrap();
        // SAFETY: authenticated output of this test's compatible Engine.
        assert!(unsafe { wasmtime::component::Component::deserialize(&engine, native) }.is_ok());
        assert!(cache.fetch(&sha, &format!("{url}/corrupt")).await.is_none());
        assert!(cache
            .fetch(&"cd".repeat(32), &format!("{url}/valid"))
            .await
            .is_none());
        assert!(cache.fetch(&sha, &format!("{url}/missing")).await.is_none());
        server.abort();
    }
}
