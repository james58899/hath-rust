use std::{
    io::{
        Error as IoError,
        ErrorKind::{AddrNotAvailable, InvalidInput, TimedOut},
    },
    net::ToSocketAddrs,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt, Collected, Empty};
use hyper::{body::Incoming, client::conn::http1::handshake, http::Error as HttpError, Request, Response};
use hyper_util::rt::TokioIo;
use log::debug;
use parking_lot::Mutex;
use rand::{seq::IteratorRandom, thread_rng};
use reqwest::{IntoUrl, Url};
use socket2::{SockRef, TcpKeepalive};
use tokio::{
    net::{TcpSocket, TcpStream},
    task::AbortHandle,
    time::timeout,
};

use crate::CLIENT_VERSION;

type Connection = Arc<Mutex<Option<(String, TokioIo<TcpStream>)>>>;
type RequestError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Default)]
pub struct RPCHttpClient {
    timeout: Duration,
    preconnect: Connection,
    connecting: Mutex<Option<AbortHandle>>,
}

impl RPCHttpClient {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Default::default()
        }
    }

    pub async fn get<U: IntoUrl>(&self, url: U) -> Result<Response<Incoming>, RequestError> {
        let url = url.into_url()?;
        let host = url.host().ok_or(IoError::new(InvalidInput, "uri has no host"))?;
        let port = url.port().unwrap_or(80);
        let server = format!("{host}:{port}");
        if url.scheme() == "https" {
            return Err(IoError::new(InvalidInput, "https not supported yet").into());
        }

        let conn = self.preconnect.lock().take();
        let conn = match conn.and_then(|(key, stream)| if key == server { Some(stream) } else { None }) {
            Some(v) => {
                // Connection timeout 5s
                match timeout(Duration::from_secs(5), v.inner().writable()).await {
                    Ok(Ok(_)) => v,
                    _ => create_stream(&server).await?,
                }
            }
            None => create_stream(&server).await?,
        };
        let (mut sender, conn) = handshake(conn).await?;
        // spawn a task to poll the connection and drive the HTTP state
        tokio::spawn(conn);

        match timeout(self.timeout, sender.send_request(build_request(&url)?)).await.ok() {
            Some(v) => {
                self.preconnect(&server);
                match v {
                    Ok(r) => self.check_status(r).await,
                    Err(e) => Err(e.into()),
                }
            }
            None => Err(Box::new(IoError::new(TimedOut, format!("Request timeout: url={}", url)))),
        }
    }

    pub fn preconnect(&self, server: &str) {
        // Connected
        if self.preconnect.lock().is_some() {
            return;
        }
        // Connecting
        let mut job = self.connecting.lock();
        if !job.as_ref().map_or(true, |job| job.is_finished()) {
            return;
        }

        let server = server.to_owned();
        let preconnect = Arc::downgrade(&self.preconnect);
        job.replace(
            tokio::spawn(async move {
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
            })
            .abort_handle(),
        );
    }

    async fn check_status(&self, res: Response<Incoming>) -> Result<Response<Incoming>, RequestError> {
        let status = res.status();
        if status.is_client_error() || status.is_server_error() {
            Err(crate::error::Error::ServerError {
                status: res.status().as_u16(),
                body: self.to_bytes(res).await.ok().and_then(|b| String::from_utf8(b.to_vec()).ok()),
            }
            .into())
        } else {
            Ok(res)
        }
    }

    pub async fn to_text(&self, res: Response<Incoming>) -> Result<String, RequestError> {
        Ok(String::from_utf8(self.to_bytes(res).await?.to_vec())?)
    }

    pub async fn to_bytes(&self, res: Response<Incoming>) -> Result<Bytes, RequestError> {
        match timeout(self.timeout, res.into_body().collect()).await.ok() {
            Some(v) => Ok(v.map(Collected::to_bytes)?),
            None => Err(Box::new(IoError::new(TimedOut, "Read response timeout".to_string()))),
        }
    }
}

async fn create_stream(server: &str) -> Result<TokioIo<TcpStream>, IoError> {
    if let Ok(addrs) = server.to_socket_addrs() {
        let addr = addrs
            .choose(&mut thread_rng())
            .ok_or(IoError::new(AddrNotAvailable, format!("Fail to resolve {}", server)))?;

        // Socket settings
        let socket = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }?;
        let socket2 = SockRef::from(&socket);
        let keepalive = TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(15));
        let _ = socket2.set_tcp_keepalive(&keepalive);
        let _ = socket2.set_nodelay(true);

        // Connect
        match timeout(Duration::from_secs(5), socket.connect(addr)).await.ok() {
            Some(r) => r.map(TokioIo::new),
            None => Err(IoError::new(TimedOut, format!("Connect timeout: addr={addr}"))),
        }
    } else {
        Err(IoError::new(InvalidInput, format!("Fail parse host {}", server)))
    }
}

fn build_request(url: &Url) -> Result<Request<Empty<Bytes>>, HttpError> {
    Request::builder()
        .header("Host", url.host_str().unwrap())
        .header("User-Agent", format!("Hentai@Home {CLIENT_VERSION}"))
        .uri(url.as_str())
        .body(Empty::<Bytes>::new())
}
