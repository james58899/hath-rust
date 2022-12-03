use std::{
    collections::HashMap,
    error::Error,
    net::{Ipv4Addr, SocketAddrV4},
    ops::RangeInclusive,
    path::Path,
    sync::Arc,
    time::Duration,
};

use actix_tls::accept::openssl::TlsStream;
use actix_web::{
    dev::Service,
    http::{header, ConnectionType},
    middleware::DefaultHeaders,
    rt::net::TcpStream,
    web::{to, Data},
    App, HttpServer,
};
use clap::Parser;
use futures::TryFutureExt;
use log::{error, info, warn};
use mimalloc::MiMalloc;
use openssl::{
    asn1::Asn1Time,
    pkcs12::ParsedPkcs12,
    ssl::{ClientHelloResponse, SslAcceptor, SslAcceptorBuilder, SslMethod, SslOptions},
};
use parking_lot::{Mutex, RwLock};
use tempfile::TempPath;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, BufReader},
    runtime::Handle,
    signal::{self, unix::SignalKind},
    sync::{
        mpsc::{self, Sender, UnboundedReceiver},
        watch,
    },
    time::{sleep, sleep_until, Instant},
};

use crate::{
    cache_manager::{CacheFileInfo, CacheManager},
    gallery_downloader::GalleryDownloader,
    rpc::RPCClient,
    util::{create_dirs, create_http_client},
};

mod cache_manager;
mod error;
mod gallery_downloader;
mod logger;
mod middleware;
mod route;
mod rpc;
mod util;

type DownloadState = RwLock<HashMap<[u8; 20], (Arc<TempPath>, watch::Receiver<u64>)>>;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

static CLIENT_VERSION: &str = "1.6.1";
static MAX_KEY_TIME_DRIFT: RangeInclusive<i64> = -300..=300;

#[derive(Parser)]
struct Args {
    // Overrides the port set in the client's settings.
    #[arg(long)]
    port: Option<u16>,

    #[arg(long)]
    cache_dir: Option<String>,

    #[arg(long)]
    data_dir: Option<String>,

    #[arg(long)]
    download_dir: Option<String>,

    #[arg(long)]
    log_dir: Option<String>,

    #[arg(long)]
    temp_dir: Option<String>,
}

struct AppState {
    runtime: Handle,
    reqwest: reqwest::Client,
    id: i32,
    key: String,
    rpc: Arc<RPCClient>,
    download_state: DownloadState,
    cache_manager: Arc<CacheManager>,
    command_channel: Sender<Command>,
}

pub enum Command {
    ReloadCert,
    RefreshSettings,
    StartDownloader,
    Overload,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let cache_dir = args.cache_dir.unwrap_or_else(|| "./cache".to_string());
    let data_dir = args.data_dir.unwrap_or_else(|| "./data".to_string());
    let download_dir = args.download_dir.unwrap_or_else(|| "./download".to_string());
    let log_dir = args.log_dir.unwrap_or_else(|| "./log".to_string());
    let temp_dir = args.temp_dir.unwrap_or_else(|| "./tmp".to_string());

    create_dirs(vec![&data_dir, &log_dir, &cache_dir, &temp_dir, &download_dir]).await?;

    init_logger();

    info!("Hentai@Home {} (Rust) starting up", CLIENT_VERSION);

    let (id, key) = match read_credential(data_dir).await {
        Some(i) => i,
        None => todo!("Setup client"),
    };
    let client = Arc::new(RPCClient::new(id, &key));
    let init_settings = client.login().await?;

    let (shutdown_send, shutdown_recv) = mpsc::unbounded_channel::<()>();
    let settings = client.settings();
    let cache_manager = CacheManager::new(cache_dir, temp_dir, settings.clone(), &init_settings, shutdown_send.clone()).await?;

