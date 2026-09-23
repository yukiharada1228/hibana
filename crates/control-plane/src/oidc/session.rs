//! Console-only cookies. Protocol tokens remain on the server; CLI uses Bearer.
use super::config::OidcConfig;
use axum::http::{header, HeaderMap, HeaderValue, Method};
use cookie::{time::Duration, Cookie, SameSite};
use hibana_shared::FaasError;

pub const CONSOLE_HEADER: &str = "x-hibana-console";
pub const SESSION_HEADER: &str = "x-hibana-session";

fn secure(config: &OidcConfig) -> bool {
    config.console_url.starts_with("https://")
}

pub fn cookie_name(config: &OidcConfig) -> String {
    // Cookies have no port isolation. Keep independent local consoles apart.
    let origin = reqwest::Url::parse(&config.console_url)
        .unwrap()
        .origin()
        .ascii_serialization();
    format!(
        "{}hibana_session_{}",
        if secure(config) { "__Host-" } else { "" },
        &crate::auth::hash_token(&origin)[..16]
    )
}

pub fn cookie(config: &OidcConfig, secret: &str, ttl: i64) -> HeaderValue {
    let cookie = Cookie::build((cookie_name(config), secret))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .max_age(Duration::seconds(ttl))
        .secure(secure(config))
        .build();
    HeaderValue::from_str(&cookie.to_string()).expect("generated session cookie")
}

pub fn credential(config: &OidcConfig, headers: &HeaderMap) -> Result<Option<String>, FaasError> {
    let name = cookie_name(config);
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        let value = header.to_str().map_err(|_| FaasError::Unauthorized)?;
        // Do not use a CookieJar: duplicate session cookies must not be collapsed.
        for entry in Cookie::split_parse(value) {
            let entry = entry.map_err(|_| FaasError::Unauthorized)?;
            if entry.name() != name {
                continue;
            }
            let value = entry.value();
            if found.is_some()
                || value.len() != 64
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(FaasError::Unauthorized);
            }
            found = Some(value.to_owned());
        }
    }
    Ok(found)
}

/// A custom header excludes HTML forms/images. Unsafe requests additionally
/// require the configured console Origin; same-site sibling apps are untrusted.
pub fn require_console(
    config: &OidcConfig,
    headers: &HeaderMap,
    method: &Method,
) -> Result<(), FaasError> {
    let origin = reqwest::Url::parse(&config.console_url)
        .unwrap()
        .origin()
        .ascii_serialization();
    let origin_header = headers.get(header::ORIGIN);
    let fetch_site = headers.get("sec-fetch-site");
    if headers.get(CONSOLE_HEADER).is_none_or(|value| value != "1")
        || fetch_site.is_some_and(|value| value != "same-origin")
        || origin_header.is_some_and(|value| value != origin.as_str())
        || (origin_header.is_none()
            && (!matches!(*method, Method::GET | Method::HEAD) || fetch_site.is_none()))
    {
        return Err(FaasError::Forbidden);
    }
    Ok(())
}

