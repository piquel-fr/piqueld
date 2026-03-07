use log::{Level, Metadata, Record};
use time::{self, OffsetDateTime};

pub struct Logger {
    enable: bool,
    max_level: Level,
    date_time: bool,
}

pub fn init(logger: Box<Logger>) -> Result<(), Box<dyn std::error::Error>> {
    let max_level = logger.max_level.to_level_filter();
    log::set_boxed_logger(logger)?;
    log::set_max_level(max_level);

    Ok(())
}

impl Logger {
    pub fn new(enable: bool, max_level: Level, date_time: bool) -> Self {
        Logger {
            enable,
            max_level,
            date_time,
        }
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        if !self.enable {
            return false;
        }

        metadata.level() <= self.max_level
    }
    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = OffsetDateTime::now_utc();
        let fmt = time::format_description::parse("[year]/[month]/[day] [hour]:[minute]:[second]")
            .unwrap();

        let timestamp = now.format(&fmt).unwrap();
        let log_level = format_log_level(record.level());

        let prefix = if self.date_time {
            format!("{} {}", timestamp, log_level)
        } else {
            log_level
        };

        let msg = format!("{} {}", prefix, record.args());

        if record.level() == Level::Error {
            println!("{msg}");
        } else {
            eprintln!("{msg}");
        }
    }

    fn flush(&self) {}
}

fn format_log_level(level: Level) -> String {
    // TODO: color the string
    format!("[{}]", level.to_string())
}
