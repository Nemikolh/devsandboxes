//! Intercept mode's TLS (credential-broker, *Shape*, "The broker"): the
//! broker's side of a tool's TLS to a bound upstream, which devsbd's proxy
//! port already answered `200` for. The leaf is the member CA's for the
//! `CONNECT` target (`credentials::ca`); a ClientHello naming no host, or
//! another one, gets no certificate and the handshake fails. ALPN is
//! `http/1.1` only: a client offering only `h2` fails the handshake, one
//! offering both gets HTTP/1.1.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::credentials::ca::Leaf;

pub const ALPN: &[u8] = b"http/1.1";

/// A `CONNECT` target or SNI as the CA and the claims name hosts:
/// lowercase, one trailing dot dropped.
pub fn fold(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

/// The server side for one intercepted connection to `host`.
pub fn server_config(host: &str, leaf: Leaf) -> Result<Arc<ServerConfig>> {
    let key = rustls::crypto::ring::sign::any_supported_type(&leaf.key).context("the leaf's key")?;
    let only = OnlyHost { host: host.to_string(), key: Arc::new(CertifiedKey::new(leaf.chain, key)) };
    let mut config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(only));
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(config))
}

/// Answers only a ClientHello whose SNI is the `CONNECT` target.
struct OnlyHost {
    host: String,
    key: Arc<CertifiedKey>,
}

impl fmt::Debug for OnlyHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnlyHost").field("host", &self.host).finish_non_exhaustive()
    }
}

impl ResolvesServerCert for OnlyHost {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name()?;
        (fold(sni) == self.host).then(|| self.key.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_lowercases_and_drops_one_trailing_dot() {
        assert_eq!(fold("API.GitHub.com."), "api.github.com");
        assert_eq!(fold("api.github.com"), "api.github.com");
    }
}
