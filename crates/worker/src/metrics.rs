use std::sync::Arc;

use hibana_shared::metrics::{default_latency_buckets, register, render};
use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
};

pub struct Metrics {
    pub registry: Registry,
    pub guest_memory_budget_bytes: IntGauge,
    pub guest_memory_reserved_bytes: IntGauge,
    pub capacity_rejections_total: IntCounterVec,
    pub active_compilations: IntGauge,
    pub wasmtime_execution_duration_seconds: HistogramVec,
    pub wasmtime_component_cache_hits_total: IntCounterVec,
    pub wasmtime_component_cache_misses_total: IntCounter,
    pub executions_total: IntCounterVec,
    pub guest_log_dropped_bytes_total: IntCounter,
    pub inflight_executions: IntGauge,
    pub control_plane_request_duration_seconds: HistogramVec,
}

impl Metrics {
    pub fn init() -> Arc<Self> {
        let registry = Registry::new();
        let control_plane_request_duration_seconds = register(&registry, HistogramVec::new(
            HistogramOpts::new(
                "hibana_worker_control_plane_request_duration_seconds",
                "Internal control-plane requests including response body, by operation and outcome",
            )
            .buckets(default_latency_buckets()),
            &["operation", "outcome"],
        ));

        let wasmtime_execution_duration_seconds = register(
            &registry,
            HistogramVec::new(
                HistogramOpts::new(
                    "wasmtime_execution_duration_seconds",
                    "Wasm component execution wall-clock duration in seconds",
                )
                .buckets(default_latency_buckets()),
                &["outcome"],
            ),
        );

        let wasmtime_component_cache_hits_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "wasmtime_component_cache_hits_total",
                    "Component cache hits broken down by tier (lru / cwasm)",
                ),
                &["tier"],
            ),
        );

        let wasmtime_component_cache_misses_total = register(
            &registry,
            IntCounter::new(
                "wasmtime_component_cache_misses_total",
                "Component cache misses (forced download + precompile)",
            ),
        );

        let executions_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "executions_total",
                    "Total executions handled by this worker, by outcome",
                ),
                &["outcome"],
            ),
        );

        let guest_log_dropped_bytes_total = register(
            &registry,
            IntCounter::new(
                "faas_guest_log_dropped_bytes_total",
                "Guest stdout/stderr bytes discarded after the per-execution log capture limit",
            ),
        );

        let inflight_executions = register(
            &registry,
            IntGauge::new(
                "wasmtime_inflight_executions",
                "Executions currently running in this worker process",
            ),
        );

        let guest_memory_budget_bytes = register(
            &registry,
            IntGauge::new(
                "hibana_worker_guest_memory_budget_bytes",
                "Configured guest linear-memory reservation budget",
            ),
        );
        let guest_memory_reserved_bytes = register(
            &registry,
            IntGauge::new(
                "hibana_worker_guest_memory_reserved_bytes",
                "Reserved guest linear memory, rounded up to MiB",
            ),
        );
        let active_compilations = register(
            &registry,
            IntGauge::new(
                "hibana_worker_active_compilations",
                "Currently compiling components",
            ),
        );
        let capacity_rejections_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "hibana_worker_capacity_rejections_total",
                    "Pre-execution capacity refusals",
                ),
                &["reason"],
            ),
        );
        Arc::new(Self {
            control_plane_request_duration_seconds,
            guest_memory_budget_bytes,
            guest_memory_reserved_bytes,
            active_compilations,
            capacity_rejections_total,
            registry,
            wasmtime_execution_duration_seconds,
            wasmtime_component_cache_hits_total,
            wasmtime_component_cache_misses_total,
            executions_total,
            guest_log_dropped_bytes_total,
            inflight_executions,
        })
    }

    pub fn render(&self) -> (axum::http::HeaderMap, String) {
        render(&self.registry)
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
        m.guest_memory_reserved_bytes.set(1_048_576);
        let (headers, body) = m.render();
        assert_eq!(
            headers[axum::http::header::CONTENT_TYPE],
            "text/plain; version=0.0.4"
        );
        assert!(body.contains("executions_total{outcome=\"succeeded\"} 1"));
        assert!(body.contains("wasmtime_component_cache_hits_total{tier=\"lru\"} 1"));
        assert!(body.contains("hibana_worker_guest_memory_reserved_bytes 1048576"));
        assert!(!body.contains("wasmtime_memory_pages"));
    }
}
