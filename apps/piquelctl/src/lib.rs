//! Operator command-line client for the versioned piqueld API. The
//! `piquelctl` binary calls [`run`]; [`command`] exposes the clap model, which
//! `just generate` renders into the command reference.

mod accounts;
mod analytics;
mod auth;
mod cli;
mod commands;
mod deployments;
mod editing;
mod environments;
mod error;
mod exec;
mod notifications;
mod output;
mod previews;
mod profiles;
mod secrets;
mod snapshot;
mod support;

use clap::{CommandFactory, FromArgMatches};
use cli::Cli;
use error::{CliCode, CliError, ErrorKind, ErrorReport};
use output::{Console, reports::ProfilesReport};
use std::{num::NonZeroU64, process::ExitCode, time::Duration};
use support::Waiting;
use tokio::time::Instant;

/// Parses arguments, then dispatches in three stages:
/// 1. `profiles` is answered from loaded profile files without connecting.
/// 2. Connection settings are resolved; `login` runs under its own device-flow deadline.
/// 3. Every other command runs under the supervisor (`run_with_timeout`):
///    `--timeout`, and Ctrl-C, SIGTERM and SIGHUP. `app exec` and
///    `preview exec` sessions stream for as long as the command runs.
///
/// Errors are rendered to stderr and mapped to `ErrorKind` exit codes.
///
/// # Panics
///
/// If the Tokio runtime cannot start.
#[tokio::main]
pub async fn run() -> ExitCode {
    let matches = match Cli::command().try_get_matches() {
        Ok(matches) => matches,
        Err(error) => return usage_error(&error),
    };
    let mut cli = Cli::from_arg_matches(&matches)
        .expect("matches were validated by the same Cli command definition");
    let mut console = Console::new(&cli);
    let result = async {
        // Completions and validation are purely local, so they need neither
        // profiles nor a daemon.
        if let cli::Command::Completions { shell } = &cli.command {
            // `generate` panics on a write failure, such as a closed pipe.
            let mut script = Vec::new();
            clap_complete::generate(*shell, &mut command(), "piquelctl", &mut script);
            std::io::Write::write_all(&mut std::io::stdout(), &script)?;
            return Ok(ExitCode::SUCCESS);
        }
        if let cli::Command::App {
            command: cli::AppCommand::Validate { file },
        } = &cli.command
        {
            return commands::validate(&mut console, file)
                .await
                .map(|()| ExitCode::SUCCESS);
        }
        let profiles = profiles::Profiles::load(&cli)?;
        if matches!(cli.command, cli::Command::Profiles) {
            return console
                .emit(&ProfilesReport {
                    profiles: profiles.summaries(),
                })
                .map(|()| ExitCode::SUCCESS);
        }
        profiles.resolve(&mut cli, &matches)?;
        match &cli.command {
            cli::Command::Login { .. } => {
                let client = commands::build_client(&cli)?;
                auth::login(&cli, &client, &mut console).await?;
            }
            // One request, bounded by the client's own timeout, so a timeout
            // still reports the resolved connection rather than dropping it.
            cli::Command::Doctor => {
                commands::doctor(&cli, &commands::build_client(&cli)?, &mut console).await?;
            }
            // Validate the endpoint before the supervisor's deadline can win,
            // so timeout diagnostics can never echo rejected URL input.
            cli::Command::App {
                command: cli::AppCommand::Exec(args),
            } => {
                let client = commands::build_client(&cli)?;
                return run_with_timeout(&cli, args.run(&client)).await;
            }
            cli::Command::Preview {
                command: previews::PreviewCommand::Exec(args),
            } => {
                let client = commands::build_client(&cli)?;
                return run_with_timeout(&cli, args.run(&client)).await;
            }
            _ => {
                let client = commands::build_client(&cli)?;
                // Boxed: every command's state would otherwise be inlined here.
                let command = Box::pin(commands::run(&cli, &client, &mut console));
                run_with_timeout(&cli, command).await?;
            }
        }
        Ok(ExitCode::SUCCESS)
    }
    .await;
    match result {
        Ok(code) => code,
        Err(error) => {
            console.error(&ErrorReport::new(&error, &cli));
            let code = error.exit_code();
            // A blocking stdin reader cannot be cancelled. Do not let runtime
            // shutdown wait for it after the command has been interrupted.
            if support::interaction_active() {
                std::process::exit(i32::from(code));
            }
            ExitCode::from(code)
        }
    }
}

/// The complete command-line model, as `--help` renders it.
#[must_use]
pub fn command() -> clap::Command {
    Cli::command()
}

