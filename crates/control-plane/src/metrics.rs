#![allow(dead_code)]

use std::sync::Arc;

use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry, TextEncoder,
};

pub struct Metrics {
    pub registry: Registry,

    pub http_requests_total: IntCounterVec,
    pub http_request_duration_seconds: HistogramVec,

    pub executions_total: IntCounterVec,
    pub execution_duration_seconds: HistogramVec,
    pub tenant_invoke_total: IntCounterVec,

    pub admission_rejections_total: IntCounterVec,
    pub reaper_swept_total: IntCounter,
    pub reaper_tenants_last: IntGauge,

    pub secret_material_issued_total: IntCounterVec,
    pub secret_versions_by_kid: IntGaugeVec,
}

impl Metrics {
    pub fn init() -> Arc<Self> {
        let registry = Registry::new();

        let http_requests_total = IntCounterVec::new(
            Opts::new("faas_http_requests_total", "Total HTTP requests received"),
            &["method", "path", "status"],
        )
        .expect("metric: http_requests_total");
        registry
            .register(Box::new(http_requests_total.clone()))
            .expect("register http_requests_total");

        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "faas_http_request_duration_seconds",
                "HTTP request handler duration in seconds",
            )
            .buckets(default_latency_buckets()),
            &["method", "path"],
        )
        .expect("metric: http_request_duration_seconds");
        registry
            .register(Box::new(http_request_duration_seconds.clone()))
            .expect("register http_request_duration_seconds");

        let executions_total = IntCounterVec::new(
            Opts::new(
                "faas_executions_total",
                "Total executions finalized (succeeded/failed/timeout)",
            ),
            &["status"],
        )
        .expect("metric: executions_total");
        registry
            .register(Box::new(executions_total.clone()))
            .expect("register executions_total");

        let execution_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "faas_execution_duration_seconds",
                "End-to-end execution duration (created_at to finished_at) in seconds",
            )
            .buckets(default_latency_buckets()),
            &["status"],
        )
        .expect("metric: execution_duration_seconds");
        registry
            .register(Box::new(execution_duration_seconds.clone()))
            .expect("register execution_duration_seconds");

        let tenant_invoke_total = IntCounterVec::new(
            Opts::new(
                "faas_tenant_invoke_total",
                "HTTP requests received by tenant",
            ),
            &["tenant_id"],
        )
        .expect("metric: tenant_invoke_total");
        registry
            .register(Box::new(tenant_invoke_total.clone()))
            .expect("register tenant_invoke_total");

        let admission_rejections_total = IntCounterVec::new(
            Opts::new(
                "faas_admission_rejections_total",
                "Admission rejections (429) by reason",
            ),
            &["kind"],
        )
        .expect("metric: admission_rejections_total");
        registry
            .register(Box::new(admission_rejections_total.clone()))
            .expect("register admission_rejections_total");

        let reaper_swept_total = IntCounter::new(
            "faas_reaper_swept_total",
            "Number of stuck pending/running rows finalized to 'failed' by reaper",
        )
        .expect("metric: reaper_swept_total");
        registry
            .register(Box::new(reaper_swept_total.clone()))
            .expect("register reaper_swept_total");

        let reaper_tenants_last = IntGauge::new(
            "faas_reaper_tenants_last",
            "Number of active tenants processed in the most recent reaper pass",
        )
        .expect("metric: reaper_tenants_last");
        registry
            .register(Box::new(reaper_tenants_last.clone()))
            .expect("register reaper_tenants_last");

        let secret_material_issued_total = IntCounterVec::new(
            Opts::new(
                "faas_secret_material_issued_total",
                "Secret material exchanges served on the internal job-env endpoint by outcome",
            ),
            &["outcome"],
        )
        .expect("metric: secret_material_issued_total");
        registry
            .register(Box::new(secret_material_issued_total.clone()))
            .expect("register secret_material_issued_total");

        let secret_versions_by_kid = IntGaugeVec::new(
            Opts::new(
                "faas_secret_versions_by_kid",
                "Live secrets whose current generation is wrapped by each KEK kid",
            ),
            &["kid"],
        )
        .expect("metric: secret_versions_by_kid");
        registry
            .register(Box::new(secret_versions_by_kid.clone()))
            .expect("register secret_versions_by_kid");

        Arc::new(Self {
            registry,
            http_requests_total,
            http_request_duration_seconds,
            executions_total,
            execution_duration_seconds,
            tenant_invoke_total,
            admission_rejections_total,
            reaper_swept_total,
            reaper_tenants_last,
            secret_material_issued_total,
            secret_versions_by_kid,
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

    pub fn observe_http(&self, method: &str, path: &str, status: u16, dur: std::time::Duration) {
        self.http_request_duration_seconds
            .with_label_values(&[method, path])
            .observe(dur.as_secs_f64());
        let status_str = status.to_string();
        self.http_requests_total
            .with_label_values(&[method, path, status_str.as_str()])
            .inc();
    }
}

pub fn default_latency_buckets() -> Vec<f64> {
    vec![
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    ]
}

pub fn observe_secs(h: &Histogram, dur: std::time::Duration) {
    h.observe(dur.as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_and_render_round_trip() {
        let m = Metrics::init();
        m.executions_total.with_label_values(&["succeeded"]).inc();
        m.tenant_invoke_total.with_label_values(&["ten_a"]).inc();
        let (_headers, body) = m.render();
        assert!(body.contains("faas_executions_total"));
        assert!(body.contains("faas_tenant_invoke_total"));
        assert!(body.contains("status=\"succeeded\""));
    }

    #[test]
    fn latency_buckets_are_monotonic() {
        let b = default_latency_buckets();
        assert!(!b.is_empty());
        assert!(b.windows(2).all(|w| w[0] < w[1]));
    }
}
