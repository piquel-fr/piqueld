//! `piqueld-probe`: runs one [`HttpProbe`] and reports it as a Docker health
//! check, exiting 0 when healthy and 1 otherwise. Docker keeps the printed
//! line in the container's health log.

use piqueld_probe::HttpProbe;
use std::{error::Error, process::ExitCode};

fn main() -> ExitCode {
    let result = HttpProbe::parse(std::env::args().skip(1))
        .map_err(Box::<dyn Error>::from)
        .and_then(|probe| Ok(probe.check()?));
    match result {
        Ok(status) => {
            println!("healthy: HTTP {status}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            print!("unhealthy: {error}");
            let mut source = error.source();
            while let Some(cause) = source {
                print!(": {cause}");
                source = cause.source();
            }
            println!();
            ExitCode::FAILURE
        }
    }
}
