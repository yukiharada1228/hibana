use std::sync::Arc;

use hibana_shared::metrics::{default_latency_buckets, register, render};
use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
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

        let http_requests_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new("faas_http_requests_total", "Total HTTP requests received"),
                &["method", "path", "status"],
            ),
        );

        let http_request_duration_seconds = register(
            &registry,
            HistogramVec::new(
                HistogramOpts::new(
                    "faas_http_request_duration_seconds",
                    "HTTP request handler duration in seconds",
                )
                .buckets(default_latency_buckets()),
                &["method", "path"],
            ),
        );

        let executions_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "faas_executions_total",
                    "Total executions finalized (succeeded/failed/timeout)",
                ),
                &["status"],
            ),
        );

        let execution_duration_seconds = register(
            &registry,
            HistogramVec::new(
                HistogramOpts::new(
                    "faas_execution_duration_seconds",
                    "End-to-end execution duration (created_at to finished_at) in seconds",
                )
                .buckets(default_latency_buckets()),
                &["status"],
            ),
        );

        let tenant_invoke_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "faas_tenant_invoke_total",
                    "HTTP requests received by tenant",
                ),
                &["tenant_id"],
            ),
        );

        let admission_rejections_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "faas_admission_rejections_total",
                    "Admission rejections (429) by reason",
                ),
                &["kind"],
            ),
        );

        let reaper_swept_total = register(
            &registry,
            IntCounter::new(
                "faas_reaper_swept_total",
                "Number of stuck pending/running rows finalized to 'failed' by reaper",
            ),
        );

        let reaper_tenants_last = register(
            &registry,
            IntGauge::new(
                "faas_reaper_tenants_last",
                "Number of active tenants processed in the most recent reaper pass",
            ),
        );

        let secret_material_issued_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "faas_secret_material_issued_total",
                    "Secret material exchanges served on the internal job-env endpoint by outcome",
                ),
                &["outcome"],
            ),
        );

        let secret_versions_by_kid = register(
            &registry,
            IntGaugeVec::new(
                Opts::new(
                    "faas_secret_versions_by_kid",
                    "Live secrets whose current generation is wrapped by each KEK kid",
                ),
                &["kid"],
            ),
        );

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
        render(&self.registry)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_and_render_round_trip() {
        let m = Metrics::init();
        m.executions_total.with_label_values(&["succeeded"]).inc();
        m.tenant_invoke_total.with_label_values(&["ten_a"]).inc();
        m.observe_http(
            "GET",
            "/components",
            200,
            std::time::Duration::from_millis(25),
        );
        let (headers, body) = m.render();
        assert_eq!(
            headers[axum::http::header::CONTENT_TYPE],
            "text/plain; version=0.0.4"
        );
        assert!(body.contains("faas_executions_total{status=\"succeeded\"} 1"));
        assert!(body.contains("faas_tenant_invoke_total{tenant_id=\"ten_a\"} 1"));
        assert!(body.contains(
            "faas_http_requests_total{method=\"GET\",path=\"/components\",status=\"200\"} 1"
        ));
        assert!(body.contains(
            "faas_http_request_duration_seconds_count{method=\"GET\",path=\"/components\"} 1"
        ));
    }

    #[test]
    fn latency_buckets_are_monotonic() {
        let b = default_latency_buckets();
        assert!(!b.is_empty());
        assert!(b.windows(2).all(|w| w[0] < w[1]));
    }
}
