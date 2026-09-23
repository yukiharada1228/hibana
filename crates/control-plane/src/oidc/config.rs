use hibana_shared::Redacted;
use reqwest::Url;

#[derive(Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Redacted<String>,
    pub callback_url: String,
    pub console_url: String,
    pub session_ttl_secs: i64,
    pub allow_insecure_http: bool,
    pub ca_certificates: Vec<reqwest::Certificate>,
}

impl OidcConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_values(|key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()))
    }

    fn from_values(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            get("AUTH_MODE").is_none(),
            "AUTH_MODE was removed; unset it and configure OIDC"
        );
        let required =
            |key: &str| get(key).ok_or_else(|| anyhow::anyhow!("{key} is required for OIDC"));
        let allow_insecure_http = match get("OIDC_ALLOW_INSECURE_HTTP").as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            _ => anyhow::bail!("OIDC_ALLOW_INSECURE_HTTP must be true or false"),
        };
        let issuer = required("OIDC_ISSUER_URL")?;
        let callback_url = required("OIDC_CALLBACK_URL")?;
        let console_url = required("OIDC_CONSOLE_URL")?;
        for (name, value) in [
            ("OIDC_ISSUER_URL", &issuer),
            ("OIDC_CALLBACK_URL", &callback_url),
            ("OIDC_CONSOLE_URL", &console_url),
        ] {
            validated_url(value, allow_insecure_http).map_err(|_| anyhow::anyhow!("{name} must be an HTTPS URL without credentials, query or fragment (explicit development mode permits loopback HTTP)"))?;
        }
        let session_ttl_secs = get("OIDC_SESSION_TTL_SECS")
            .unwrap_or_else(|| "900".into())
            .parse::<i64>()?;
        anyhow::ensure!(
            (60..=3600).contains(&session_ttl_secs),
            "OIDC_SESSION_TTL_SECS must be 60..3600"
        );
        Ok(Self {
            // OIDC compares the issuer byte-for-byte. URL serialization can
            // add a trailing slash or otherwise change a valid identifier.
            issuer,
            callback_url: Url::parse(&callback_url)?.to_string(),
            console_url: Url::parse(&console_url)?.to_string(),
            client_id: required("OIDC_CLIENT_ID")?,
            client_secret: Redacted::new(required("OIDC_CLIENT_SECRET")?),
            session_ttl_secs,
            allow_insecure_http,
            ca_certificates: match get("OIDC_CA_CERT_FILE") {
                Some(path) => {
                    use std::io::Read;
                    let mut pem = Vec::new();
                    std::fs::File::open(path)?
                        .take(1_048_577)
                        .read_to_end(&mut pem)?;
                    anyhow::ensure!(pem.len() <= 1_048_576, "OIDC_CA_CERT_FILE is too large");
                    let certs = reqwest::Certificate::from_pem_bundle(&pem)?;
                    anyhow::ensure!(
                        !certs.is_empty(),
                        "OIDC_CA_CERT_FILE contains no certificates"
                    );
                    certs
                }
                None => Vec::new(),
            },
        })
    }
}

pub fn validated_url(raw: &str, allow_http: bool) -> anyhow::Result<Url> {
    let url = validated_endpoint_url(raw, allow_http)?;
    anyhow::ensure!(url.query().is_none(), "query is not allowed");
    Ok(url)
}

/// Provider endpoints may carry a routing query, which must be preserved when
/// adding authorization parameters and when making JWKS/token requests.
pub fn validated_endpoint_url(raw: &str, allow_http: bool) -> anyhow::Result<Url> {
    let url = Url::parse(raw)?;
    anyhow::ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "invalid URL"
    );
    anyhow::ensure!(
        url.scheme() == "https"
            || (allow_http
                && url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))),
        "HTTPS required"
    );
    Ok(url)
}

impl OidcConfig {
    pub fn client_secret(&self) -> &str {
        self.client_secret.expose()
    }
    /// The browser return URL is exact; CLI callbacks are restricted to an
    /// ephemeral listener on literal IPv4 loopback, at one fixed path.
    pub fn validate_return_url(&self, raw: &str) -> anyhow::Result<Url> {
        let url = Url::parse(raw)?;
        anyhow::ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid return URL"
        );
        anyhow::ensure!(
            url.as_str() == self.console_url
                || (url.scheme() == "http"
                    && url.host_str() == Some("127.0.0.1")
                    && url.port().is_some_and(|port| port >= 1024)
                    && url.path() == "/oidc/callback"),
            "return URL is not allowed"
        );
        Ok(url)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    pub fn config() -> OidcConfig {
        OidcConfig {
            issuer: "https://id.example/realms/hibana".into(),
            client_id: "hibana".into(),
            client_secret: Redacted::new("fixture-secret".into()),
            callback_url: "https://hibana.example/api/auth/oidc/callback".into(),
            console_url: "https://hibana.example/".into(),
            session_ttl_secs: 900,
            allow_insecure_http: false,
            ca_certificates: Vec::new(),
        }
    }
    #[test]
    fn fails_closed_without_explicit_configuration() {
        assert!(OidcConfig::from_values(|_| None).is_err());
        for mode in ["oidc", "password", "migration", "typo"] {
            let error = OidcConfig::from_values(|key| (key == "AUTH_MODE").then(|| mode.into()))
                .err()
                .unwrap();
            assert!(error.to_string().contains("AUTH_MODE was removed"));
        }
    }