    // command channel
    let (tx, mut rx) = mpsc::channel::<Command>(1);
    let cert = client.get_cert().await.unwrap();
    if cert.cert.not_after() < Asn1Time::days_from_now(1).unwrap() {
        error!(
            "The retrieved certificate is expired, or the system time is off by more than a day. Correct the system time and try again."
        );
        return Err(error::Error::CertExpired.into());
    }

    let (server, cert_changer) = create_server(
        args.port.unwrap_or_else(|| init_settings.client_port()),
        cert,
        AppState {
            runtime: Handle::current(),
            reqwest: create_http_client(Duration::from_secs(60)),
            id,
            key,
            rpc: client.clone(),
            download_state: RwLock::new(HashMap::new()),
            cache_manager: cache_manager.clone(),
            command_channel: tx,
        },
    );
    let server_handle = server.handle();

    // Http server loop
    info!("Starting HTTP server...");
    tokio::spawn(server);

    info!("Notifying the server that we have finished starting up the client...");
    if client.connect_check(init_settings).await.is_none() {
        error!("Startup notification failed.");
        return Err(error::Error::ConnectTestFail.into());
    }

    // Check download jobs
    client.refresh_settings().await;

    // Check purge list
    if let Some(list) = client.get_purgelist(259200).await {
        for info in list.iter().filter_map(CacheFileInfo::from_file_id) {
            cache_manager.remove_cache(&info).await;
        }
    }

    info!("H@H initialization completed successfully. Starting normal operation");

    // Command listener
    let client2 = client.clone();
    let downloader = Arc::new(Mutex::new(None));
    let downloader2 = downloader.clone();
    tokio::spawn(async move {
        let mut last_overload = Instant::now();
        while let Some(command) = rx.recv().await {
            match command {
                Command::ReloadCert => {
                    if let Some(cert) = client2.get_cert().await {
                        if cert_changer.send(cert).is_err() {
                            error!("Update SSL Cert fail");
                        }
                    }
                }
                Command::RefreshSettings => {
                    client2.refresh_settings().await;
                }
                Command::StartDownloader => {
                    let mut downloader = downloader2.lock();
                    if downloader.is_none() {
                        let new = GalleryDownloader::new(client2.clone(), &download_dir);
                        let downloader3 = downloader2.clone();
                        *downloader = Some(tokio::spawn(async move {
                            new.run().await;
                            *downloader3.lock() = None;
                        }))
                    }
                }
                Command::Overload => {
                    if last_overload.elapsed() > Duration::from_secs(30) {
                        last_overload = Instant::now();
                        warn!("Server overloaded!");
                        // TODO notify overload
                        // client2.notify_overload().await;
                    }
                }
            }
        }
    });

    // Schedule task
    let client3 = client.clone();
    let keepalive = tokio::spawn(async move {
        let mut counter: u32 = 0;
        let mut next_run = Instant::now() + Duration::from_secs(10);
        loop {
            sleep_until(next_run).await;

            if !client3.is_running() {
                break;
            }

            if counter % 11 == 0 {
                client3.still_alive(false).await;
            }

            // Check purge list every 7hr
            if counter % 2160 == 2159 {
                if let Some(list) = client3.get_purgelist(43200).await {
                    for info in list.iter().filter_map(CacheFileInfo::from_file_id) {
                        cache_manager.remove_cache(&info).await;
                    }
                }
            }

            counter = counter.wrapping_add(1);
            next_run = Instant::now() + Duration::from_secs(10);
        }
    });

    // Shutdown handle
    wait_shutdown_signal(shutdown_recv).await; // TODO force shutdown
    info!("Shutting down...");
    keepalive.abort();
    client.shutdown().await;
    if let Some(job) = downloader.lock().as_ref() {
        job.abort();
    }
    info!("Shutdown in progress - please wait");
    sleep(Duration::from_secs(5)).await;
    server_handle.stop(true).await;
    Ok(())
}

/**
 * main helper
*/
fn init_logger() {
    logger::init().unwrap();
}

