//! Approved outbound HTTP with DNS/IP checks, pinned connections and bounded buffering.
use futures::StreamExt;
use http_body_util::{BodyExt, Full, Limited};
use std::sync::Arc;
use wasmtime_wasi_http::body::HyperOutgoingBody;
pub(super) async fn gated_send_request(
    request: hyper::Request<HyperOutgoingBody>,
    config: wasmtime_wasi_http::types::OutgoingRequestConfig,
    allowed: Arc<std::collections::HashSet<std::net::SocketAddr>>,
) -> Result<
    wasmtime_wasi_http::types::IncomingResponse,
    wasmtime_wasi_http::bindings::http::types::ErrorCode,
> {
    use wasmtime_wasi_http::bindings::http::types::ErrorCode;

    if allowed.is_empty() {
        return Err(ErrorCode::HttpRequestDenied);
    }

    let (parts, body) = request.into_parts();
    let use_tls = config.use_tls;
    let host = match parts.uri.host() {
        Some(h) => h.to_string(),
        None => return Err(ErrorCode::HttpRequestUriInvalid),
    };
    let port = parts
        .uri
        .port_u16()
        .unwrap_or(if use_tls { 443 } else { 80 });

    let mut pinned: Option<std::net::SocketAddr> = None;
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addrs) => {
            for addr in addrs {
                if faas_shared::egress::is_hard_denied(addr.ip()) {
                    continue;
                }
                if allowed.contains(&addr) {
                    pinned = Some(addr);
                    break;
                }
            }
        }
        Err(_) => return Err(ErrorCode::HttpRequestDenied),
    }
    let pinned = match pinned {
        Some(a) => a,
        None => return Err(ErrorCode::HttpRequestDenied),
    };

    let body_bytes = Limited::new(body, 8 * 1024 * 1024)
        .collect()
        .await
        .map_err(|_| ErrorCode::HttpRequestBodySize(None))?
        .to_bytes();

    let scheme = if use_tls { "https" } else { "http" };
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{scheme}://{host}:{port}{path}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, pinned)
        .connect_timeout(config.connect_timeout)
        .build()
        .map_err(|e| ErrorCode::InternalError(Some(format!("client build: {e}"))))?;

    let mut rb = client
        .request(parts.method.clone(), url.as_str())
        .body(body_bytes.to_vec());
    for (k, v) in parts.headers.iter() {
        if k == hyper::header::HOST {
            continue;
        }
        rb = rb.header(k.clone(), v.clone());
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| ErrorCode::InternalError(Some(format!("egress send failed: {e}"))))?;

    let status = resp.status();
    let headers = resp.headers().clone();
    let mut resp_body = bytes::BytesMut::new();
    let mut chunks = resp.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| ErrorCode::HttpResponseBodySize(None))?;
        if resp_body.len() + chunk.len() > 64 * 1024 * 1024 {
            return Err(ErrorCode::HttpResponseBodySize(None));
        }
        resp_body.extend_from_slice(&chunk);
    }
    let resp_body = resp_body.freeze();

    let mut builder = hyper::Response::builder().status(status);
    if let Some(h) = builder.headers_mut() {
        *h = headers;
    }
    let hy_body = Full::new(resp_body).map_err(|e| match e {}).boxed();
    let resp = builder
        .body(hy_body)
        .map_err(|e| ErrorCode::InternalError(Some(format!("resp build: {e}"))))?;

    Ok(wasmtime_wasi_http::types::IncomingResponse {
        resp,
        worker: None,
        between_bytes_timeout: config.between_bytes_timeout,
    })
}
