//! The platform owns application URLs, including the scheme and external port.
use reqwest::Url;

#[derive(Clone, Debug, Default)]
pub struct PublicApps(Option<Url>);

impl PublicApps {
    pub fn parse(origin: Option<&str>) -> anyhow::Result<Self> {
        let Some(origin) = origin else {
            return Ok(Self::default());
        };
        let url = Url::parse(origin)?;
        let domain = url.domain().unwrap_or_default();
        anyhow::ensure!(
            !domain.is_empty() && domain.split('.').all(valid_label)
                && url.username().is_empty() && url.password().is_none()
                && url.query().is_none() && url.fragment().is_none() && url.path() == "/"
                && (url.scheme() == "https" || (url.scheme() == "http" && domain == "localhost")),
            "APP_PUBLIC_ORIGIN must be an HTTPS origin with a DNS hostname (HTTP is allowed for localhost)"
        );
        Ok(Self(Some(url)))
    }

    pub fn domain(&self) -> Option<&str> {
        self.0.as_ref().and_then(Url::domain)
    }

    pub fn url(&self, app: &str, tenant: &str) -> Option<String> {
        if !valid_label(app) || !valid_label(tenant) {
            return None;
        }
        let mut url = self.0.clone()?;
        let host = format!("{app}.{tenant}.{}", url.domain()?);
        if host.len() > 253 {
            return None;
        }
        url.set_host(Some(&host)).ok()?;
        Some(url.into())
    }
}

/// App names and tenant slugs become labels in the public hostname.
pub(crate) fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_single_lowercase_dns_labels() {
        for label in ["a", "0", "app-42", &"a".repeat(63)] {
            assert!(valid_label(label), "{label}");
        }
        for label in [
            "",
            "TeamA",
            "under_score",
            "two.labels",
            "-app",
            "app-",
            " app",
            "app ",
            "日本語",
            &"a".repeat(64),
        ] {
            assert!(!valid_label(label), "{label}");
        }
    }

    #[test]
    fn public_urls_include_the_platform_scheme_and_port() {
        for (origin, expected) in [
            (
                "https://apps.example.internal",
                "https://hello.team.apps.example.internal/",
            ),
            (
                "https://apps.example.internal:8443",
                "https://hello.team.apps.example.internal:8443/",
            ),
            (
                "http://localhost:28084",
                "http://hello.team.localhost:28084/",
            ),
        ] {
            let apps = PublicApps::parse(Some(origin)).unwrap();
            assert_eq!(apps.url("hello", "team").as_deref(), Some(expected));
            assert!(apps.url("bad/name", "team").is_none());
            assert!(apps.url("hello", "bad.team").is_none());
        }
        assert!(PublicApps::default().url("hello", "team").is_none());
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_origins() {
        for origin in [
            "invalid",
            "http://remote.example.com",
            "https://127.0.0.1",
            "https://user:password@apps.example.com",
            "https://apps.example.com/path",
            "https://apps.example.com/?token=x",
            "https://apps.example.com/#fragment",
            "https://*.example.com",
            "https://-bad.example.com",
        ] {
            assert!(PublicApps::parse(Some(origin)).is_err(), "{origin}");
        }
    }
}