pub fn matches_session(headers: &HeaderMap, method: &Method, path: &str, token_id: &str) -> bool {
    match headers.get(SESSION_HEADER) {
        Some(value) => value == token_id,
        None => *method == Method::GET && path == "/auth/session",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_are_host_only_http_only_and_isolated_by_console_origin() {
        let mut cfg = super::super::config::tests::config();
        let value = cookie(&cfg, &"a".repeat(64), 900)
            .to_str()
            .unwrap()
            .to_owned();
        assert!(value.starts_with("__Host-hibana_session_"));
        for attribute in [
            "Path=/",
            "HttpOnly",
            "SameSite=Strict",
            "Max-Age=900",
            "Secure",
        ] {
            assert!(value.contains(attribute));
        }
        assert!(!value.contains("Domain="));
        cfg.console_url = "http://127.0.0.1:5173/".into();
        let first = cookie_name(&cfg);
        assert!(!cookie(&cfg, "", 0).to_str().unwrap().contains("Secure"));
        cfg.console_url = "http://127.0.0.1:5174/".into();
        assert_ne!(first, cookie_name(&cfg));
    }

    #[test]
    fn rejects_ambiguous_or_malformed_session_cookies() {
        let cfg = super::super::config::tests::config();
        let name = cookie_name(&cfg);
        let secret = "a".repeat(64);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("other=x; {name}={secret}").parse().unwrap(),
        );
        assert_eq!(
            credential(&cfg, &headers).unwrap().as_deref(),
            Some(secret.as_str())
        );
        // Cookie syntax/whitespace is parsed by the library. Check duplicates
        // after normalization, including pairs carried in one header.
        headers.insert(
            header::COOKIE,
            format!("other=x; {name} = {secret}").parse().unwrap(),
        );
        assert_eq!(
            credential(&cfg, &headers).unwrap().as_deref(),
            Some(secret.as_str())
        );
        headers.insert(
            header::COOKIE,
            format!("{name}={secret}; {name} = {secret}")
                .parse()
                .unwrap(),
        );
        assert!(credential(&cfg, &headers).is_err());
        headers.insert(header::COOKIE, format!("{name}={secret}").parse().unwrap());
        headers.append(header::COOKIE, format!("{name}={secret}").parse().unwrap());
        assert!(credential(&cfg, &headers).is_err());
        for bad in ["short".to_owned(), "A".repeat(64), "a".repeat(65)] {
            headers.insert(header::COOKIE, format!("{name}={bad}").parse().unwrap());
            assert!(credential(&cfg, &headers).is_err());
        }
        headers.insert(header::COOKIE, "malformed".parse().unwrap());
        assert!(credential(&cfg, &headers).is_err());
    }

    #[test]
    fn csrf_checks_origin_fetch_metadata_and_custom_header() {
        let cfg = super::super::config::tests::config();
        let mut headers = HeaderMap::new();
        headers.insert(CONSOLE_HEADER, "1".parse().unwrap());
        headers.insert(header::ORIGIN, "https://hibana.example".parse().unwrap());
        assert!(require_console(&cfg, &headers, &Method::POST).is_ok());
        for origin in [
            "null",
            "https://evil.example",
            "https://app.hibana.example",
            "https://hibana.example:8443",
        ] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert!(require_console(&cfg, &headers, &Method::GET).is_err());
            assert!(require_console(&cfg, &headers, &Method::POST).is_err());
        }
        headers.remove(header::ORIGIN);
        assert!(require_console(&cfg, &headers, &Method::GET).is_err());
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(require_console(&cfg, &headers, &Method::GET).is_ok());
        assert!(require_console(&cfg, &headers, &Method::POST).is_err());
        headers.insert(header::ORIGIN, "https://hibana.example".parse().unwrap());
        headers.insert("sec-fetch-site", "same-site".parse().unwrap());
        assert!(require_console(&cfg, &headers, &Method::POST).is_err());
        headers.remove("sec-fetch-site");
        headers.remove(CONSOLE_HEADER);
        assert!(require_console(&cfg, &headers, &Method::POST).is_err());
    }

    #[test]
    fn tab_is_bound_to_the_session_it_loaded() {
        let mut headers = HeaderMap::new();
        assert!(matches_session(
            &headers,
            &Method::GET,
            "/auth/session",
            "token"
        ));
        assert!(!matches_session(
            &headers,
            &Method::POST,
            "/auth/logout",
            "token"
        ));
        assert!(!matches_session(
            &headers,
            &Method::GET,
            "/components",
            "token"
        ));
        headers.insert(SESSION_HEADER, "old-token".parse().unwrap());
        assert!(!matches_session(
            &headers,
            &Method::DELETE,
            "/components/app",
            "new-token"
        ));
        headers.insert(SESSION_HEADER, "new-token".parse().unwrap());
        assert!(matches_session(
            &headers,
            &Method::DELETE,
            "/components/app",
            "new-token"
        ));
    }
}
