#![allow(dead_code)]

use std::sync::Arc;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

fn default_latency_buckets() -> Vec<f64> {
    vec![
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    ]
}

pub struct Metrics {
    pub registry: Registry,
    pub wasmtime_execution_duration_seconds: HistogramVec,
    pub wasmtime_memory_pages: IntGauge,
    pub wasmtime_component_cache_hits_total: IntCounterVec,
    pub wasmtime_component_cache_misses_total: IntCounter,
    pub executions_total: IntCounterVec,
    pub guest_stderr_dropped_bytes_total: IntCounter,
    pub inflight_executions: IntGauge,
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

        let guest_stderr_dropped_bytes_total = IntCounter::new(
            "faas_guest_stderr_dropped_bytes_total",
            "Guest stderr bytes discarded because the execution had secrets injected",
        )
        .expect("metric: guest_stderr_dropped_bytes_total");
        registry
            .register(Box::new(guest_stderr_dropped_bytes_total.clone()))
            .expect("register guest_stderr_dropped_bytes_total");

        let inflight_executions = IntGauge::new(
            "wasmtime_inflight_executions",
            "Executions currently running in this worker process",
        )
        .expect("metric: inflight_executions");
        registry
            .register(Box::new(inflight_executions.clone()))
            .expect("register inflight_executions");

        Arc::new(Self {
            registry,
            wasmtime_execution_duration_seconds,
            wasmtime_memory_pages,
            wasmtime_component_cache_hits_total,
            wasmtime_component_cache_misses_total,
            executions_total,
            guest_stderr_dropped_bytes_total,
            inflight_executions,
        })
    }

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
