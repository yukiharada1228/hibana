use super::{fail, tls_error, Failure, Options};
use rustls::{pki_types::ServerName, ClientConfig, ClientConnection, RootCertStore};
use std::sync::Arc;

pub(super) fn make_tls(options: Options) -> Result<ClientConnection, Failure> {
    let mut roots = RootCertStore::empty();
    if let Some(pem) = options.ca_pem {
        if pem.len() > 64 * 1024 {
            return Err(fail("EINVAL", "CA PEM exceeds 64 KiB"));
        }
        for cert in rustls_pemfile::certs(&mut pem.as_bytes()) {
            roots.add(cert.map_err(tls_error)?).map_err(tls_error)?;
        }
        if roots.is_empty() {
            return Err(fail("EINVAL", "CA must contain PEM certificates"));
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    if options.alpn.len() > 16 || options.alpn.iter().any(|p| p.is_empty() || p.len() > 255) {
        return Err(fail("EINVAL", "Invalid ALPN protocols"));
    }
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(tls_error)?
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = options.alpn.into_iter().map(String::into_bytes).collect();
    let name = ServerName::try_from(options.server_name).map_err(tls_error)?;
    let mut connection = ClientConnection::new(Arc::new(config), name).map_err(tls_error)?;
    connection.set_buffer_limit(Some(128 * 1024));
    Ok(connection)
}
