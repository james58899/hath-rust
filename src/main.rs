#![windows_subsystem = "windows"]
use std::{collections::HashMap, error::Error, ops::RangeInclusive, path::Path, sync::Arc, time::Duration};

use clap::Parser;
use futures::TryFutureExt;
use inquire::{
    validator::{ErrorMessage, Validation},
    CustomType, Text,
};
use log::{error, info, warn};
use parking_lot::Mutex;
use regex::Regex;
use reqwest::Proxy;
use tempfile::TempPath;
use tokio::{
    fs::{self, remove_file, try_exists, File},
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
    server::Server,
    util::{create_dirs, create_http_client},
};

mod cache_manager;
mod error;
mod gallery_downloader;
mod logger;
mod middleware;
mod route;
mod rpc;
mod server;
mod util;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// jemalloc config
#[cfg(not(target_env = "msvc"))]
#[no_mangle]
pub static mut malloc_conf: *const u8 = b"percpu_arena:phycpu,tcache:false,dirty_decay_ms:1000,muzzy_decay_ms:0\0".as_ptr();

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-", env!("VERGEN_GIT_SHA"));
pub const CLIENT_VERSION: &str = "1.6.2";
const MAX_KEY_TIME_DRIFT: RangeInclusive<i64> = -300..=300;

#[derive(Parser)]
#[command(version = VERSION)]
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

type DownloadState = Mutex<HashMap<[u8; 20], (watch::Receiver<Option<Arc<TempPath>>>, Arc<watch::Sender<u64>>)>>;

pub struct AppState {
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

    info!("Hentai@Home {} (Rust {}) starting up", CLIENT_VERSION, VERSION);

    let (id, key) = match read_credential(&args.data_dir).await? {
        Some(i) => i,
        None => setup(&args.data_dir).await?,
    };
    let client = Arc::new(RPCClient::new(id, &key, args.disable_ip_origin_check, args.max_connection));
    let init_settings = client.login().await?;

    let (shutdown_send, shutdown_recv) = mpsc::unbounded_channel::<()>();
    let settings = client.settings();
    logger.config().write_info(!settings.disable_logging());
    delete_java_cache_data(args.data_dir).await; // Rust cache data incompatible with Java, so we must delete it
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

    info!("Starting HTTP server...");
    let server = Server::new(
        args.port.unwrap_or_else(|| init_settings.client_port()),
        client.get_cert().await.unwrap(),
        AppState {
            runtime: Handle::current(),
            reqwest: create_http_client(Duration::from_secs(30), proxy.clone()),
            rpc: client.clone(),
            download_state: Default::default(),
            cache_manager: cache_manager.clone(),
            command_channel: tx.clone(),
            has_proxy,
        },
    );
    let server_handle = server.handle();

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
    let cache_manager2 = cache_manager.clone();
    let downloader = Arc::new(Mutex::new(None));
    let downloader2 = downloader.clone();
    let logger_config = logger.config();
    tokio::spawn(async move {
        let mut last_overload = Instant::now().checked_sub(Duration::from_secs(30)).unwrap_or_else(Instant::now);
        while let Some(command) = rx.recv().await {
            match command {
                Command::ReloadCert => {
                    match client2.get_cert().await {
                        Some(cert) => server_handle.update_cert(cert),
                        None => {
                            error!("Fetch SSL cert fail");
                            // Retry after 10s
                            let tx2 = tx.clone();
                            tokio::spawn(async move {
                                sleep(Duration::from_secs(10)).await;
                                _ = tx2.send(Command::ReloadCert).await;
                            });
                        }
                    }
                }
                Command::RefreshSettings => {
                    client2.refresh_settings().await;
                    cache_manager2.update_settings(client2.settings());
                    if !args.disable_logging {
                        logger_config.write_info(!client2.settings().disable_logging());
                    }
                }
                Command::StartDownloader => {
                    let mut downloader = downloader2.lock();
                    if downloader.is_none() {
                        let new = GalleryDownloader::new(client2.clone(), &args.download_dir, proxy.clone());
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
    sleep(Duration::from_secs(15)).await;
    server.shutdown().await;
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

async fn delete_java_cache_data<P: AsRef<Path>>(data_dir: P) {
    let base = data_dir.as_ref();
    let _ = remove_file(base.join("pcache_info")).await;
    let _ = remove_file(base.join("pcache_ages")).await;
    let _ = remove_file(base.join("pcache_lru")).await;
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
