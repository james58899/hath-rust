use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use chrono::{SecondsFormat, Utc};
use log::{info, Level, LevelFilter, Metadata, Record, SetLoggerError};
use tokio::{
    fs::{rename, try_exists, File},
    io::{AsyncWriteExt, BufWriter},
    select,
    sync::{
        mpsc::{unbounded_channel, UnboundedSender},
        Notify,
    },
    task::JoinHandle,
};

pub struct Logger {
    config: Arc<LoggerConfig>,
    shutdown: Arc<Notify>,
    handle: JoinHandle<()>,
}

pub struct LoggerConfig {
    write_info: AtomicBool,
    flush: AtomicBool,
}

struct LoggerWorker {
    tx: UnboundedSender<LoggerMessage>,
}

struct LoggerMessage {
    level: Level,
    message: String,
}

impl Logger {
    pub fn init<P: AsRef<Path>>(log_dir: P) -> Result<Self, SetLoggerError> {
        let config = Arc::new(LoggerConfig {
            write_info: true.into(),
            flush: false.into(),
        });

        let (worker, shutdown, handle) = LoggerWorker::new(log_dir, config.clone());
        let logger = Self {
            config: config.clone(),
            shutdown,
            handle,
        };

        log::set_boxed_logger(Box::new(worker))
            .map(|()| log::set_max_level(LevelFilter::Debug))
            .map(|()| logger)
    }

    pub fn config(&self) -> Arc<LoggerConfig> {
        self.config.clone()
    }

    pub async fn shutdown(self) {
        info!("Shutdown logger...");
        self.shutdown.notify_one();
        let _ = self.handle.await;
    }
}

impl LoggerConfig {
    pub fn write_info(&self, enabled: bool) -> &Self {
        self.write_info.store(enabled, Ordering::Relaxed);
        self
    }

    pub fn flush(&self, enabled: bool) -> &Self {
        self.flush.store(enabled, Ordering::Relaxed);
        self
    }
}

impl LoggerWorker {
    fn new<P: AsRef<Path>>(log_dir: P, config: Arc<LoggerConfig>) -> (Self, Arc<Notify>, JoinHandle<()>) {
        let shutdown = Arc::new(Notify::new());
        let (tx, mut rx) = unbounded_channel::<LoggerMessage>();
        let log_dir = log_dir.as_ref().to_owned();
        let shutdown2 = shutdown.clone();
        let worker = tokio::spawn(async move {
            let log_out = &log_dir.join("log_out");
            let log_err = &log_dir.join("log_err");

            rotate_log(&[log_out, log_err]).await;

            let mut err_lines: u32 = 0;
            let mut out_lines: u32 = 0;
            let mut writer_err = match std::fs::File::create(log_err).map(File::from_std).map(BufWriter::new) {
                Ok(w) => w,
                Err(err) => {
                    eprintln!("Log create error: {:?}", err);
                    return;
                }
            };
            let mut writer_out = std::fs::File::create(log_out).map(File::from_std).map(BufWriter::new).unwrap();

            loop {
                select! {
                    log = rx.recv() => {
                        if log.is_none() {
                            let _ = writer_err.shutdown().await;
                            let _ = writer_out.shutdown().await;
                            break
                        }
                        let log = log.unwrap();

                        if log.level <= Level::Warn {
                            // stderr
                            eprintln!("{}", log.message);

                            // file
                            err_lines += 1;
                            if err_lines > 100000 {
                                // log routate
                                let _ = writer_err.shutdown().await;
                                rotate_log(&[log_err]).await;
                                writer_err = File::create(log_err).await.map(BufWriter::new).unwrap();
                                err_lines = 0;
                            }
                            let _ = writer_err.write_all(&[log.message.as_bytes(), b"\n"].concat()).await;
                            if config.flush.load(Ordering::Relaxed) {
                                let _ = writer_err.flush().await;
                            }
                        } else {
                            // stdout
                            println!("{}", log.message);

                            // file
                            if config.write_info.load(Ordering::Relaxed) {
                                out_lines += 1;
                                if out_lines > 100000 {
                                    // log routate
                                    let _ = writer_out.shutdown().await;
                                    rotate_log(&[log_out]).await;
                                    writer_out = File::create(log_out).await.map(BufWriter::new).unwrap();
                                    out_lines = 0;
                                }
                                let _ = writer_out.write_all(&[log.message.as_bytes(), b"\n"].concat()).await;
                                if config.flush.load(Ordering::Relaxed) {
                                    let _ = writer_out.flush().await;
                                }
                            }
                        }
                    }
                    _ = shutdown2.notified() => {
                        rx.close()
                    }
                }
            }
        });

        (Self { tx }, shutdown, worker)
    }
}

impl log::Log for LoggerWorker {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        };

        let level = record.level();
        let time = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let _ = self.tx.send(LoggerMessage {
            level,
            message: format!(
                "{} [{}/{}] {}",
                time,
                level.to_string().to_lowercase(),
                record.target(),
                record.args()
            ),
        });
    }

    fn flush(&self) {}
}

async fn rotate_log(files: &[&PathBuf]) {
    for path in files {
        if try_exists(path).await.unwrap_or(false) {
            let mut old = path.to_path_buf();
            old.set_extension("old");
            rename(path, old).await.unwrap();
        }
    }
}
