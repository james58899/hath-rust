use std::time::Duration;

use aws_lc_rs::digest;
use const_format::concatcp;
use futures::future::try_join_all;
use reqwest::Proxy;
use rustls::{
    ClientConfig, RootCertStore,
    compress::CompressionCache,
    crypto::{
        CryptoProvider,
        aws_lc_rs::{cipher_suite, default_provider},
    },
};
use tokio::fs::create_dir_all;

use crate::CLIENT_VERSION;

pub fn string_to_hash(str: String) -> String {
    hex::encode(digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, str.as_bytes()))
}

pub fn create_http_client(timeout: Duration, proxy: Option<Proxy>) -> reqwest::Client {
    let root_store = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut tls = ClientConfig::builder_with_provider(ssl_provider().into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls.cert_compression_cache = CompressionCache::Disabled.into(); // Client does not send certificate

    let mut builder = reqwest::ClientBuilder::new()
        .user_agent(concatcp!("Hentai@Home ", CLIENT_VERSION))
        .use_preconfigured_tls(tls)
        .tcp_keepalive(Duration::from_secs(75)) // Linux default keepalive inverval
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(30))
        .timeout(timeout)
        .pool_idle_timeout(Duration::from_secs(3600))
        .pool_max_idle_per_host(8)
        .http1_title_case_headers()
        .http1_only();

    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    } else {
        builder = builder.no_proxy();
    }

    builder.build().unwrap()
}

pub async fn create_dirs(dirs: Vec<&str>) -> Result<Vec<()>, std::io::Error> {
    try_join_all(dirs.iter().map(create_dir_all)).await
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
pub fn aes_support() -> bool {
    cpufeatures::new!(cpuid_aes, "aes");
    cpuid_aes::get()
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
pub fn aes_support() -> bool {
    false // Unable to check AES acceleration support, assumed negative.
}

pub fn ssl_provider() -> CryptoProvider {
    let mut provider = default_provider();
    // Prefer ChaCha20 when AES acceleration is not available.
    provider.cipher_suites = if aes_support() {
        vec![
            cipher_suite::TLS13_AES_128_GCM_SHA256,
            cipher_suite::TLS13_AES_256_GCM_SHA384,
            cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
            cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
            cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        ]
    } else {
        vec![
            cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
            cipher_suite::TLS13_AES_128_GCM_SHA256,
            cipher_suite::TLS13_AES_256_GCM_SHA384,
            cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
            cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        ]
    };

    provider
}
