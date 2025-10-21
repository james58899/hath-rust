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
    vec,
};

use arc_swap::ArcSwap;
use axum::{
    Extension, Router,
    extract::{FromRequestParts, Request},
    http::{HeaderValue, request::Parts},
    response::Response,
    routing::get,
};
use futures::{future::join, pin_mut};
use h3::{error::Code, server::Connection};
use h3_quinn::quinn;
use http_body_util::BodyExt;
use hyper::{header::{ALT_SVC, CONNECTION, DATE}, server::conn::http1, Version};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use log::{debug, error, info, warn};
use p12::PFX;
use quinn::{Endpoint, TransportConfig, crypto::rustls::QuicServerConfig};
use rustls::{
    ServerConfig,
    client::verify_server_name,
    compress::CompressionCache,
    crypto::{CryptoProvider, aws_lc_rs::Ticketer},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    server::{ClientHello, NoServerSessionStorage, ParsedCertificate, ResolvesServerCert},
    sign::CertifiedKey,
    version::TLS13,
};
use tokio::{
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{Notify, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tower::{Layer, Service};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::{
    AppState, middleware, route,
    util::{aes_support, ssl_provider},
};

mod date_header;

pub struct Server {
    handle: Arc<ServerHandle>,
    accept_task: JoinHandle<()>,
    accept_task_h3: Option<JoinHandle<()>>,
}

impl Server {
    pub fn new(port: u16, cert: ParsedCert, data: AppState, flood_control: bool, enable_metrics: bool, sni_strict: bool, h3: bool) -> Self {
        let provider = Arc::new(ssl_provider());
        let cert_store = Arc::new(CertStore::new(provider.clone(), cert, sni_strict));
        let handle = Arc::new(ServerHandle::new(cert_store.clone()));

        let acceptor = TlsAcceptor::from(Arc::new(create_ssl_config(provider.clone(), cert_store.clone())));
        let listener = bind(SocketAddr::from(([0, 0, 0, 0], port)));

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

        // Accept loop
        let h3_port = if h3 { Some(port) } else { None };
        let accept_task = tokio::spawn(accept_loop(handle.clone(), listener, acceptor, http, router.clone(), flood_control, h3_port));

        // h3
        let accept_task_h3 = if h3 {
            let quinn_config = create_quic_config(provider, cert_store);
            let endpoint = quinn::Endpoint::server(quinn_config, SocketAddr::from(([0, 0, 0, 0], port))).unwrap();
            Some(tokio::spawn(accept_loop_h3(handle.clone(), endpoint, router)))
        } else {
            None
        };

        Self {
            handle,
            accept_task,
            accept_task_h3,
        }
    }

    pub fn handle(&self) -> Arc<ServerHandle> {
        self.handle.clone()
    }

    /// Graceful shutdown and wait server loop stop.
    pub async fn shutdown(self) {
        self.handle.shutdown();
        let _ = self.accept_task.await;
        if let Some(handle) = self.accept_task_h3 {
            let _ = handle.await;
        }
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

fn create_quic_config(provider: Arc<CryptoProvider>, cert_store: Arc<CertStore>) -> quinn::ServerConfig {
    let mut ssl_config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13]) // QUIC requires TLS 1.3
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(cert_store);

    // Prefer ChaCha20 on non-AES hardware.
    ssl_config.ignore_client_order = true;
    ssl_config.prioritize_chacha = aes_support();
    // Only support ticket resumption.
    ssl_config.session_storage = Arc::new(NoServerSessionStorage {});
    ssl_config.ticketer = Ticketer::new().unwrap();
    ssl_config.send_tls13_tickets = 1;
    ssl_config.cert_compression_cache = CompressionCache::new(2).into(); // 1 cert * 2 compression algorithms
    // h3 settings
    ssl_config.alpn_protocols = vec![b"h3".into()];
    ssl_config.max_early_data_size = u32::MAX; // 0-RTT support

    let mut transport_config = TransportConfig::default();
    transport_config.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default())); // TODO replace with BBRv3
    transport_config.send_fairness(false); // We don't need send fairness
    transport_config.send_window(2500000); // 100Mbps * 200ms (2.5MB per connection) // TODO check size
    transport_config.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(ssl_config).unwrap()));
    server_config.transport_config(transport_config.into());
    server_config
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

async fn accept_loop(
    handle: Arc<ServerHandle>,
    mut listener: TcpListener,
    acceptor: TlsAcceptor,
    http: http1::Builder,
    mut router: Router,
    flood_control: bool,
    h3_port: Option<u16>,
) {
    let (shutdown_tx, _) = watch::channel(());
    let mut flood_control = if flood_control { Some(FloodControl::new()) } else { None };
    // Add h3 Alt-svc header
    if let Some(port) = h3_port {
        let value = HeaderValue::from_str(&format!("h3=\":{}\"", port)).unwrap();
        let h3_header = SetResponseHeaderLayer::appending(ALT_SVC, value);
        router = router.layer(h3_header)
    }
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
        let acceptor = acceptor.clone();
        let http = http.clone();
        let service = Extension(ClientAddr(addr)).layer(router.clone());
        let mut shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            // TLS handshake
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
}

