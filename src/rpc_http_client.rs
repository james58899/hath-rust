use std::{
    error::Error,
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
use hyper::{Request, Response, body::Incoming, client::conn::http1::handshake, http::Error as HttpError};
use hyper_util::rt::TokioIo;
use log::debug;
use parking_lot::Mutex;
use reqwest::{IntoUrl, Url};
use socket2::{SockRef, TcpKeepalive};
use tokio::{
    net::{TcpSocket, TcpStream},
    task::AbortHandle,
    time::timeout,
};

use crate::CLIENT_VERSION;

type Connection = Arc<Mutex<Option<(String, TokioIo<TcpStream>)>>>;
type RequestError = Box<dyn Error + Send + Sync>;

#[derive(Default)]
pub struct RPCHttpClient {
    timeout: Duration,
    conn: Connection,
    task: Mutex<Option<AbortHandle>>,
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
        if url.scheme() != "http" {
            return Err(IoError::new(InvalidInput, "Support http only").into());
        }

        let conn = self.conn.lock().take_if(|(key, _)| key == &server);
        let conn = match conn {
            Some((_, stream)) => {
                // Connection timeout 5s
                match timeout(Duration::from_secs(5), stream.inner().writable()).await {
                    Ok(Ok(_)) => stream,
                    _ => create_stream(&server).await?,
                }
            }
            None => create_stream(&server).await?,
        };
        let (mut sender, task) = handshake(conn).await?;
        // spawn a task to poll the connection and drive the HTTP state
        tokio::spawn(task);

        match timeout(self.timeout, sender.send_request(build_request(&url)?)).await.ok() {
            Some(res) => {
                match res {
                    Ok(r) => {
                        self.preconnect(&server);
                        self.check_status(r).await
                    },
                    Err(e) => Err(e.into()),
                }
            }
            None => Err(IoError::new(TimedOut, format!("Request timeout: url={}", url)).into()),
        }
    }

    pub fn preconnect(&self, server: &str) {
        // Connected
        if self.conn.lock().as_ref().is_some_and(|(key, _)| key == server) {
            return;
        }
        // Connecting
        let mut task = self.task.lock();
        if !task.as_ref().is_none_or(|job| job.is_finished()) {
            return;
        }

        let server = server.to_owned();
        let preconnect = Arc::downgrade(&self.conn);
        task.replace(
            tokio::spawn(async move {
                debug!("Preconnecting to {}", server);
                match create_stream(&server).await {
                    Ok(stream) => {
                        debug!("Preconnected to {}", server);
                        if let Some(preconnect) = preconnect.upgrade() {
                            preconnect.lock().replace((server.to_owned(), stream));
                        }
                    }
                    Err(err) => debug!("Preconnect error: {}", err),
                }
            })
            .abort_handle(),
        );
    }

    async fn check_status(&self, res: Response<Incoming>) -> Result<Response<Incoming>, RequestError> {
        let status = res.status();
        if status.is_client_error() || status.is_server_error() {
            Err(crate::error::Error::ServerError {
                status,
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
        let addr = fastrand::choice(addrs).ok_or(IoError::new(AddrNotAvailable, format!("Fail to resolve {}", server)))?;

        // Socket settings
        let socket = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }?;
        let socket2 = SockRef::from(&socket);
        let keepalive = TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(15))
            .with_retries(6);
        let _ = socket2.set_tcp_keepalive(&keepalive);
        let _ = socket2.set_tcp_nodelay(true);

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
