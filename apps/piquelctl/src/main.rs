//! Safe, small operator command-line client for the Plan 06 piqueld API.

mod cli;
mod commands;
mod error;
mod output;
mod support;

use clap::Parser;
use cli::Cli;
use error::{CliError, ErrorKind, finish_error};
use std::process::ExitCode;
use tokio::time;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let timeout = cli.timeout;
    match time::timeout(timeout, commands::run(&cli)).await {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => finish_error(&cli, error),
        Err(_) => finish_error(
            &cli,
            CliError::new(
                ErrorKind::Unavailable,
                format!(
                    "command timed out after {}",
                    support::format_duration(timeout)
                ),
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use std::time::Duration;

    use super::{
        cli::{Cli, parse_duration},
        support::looks_like_application_id,
    };

    #[test]
    fn parser_covers_the_initial_command_surface() {
        let cases = [
            vec!["piquelctl", "status"],
            vec!["piquelctl", "list"],
            vec!["piquelctl", "show", "notes"],
            vec!["piquelctl", "plan", "--file", "application.toml"],
            vec!["piquelctl", "apply", "--file", "application.toml", "--yes"],
            vec!["piquelctl", "delete", "notes", "--yes", "--no-wait"],
            vec!["piquelctl", "operation", "operation-01", "--no-wait"],
        ];
        for arguments in cases {
            Cli::try_parse_from(arguments).expect("command parses");
        }
    }

    #[test]
    fn parser_rejects_conflicting_transports_and_invalid_timeout() {
        assert!(
            Cli::try_parse_from([
                "piquelctl",
                "--socket",
                "/tmp/piqueld.sock",
                "--url",
                "http://127.0.0.1:8080/",
                "status",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["piquelctl", "--timeout", "zero", "status"]).is_err());
        assert!(Cli::try_parse_from(["piquelctl", "--timeout", "0s", "status"]).is_err());
        assert!(Cli::try_parse_from(["piquelctl", "plan", "status"]).is_err());
    }

    #[test]
    fn timeout_parser_accepts_bounded_units() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_mins(2));
        assert_eq!(parse_duration("1").unwrap(), Duration::from_secs(1));
    }

    #[test]
    fn application_ids_are_distinguished_from_names() {
        assert!(looks_like_application_id("app-notes-01"));
        assert!(!looks_like_application_id("notes"));
        assert!(!looks_like_application_id("APP-NOTES"));
        assert!(!looks_like_application_id("--------"));
    }
}
