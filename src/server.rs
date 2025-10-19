use std::{
    cmp::min,
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use axum::{Extension, Router, extract::FromRequestParts, http::request::Parts, routing::get};
use futures::pin_mut;
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use log::{info, warn};
use p12::PFX;
use rustls::{
    ServerConfig,
    client::verify_server_name,
    compress::CompressionCache,
    crypto::{CryptoProvider, aws_lc_rs::Ticketer},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    server::{ClientHello, NoServerSessionStorage, ParsedCertificate, ResolvesServerCert},
    sign::CertifiedKey,
};
use tokio::{
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{Notify, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tower::Layer;

use crate::{
    AppState, middleware, route,
    util::{aes_support, ssl_provider},
};

pub struct Server {
    handle: Arc<ServerHandle>,
    task: JoinHandle<()>,
}

pub struct ServerHandle {
    shutdown: AtomicBool,
    shutdown_notify: Notify,
    cert_store: Arc<CertStore>,
}

impl Server {
    pub fn new(port: u16, cert: ParsedCert, data: AppState, flood_control: bool, enable_metrics: bool, sni_strict: bool) -> Self {
        let provider = Arc::new(ssl_provider());
        let cert_store = Arc::new(CertStore::new(provider.clone(), cert, sni_strict));
        let ssl_config = Arc::new(create_ssl_config(provider, cert_store.clone()));
        let handle = Arc::new(ServerHandle::new(cert_store));
        let mut listener = bind(SocketAddr::from(([0, 0, 0, 0], port)));

        let mut http = http1::Builder::new();
        http.keep_alive(false)
            .max_buf_size(16384) // SSL max record size is 16KiB
            .timer(TokioTimer::new())
            .title_case_headers(true)
            .writev(true)
            .header_read_timeout(Duration::from_secs(15));

        let mut router = Router::new();
        router = route::register_route(router);
        if enable_metrics {
            router = router.route("/metrics", get(route::metrics));
            info!("Metrics endpoint enabled at '/metrics'");
        }
        router = middleware::register_layer(router, &data);
        let router = router.with_state(Arc::new(data));

        let handle2 = handle.clone();
        // Accept loop
        let task = tokio::spawn(async move {
            let (shutdown_tx, _) = watch::channel(());
            let mut flood_control = if flood_control { Some(FloodControl::new()) } else { None };
            loop {
                if handle.shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // Wait for new tcp connection
                let (stream, addr) = tokio::select! {
                    biased;
                    result = accept(&mut listener) => result,
                    _ = handle.shutdown_notify.notified() => break, // Shutdown
                };

                // Flood control
                if flood_control.as_mut().is_some_and(|fc| fc.hit(addr)) {
                    // Flood detected
                    let _ = stream.set_linger(Some(Duration::ZERO)); // RST connection
                    drop(stream);
                    continue;
                }

                // Process connection
                let ssl_config = ssl_config.clone();
                let http = http.clone();
                let service = Extension(ClientAddr(addr)).layer(router.clone());
                let mut shutdown_rx = shutdown_tx.subscribe();
                tokio::spawn(async move {
                    // TLS handshake
                    let acceptor = TlsAcceptor::from(ssl_config);
                    let ssl_stream = match timeout(Duration::from_secs(10), acceptor.accept(stream)).await {
                        Ok(Ok(s)) => s,
                        _ => return, // Handshake timeout or error
                    };

                    // Disable nodelay after handshake
                    let _ = ssl_stream.get_ref().0.set_nodelay(false);

                    // Process request
                    let stream = TokioIo::new(ssl_stream); // Tokio to hyper trait
                    let service = TowerToHyperService::new(service); // Tower to hyper trait
                    let fut = timeout(Duration::from_secs(181), http.serve_connection(stream, service));
                    pin_mut!(fut);
                    tokio::select! {
                        biased;
                        _ = &mut fut => (),
                        _ = shutdown_rx.changed() => { // Graceful shutdown
                            let _ = timeout(Duration::from_secs(10), fut).await;
                        },
                    };
                });
            }

            // Close listener
            drop(listener);
            info!("Server listener closed!");

            // Wait connection complated or timeout
            shutdown_tx.send_replace(()); // Notify all connection
            let _ = timeout(Duration::from_secs(15), shutdown_tx.closed()).await;
        });

        Self { handle: handle2, task }
    }

    pub fn handle(&self) -> Arc<ServerHandle> {
        self.handle.clone()
    }

    /// Graceful shutdown and wait server loop stop.
    pub async fn shutdown(self) {
        self.handle.shutdown();
        let _ = self.task.await;
    }
}

impl ServerHandle {
    fn new(cert_store: Arc<CertStore>) -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
            cert_store,
        }
    }

    pub fn update_cert(&self, cert: ParsedCert) -> Result<(), rustls::Error> {
        self.cert_store.update(cert)
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.shutdown_notify.notify_waiters();
    }
}

#[derive(Clone)]
pub struct ClientAddr(pub SocketAddr);

impl Deref for ClientAddr {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> FromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = <Extension<Self> as FromRequestParts<S>>::Rejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Extension::<Self>::from_request_parts(parts, state).await {
            Ok(Extension(connect_info)) => Ok(connect_info),
            Err(err) => Err(err),
        }
    }
}