/// Reports arguments clap rejected, exiting 2. Without `--json` (and for
/// help and version) clap prints its own report; with it, the report is one
/// `usage` error event, like every other failure.
fn usage_error(error: &clap::Error) -> ExitCode {
    let json = std::env::args_os()
        .skip(1)
        .take_while(|argument| argument != "--")
        .any(|argument| argument == "--json");
    if !json || !error.use_stderr() {
        error.exit();
    }
    // The first paragraph names the problem; usage and help hints follow it.
    let rendered = error.render().to_string();
    let message = rendered
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches("error: ");
    let error = CliError::usage(message);
    let mut stderr = std::io::stderr().lock();
    let _ = output::Event::Error(error.body()).write(&mut stderr);
    ExitCode::from(error.exit_code())
}

/// Bounds `command` by `--timeout` while excluding interactive prompts and
/// sessions: operator time must not consume the network-phase budget. Waits
/// for the daemon are excluded too, and each is bounded by `--wait-timeout`
/// instead. Ends it on Ctrl-C, SIGTERM or SIGHUP; dropping it restores a raw
/// terminal and closes its connections. The command future stays polled even
/// while a prompt is open so its completion and signal handling keep making
/// progress.
async fn run_with_timeout<T>(
    cli: &Cli,
    command: impl Future<Output = Result<T, CliError>>,
) -> Result<T, CliError> {
    let interrupt =
        CliError::on_interrupt("command interrupted; any server-side operation was not cancelled");
    tokio::pin!(command, interrupt);
    let mut deadline = checked_deadline(Instant::now(), cli.timeout)
        .map_err(|error| error.configuration(cli.connection_sources.timeout.to_string()))?;
    // Checked now, not once a mutation was accepted and a wait starts.
    checked_deadline(Instant::now(), cli.wait_timeout)
        .map_err(|error| error.configuration(cli.connection_sources.wait_timeout.to_string()))?;
    // Set while an interactive prompt is open or the command waits; that
    // time is excluded by shifting the deadline once it ends.
    let mut suspended: Option<Instant> = None;
    // The current wait's number and its own deadline.
    let mut wait: Option<(NonZeroU64, Instant)> = None;
    loop {
        let waiting = Waiting::current();
        match (
            support::interaction_active() || waiting.is_some(),
            suspended,
        ) {
            (true, None) => suspended = Some(Instant::now()),
            (false, Some(started)) => {
                deadline = checked_deadline(deadline, started.elapsed())?;
                suspended = None;
            }
            _ => {}
        }
        wait = match (waiting, wait) {
            (Some(current), Some((number, until))) if current == number => Some((number, until)),
            (Some(current), _) => {
                Some((current, checked_deadline(Instant::now(), cli.wait_timeout)?))
            }
            (None, _) => None,
        };
        tokio::select! {
            // Requests share the `--timeout` budget, so one can time out at the
            // command's own deadline, and either branch can then win. Once the
            // deadline has passed, the command is what timed out.
            result = &mut command => {
                return match result {
                    Err(error)
                        if error.is_request_timeout()
                            && suspended.is_none()
                            && Instant::now() >= deadline =>
                    {
                        Err(timeout_error(cli.timeout))
                    }
                    result => result,
                };
            }
            error = &mut interrupt => return Err(error),
            // An interaction or wait started or finished; re-evaluate from the top.
            () = support::interaction_changed() => {}
            // The deadline is suspended while a prompt is open or the command waits.
            () = tokio::time::sleep_until(deadline), if suspended.is_none() => {
                // The guard above is evaluated when the branch is registered.
                // A prompt or wait can start afterwards, so recheck before
                // treating the elapsed deadline as a command timeout.
                if !support::interaction_active() && Waiting::current().is_none() {
                    return Err(timeout_error(cli.timeout));
                }
            }
            () = tokio::time::sleep_until(wait.map_or(deadline, |(_, until)| until)),
                if wait.is_some() =>
            {
                if Waiting::current() == wait.map(|(number, _)| number) {
                    return Err(wait_timeout_error(cli));
                }
            }
        }
    }
}

/// Adds `duration` to `start`, failing with an input error instead of panicking
/// when the platform cannot represent the resulting instant.
fn checked_deadline(start: Instant, duration: Duration) -> Result<Instant, CliError> {
    start
        .checked_add(duration)
        .ok_or_else(|| CliError::new(ErrorKind::Input, "timeout is too large"))
}

