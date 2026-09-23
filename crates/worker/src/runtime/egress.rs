//! Approved outbound HTTP with DNS/IP checks, pinned connections and bounded buffering.
use super::{
    http_buffer::{Budget, Buffer},
    ApprovedEgress,
};
use futures::StreamExt;
use http_body_util::{BodyExt, Full, Limited};
use std::{net::SocketAddr, sync::Arc};
use tokio::time::{timeout_at, Instant};
use wasmtime_wasi_http::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::body::HyperOutgoingBody;
use wasmtime_wasi_http::types::{IncomingResponse, OutgoingRequestConfig};
pub(super) async fn gated_send_request(
    request: hyper::Request<HyperOutgoingBody>,
    config: OutgoingRequestConfig,
    allowed: Arc<ApprovedEgress>,
    budget: Budget,
) -> Result<IncomingResponse, ErrorCode> {
    let pinned = approved_destinations(request.uri(), &config, &allowed)?;
    send_pinned_request(request, config, &pinned, budget).await
}

fn approved_destinations(
    uri: &hyper::Uri,
    config: &OutgoingRequestConfig,
    allowed: &ApprovedEgress,
) -> Result<Vec<SocketAddr>, ErrorCode> {
    let host = uri.host().ok_or(ErrorCode::HttpRequestUriInvalid)?;
    let port = uri
        .port_u16()
        .unwrap_or(if config.use_tls { 443 } else { 80 });
    // Never resolve a guest-provided hostname through the OS. HTTP and WASI
    // sockets consume the same per-invocation snapshot of approved names/IPs.
    let addresses: Vec<_> = allowed
        .resolve(host)
        .filter(|addr| addr.port() == port && !hibana_shared::egress::is_hard_denied(addr.ip()))
        .collect();
    if addresses.is_empty() {
        return Err(ErrorCode::HttpRequestDenied);
    }
    Ok(addresses)
}

