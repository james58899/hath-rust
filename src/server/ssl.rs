use std::{sync::Arc, time::Duration};

use arc_swap::{access::Access, ArcSwapOption};
use chrono::{DateTime, NaiveDateTime, Utc};
use log::debug;
use openssl::{
    base64,
    hash::MessageDigest,
    ocsp::{OcspCertId, OcspCertStatus, OcspRequest, OcspResponse},
    pkcs12::ParsedPkcs12_2,
    ssl::{SslAcceptor, SslMethod, SslMode, SslOptions, SslSessionCacheMode},
    x509::X509VerifyResult,
};
use reqwest::Url;
use tokio::time::{sleep_until, Instant};

pub fn create_ssl_acceptor(cert: ParsedPkcs12_2) -> SslAcceptor {
    // TODO error handle
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls_server()).unwrap();
    builder.clear_options(SslOptions::NO_TLSV1_3);
    builder.set_options(SslOptions::NO_RENEGOTIATION | SslOptions::ENABLE_MIDDLEBOX_COMPAT);
    builder.set_mode(SslMode::RELEASE_BUFFERS);
    builder.set_session_cache_mode(SslSessionCacheMode::OFF); // Disable session ID resumption
    let _ = builder.set_num_tickets(1);

    // From https://wiki.mozilla.org/Security/Server_Side_TLS#Old_backward_compatibility
    if !aes_support() {
        // Not have AES hardware acceleration, prefer ChaCha20.
        builder
            .set_cipher_list(
                "@SECLEVEL=0:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
                ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
                ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
                DHE-RSA-CHACHA20-POLY1305:DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384:\
                ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256:\
                ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:\
                ECDHE-ECDSA-AES256-SHA384:ECDHE-RSA-AES256-SHA384:\
                ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:\
                DHE-RSA-AES128-SHA256:DHE-RSA-AES256-SHA256:\
                AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA256:AES256-SHA256:AES128-SHA:AES256-SHA:\
                DES-CBC3-SHA",
            )
            .unwrap();
        builder
            .set_ciphersuites("TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384")
            .unwrap();
    } else {
        // Prioritize ChaCha ciphers when preferred by clients.
        builder.set_options(SslOptions::PRIORITIZE_CHACHA);

        builder
            .set_cipher_list(
                "@SECLEVEL=0:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
                ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
                ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
                DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384:DHE-RSA-CHACHA20-POLY1305:\
                ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256:\
                ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:\
                ECDHE-ECDSA-AES256-SHA384:ECDHE-RSA-AES256-SHA384:\
                ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:\
                DHE-RSA-AES128-SHA256:DHE-RSA-AES256-SHA256:\
                AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA256:AES256-SHA256:AES128-SHA:AES256-SHA:\
                DES-CBC3-SHA",
            )
            .unwrap();
        builder
            .set_ciphersuites("TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256")
            .unwrap();
    }
    builder.set_private_key(cert.pkey.as_ref().unwrap()).unwrap();
    builder.set_certificate(cert.cert.as_ref().unwrap()).unwrap();
    if let Some(i) = &cert.ca {
        i.iter().for_each(|j| builder.add_extra_chain_cert(j.to_owned()).unwrap());
    }

    // OCSP stapling
    let ocsp_store = Arc::new(ArcSwapOption::empty());
    let weak_ocsp_store = Arc::downgrade(&ocsp_store);
    tokio::spawn(async move {
        debug!("Start new OCSP stapling worker");
        loop {
            let mut next_run = Instant::now() + Duration::from_secs(3600);
            if let Some(ocsp_store) = weak_ocsp_store.upgrade() {
                ocsp_store.store(None);
                if let Some((res, date)) = fetch_ocsp(&cert).await {
                    debug!("Fetch OCSP response success");
                    ocsp_store.store(Some(Arc::new(res)));
                    next_run = Instant::now() + date.signed_duration_since(Utc::now()).to_std().unwrap_or_default();
                }
            } else {
                debug!("Cert dropped, stop old OCSP stapling worker");
                break;
            }

            sleep_until(next_run).await;
        }
    });
    let _ = builder.set_status_callback(move |ssl| {
        if let Some(res) = ocsp_store.load().as_ref() {
            Ok(ssl.set_ocsp_status(res).is_ok())
        } else {
            Ok(false)
        }
    });

    builder.build()
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
fn aes_support() -> bool {
    cpufeatures::new!(cpuid_aes, "aes");
    cpuid_aes::get()
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
fn aes_support() -> bool {
    false // Unable to check AES acceleration support, assumed negative.
}

async fn fetch_ocsp(full_chain: &ParsedPkcs12_2) -> Option<(Vec<u8>, DateTime<Utc>)> {
    let cert = full_chain.cert.as_ref()?;
    let chain = full_chain.ca.as_ref()?;
    let issuer = chain.iter().find(|ca| ca.issued(cert) == X509VerifyResult::OK)?;
    let mut url = cert.ocsp_responders().ok()?.get(0).and_then(|url| Url::parse(url).ok())?;
    let mut request = OcspRequest::new().unwrap();
    // Let's Encrypt follow rfc5019 2.1.1: Clients MUST use SHA1 as the hashing algorithm for the CertID.issuerNameHash and the CertID.issuerKeyHash values.
    // ref: https://github.com/letsencrypt/boulder/issues/5523#issuecomment-877301162
    request
        .add_id(OcspCertId::from_cert(MessageDigest::sha1(), cert, issuer).ok()?)
        .ok()?;
    let ocsp_encoded = request.to_der().map(|data| base64::encode_block(&data)).ok()?;
    url.path_segments_mut().unwrap().push(&ocsp_encoded);

    // Check response
    let response = reqwest::get(url).await.ok()?.bytes().await.ok()?;
    let ocsp = OcspResponse::from_der(&response).ok()?.basic().ok()?;
    let oid = OcspCertId::from_cert(MessageDigest::sha1(), cert, issuer).unwrap();
    let ocsp_status = ocsp.find_status(&oid)?;
    if ocsp_status.status == OcspCertStatus::GOOD {
        let update = NaiveDateTime::parse_from_str(&ocsp_status.next_update.to_string(), "%b %d %H:%M:%S %Y %Z")
            .ok()?
            .and_utc();
        return Some((response.to_vec(), update));
    }

    None
}
