//! Worker メトリクス (M4a, §3.8) — Prometheus exposition + /readyz。
//!
//! Worker は HTTP サーバを持たないため、観測のために最小 axum サーバを 1 本 spawn する
//! （`METRICS_BIND_ADDR`、既定 `0.0.0.0:9090`）。これは JetStream pull ループから完全に独立した
//! tokio task で動かす。pull ループが詰まっても liveness/metrics が応答できるようにするための
//! 分離である（§3.8）。
//!
//! 命名は control-plane 側と整合させ、worker 固有のメトリクスは `wasmtime_` プレフィックスを
//! つける（仕様書 §3.8 の例示に合わせる）。
//!
//! このスライスでは:
//! - `wasmtime_execution_duration_seconds`（histogram, labels: outcome）
//! - `wasmtime_memory_pages`（gauge; 最終実行の Wasm linear memory ページ数）
//! - `wasmtime_component_cache_hits_total` / `..._misses_total`（counter; LRU + cwasm）
//! - `executions_total`（counter, labels: outcome=succeeded|failed|timeout）
//!
//! を登録する。実コードへの計装は最小限（run_component 終了時 / cache hit/miss 経路）。
//!
//! 注: `wasmtime_memory_pages` は本スライスでは observe しない（Wasmtime の Store から実 page を
//! 取り出す経路の追加が後続）。登録だけは残して /metrics に固定で見えるようにする（dashboards 安定性）。
#![allow(dead_code)]

use std::sync::Arc;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

/// p50/p99 を取れる汎用バケット（秒, §3.8）。control-plane と同帯。
fn default_latency_buckets() -> Vec<f64> {
    vec![
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    ]
}

/// Worker の観測メトリクス一式。
pub struct Metrics {
    pub registry: Registry,
    /// 1 実行の wall-clock duration（秒）。labels: outcome (succeeded/failed/timeout)。
    pub wasmtime_execution_duration_seconds: HistogramVec,
    /// 直近実行の linear memory ページ数（gauge）。1 page = 64KiB。後続で `Store` の現在
    /// メモリサイズから取り出して set する。本スライスでは登録のみ（observe は将来配線）。
    pub wasmtime_memory_pages: IntGauge,
    /// in-memory LRU + cwasm hit 数。labels: tier ("lru" / "cwasm")。
    pub wasmtime_component_cache_hits_total: IntCounterVec,
    /// in-memory LRU miss → ダウンロード + 事前コンパイル発生数。
    pub wasmtime_component_cache_misses_total: IntCounter,
    /// 終端化件数（worker 視点）。labels: outcome (succeeded/failed/timeout)。
    /// CP 側の executions_total と命名は同じだが、こちらは「worker が結果 publish に到達した」
    /// 視点での計上（subscriber の CAS 確定とはタイミングが違う点に注意）。
    pub executions_total: IntCounterVec,
    /// DLQ (`.failed`) に publish した最終配送失敗の累計 (M4c, §6.6)。labels: outcome
    /// (`published` = `.failed` への publish に成功 / `publish_failed` = publish 自体が失敗、
    /// reaper の stuck-execution sweeper に救済を委ねる)。
    pub dlq_published_total: IntCounterVec,
}

impl Metrics {
    pub fn init() -> Arc<Self> {
        let registry = Registry::new();

        let wasmtime_execution_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "wasmtime_execution_duration_seconds",
                "Wasm component execution wall-clock duration in seconds",
            )
            .buckets(default_latency_buckets()),
            &["outcome"],
        )
        .expect("metric: wasmtime_execution_duration_seconds");
        registry
            .register(Box::new(wasmtime_execution_duration_seconds.clone()))
            .expect("register wasmtime_execution_duration_seconds");

        let wasmtime_memory_pages = IntGauge::new(
            "wasmtime_memory_pages",
            "Last observed Wasm linear memory size in pages (1 page = 64KiB)",
        )
        .expect("metric: wasmtime_memory_pages");
        registry
            .register(Box::new(wasmtime_memory_pages.clone()))
            .expect("register wasmtime_memory_pages");

        let wasmtime_component_cache_hits_total = IntCounterVec::new(
            Opts::new(
                "wasmtime_component_cache_hits_total",
                "Component cache hits broken down by tier (lru / cwasm)",
            ),
            &["tier"],
        )
        .expect("metric: wasmtime_component_cache_hits_total");
        registry
            .register(Box::new(wasmtime_component_cache_hits_total.clone()))
            .expect("register wasmtime_component_cache_hits_total");

        let wasmtime_component_cache_misses_total = IntCounter::new(
            "wasmtime_component_cache_misses_total",
            "Component cache misses (forced download + precompile)",
        )
        .expect("metric: wasmtime_component_cache_misses_total");
        registry
            .register(Box::new(wasmtime_component_cache_misses_total.clone()))
            .expect("register wasmtime_component_cache_misses_total");

        let executions_total = IntCounterVec::new(
            Opts::new(
                "executions_total",
                "Total executions handled by this worker, by outcome",
            ),
            &["outcome"],
        )
        .expect("metric: executions_total");
        registry
            .register(Box::new(executions_total.clone()))
            .expect("register executions_total");

        let dlq_published_total = IntCounterVec::new(
            Opts::new(
                "dlq_published_total",
                "Final-delivery failures emitted by worker to the .failed (DLQ) subject (M4c)",
            ),
            &["outcome"],
        )
        .expect("metric: dlq_published_total");
        registry
            .register(Box::new(dlq_published_total.clone()))
            .expect("register dlq_published_total");

        Arc::new(Self {
            registry,
            wasmtime_execution_duration_seconds,
            wasmtime_memory_pages,
            wasmtime_component_cache_hits_total,
            wasmtime_component_cache_misses_total,
            executions_total,
            dlq_published_total,
        })
    }

    /// `/metrics` ハンドラ本体（text/plain Prometheus exposition）。
    pub fn render(&self) -> (axum::http::HeaderMap, String) {
        let metric_families = self.registry.gather();
        let mut buf = Vec::new();
        let encoder = TextEncoder::new();
        if let Err(e) = encoder.encode(&metric_families, &mut buf) {
            tracing::warn!(error = %e, "failed to encode prometheus metrics");
        }
        let body = String::from_utf8(buf).unwrap_or_default();
        let mut headers = axum::http::HeaderMap::new();
        if let Ok(v) = axum::http::HeaderValue::from_str(TextEncoder::new().format_type()) {
            headers.insert(axum::http::header::CONTENT_TYPE, v);
        }
        (headers, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_metrics_init_and_render() {
        let m = Metrics::init();
        m.executions_total.with_label_values(&["succeeded"]).inc();
        m.wasmtime_component_cache_hits_total
            .with_label_values(&["lru"])
            .inc();
        let (_h, body) = m.render();
        assert!(body.contains("executions_total"));
        assert!(body.contains("wasmtime_component_cache_hits_total"));
    }
}