fn bind(addr: SocketAddr) -> TcpListener {
    let socket = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4(),
        SocketAddr::V6(_) => TcpSocket::new_v6(),
    }
    .expect("Server listener socket create error.");

    let _ = socket.set_reuseaddr(true); // Without this need wait all connection close before restart
    let _ = socket.set_keepalive(true);
    let _ = socket.set_nodelay(true);

    socket.bind(addr).expect("Server listener bind error.");
    socket
        .listen(512)
        .inspect(|_| info!("Server is listen on {addr}"))
        .expect("Server listener listen error.")
}

async fn accept(listener: &mut TcpListener) -> (TcpStream, SocketAddr) {
    loop {
        match listener.accept().await {
            Ok(value) => return value,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await, // Too many open files, retry.
        }
    }
}

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

fn create_ssl_config(provider: Arc<CryptoProvider>, cert_store: Arc<CertStore>) -> ServerConfig {
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(cert_store);

    // Prefer ChaCha20 on non-AES hardware.
    config.ignore_client_order = true;
    config.prioritize_chacha = aes_support();
    // Only support ticket resumption.
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.ticketer = Ticketer::new().unwrap();
    config.send_tls13_tickets = 1;
    config.cert_compression_cache = CompressionCache::new(2).into(); // 1 cert * 2 compression algorithms

    config
}

#[derive(Debug)]
struct CertStore {
    provider: Arc<CryptoProvider>,
    cert: ArcSwap<CertifiedKey>,
    strict: bool,
}

impl CertStore {
    fn new(provider: Arc<CryptoProvider>, cert: ParsedCert, strict: bool) -> Self {
        let cert = ArcSwap::new(CertifiedKey::from_der(cert.certs, cert.key, &provider).unwrap().into());
        Self { provider, cert, strict }
    }

    fn update(&self, cert: ParsedCert) -> Result<(), rustls::Error> {
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

struct FloodControl {
    last_clean: Instant,
    hit_map: HashMap<IpAddr, (u8, Instant)>,
    block_list: HashMap<IpAddr, Instant>,
}

impl FloodControl {
    fn new() -> Self {
        Self {
            last_clean: Instant::now(),
            hit_map: HashMap::new(),
            block_list: HashMap::new(),
        }
    }

    fn hit(&mut self, addr: SocketAddr) -> bool {
        let now = Instant::now();
        let ip = addr.ip();

        // Clean old entry
        if self.last_clean.elapsed() > Duration::from_secs(10) {
            self.block_list.retain(|_, time| *time > now);
            self.hit_map.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(10));
            self.block_list.shrink_to_fit();
            self.hit_map.shrink_to_fit();
            self.last_clean = now;
        }

        // Check block
        if self.block_list.get(&ip).is_some_and(|time| time > &now) {
            return true;
        }

        // Update hit map
        let (hit, time) = self.hit_map.entry(ip).or_insert_with(|| (0, now));
        let time_diff = min(10, (now - *time).as_secs()) as u8;
        *hit = hit.saturating_sub(time_diff as u8) + 1;
        *time = now;

        // Check hit
        if *hit >= 10 {
            warn!("Flood control activated for {ip} (blocking for 60 seconds)");
            self.block_list.insert(ip, now + Duration::from_secs(60));
            true
        } else {
            false
        }
    }
}
