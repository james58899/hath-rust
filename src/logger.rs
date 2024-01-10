use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use chrono::{SecondsFormat, Utc};
use futures::executor::block_on;
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
    worker: Arc<LoggerWorker>,
    handle: Option<JoinHandle<()>>,
}

pub struct LoggerConfig {
    write_info: AtomicBool,
    flush: AtomicBool,
}

struct LoggerWorker {
    flush: Arc<Notify>,
    shutdown: Arc<Notify>,
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

        let (worker, handle) = LoggerWorker::new(log_dir, config.clone());
        let worker = Arc::new(worker);
        let logger = Self {
            config: config.clone(),
            worker: worker.clone(),
            handle: Some(handle),
        };

        log::set_boxed_logger(Box::new(worker))
            .map(|()| log::set_max_level(LevelFilter::Debug))
            .map(|()| logger)
    }

    pub fn config(&self) -> Arc<LoggerConfig> {
        self.config.clone()
    }

    pub async fn shutdown(&mut self) {
        info!("Shutdown logger...");
        self.worker.shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        block_on(async {
            self.shutdown().await;
        });
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
    fn new<P: AsRef<Path>>(log_dir: P, config: Arc<LoggerConfig>) -> (Self, JoinHandle<()>) {
        let flush = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());
        let (tx, mut rx) = unbounded_channel::<LoggerMessage>();
        let log_dir = log_dir.as_ref().to_owned();
        let flush2 = flush.clone();
        let shutdown2 = shutdown.clone();
        let worker = tokio::spawn(async move {
            let log_out = &log_dir.join("log_out");
            let log_err = &log_dir.join("log_err");

            rotate_log(&[log_out, log_err]).await;

            let mut err_lines: u32 = 0;
            let mut out_lines: u32 = 0;
            let mut writer_err = match std::fs::File::create(log_err).map(File::from_std) {
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
                                writer_err = File::create(log_err).await.unwrap();
                                err_lines = 0;
                            }
                            let _ = writer_err.write_all(&[log.message.as_bytes(), b"\n"].concat()).await;
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
                    _ = flush2.notified() => {
                        let _ = writer_out.flush().await;
                    }
                    _ = shutdown2.notified() => {
                        rx.close()
                    }
                }
            }
        });

        (Self { tx, flush, shutdown }, worker)
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
                level.as_str().to_lowercase(),
                record.target(),
                record.args()
            ),
        });
    }

    fn flush(&self) {
        self.flush.notify_one();
    }
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
