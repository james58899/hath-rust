use openssl::{
    pkcs12::ParsedPkcs12_2,
    ssl::{SslAcceptor, SslMethod, SslMode, SslOptions, SslSessionCacheMode},
};

pub fn create_ssl_acceptor(cert: &ParsedPkcs12_2) -> SslAcceptor {
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
