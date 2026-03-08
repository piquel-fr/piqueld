use crate::logging::logger::Logger;

pub mod logger;

pub fn init(logger: Box<Logger>) {
    let max_level = logger.max_level().to_level_filter();
    log::set_boxed_logger(logger).unwrap(); // logger should not have already been set
    log::set_max_level(max_level);
}
