#![windows_subsystem = "windows"]
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
    dev::{Server, Service},
    http::{header, ConnectionType},
    middleware::DefaultHeaders,
    rt::net::TcpStream,
    web::{to, Data},
    App, HttpServer,
};
use clap::Parser;
use futures::TryFutureExt;
use inquire::{
    validator::{ErrorMessage, Validation},
    CustomType, Text,
};
use log::{error, info, warn};
#[cfg(target_env = "msvc")]
use mimalloc::MiMalloc;
use once_cell::sync::Lazy;
use openssl::{
    pkcs12::ParsedPkcs12_2,
    ssl::{ClientHelloResponse, SslAcceptor, SslAcceptorBuilder, SslMethod, SslOptions},
};
use parking_lot::{Mutex, RwLock};
use regex::Regex;
use reqwest::Proxy;
use tempfile::TempPath;
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;
use tokio::{
    fs::{self, try_exists, File},
    io::{stderr, stdin, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    runtime::Handle,
    signal,
    sync::{
        mpsc::{self, Sender, UnboundedReceiver},
        watch,
    },
    time::{sleep, sleep_until, Instant},
};

use crate::{
    cache_manager::{CacheFileInfo, CacheManager},
    gallery_downloader::GalleryDownloader,
    logger::Logger,
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

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;
#[cfg(target_env = "msvc")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

static VERSION: Lazy<String> = Lazy::new(|| {
    format!(
        "{}-{} {}",
        built_info::PKG_VERSION,
        built_info::GIT_COMMIT_HASH_SHORT.unwrap_or("unknown"),
        built_info::BUILT_TIME_UTC
    )
});
static CLIENT_VERSION: &str = "1.6.2";
static MAX_KEY_TIME_DRIFT: RangeInclusive<i64> = -300..=300;

mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

#[derive(Parser)]
#[command(version = VERSION.as_str())]
struct Args {
    /// Overrides the port set in the client's settings
    #[arg(long)]
    port: Option<u16>,

    /// Cache data location
    #[arg(long, default_value_t = String::from("cache"))]
    cache_dir: String,

    /// Login data location
    #[arg(long, default_value_t = String::from("data"))]
    data_dir: String,

    /// Downloader save location
    #[arg(long, default_value_t = String::from("download"))]
    download_dir: String,

    /// Logs location
    #[arg(long, default_value_t = String::from("log"))]
    log_dir: String,

    /// Temporary location for proxy request
    #[arg(long, default_value_t = String::from("tmp"))]
    temp_dir: String,

    /// Disable writing non-error logs to file
    #[arg(long, default_value_t = false)]
    disable_logging: bool,

    /// Flush the logs to disk every line
    #[arg(long, default_value_t = false)]
    flush_log: bool,

    /// Override the max connection soft limit, should only be used in special cases
    #[arg(long, default_value_t = 0)]
    max_connection: u64,

    /// Disable server command ip check
    #[arg(long, default_value_t = false)]
    disable_ip_origin_check: bool,

    /// Configure proxy for fetch cache
    #[arg(long)]
    proxy: Option<String>,
}

type DownloadState = RwLock<HashMap<[u8; 20], (watch::Receiver<Option<Arc<TempPath>>>, watch::Receiver<u64>)>>;

struct AppState {
    runtime: Handle,
    reqwest: reqwest::Client,
    rpc: Arc<RPCClient>,
    download_state: DownloadState,
    cache_manager: Arc<CacheManager>,
    command_channel: Sender<Command>,
    has_proxy: bool,
}

pub enum Command {
    ReloadCert,
    RefreshSettings,
    StartDownloader,
    Overload,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Windows system tray
    if cfg!(windows) {
        build_tray_icon();
    }

    // Options
    let args = Args::try_parse();
    if let Err(ref err) = args {
        let _ = err.print();
        if cfg!(windows) {
            let mut out = stderr();
            let _ = out.write(b"\r\nPress Enter to exit...").await.unwrap();
            out.flush().await.unwrap();
            let _ = stdin().read(&mut [0]).await.unwrap();
        }
        std::process::exit(err.exit_code());
    }
    let args = args.unwrap();

    create_dirs(vec![
        &args.data_dir,
        &args.cache_dir,
        &args.log_dir,
        &args.temp_dir,
        &args.download_dir,
    ])
    .await?;

    // Init logger
    let mut logger = Logger::init(args.log_dir).unwrap();
    logger.config().write_info(!args.disable_logging).flush(args.flush_log);

    info!(
        "Hentai@Home {} (Rust {}-{}) starting up",
        CLIENT_VERSION,
        built_info::PKG_VERSION,
        built_info::GIT_COMMIT_HASH_SHORT.unwrap_or("unknown")
    );

    let (id, key) = match read_credential(&args.data_dir).await? {
        Some(i) => i,
        None => setup(&args.data_dir).await?,
    };
    let client = Arc::new(RPCClient::new(id, &key, args.disable_ip_origin_check, args.max_connection));
    let init_settings = client.login().await?;

    let (shutdown_send, shutdown_recv) = mpsc::unbounded_channel::<()>();
    let settings = client.settings();
    let cache_manager = CacheManager::new(
        args.cache_dir,
        args.temp_dir,
        settings.clone(),
        &init_settings,
        shutdown_send.clone(),
    )
    .await?;

    // Proxy
    let proxy = match args.proxy.as_ref().map(Proxy::all) {
        Some(Ok(proxy)) => {
            info!("Using proxy for fetch cache: {}", args.proxy.unwrap());
            Some(proxy)
        }
        Some(Err(err)) => {
            error!("Parser proxy setting error: {}", err);
            None
        }
        None => None,
    };
    let has_proxy = proxy.is_some();
    // Command channel
    let (tx, mut rx) = mpsc::channel::<Command>(1);
    let (server, cert_changer) = create_server(
        args.port.unwrap_or_else(|| init_settings.client_port()),
        client.get_cert().await.unwrap(),
        AppState {
            runtime: Handle::current(),
            reqwest: create_http_client(Duration::from_secs(30), proxy),
            rpc: client.clone(),
            download_state: Default::default(),
            cache_manager: cache_manager.clone(),
            command_channel: tx.clone(),
            has_proxy,
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
    let logger_config = logger.config();
    tokio::spawn(async move {
        let mut last_overload = Instant::now().checked_sub(Duration::from_secs(30)).unwrap_or_else(Instant::now);
        while let Some(command) = rx.recv().await {
            match command {
                Command::ReloadCert => {
                    match client2.get_cert().await {
                        Some(cert) => match cert_changer.send(cert) {
                            Ok(_) => continue, // Cert update send
                            Err(_) => error!("Update SSL cert fail"),
                        },
                        None => error!("Fetch SSL cert fail"),
                    }

                    // Retry after 10s
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        sleep(Duration::from_secs(10)).await;
                        _ = tx2.send(Command::ReloadCert).await;
                    });
                }
                Command::RefreshSettings => {
                    client2.refresh_settings().await;
                    if !args.disable_logging {
                        logger_config.write_info(!client2.settings().disable_logging());
                    }
                }
                Command::StartDownloader => {
                    let mut downloader = downloader2.lock();
                    if downloader.is_none() {
                        let new = GalleryDownloader::new(client2.clone(), &args.download_dir);
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
                        client2.notify_overload().await;
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

            // Alive check every 110s
            if counter % 11 == 0 && !client3.still_alive(false).await {
                let _ = shutdown_send.send(()); // Check fail, shutdown.
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
            log::logger().flush(); // Flush log every 10s
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
    sleep(Duration::from_secs(30)).await;
    server_handle.stop(true).await;
    logger.shutdown().await;
    Ok(())
}

/**
 * main helper
 */
async fn read_credential<P: AsRef<Path>>(data_path: P) -> Result<Option<(i32, String)>, Box<dyn Error>> {
    let path = data_path.as_ref().join("client_login");
    if !try_exists(&path).await? {
        return Ok(None);
    }
    let data = match File::open(&path)
        .and_then(|f| async { BufReader::new(f).lines().next_line().await })
        .await
    {
        Ok(Some(data)) => data,
        Ok(None) => return Ok(None),
        Err(err) => {
            error!("Encountered error when reading client_login: {}", err);
            return Err(Box::new(err));
        }
    };
    let mut credential = data.split('-');

    let (id, key) = match credential.next().and_then(|s| s.parse::<i32>().ok()).zip(credential.next()) {
        Some(v) => v,
        None => return Ok(None),
    };

    info!("Loaded login settings from {}", path.display());
    Ok(Some((id, key.to_owned())))
}

async fn setup<P: AsRef<Path>>(data_path: P) -> Result<(i32, String), Box<dyn Error>> {
    info!("Setup client");
    sleep(Duration::from_secs(1)).await; // Wait logger

    println!(
        "
Before you can use this client, you will have to register it at https://e-hentai.org/hentaiathome.php
IMPORTANT: YOU NEED A SEPARATE IDENT FOR EACH CLIENT YOU WANT TO RUN.
DO NOT ENTER AN IDENT THAT WAS ASSIGNED FOR A DIFFERENT CLIENT UNLESS IT HAS BEEN RETIRED.
After registering, enter your ID and Key below to start your client.
(You will only have to do this once.)
"
    );

    let id = CustomType::<i32>::new("Enter Client ID:")
        .with_error_message("Invalid Client ID. Please try again.")
        .prompt()?;
    let key = Text::new("Enter Client Key:")
        .with_validator(|key: &_| {
            if Regex::new("^[a-zA-Z0-9]{20}$").unwrap().is_match(key) {
                Ok(Validation::Valid)
            } else {
                let message = "Invalid Client Key, it must be exactly 20 alphanumerical characters. Please try again.".to_owned();
                Ok(Validation::Invalid(ErrorMessage::Custom(message)))
            }
        })
        .prompt()?;

    // Write client_login
    let path = data_path.as_ref().join("client_login");
    if let Err(err) = fs::write(&path, format!("{}-{}", id, key)).await {
        error!("Error encountered when writing client_login: {}", err);
        return Err(Box::new(err));
    }

    Ok((id, key))
}

fn create_server(port: u16, cert: ParsedPkcs12_2, data: AppState) -> (Server, watch::Sender<ParsedPkcs12_2>) {
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
            .wrap(middleware::Timeout::new(Duration::from_secs(181)))
            .wrap(logger.clone())
            .wrap(connection_counter.clone())
            .wrap(DefaultHeaders::new().add((
                header::SERVER,
                format!("Genetic Lifeform and Distributed Open Server {CLIENT_VERSION}"),
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

fn create_ssl_acceptor(cert: &ParsedPkcs12_2) -> SslAcceptorBuilder {
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls_server()).unwrap();
    builder.clear_options(SslOptions::NO_TLSV1_3);
    builder.set_options(SslOptions::NO_RENEGOTIATION | SslOptions::ENABLE_MIDDLEBOX_COMPAT);

    // From https://wiki.mozilla.org/Security/Server_Side_TLS#Old_backward_compatibility
    cpufeatures::new!(cpuid_aes, "aes");
    if !cpuid_aes::get() {
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
    builder
}

#[cfg(unix)]
async fn wait_shutdown_signal(mut shutdown_channel: UnboundedReceiver<()>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = signal::ctrl_c() => (),
        _ = sigterm.recv() => (),
        _ = shutdown_channel.recv() => (),
        else => ()
    }
}

#[cfg(windows)]
async fn wait_shutdown_signal(mut shutdown_channel: UnboundedReceiver<()>) {
    use tokio::signal::windows::{ctrl_close, ctrl_shutdown};

    let mut close = ctrl_close().unwrap();
    let mut shutdown = ctrl_shutdown().unwrap();
    tokio::select! {
        _ = signal::ctrl_c() => (),
        _ = close.recv() => (),
        _ = shutdown.recv() => (),
        _ = shutdown_channel.recv() => (),
        else => ()
    }
}

#[cfg(not(windows))]
fn build_tray_icon() {
    // Tray icon is windows only.
}

#[cfg(windows)]
fn build_tray_icon() {
    use std::thread;

    use tao::{
        event_loop::{ControlFlow, EventLoopBuilder},
        platform::windows::EventLoopBuilderExtWindows,
    };
    use tray_icon::{
        menu::{Menu, MenuEvent},
        ClickType, TrayIconBuilder, TrayIconEvent,
    };

    // Show console
    switch_window(false);

    thread::Builder::new()
        .name("TrayEvent".into())
        .spawn(|| {
            let tray_menu = Menu::new(); // TODO nemu
            let _tray_icon = TrayIconBuilder::new()
                .with_menu(Box::new(tray_menu))
                .with_tooltip("hath-rust - Hentai@Home but rusty") // TODO icon
                .build()
                .unwrap();

            let mut console_hide = false;
            let event_loop = EventLoopBuilder::new().with_any_thread(true).build();
            let tray_channel = TrayIconEvent::receiver();
            let menu_channel = MenuEvent::receiver();
            event_loop.run(move |_event, _, control_flow| {
                *control_flow = ControlFlow::WaitUntil((Instant::now() + Duration::from_millis(100)).into());

                if let Ok(event) = tray_channel.try_recv() {
                    if event.click_type == ClickType::Double {
                        console_hide = !console_hide;
                        switch_window(console_hide)
                    }
                }
                if let Ok(_event) = menu_channel.try_recv() {
                    // TODO
                }
            });
        })
        .unwrap();
}

#[cfg(windows)]
fn switch_window(hide: bool) {
    use windows::Win32::{
        System::Console::{AllocConsole, GetConsoleWindow},
        UI::WindowsAndMessaging::{DeleteMenu, GetSystemMenu, ShowWindow, MF_BYCOMMAND, SC_CLOSE, SW_HIDE, SW_SHOW},
    };

    let mut window = unsafe { GetConsoleWindow() };
    if window.0 == 0 && unsafe { AllocConsole().is_ok() } {
        window = unsafe { GetConsoleWindow() };

        // Try Disable close button
        let menu = unsafe { GetSystemMenu(window, false) };
        if !menu.is_invalid() {
            unsafe {
                let _ = DeleteMenu(menu, SC_CLOSE, MF_BYCOMMAND);
            };
        }
    }

    if window.0 != 0 {
        unsafe {
            ShowWindow(window, if hide { SW_HIDE } else { SW_SHOW });
        }
    }
}
