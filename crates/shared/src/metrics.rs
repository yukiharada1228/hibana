//! Prometheus registration and text encoding shared by both runtime processes.

use http::{header::CONTENT_TYPE, HeaderMap, HeaderValue};
use prometheus::{core::Collector, Encoder, Registry, TextEncoder};

pub fn register<T>(registry: &Registry, metric: Result<T, prometheus::Error>) -> T
where
    T: Collector + Clone + 'static,
{
    let metric = metric.expect("create metric");
    registry
        .register(Box::new(metric.clone()))
        .expect("register metric");
    metric
}

pub fn default_latency_buckets() -> Vec<f64> {
    vec![
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
    ]
}

pub fn render(registry: &Registry) -> (HeaderMap, String) {
    let mut buf = Vec::new();
    let encoder = TextEncoder::new();
    if let Err(error) = encoder.encode(&registry.gather(), &mut buf) {
        tracing::warn!(%error, "failed to encode prometheus metrics");
    }
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(encoder.format_type()) {
        headers.insert(CONTENT_TYPE, value);
    }
    (headers, String::from_utf8(buf).unwrap_or_default())
}
