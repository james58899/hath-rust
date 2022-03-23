use chrono::{SecondsFormat, Utc};
use log::{Level, LevelFilter, Metadata, Record, SetLoggerError};

struct Logger;

static LOGGER: Logger = Logger;

pub fn init() -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER).map(|()| log::set_max_level(LevelFilter::Debug))
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        };

        let level = record.level();
        let time = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        if level <= Level::Warn {
            eprintln!(
                "{} [{}/{}] {}",
                time,
                level.to_string().to_lowercase(),
                record.target(),
                record.args()
            );
        } else {
            println!(
                "{} [{}/{}] {}",
                time,
                level.to_string().to_lowercase(),
                record.target(),
                record.args()
            );
        }

        // TODO write log file
    }

    fn flush(&self) {}
}