async fn read_credential<P: AsRef<Path>>(data_path: P) -> Option<(i32, String)> {
    let path = data_path.as_ref().join("client_login");
    let mut file = File::open(path.clone()).map_ok(|f| BufReader::new(f).lines()).await.ok()?; // TODO better error handle
    let data = file.next_line().await.ok().flatten()?;
    let mut credential = data.split('-');

    let id: i32 = credential.next()?.parse().ok()?;
    let key = credential.next()?.to_owned();

    info!("Loaded login settings from {}", path.display());
    Some((id, key))
}

fn create_server(port: u16, cert: ParsedPkcs12, data: AppState) -> (actix_web::dev::Server, watch::Sender<ParsedPkcs12>) {
    let logger = middleware::Logger::default();
    let connection_counter = middleware::ConnectionCounter::new(data.rpc.settings(), data.command_channel.clone());
    let app_data = Data::new(data);

    // Cert changer
    let (cert_sender, mut cert_receiver) = watch::channel(cert);
    let ssl_context = Arc::new(RwLock::new(create_ssl_acceptor(&cert_receiver.borrow_and_update()).build()));
    let ssl_context_write = ssl_context.clone();
    let mut ssl_acceptor = create_ssl_acceptor(&cert_receiver.clone().borrow_and_update());
    ssl_acceptor.set_client_hello_callback(move |ssl, _alert| {
        ssl.set_ssl_context(ssl_context.read().context())?;
        Ok(ClientHelloResponse::SUCCESS)
    });
    tokio::spawn(async move {
        while cert_receiver.changed().await.is_ok() {
            *ssl_context_write.write() = create_ssl_acceptor(&cert_receiver.borrow()).build();
        }
    });

    let server = HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .wrap(logger.clone())
            .wrap(connection_counter.clone())
            .wrap(DefaultHeaders::new().add((
                header::SERVER,
                format!("Genetic Lifeform and Distributed Open Server {}", CLIENT_VERSION),
            )))
            .wrap_fn(|req, next| {
                next.call(req).map_ok(|mut res| {
                    let head = res.response_mut().head_mut();
                    head.set_connection_type(ConnectionType::Close);
                    head.set_camel_case_headers(true);
                    res
                })
            })
            .default_service(to(route::default))
            .configure(route::configure)
    })
    .disable_signals()
    .client_request_timeout(Duration::from_secs(15))
    .on_connect(|conn, _ext| {
        if let Some(tcp) = conn.downcast_ref::<TlsStream<TcpStream>>() {
            tcp.get_ref().set_nodelay(true).unwrap();
        }
    })
    .bind_openssl(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), port), ssl_acceptor)
    .unwrap()
    .run();

    (server, cert_sender)
}

fn create_ssl_acceptor(cert: &ParsedPkcs12) -> SslAcceptorBuilder {
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls_server()).unwrap();
    builder.clear_options(SslOptions::NO_TLSV1_3);
    builder.set_options(SslOptions::NO_RENEGOTIATION | SslOptions::ENABLE_MIDDLEBOX_COMPAT);

    // From https://wiki.mozilla.org/Security/Server_Side_TLS#Old_backward_compatibility
    cpufeatures::new!(cpuid_aes, "aes");
    if !cpuid_aes::get() {
        // Not have AES hardware acceleration, perfer ChaCha20.
        builder
            .set_cipher_list(
                "ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
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
                "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
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
    builder.set_private_key(&cert.pkey).unwrap();
    builder.set_certificate(&cert.cert).unwrap();
    if let Some(i) = &cert.chain {
        i.iter().rev().for_each(|j| builder.add_extra_chain_cert(j.to_owned()).unwrap());
    }
    builder
}

async fn wait_shutdown_signal(mut shutdown_channel: UnboundedReceiver<()>) {
    let mut sigint = signal::unix::signal(SignalKind::interrupt()).unwrap();
    let mut sigterm = signal::unix::signal(SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = signal::ctrl_c() => (),
        _ = sigint.recv() => (),
        _ = sigterm.recv() => (),
        _ = shutdown_channel.recv() => (),
        else => ()
    };
}
