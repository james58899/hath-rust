use std::{
    net::SocketAddr,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use axum::{async_trait, extract::FromRequestParts, http::request::Parts, Extension, Router};
use futures::pin_mut;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use log::info;
use openssl::{
    pkcs12::ParsedPkcs12_2,
    ssl::{Ssl, SslAcceptor},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{watch, Notify},
    task::JoinHandle,
    time::timeout,
};
#[cfg(not(unix))]
use tokio_openssl::SslStream;
use tower::{Layer, Service};

#[cfg(unix)]
use crate::server::ssl_stream::SslStream;
use crate::{middleware, route, server::ssl::create_ssl_acceptor, AppState};

pub mod ssl;
#[cfg(unix)]
mod ssl_stream;

pub struct Server {
    handle: Arc<ServerHandle>,
    task: JoinHandle<()>,
}

pub struct ServerHandle {
    shutdown: AtomicBool,
    shutdown_notify: Notify,
    ssl_acceptor: ArcSwap<SslAcceptor>,
}

impl Server {
    pub fn new(port: u16, cert: ParsedPkcs12_2, data: AppState) -> Self {
        let handle = Arc::new(ServerHandle::new(create_ssl_acceptor(cert)));
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
        router = middleware::register_layer(router, &data);
        let router = router.with_state(Arc::new(data));

        let handle2 = handle.clone();
        // Accept loop
        let task = tokio::spawn(async move {
            let (shutdown_tx, _) = watch::channel(());
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

                // Process connection
                let handle = handle.clone();
                let http = http.clone();
                let service = Extension(ClientAddr(addr)).layer(router.clone());
                let mut shutdown_rx = shutdown_tx.subscribe();
                tokio::spawn(async move {
                    // TLS handshake
                    let mut ssl_stream = SslStream::new(Ssl::new(handle.ssl_acceptor.load().context()).unwrap(), stream).unwrap();
                    match timeout(Duration::from_secs(10), ssl_accept(&mut ssl_stream)).await {
                        Ok(Ok(_)) => (),
                        _ => return, // Handshake timeout or error
                    }

                    // Disable nodelay after handshake
                    let _ = ssl_stream.get_ref().set_nodelay(false);

                    // First byte timeout
                    if (timeout(Duration::from_secs(10), ssl_peek(&mut ssl_stream)).await).is_err() {
                        let _ = ssl_stream.shutdown().await;
                        return;
                    }

                    // Process request
                    let stream = TokioIo::new(ssl_stream); // Tokio to hyper trait
                    let service = hyper::service::service_fn(move |r| service.clone().call(r)); // Tower to hyper trait
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

#[cfg(unix)]
#[inline]
async fn ssl_accept(stream: &mut SslStream) -> Result<(), std::io::Error> {
    stream.accept().await
}

#[cfg(not(unix))]
#[inline]
async fn ssl_accept(stream: &mut SslStream<TcpStream>) -> Result<(), openssl::ssl::Error> {
    use std::pin::Pin;

    SslStream::accept(Pin::new(stream)).await
}

#[cfg(unix)]
#[inline]
async fn ssl_peek(stream: &mut SslStream) {
    let _ = stream.peek(&mut [0]).await;
}

#[cfg(not(unix))]
#[inline]
async fn ssl_peek(stream: &mut SslStream<TcpStream>) {
    use std::pin::Pin;

    let _ = SslStream::peek(Pin::new(stream), &mut [0]).await;
}

impl ServerHandle {
    fn new(ssl: SslAcceptor) -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
            ssl_acceptor: ArcSwap::new(Arc::new(ssl)),
        }
    }

    pub fn update_cert(&self, cert: ParsedPkcs12_2) {
        let acceptor = create_ssl_acceptor(cert);
        self.ssl_acceptor.store(Arc::new(acceptor));
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

#[async_trait]
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