/// A wait that exceeded `--wait-timeout`. The daemon goes on with the
/// operation, so it is reported like a command timeout, with the timeout,
/// its source, and the last failure the wait went on through, such as one
/// the daemon kept retrying.
fn wait_timeout_error(cli: &Cli) -> CliError {
    let timeout = support::format_duration(cli.wait_timeout);
    let last = Waiting::last_failure();
    let mut details = serde_json::json!({
        "timeout": timeout,
        "timeout_source": cli.connection_sources.wait_timeout.to_string(),
    });
    if let Some(code) = &last {
        details["last_error_code"] = code.as_str().into();
    }
    CliError::new(
        ErrorKind::Unavailable,
        format!(
            "stopped waiting after {timeout} (--wait-timeout); the operation goes on{}",
            last.map_or_else(String::new, |code| format!(", and last failed with {code}"))
        ),
    )
    .code(CliCode::Timeout)
    .with_details(details)
}

/// Whole-command timeout error, tagged so the report shows timeout sources and hints.
fn timeout_error(timeout: Duration) -> CliError {
    CliError::new(
        ErrorKind::Unavailable,
        format!(
            "command timed out after {}",
            support::format_duration(timeout)
        ),
    )
    .command_timeout()
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use std::time::Duration;

    use super::{
        cli::{Cli, parse_duration},
        support::looks_like_application_id,
    };

    /// The argument forms no command test in `tests/cli.rs` runs.
    #[test]
    fn parser_accepts_combined_exec_flags_and_build_filters() {
        for arguments in [
            &["piquelctl", "builds", "list", "--environment", "app-01"][..],
            &[
                "piquelctl",
                "app",
                "exec",
                "piquel-fr",
                "auth",
                "-it",
                "--",
                "sh",
                "-c",
                "id",
            ],
        ] {
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
        assert!(Cli::try_parse_from(["piquelctl", "app", "plan", "status"]).is_err());
        // Commands follow `--`, and services are validated logical names.
        assert!(Cli::try_parse_from(["piquelctl", "app", "exec", "notes", "web"]).is_err());
        assert!(
            Cli::try_parse_from(["piquelctl", "app", "exec", "notes", "Web", "--", "id"]).is_err()
        );
    }

    #[test]
    fn account_targets_do_not_select_the_credential() {
        use super::{accounts::AccountCommand, cli::Command};
        for (command, extra) in [
            ("access", &["--permission", "apps:read"][..]),
            ("enroll", &[]),
        ] {
            let mut arguments = vec!["piquelctl", "--account", "alice", "account", command, "bob"];
            arguments.extend(extra);
            let cli = Cli::try_parse_from(arguments).expect("command parses");
            assert_eq!(cli.auth.account.as_deref(), Some("alice"));
            let Command::Account {
                command: AccountCommand::Access { target, .. } | AccountCommand::Enroll { target },
            } = cli.command
            else {
                panic!("account command");
            };
            assert_eq!(target, "bob");
        }
    }

    // Clap stores arguments by id, so a subcommand argument sharing a global's
    // id silently overwrites it (`app repository connect <URL>` once replaced `--url`).
    #[test]
    fn subcommand_arguments_never_reuse_global_ids() {
        fn check(command: &clap::Command, globals: &[String], path: &str) {
            for subcommand in command.get_subcommands() {
                let path = format!("{path} {}", subcommand.get_name());
                for argument in subcommand.get_arguments() {
                    let id = argument.get_id().as_str();
                    assert!(
                        !globals.iter().any(|global| global == id),
                        "`{path}` argument `{id}` shadows a global argument",
                    );
                }
                check(subcommand, globals, &path);
            }
        }
        let command = <Cli as clap::CommandFactory>::command();
        let globals: Vec<String> = command
            .get_arguments()
            .filter(|argument| argument.is_global_set())
            .map(|argument| argument.get_id().to_string())
            .collect();
        assert!(globals.iter().any(|global| global == "url"));
        check(&command, &globals, "piquelctl");
    }

    #[test]
    fn timeout_parser_accepts_bounded_units() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_mins(2));
        assert_eq!(parse_duration("1").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_hours(7 * 24));
    }

    #[test]
    fn application_ids_are_distinguished_from_names() {
        assert!(looks_like_application_id("app-notes-01"));
        assert!(!looks_like_application_id("notes"));
        assert!(!looks_like_application_id("APP-NOTES"));
        assert!(!looks_like_application_id("--------"));
    }

    #[tokio::test]
    async fn oversized_timeout_is_an_input_error_instead_of_a_panic() {
        let cli =
            Cli::try_parse_from(["piquelctl", "--timeout", "18446744073709551615s", "status"])
                .expect("the duration parser accepts a syntactically valid value");

        let error = super::run_with_timeout(&cli, std::future::pending::<Result<(), _>>())
            .await
            .expect_err("the platform cannot represent the deadline");

        assert_eq!(error.to_string(), "timeout is too large");
    }
}
