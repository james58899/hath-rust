use std::{
    io::{
        Error as IoError,
        ErrorKind::{AddrNotAvailable, InvalidInput, TimedOut, self},
    },
    net::ToSocketAddrs,
    time::Duration, sync::Arc,
};

use hyper::{client::conn::handshake, http::Error as HttpError, Body, Request, Response};
use log::{debug, error};
use parking_lot::Mutex;
use rand::{seq::IteratorRandom, thread_rng};
use reqwest::{IntoUrl, Url};
use socket2::{SockRef, TcpKeepalive};
use tokio::{net::TcpStream, time::timeout, task::AbortHandle};

use crate::CLIENT_VERSION;

type RequestError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Default)]
pub struct RPCHttpClient {
    timeout: Duration,
    preconnect: Arc<Mutex<Option<(String, TcpStream)>>>,
    connecting: Mutex<Option<AbortHandle>>
}

impl RPCHttpClient {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Default::default()
        }
    }

    pub async fn get<U: IntoUrl>(&self, url: U) -> Result<Response<Body>, RequestError> {
        let url = url.into_url()?;
        let host = url.host().ok_or(IoError::new(InvalidInput, "uri has no host"))?;
        let port = url.port().unwrap_or_else(|| if url.scheme() == "https" { 443 } else { 80 });
        let server = format!("{host}:{port}");

        let conn = self.preconnect.lock().take();
        let conn = match conn.and_then(|(key, stream)| if key == server { Some(stream) } else { None }) {
            Some(v) => {
                let writeable = v.writable().await;
                if writeable.is_ok() || writeable.unwrap_err().kind() == ErrorKind::WouldBlock {
                    v
                } else {
                    // Connection broken, create new
                    create_stream(&server).await?
                }
            },
            None => create_stream(&server).await?,
        };
        let (mut sender, conn) = handshake(conn).await?;
        // spawn a task to poll the connection and drive the HTTP state
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("Error in connection: {}", e);
            }
        });        

        match timeout(self.timeout, sender.send_request(build_request(&url)?)).await.ok() {
            Some(v) => {
                self.preconnect(&server);
                v.map_err(|e| e.into())
            },
            None => Err(Box::new(IoError::new(TimedOut, format!("Request timeout: url={}", url)))),
        }
    }

    pub fn preconnect(&self, server: &str) {
        if self.preconnect.lock().is_some() { // Connected
            return;
        }
        let mut job = self.connecting.lock();
        if !job.as_ref().map_or(true, |job|job.is_finished()) { // Connecting
            return;
        }

        let server = server.to_owned();
        let preconnect = Arc::downgrade(&self.preconnect);
        job.replace(tokio::spawn(async move {
            debug!("Preconnecting to {}", server);
            match create_stream(&server).await {
                Ok(stream) => {
                    debug!("Preconnected to {}", server);
                    if let Some(preconnect) = preconnect.upgrade() {
                        preconnect.lock().replace((server.to_owned(), stream));
                    }
                }
                Err(err) => debug!("Pre connect error: {}", err),
            }
        }).abort_handle());
    }
}

async fn create_stream(server: &str) -> Result<TcpStream, IoError> {
    if let Ok(addrs) = server.to_socket_addrs() {
        let addr = addrs
            .choose(&mut thread_rng())
            .ok_or(IoError::new(AddrNotAvailable, format!("Fail to resolve {}", server)))?;

        let stream = match timeout(Duration::from_secs(5), TcpStream::connect(addr)).await.ok() {
            Some(r) => r,
            None => Err(IoError::new(TimedOut, format!("Connect timeout: addr={addr}"))),
        };

        if let Ok(stream) = &stream {
            let socket = SockRef::from(&stream);
            let keepalive = TcpKeepalive::new()
                .with_time(Duration::from_secs(30))
                .with_interval(Duration::from_secs(75));
            let _ = socket.set_tcp_keepalive(&keepalive);
            let _ = stream.set_nodelay(true);
        }

        stream
    } else {
        Err(IoError::new(InvalidInput, format!("Fail parse host {}", server)))
    }
}

fn build_request(url: &Url) -> Result<Request<Body>, HttpError> {
    Request::builder()
        .header("Host", url.host_str().unwrap())
        .header("User-Agent", format!("Hentai@Home {CLIENT_VERSION}"))
        .uri(url.as_str())
        .body(Body::empty())
}

pub async fn check_status(res: Response<Body>) -> Result<Response<Body>, RequestError> {
    let status = res.status();
    if status.is_client_error() || status.is_server_error() {
        Err(crate::error::Error::ServerError {
            status: res.status().as_u16(),
            body: hyper::body::to_bytes(res.into_body())
                .await
                .ok()
                .and_then(|b| String::from_utf8(b.to_vec()).ok()),
        }
        .into())
    } else {
        Ok(res)
    }
}

pub async fn to_text(res: Response<Body>) -> Result<String, RequestError> {
    String::from_utf8(hyper::body::to_bytes(res.into_body()).await?.to_vec()).map_err(|e| e.into())
}