async fn accept_loop_h3(handle: Arc<ServerHandle>, endpoint: Endpoint, router: Router) {
    let (shutdown_tx, _) = watch::channel(());

    loop {
        if handle.shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Wait new connection
        let new_conn = tokio::select! {
            biased;
            Some(result) = endpoint.accept() => result,
            _ = handle.shutdown_notify.notified() => break, // Shutdown
        };

        let addr = new_conn.remote_address();

        // TODO h3 need flood control?

        // Process connection
        let router = router.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            // QUIC handshake
            let conn = match new_conn.await {
                Ok(conn) => conn,
                Err(err) => {
                    debug!("quic handshake error: {err}");
                    return;
                }
            };

            // H3 Handshake
            let mut h3_conn = match Connection::new(h3_quinn::Connection::new(conn)).await {
                Ok(conn) => conn,
                Err(err) => {
                    debug!("h3 handshake error: {err}");
                    return;
                }
            };

            // Request loop
            loop {
                let resolver = tokio::select! {
                    biased;
                    conn = h3_conn.accept() => conn,
                    _ = shutdown_rx.changed() => { // Graceful shutdown
                        let _ = h3_conn.shutdown(1).await;
                        continue;
                    },
                };
                match resolver {
                    Ok(Some(resolver)) => {
                        let router = router.clone();
                        let shutdown_rx = shutdown_rx.clone();
                        tokio::spawn(async move {
                            let (req, mut stream) = match resolver.resolve_request().await {
                                Ok(res) => res,
                                Err(err) => {
                                    debug!("h3 read request error: {err}");
                                    return;
                                }
                            };
                            stream.stop_sending(Code::H3_NO_ERROR);
                            while let Ok(Some(_)) = stream.recv_data().await {} // Drain data
                            let mut request = Request::builder().version(req.version()).method(req.method()).uri(req.uri());
                            *request.headers_mut().unwrap() = req.headers().clone();
                            let request = request.body(http_body_util::Empty::new()).unwrap(); // We don't need body

                            // ref: https://github.com/hyperium/h3/issues/261#issuecomment-2352838801
                            // Call Service
                            let mut service = Extension(ClientAddr(addr)).layer(router.clone());
                            let res = service.call(request).await.unwrap();

                            // Process response
                            let mut streamed_res = Response::builder().status(res.status()).version(Version::HTTP_3);
                            let header_mut = streamed_res.headers_mut().unwrap();
                            *header_mut = res.headers().clone();
                            header_mut.remove(CONNECTION); // H3 not allow connection header
                            header_mut.entry(DATE).or_insert_with(date_header::update_and_header_value); // Add date header

                            // Send response back
                            if let Err(err) = stream.send_response(streamed_res.body(()).unwrap()).await {
                                debug!("h3 send response error: {err}");
                                return;
                            }
                            // send response body and trailers
                            let mut buf = res.into_body().into_data_stream();
                            while let Some(chunk) = buf.frame().await {
                                match chunk {
                                    Ok(frame) => {
                                        // trailers
                                        if frame.is_trailers() {
                                            let trailers = frame.into_trailers().unwrap();
                                            if let Err(err) = stream.send_trailers(trailers).await {
                                                debug!("h3 send trailers error: {err}");
                                                return;
                                            }
                                        } else if frame.is_data() {
                                            let data = frame.into_data().unwrap();
                                            if let Err(err) = stream.send_data(data).await {
                                                debug!("h3 send data error: {err}");
                                                return;
                                            };
                                        }
                                    }
                                    Err(err) => {
                                        error!("Failed to read stream {err}");
                                    }
                                }
                            }
                            let _ = stream.finish().await;
                            // TODO Find a way to wait send buffer empty before return
                            drop(shutdown_rx); // Hold shutdown channel
                        });
                    }
                    // indicating that the remote sent a goaway frame
                    // all requests have been processed
                    Ok(None) => {
                        break;
                    }
                    Err(_) => {
                        break;
                    }
                }
            }
        });
    }

    // Wait connection complated or timeout
    shutdown_tx.send_replace(()); // Notify all connection
    let _ = timeout(Duration::from_secs(15), join(shutdown_tx.closed(), endpoint.wait_idle())).await;
    endpoint.close(0u8.into(), b"Shutdown");
}

pub struct ServerHandle {
    shutdown: AtomicBool,
    shutdown_notify: Notify,
    cert_store: Arc<CertStore>,
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