// Only called after the DNS/IP policy check. Keeping transport separate lets
// tests use a loopback fixture without weakening the production allowlist.
async fn send_pinned_request(
    request: hyper::Request<HyperOutgoingBody>,
    config: OutgoingRequestConfig,
    pinned: &[SocketAddr],
    budget: Budget,
) -> Result<IncomingResponse, ErrorCode> {
    super::http_limits::check(request.headers())
        .map_err(|_| ErrorCode::HttpRequestHeaderSize(None))?;
    let (parts, body) = request.into_parts();
    let host = parts.uri.host().ok_or(ErrorCode::HttpRequestUriInvalid)?;
    let use_tls = config.use_tls;
    let port = pinned.first().ok_or(ErrorCode::HttpRequestDenied)?.port();

    let mut body = Limited::new(body, 8 * 1024 * 1024);
    let mut body_bytes = Buffer::new(budget.clone());
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ErrorCode::HttpRequestBodySize(None))?;
        if let Ok(chunk) = frame.into_data() {
            body_bytes.extend(&chunk)?;
        }
    }
    let body_bytes = body_bytes.into_bytes();

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
        // Let the connector try every approved IP before sending the request.
        // Keep the hostname for Host/TLS; never re-resolve or replay HTTP here.
        .resolve_to_addrs(host, pinned)
        .connect_timeout(config.connect_timeout)
        .build()
        .map_err(|e| ErrorCode::InternalError(Some(format!("client build: {e}"))))?;

    let mut rb = client
        .request(parts.method.clone(), url.as_str())
        .body(body_bytes);
    for (k, v) in parts.headers.iter() {
        if k == hyper::header::HOST {
            continue;
        }
        rb = rb.header(k.clone(), v.clone());
    }
    // Bound header reception and the first body bytes by one deadline. An idle
    // deadline then restarts only when the upstream supplies another chunk.
    let first_byte_deadline = Instant::now() + config.first_byte_timeout;
    let resp = timeout_at(first_byte_deadline, rb.send())
        .await
        .map_err(|_| ErrorCode::ConnectionReadTimeout)?
        .map_err(|e| {
            if e.is_timeout() {
                ErrorCode::ConnectionTimeout
            } else {
                ErrorCode::InternalError(Some(format!("egress send failed: {e}")))
            }
        })?;

    let status = resp.status();
    super::http_limits::check(resp.headers()).map_err(|_| {
        ErrorCode::HttpResponseHeaderSize(
            wasmtime_wasi_http::bindings::http::types::FieldSizePayload {
                field_name: None,
                field_size: None,
            },
        )
    })?;
    let headers = resp.headers().clone();
    let mut resp_body = Buffer::new(budget);
    let mut chunks = resp.bytes_stream();
    let mut deadline = first_byte_deadline;
    while let Some(chunk) = timeout_at(deadline, chunks.next())
        .await
        .map_err(|_| ErrorCode::ConnectionReadTimeout)?
    {
        let chunk = chunk.map_err(|_| ErrorCode::HttpResponseBodySize(None))?;
        if resp_body.len() + chunk.len() > 64 * 1024 * 1024 {
            return Err(ErrorCode::HttpResponseBodySize(None));
        }
        resp_body.extend(&chunk)?;
        deadline = Instant::now() + config.between_bytes_timeout;
    }
    let resp_body = resp_body.into_bytes();

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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    fn test_budget() -> Budget {
        Budget::new(super::super::http_buffer::worker_budget())
    }

    fn config(use_tls: bool) -> OutgoingRequestConfig {
        OutgoingRequestConfig {
            use_tls,
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            between_bytes_timeout: Duration::from_secs(1),
        }
    }

    const POST_BODY: &[u8] = b"fallback-probe";

    fn post_request(port: u16) -> hyper::Request<HyperOutgoingBody> {
        hyper::Request::builder()
            .method("POST")
            .uri(format!("http://fixture.invalid:{port}/probe?value=1"))
            .body(
                Full::new(bytes::Bytes::from_static(POST_BODY))
                    .map_err(|e| match e {})
                    .boxed(),
            )
            .unwrap()
    }

    async fn read_post(socket: &mut tokio::net::TcpStream, port: u16) {
        let mut received = Vec::new();
        let mut buf = [0; 1024];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "peer closed before sending the request");
            received.extend_from_slice(&buf[..n]);
            assert!(received.len() < 4096);
            if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                if received.len() >= end + 4 + POST_BODY.len() {
                    let headers = std::str::from_utf8(&received[..end + 4]).unwrap();
                    assert!(headers.starts_with("POST /probe?value=1 HTTP/1.1\r\n"));
                    assert!(headers
                        .to_ascii_lowercase()
                        .contains(&format!("\r\nhost: fixture.invalid:{port}\r\n")));
                    assert_eq!(&received[end + 4..], POST_BODY);
                    return;
                }
            }
        }
    }

    #[tokio::test]
    async fn transport_tries_other_pinned_ips_before_sending_the_request() {
        // Loopback is confined to transport tests. The production gate still
        // rejects it. A reserved hostname proves the connector uses our IPs.
        for ipv6 in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let healthy = listener.local_addr().unwrap();
            let (unavailable, socket) = if ipv6 {
                ("::1", tokio::net::TcpSocket::new_v6().unwrap())
            } else {
                ("127.0.0.2", tokio::net::TcpSocket::new_v4().unwrap())
            };
            let unavailable = SocketAddr::new(unavailable.parse().unwrap(), healthy.port());
            // Reserve the port without listening where the address is configured.
            // macOS does not configure 127.0.0.2; it is already unreachable there.
            if let Err(error) = socket.bind(unavailable) {
                assert!(
                    !ipv6 && error.kind() == std::io::ErrorKind::AddrNotAvailable,
                    "could not reserve the unavailable address: {error}"
                );
            }
            let pinned = [unavailable, healthy];
            let server = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_post(&mut stream, healthy.port()).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
            };
            let (_, response) = timeout(Duration::from_secs(3), async {
                tokio::join!(
                    server,
                    send_pinned_request(
                        post_request(healthy.port()),
                        config(false),
                        &pinned,
                        test_budget()
                    )
                )
            })
            .await
            .unwrap();
            let response = response.unwrap().resp;
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "ok"
            );
            assert!(
                listener.accept().now_or_never().is_none(),
                "only one HTTP request"
            );
        }
    }

    #[tokio::test]
    async fn transport_does_not_replay_a_sent_request_on_another_ip() {
        for disconnect in [false, true] {
            let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = first.local_addr().unwrap();
            let second = tokio::net::TcpListener::bind(("::1", addr.port()))
                .await
                .unwrap();
            let pinned = [addr, second.local_addr().unwrap()];
            let server = async {
                let (mut stream, _) = first.accept().await.unwrap();
                read_post(&mut stream, addr.port()).await;
                if !disconnect {
                    stream
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                }
            };
            let (_, response) = timeout(Duration::from_secs(3), async {
                tokio::join!(
                    server,
                    send_pinned_request(
                        post_request(addr.port()),
                        config(false),
                        &pinned,
                        test_budget()
                    )
                )
            })
            .await
            .unwrap();
            if disconnect {
                assert!(response.is_err());
            } else {
                assert_eq!(response.unwrap().resp.status(), 503);
            }
            assert!(
                second.accept().now_or_never().is_none(),
                "must not replay on another IP"
            );
            assert!(
                first.accept().now_or_never().is_none(),
                "must not replay on the first IP"
            );
        }
    }

    #[tokio::test]
    async fn transport_rejects_an_empty_address_list() {
        assert!(matches!(
            send_pinned_request(post_request(80), config(false), &[], test_budget()).await,
            Err(ErrorCode::HttpRequestDenied)
        ));
    }

    #[tokio::test]
    async fn ip_literals_require_an_approved_public_ip_and_port() {
        for (url, approved, accepted) in [
            (
                "https://[2606:4700:4700::1111]/",
                "[2606:4700:4700::1111]:443",
                true,
            ),
            (
                "http://[2606:4700:4700:0:0:0:0:1111]:8080/",
                "[2606:4700:4700::1111]:8080",
                true,
            ),
            (
                "http://[2606:4700:4700::1111]/",
                "[2606:4700:4700::1111]:80",
                true,
            ),
            ("https://1.1.1.1/", "1.1.1.1:443", true),
            (
                "https://[2606:4700:4700::1111]/",
                "[2606:4700:4700::1111]:80",
                false,
            ),
            (
                "https://[2606:4700:4700::1111]/",
                "[2606:4700:4700::1001]:443",
                false,
            ),
            ("http://[::1]/", "[::1]:80", false),
            ("http://[fd00::1]/", "[fd00::1]:80", false),
            ("http://[fe80::1]/", "[fe80::1]:80", false),
            ("http://[::ffff:127.0.0.1]/", "[::ffff:127.0.0.1]:80", false),
            ("http://127.0.0.1/", "127.0.0.1:80", false),
        ] {
            let uri = url.parse::<hyper::Uri>().unwrap();
            let addr = approved.parse::<SocketAddr>().unwrap();
            let mut config = config(uri.scheme_str() == Some("https"));
            // Resolving an IP literal consumes no DNS/connect budget or network I/O.
            config.connect_timeout = Duration::ZERO;
            let mut allowed = ApprovedEgress::default();
            allowed.insert(&addr.ip().to_string(), addr);
            let result = approved_destinations(&uri, &config, &allowed);
            if accepted {
                assert_eq!(result.unwrap(), [addr], "{url}");
            } else {
                assert!(matches!(result, Err(ErrorCode::HttpRequestDenied)), "{url}");
            }
            assert!(matches!(
                approved_destinations(&uri, &config, &ApprovedEgress::default()),
                Err(ErrorCode::HttpRequestDenied)
            ));
        }
    }

    #[tokio::test]
    async fn ipv6_transport_preserves_authority_and_path() {
        // Only the transport uses loopback; the production gate above still rejects it.
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pinned = [addr];
        let request = hyper::Request::builder()
            .uri(format!("http://{addr}/probe?value=1"))
            .body(
                Full::new(bytes::Bytes::new())
                    .map_err(|e| match e {})
                    .boxed(),
            )
            .unwrap();
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0; 1024];
            while !received.windows(4).any(|w| w == b"\r\n\r\n") {
                let size = socket.read(&mut buf).await.unwrap();
                assert_ne!(size, 0);
                received.extend_from_slice(&buf[..size]);
            }
            let received = String::from_utf8(received).unwrap();
            assert!(received.starts_with("GET /probe?value=1 HTTP/1.1\r\n"));
            assert!(received
                .to_ascii_lowercase()
                .contains(&format!("\r\nhost: {addr}\r\n")));
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .await
                .unwrap();
        };
        let (_, response) = timeout(Duration::from_secs(3), async {
            tokio::join!(
                server,
                send_pinned_request(request, config(false), &pinned, test_budget())
            )
        })
        .await
        .unwrap();
        assert_eq!(response.unwrap().resp.status(), 204);
    }

    // Script a real HTTP peer, including pauses before headers/first data and
    // between chunks. No external DNS/service and no allowlist exceptions.
    async fn upstream(
        chunks: Vec<(u64, &'static [u8])>,
        first_ms: u64,
        idle_ms: u64,
    ) -> Result<IncomingResponse, ErrorCode> {
        upstream_with_budget(chunks, first_ms, idle_ms, test_budget()).await
    }

    async fn upstream_with_budget(
        chunks: Vec<(u64, &'static [u8])>,
        first_ms: u64,
        idle_ms: u64,
        budget: Budget,
    ) -> Result<IncomingResponse, ErrorCode> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 1024];
            while !received.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                received.extend_from_slice(&buf[..n]);
            }
            for (delay, bytes) in chunks {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if socket.write_all(bytes).await.is_err() {
                    return;
                }
            }
        });
        let request = hyper::Request::builder()
            .uri(format!("http://fixture.invalid:{}/", addr.port()))
            .body(
                Full::new(bytes::Bytes::new())
                    .map_err(|e| match e {})
                    .boxed(),
            )
            .unwrap();
        let result = timeout(
            Duration::from_secs(3),
            send_pinned_request(
                request,
                OutgoingRequestConfig {
                    use_tls: false,
                    connect_timeout: Duration::from_secs(1),
                    first_byte_timeout: Duration::from_millis(first_ms),
                    between_bytes_timeout: Duration::from_millis(idle_ms),
                },
                &[addr],
                budget,
            ),
        )
        .await;
        server.abort();
        let _ = server.await;
        result.expect("transport must finish within the test deadline")
    }

    const HEADERS: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
    const FIRST: &[u8] = b"3\r\none\r\n";
    const END: &[u8] = b"0\r\n\r\n";

    #[tokio::test]
    async fn retained_responses_and_consumed_frames_keep_both_budgets_reserved() {
        let worker = Arc::new(tokio::sync::Semaphore::new(16));
        let first = Budget::for_test(8, worker.clone());
        let second = Budget::for_test(16, worker.clone());
        async fn send(budget: Budget) -> Result<IncomingResponse, ErrorCode> {
            upstream_with_budget(vec![(0, HEADERS), (0, FIRST), (0, END)], 1000, 1000, budget).await
        }
        let a = send(first.clone()).await.unwrap();
        let b = send(first.clone()).await.unwrap();
        assert!(matches!(
            send(first.clone()).await,
            Err(ErrorCode::InternalError(_))
        ));
        let c = send(second.clone()).await.unwrap();
        let d = send(second.clone()).await.unwrap();
        assert!(matches!(
            send(second.clone()).await,
            Err(ErrorCode::InternalError(_))
        ));
        let bytes = a.resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes, "one");
        let slice = bytes.slice(0..1);
        drop(bytes);
        assert!(
            send(second.clone()).await.is_err(),
            "a retained data frame still consumes capacity"
        );
        drop(slice);
        let recovered = send(first).await.unwrap();
        drop((b, c, d, recovered));
        assert_eq!(worker.available_permits(), 16);
    }

    #[tokio::test]
    async fn first_byte_deadline_covers_headers_and_first_body_data() {
        for chunks in [
            vec![(300, HEADERS), (0, END)],
            vec![(0, HEADERS), (300, FIRST), (0, END)],
        ] {
            assert!(matches!(
                upstream(chunks, 60, 1000).await,
                Err(ErrorCode::ConnectionReadTimeout)
            ));
        }
    }

    #[tokio::test]
    async fn idle_body_times_out_even_after_headers_and_data_arrive() {
        assert!(matches!(
            upstream(vec![(0, HEADERS), (0, FIRST), (300, END)], 1000, 60).await,
            Err(ErrorCode::ConnectionReadTimeout)
        ));
    }

    #[tokio::test]
    async fn progress_resets_idle_timeout_without_a_total_first_byte_deadline() {
        let response = upstream(
            vec![
                (0, HEADERS),
                (0, FIRST),
                (100, FIRST),
                (100, FIRST),
                (0, END),
            ],
            150,
            500,
        )
        .await
        .unwrap();
        assert_eq!(response.resp.status(), 200);
        assert_eq!(
            response
                .resp
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes(),
            "oneoneone"
        );
        assert!(
            upstream(vec![(0, b"HTTP/1.1 204 No Content\r\n\r\n")], 1000, 60)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn production_gate_still_denies_loopback_even_when_listed() {
        let mut allowed = ApprovedEgress::default();
        allowed.insert("127.0.0.1", "127.0.0.1:80".parse().unwrap());
        let request = hyper::Request::builder()
            .uri("http://127.0.0.1:80/")
            .body(
                Full::new(bytes::Bytes::new())
                    .map_err(|e| match e {})
                    .boxed(),
            )
            .unwrap();
        let config = OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            between_bytes_timeout: Duration::from_secs(1),
        };
        assert!(matches!(
            gated_send_request(request, config, Arc::new(allowed), test_budget()).await,
            Err(ErrorCode::HttpRequestDenied)
        ));
    }

    #[test]
    fn http_uses_only_the_approved_hostname_snapshot_and_port() {
        let mut allowed = ApprovedEgress::default();
        let addr = "1.1.1.1:443".parse().unwrap();
        // A reserved name proves the lookup comes from the approved snapshot.
        allowed.insert("approved.invalid", addr);
        for url in [
            "https://approved.invalid/",
            "https://APPROVED.invalid./",
            "https://1.1.1.1/",
        ] {
            assert_eq!(
                approved_destinations(&url.parse().unwrap(), &config(true), &allowed).unwrap(),
                [addr]
            );
        }
        for url in [
            "https://unapproved.invalid/",
            "http://approved.invalid/",
            "https://approved.invalid:8443/",
            "https://localhost/",
        ] {
            assert!(matches!(
                approved_destinations(
                    &url.parse().unwrap(),
                    &config(url.starts_with("https")),
                    &allowed
                ),
                Err(ErrorCode::HttpRequestDenied)
            ));
        }
    }

    #[test]
    fn http_keeps_all_safe_addresses_for_the_requested_host_and_port() {
        let mut allowed = ApprovedEgress::default();
        // Preserve the approved resolver order, including IPv4 and IPv6.
        for value in [
            "1.1.1.1:443",
            "127.0.0.1:443",
            "0.0.0.1:443",
            "[64:ff9b::a9fe:a9fe]:443",
            "[2002:a9fe:a9fe::1]:443",
            "[fec0::1]:443",
            "[2606:4700:4700::1111]:443",
            "10.0.0.1:443",
            "1.0.0.1:8443",
            "1.0.0.1:443",
        ] {
            allowed.insert("approved.invalid", value.parse().unwrap());
        }
        allowed.insert("other.invalid", "8.8.8.8:443".parse().unwrap());
        assert_eq!(
            approved_destinations(
                &"https://APPROVED.invalid./".parse().unwrap(),
                &config(true),
                &allowed
            )
            .unwrap(),
            ["1.1.1.1:443", "[2606:4700:4700::1111]:443", "1.0.0.1:443"]
                .map(|value| value.parse::<SocketAddr>().unwrap())
        );
    }
}
