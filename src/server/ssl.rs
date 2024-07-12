use std::{
    error::Error,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use base64::{prelude::BASE64_STANDARD, Engine};
use chrono::{DateTime, Utc};
use log::{debug, warn};
use p12::PFX;
use reqwest::Url;
use rustls::{
    crypto::ring::Ticketer,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::{ClientHello, NoServerSessionStorage, ResolvesServerCert},
    sign::CertifiedKey,
    ServerConfig,
};
use sha1::Sha1;
use tokio::time::{sleep_until, Instant};
use x509_cert::{
    der::{oid::db::rfc5912::ID_AD_OCSP, Decode, Encode},
    ext::pkix::{name::GeneralName, AccessDescription, AuthorityInfoAccessSyntax},
    Certificate,
};
use x509_ocsp::{builder::OcspRequestBuilder, BasicOcspResponse, CertStatus, OcspResponse, OcspResponseStatus, Request};

use crate::util::{aes_support, ssl_provider};

pub struct ParsedCert {
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

#[derive(Debug)]
struct CertStore {
    cert: ArcSwap<CertifiedKey>,
}

impl CertStore {
    fn new(cert: &ParsedCert) -> Result<Self, Box<dyn Error>> {
        let key = ssl_provider().key_provider.load_private_key(cert.key.clone_key())?;
        let cert = CertifiedKey::new(cert.certs.clone(), key);

        Ok(CertStore {
            cert: ArcSwap::new(cert.into()),
        })
    }

    fn update_ocsp(&self, ocsp: Option<Vec<u8>>) {
        let old = self.cert.load();
        if let Some(ocsp) = ocsp {
            let mut new = CertifiedKey::new(old.cert.clone(), old.key.clone());
            new.ocsp = Some(ocsp);
            self.cert.store(Arc::new(new));
        } else {
            let new = CertifiedKey::new(old.cert.clone(), old.key.clone());
            self.cert.store(Arc::new(new));
        }
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.cert.load().clone())
    }
}

impl ParsedCert {
    pub fn from_p12(p12: &[u8], password: &str) -> Option<ParsedCert> {
        let pfx = PFX::parse(p12).ok()?;
        let key = PrivatePkcs8KeyDer::from(pfx.key_bags(password).ok()?.pop()?).into();
        let certs: Vec<CertificateDer> = pfx.cert_x509_bags(password).ok()?.into_iter().map(CertificateDer::from).collect();

        Some(ParsedCert { certs, key })
    }

    pub fn cert(&self) -> Option<&CertificateDer> {
        self.certs.first()
    }

    pub fn issue(&self) -> Option<&CertificateDer> {
        self.certs.get(1)
    }
}

pub fn create_ssl_config(cert: ParsedCert) -> ServerConfig {
    // TODO error handle
    let cert_store = Arc::new(CertStore::new(&cert).unwrap());
    let weak_cert_store = Arc::downgrade(&cert_store); // For OCSP worker
    let mut config = ServerConfig::builder_with_provider(ssl_provider().into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(cert_store);

    // Prefer ChaCha20 on non-AES hardware.
    config.ignore_client_order = !aes_support();
    // Only support ticket resumption.
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.ticketer = Ticketer::new().unwrap();
    config.send_tls13_tickets = 1;

    // OCSP worker
    tokio::spawn(async move {
        debug!("Start new OCSP stapling worker");
        loop {
            let mut next_run = Instant::now() + Duration::from_secs(3600);
            if let Some(cert_store) = weak_cert_store.upgrade() {
                cert_store.update_ocsp(None); // Remove old OCSP
                if let Some((res, date)) = fetch_ocsp(&cert).await {
                    debug!("Fetch OCSP response success");
                    cert_store.update_ocsp(Some(res));
                    next_run = Instant::now() + date.signed_duration_since(Utc::now()).to_std().unwrap_or_default();
                }
            } else {
                debug!("Cert dropped, stop old OCSP stapling worker");
                break;
            }

            sleep_until(next_run).await;
        }
    });

    config
}

async fn fetch_ocsp(full_chain: &ParsedCert) -> Option<(Vec<u8>, DateTime<Utc>)> {
    let cert = Certificate::from_der(full_chain.cert()?).ok()?;
    let issuer = Certificate::from_der(full_chain.issue()?).ok()?;

    if cert.tbs_certificate.issuer != issuer.tbs_certificate.subject {
        warn!("OCSP stapling: issuer and subject mismatch");
        return None;
    }

    // Extract OCSP URL
    let aia: Vec<AccessDescription> = cert.tbs_certificate.get::<AuthorityInfoAccessSyntax>().ok().flatten()?.1 .0;
    let mut url = match aia.iter().find(|aia| aia.access_method == ID_AD_OCSP)?.access_location.clone() {
        GeneralName::UniformResourceIdentifier(url) => Url::parse(url.as_str()).ok()?,
        _ => return None,
    };

    // Create OCSP request
    // Let's Encrypt follow rfc5019 2.1.1: Clients MUST use SHA1 as the hashing algorithm for the CertID.issuerNameHash and the CertID.issuerKeyHash values.
    // ref: https://github.com/letsencrypt/boulder/issues/5523#issuecomment-877301162
    let ocsp_request = OcspRequestBuilder::default()
        .with_request(Request::from_cert::<Sha1>(&issuer, &cert).ok()?)
        .build()
        .to_der()
        .ok()?;

    url.path_segments_mut().unwrap().push(&BASE64_STANDARD.encode(ocsp_request));

    // Check response
    let res = reqwest::get(url).await.ok()?.bytes().await.ok()?;
    let ocsp = OcspResponse::from_der(&res).ok()?;
    if ocsp.response_status == OcspResponseStatus::Successful && ocsp.response_bytes.is_some() {
        let response = BasicOcspResponse::from_der(ocsp.response_bytes.unwrap().response.as_bytes())
            .ok()?
            .tbs_response_data
            .responses
            .into_iter()
            .next()?;
        if response.cert_status == CertStatus::good() {
            let update = DateTime::from(UNIX_EPOCH + response.next_update?.0.to_unix_duration());
            return Some((res.to_vec(), update));
        }
    }

    None
}