    #[test]
    fn issuer_is_preserved_for_exact_discovery_matching() {
        use openidconnect::{core::CoreProviderMetadata, HttpRequest, IssuerUrl};

        for issuer in ["https://id.example", "https://id.example/"] {
            let cfg = OidcConfig::from_values(|key| {
                Some(
                    match key {
                        "OIDC_ISSUER_URL" => issuer,
                        "OIDC_CLIENT_ID" => "fixture",
                        "OIDC_CLIENT_SECRET" => "fixture-secret",
                        "OIDC_CALLBACK_URL" => "https://hibana.example/api/auth/oidc/callback",
                        "OIDC_CONSOLE_URL" => "https://hibana.example/",
                        _ => return None,
                    }
                    .to_owned(),
                )
            })
            .unwrap();
            assert_eq!(cfg.issuer, issuer);
            for published_issuer in [issuer, "https://other.example"] {
                let http = |request: HttpRequest| -> Result<_, std::io::Error> {
                    let body = if request.uri().path() == "/keys" {
                        serde_json::json!({"keys": []})
                    } else {
                        serde_json::json!({
                            "issuer": published_issuer,
                            "authorization_endpoint": "https://id.example/auth",
                            "token_endpoint": "https://id.example/token",
                            "jwks_uri": "https://id.example/keys",
                            "response_types_supported": ["code"],
                            "subject_types_supported": ["public"],
                            "id_token_signing_alg_values_supported": ["RS256"],
                        })
                    };
                    Ok(openidconnect::http::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(serde_json::to_vec(&body).unwrap())
                        .unwrap())
                };
                let result = CoreProviderMetadata::discover(
                    &IssuerUrl::new(cfg.issuer.clone()).unwrap(),
                    &http,
                );
                assert_eq!(result.is_ok(), published_issuer == issuer);
            }
        }
    }
    #[test]
    fn redirects_cannot_leave_console_or_loopback() {
        let cfg = config();
        for good in [
            "https://hibana.example/",
            "http://127.0.0.1:48123/oidc/callback",
        ] {
            assert!(cfg.validate_return_url(good).is_ok());
        }
        for bad in [
            "https://evil.example/",
            "https://hibana.example.evil/",
            "https://hibana.example/else",
            "https://hibana.example/?next=evil",
            "https://user@hibana.example/",
            "http://localhost:48123/oidc/callback",
            "http://127.0.0.1:80/oidc/callback",
            "http://127.0.0.1:48123/else",
            "http://127.0.0.1:48123/oidc/callback#fragment",
        ] {
            assert!(cfg.validate_return_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn provider_urls_require_tls_and_no_embedded_credentials() {
        for bad in [
            "http://id.example/realm",
            "https://user:secret@id.example/realm",
            "https://id.example/realm?next=other",
            "https://id.example/realm#fragment",
            "file:///tmp/realm",
        ] {
            assert!(validated_url(bad, true).is_err(), "{bad}");
        }
        assert!(validated_url("https://id.example/realm", false).is_ok());
        assert!(validated_url("http://127.0.0.1:8180/realm", false).is_err());
        assert!(validated_url("http://127.0.0.1:8180/realm", true).is_ok());
    }

    #[test]
    fn only_provider_endpoints_allow_queries() {
        let endpoint = "https://id.example/token?realm=one%2Ftwo&route=a&route=b";
        assert_eq!(
            validated_endpoint_url(endpoint, false).unwrap().as_str(),
            endpoint
        );
        assert!(validated_url(endpoint, false).is_err());
        for bad in [
            "http://id.example/token?realm=one",
            "https://user:secret@id.example/token?realm=one",
            "https://id.example/token?realm=one#fragment",
            "file:///tmp/keys?realm=one",
        ] {
            assert!(validated_endpoint_url(bad, true).is_err(), "{bad}");
        }
    }
}
