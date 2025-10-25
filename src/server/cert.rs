use std::sync::Arc;

use arc_swap::ArcSwap;
use p12::PFX;
use rustls::{
    client::verify_server_name,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    server::{ClientHello, ParsedCertificate, ResolvesServerCert},
    sign::CertifiedKey,
};

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

#[derive(Debug)]
pub(super) struct CertStore {
    provider: Arc<CryptoProvider>,
    cert: ArcSwap<CertifiedKey>,
    strict: bool,
}

impl CertStore {
    pub fn new(provider: Arc<CryptoProvider>, cert: ParsedCert, strict: bool) -> Self {
        let cert = ArcSwap::new(CertifiedKey::from_der(cert.certs, cert.key, &provider).unwrap().into());
        Self { provider, cert, strict }
    }

    pub fn update(&self, cert: ParsedCert) -> Result<(), rustls::Error> {
        let cert = CertifiedKey::from_der(cert.certs, cert.key, &self.provider)?;
        self.cert.store(cert.into());
        Ok(())
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let certs = self.cert.load();
        if self.strict {
            let server_name: ServerName = client_hello.server_name()?.try_into().ok()?;
            let cert = certs.end_entity_cert().and_then(ParsedCertificate::try_from).ok()?;
            if verify_server_name(&cert, &server_name).is_err() {
                return None; // SNI check fail
            }
        }

        Some(certs.clone())
    }
}
