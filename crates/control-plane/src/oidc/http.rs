//! Bound and validate every provider request, including discovery's JWKS fetch.
use openidconnect::{AsyncHttpClient, HttpRequest, HttpResponse};
use std::{future::Future, pin::Pin, time::Duration};

pub struct Http {
    client: reqwest::Client,
    allow_insecure_http: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("OIDC endpoint or response rejected")]
    Rejected,
    #[error("OIDC transport failed")]
    Transport(#[from] reqwest::Error),
    #[error("OIDC response invalid")]
    Response(#[from] axum::http::Error),
}

impl Http {
    pub fn new(config: &super::config::OidcConfig) -> Result<Self, reqwest::Error> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10));
        for certificate in &config.ca_certificates {
            builder = builder.add_root_certificate(certificate.clone());
        }
        Ok(Self {
            client: builder.build()?,
            allow_insecure_http: config.allow_insecure_http,
        })
    }
}

impl<'c> AsyncHttpClient<'c> for Http {
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, Error>> + Send + 'c>>;
    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            super::config::validated_endpoint_url(
                &request.uri().to_string(),
                self.allow_insecure_http,
            )
            .map_err(|_| Error::Rejected)?;
            let mut response = self.client.execute(request.try_into()?).await?;
            const LIMIT: usize = 1024 * 1024;
            if response.content_length().is_some_and(|n| n > LIMIT as u64) {
                return Err(Error::Rejected);
            }
            let mut result = axum::http::Response::builder().status(response.status());
            for (key, value) in response.headers() {
                result = result.header(key, value);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if body.len() + chunk.len() > LIMIT {
                    return Err(Error::Rejected);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(result.body(body)?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, routing::get, Router};

    #[tokio::test]
    async fn discovery_authorization_jwks_and_token_preserve_endpoint_queries() {
        use axum::{extract::OriginalUri, routing::post, Json};
        use openidconnect::{core::CoreAuthenticationFlow, AuthorizationCode, CsrfToken, Nonce};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let metadata_origin = origin.clone();
        let keys_requests = Arc::new(AtomicUsize::new(0));
        let token_requests = Arc::new(AtomicUsize::new(0));
        let keys_count = keys_requests.clone();
        let token_count = token_requests.clone();
        let query = "realm=one%2Ftwo&route=a&route=b";
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || async move {
                    Json(serde_json::json!({
                        "issuer": metadata_origin,
                        "authorization_endpoint": format!("{metadata_origin}/auth?{query}"),
                        "token_endpoint": format!("{metadata_origin}/token?{query}"),
                        "jwks_uri": format!("{metadata_origin}/keys?{query}"),
                        "response_types_supported": ["code"],
                        "subject_types_supported": ["public"],
                        "id_token_signing_alg_values_supported": ["RS256"]
                    }))
                }),
            )
            .route(
                "/keys",
                get(move |OriginalUri(uri): OriginalUri| async move {
                    assert_eq!(uri.query(), Some(query));
                    keys_count.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"keys": []}))
                }),
            )
            .route(
                "/token",
                post(
                    move |OriginalUri(uri): OriginalUri, body: String| async move {
                        assert_eq!(uri.query(), Some(query));
                        assert!(body.contains("grant_type=authorization_code"));
                        token_count.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({"access_token": "fixture", "token_type": "Bearer"}))
                    },
                ),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = super::super::config::tests::config();
        config.issuer = origin;
        config.allow_insecure_http = true;
        let (client, http) = config.client().await.unwrap();
        let (authorization, _, _) = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .url();
        assert!(authorization
            .query()
            .unwrap()
            .starts_with(&format!("{query}&")));
        assert!(authorization
            .query_pairs()
            .any(|(key, value)| key == "response_type" && value == "code"));
        client
            .exchange_code(AuthorizationCode::new("fixture-code".into()))
            .unwrap()
            .request_async(&http)
            .await
            .unwrap();
        assert_eq!(keys_requests.load(Ordering::SeqCst), 1);
        assert_eq!(token_requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn bounds_provider_responses_and_does_not_follow_redirects() {
        let app = Router::new()
            .route("/ok", get(|| async { "fixture" }))
            .route(
                "/redirect",
                get(|| async { axum::response::Redirect::temporary("/ok") }),
            )
            .route("/large", get(|| async { vec![b'x'; 1_048_577] }))
            .route(
                "/chunked",
                get(|| async {
                    Body::from_stream(futures::stream::iter([
                        Ok::<_, std::io::Error>(vec![b'x'; 524_288]),
                        Ok(vec![b'x'; 524_289]),
                    ]))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let request = |path: &str| {
            axum::http::Request::builder()
                .uri(format!("{origin}{path}"))
                .body(Vec::new())
                .unwrap()
        };
        let mut config = super::super::config::tests::config();
        let secure = Http::new(&config).unwrap();
        assert!(matches!(
            secure.call(request("/ok")).await,
            Err(Error::Rejected)
        ));
        config.allow_insecure_http = true;
        let client = Http::new(&config).unwrap();
        assert_eq!(
            client.call(request("/ok")).await.unwrap().body(),
            b"fixture"
        );
        assert_eq!(
            client.call(request("/redirect")).await.unwrap().status(),
            307
        );
        for path in ["/large", "/chunked"] {
            assert!(
                matches!(client.call(request(path)).await, Err(Error::Rejected)),
                "{path}"
            );
        }
        server.abort();
    }
}
