//! TLS helpers for OxiGate termination.

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

/// Load a rustls ServerConfig from PEM cert + key paths.
/// Enables ALPN for HTTP/1.1 and HTTP/2.
pub fn load_server_config(cert_path: &str, key_path: &str) -> Result<Arc<ServerConfig>> {
    let cert_file = File::open(cert_path)
        .with_context(|| format!("failed to open TLS cert: {cert_path}"))?;
    let mut cert_reader = BufReader::new(cert_file);
    let cert_chain: Vec<CertificateDer<'static>> = certs(&mut cert_reader)
        .collect::<std::result::Result<_, _>>()
        .context("failed to parse TLS certificate chain")?;

    let key_file =
        File::open(key_path).with_context(|| format!("failed to open TLS key: {key_path}"))?;
    let mut key_reader = BufReader::new(key_file);

    // Try PKCS#8 first, then RSA.
    let key: PrivateKeyDer<'static> = if let Some(key) = pkcs8_private_keys(&mut key_reader)
        .next()
        .transpose()
        .context("failed to parse PKCS#8 private key")?
    {
        key.into()
    } else {
        let key_file = File::open(key_path)?;
        let mut key_reader = BufReader::new(key_file);
        let rsa_key = rsa_private_keys(&mut key_reader)
            .next()
            .transpose()
            .context("failed to parse RSA private key")?;
        rsa_key
            .map(Into::into)
            .ok_or_else(|| anyhow::anyhow!("no private keys found in {key_path}"))?
    };

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .context("failed to build rustls ServerConfig")?;

    // Advertise both HTTP/2 and HTTP/1.1 via ALPN
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}
