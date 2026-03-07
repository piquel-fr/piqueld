use log::{Level, Metadata, Record};
use time;

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

        println!("{} - {}", record.level(), record.args());
    }

    fn flush(&self) {}
}
