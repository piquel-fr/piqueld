use log::{Level, Metadata, Record};
use time::{self, OffsetDateTime};

pub struct Logger {
    enable: bool,
    max_level: Level,
    prefix: bool,
}

impl Logger {
    pub fn new(enable: bool, verbose: bool, prefix: bool) -> Self {
        let max_level = if verbose { Level::Info } else { Level::Trace };
        Logger {
            enable,
            max_level,
            prefix,
        }
    }
    pub fn max_level(&self) -> Level {
        self.max_level
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

        let prefix = format!("{} {}", timestamp, log_level);

        let msg = if self.prefix {
            format!("{} {}", prefix, record.args())
        } else {
            record.args().to_string()
        };

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
