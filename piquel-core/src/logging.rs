use crate::logging::logger::Logger;

pub mod logger;

pub fn init(logger: Box<Logger>) -> Result<(), Box<dyn std::error::Error>> {
    let max_level = logger.max_level().to_level_filter();
    log::set_boxed_logger(logger)?;
    log::set_max_level(max_level);

    Ok(())
}
