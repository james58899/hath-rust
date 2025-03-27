use std::sync::Arc;

use p12::PFX;
use rustls::{
    ServerConfig,
    compress::CompressionCache,
    crypto::aws_lc_rs::Ticketer,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::NoServerSessionStorage,
};

use crate::util::{aes_support, ssl_provider};

pub struct ParsedCert {
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl ParsedCert {
    pub fn from_p12(p12: &[u8], password: &str) -> Option<ParsedCert> {
        let pfx = PFX::parse(p12).ok()?;
        let key = PrivatePkcs8KeyDer::from(pfx.key_bags(password).ok()?.pop()?).into();
        let certs: Vec<CertificateDer> = pfx.cert_x509_bags(password).ok()?.into_iter().map(CertificateDer::from).collect();

        Some(ParsedCert { certs, key })
    }
}

pub fn create_ssl_config(cert: ParsedCert) -> ServerConfig {
    // TODO error handle
    let mut config = ServerConfig::builder_with_provider(ssl_provider().into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(cert.certs, cert.key)
        .unwrap();

    // Prefer ChaCha20 on non-AES hardware.
    config.ignore_client_order = !aes_support();
    // Only support ticket resumption.
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.ticketer = Ticketer::new().unwrap();
    config.send_tls13_tickets = 1;
    config.cert_compression_cache = CompressionCache::new(2).into(); // 1 cert * 2 compression algorithms

    config
}
