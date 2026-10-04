//! `app exec`: run a one-off command in a running service task.
//!
//! Each API request is bounded by `--timeout`; the command session is not.
//! The CLI exits with the command's exit code.

use crate::error::{CliError, ErrorKind, Result};
use clap::Args;
use piqueld_client::{
    Client, ServiceName,
    exec::{ExecCommand, ExecInput, ExecOutput, ExecReader, ExecRequest, ExecWriter, TerminalSize},
};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use std::{
    io::{IsTerminal, Read},
    process::ExitCode,
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    signal::unix::{Signal, SignalKind, signal},
    sync::mpsc,
};

#[derive(Debug, Args)]
pub(crate) struct ExecArgs {
    /// Application name or stable ID.
    name_or_id: String,
    /// Service whose running task executes the command.
    service: ServiceName,
    /// Forward standard input to the command.
    #[arg(long, short)]
    interactive: bool,
    /// Allocate a terminal for interactive programs; implies --interactive.
    #[arg(long, short)]
    tty: bool,
    /// Program and arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    command: Vec<String>,
}

impl ExecArgs {
    /// Runs the command and returns its exit code.
    pub(crate) async fn run(&self, client: &Client) -> Result<ExitCode> {
        let command = ExecCommand::parse(self.command.clone())
            .map_err(|error| CliError::new(ErrorKind::Input, error.to_string()))?;
        if self.tty && !std::io::stdin().is_terminal() {
            return Err(CliError::new(
                ErrorKind::Input,
                "--tty requires standard input to be a terminal",
            ));
        }
        let application = crate::commands::resolve_application(client, &self.name_or_id).await?;
        let request = ExecRequest {
            service: self.service.clone(),
            command,
            stdin: self.interactive || self.tty,
            tty: self.tty.then(RawTerminal::size).transpose()?,
        };
        let (output, input) = client
            .exec(application.application.id().as_str(), &request)
            .await?;
        // Restored on drop, before main reports any error.
        let _terminal = self.tty.then(RawTerminal::enable).transpose()?;
        let resizes = self
            .tty
            .then(|| signal(SignalKind::window_change()))
            .transpose()
            .map_err(|error| local_error("watch terminal size", &error))?;
        // A local input failure aborts the session rather than ending the
        // command's input early, which would look like success.
        let code = tokio::select! {
            code = receive(output) => code?,
            Err(error) = forward(input, request.stdin.then(read_stdin), resizes) => return Err(error),
        };
        Ok(ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX)))
    }
}

/// Writes command output until the final frame and returns the exit code.
/// Writes are asynchronous so a blocked output pipe never stops `forward`.
async fn receive(mut output: ExecReader) -> Result<i64> {
    let (mut stdout, mut stderr) = (tokio::io::stdout(), tokio::io::stderr());
    loop {
        match output.next().await? {
            Some(ExecOutput::Stdout(data)) => write(&mut stdout, &data).await?,
            Some(ExecOutput::Stderr(data)) => write(&mut stderr, &data).await?,
            Some(ExecOutput::Exit(code)) => return Ok(code),
            Some(ExecOutput::Failed(error)) => {
                return Err(CliError::new(
                    ErrorKind::Unavailable,
                    format!("{} ({})", error.message, error.code),
                )
                .api(error.code, error.request_id, error.details));
            }
            None => {
                return Err(CliError::new(
                    ErrorKind::Unavailable,
                    "exec stream ended before the command exited",
                ));
            }
        }
    }
}

async fn write(target: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> Result<()> {
    let written = async {
        target.write_all(data).await?;
        target.flush().await
    };
    written
        .await
        .map_err(|error| local_error("write command output", &error))
}

/// Sends standard input and terminal size changes until either the daemon
/// stops accepting input or there is nothing left to forward.
/// # Errors
/// Returns a failure to read standard input.
async fn forward(
    mut input: ExecWriter,
    mut stdin: Option<mpsc::Receiver<std::io::Result<Vec<u8>>>>,
    mut resizes: Option<Signal>,
) -> Result<()> {
    loop {
        // Disabled branches still evaluate their expressions, so each source
        // is only touched inside a lazy `async` block that is never polled.
        let frame = tokio::select! {
            chunk = async { stdin.as_mut().expect("enabled").recv().await }, if stdin.is_some() => {
                match chunk {
                    Some(Ok(data)) => ExecInput::Stdin(data),
                    Some(Err(error)) => return Err(local_error("read standard input", &error)),
                    None => {
                        stdin = None;
                        ExecInput::CloseStdin
                    }
                }
            }
            Some(()) = async { resizes.as_mut().expect("enabled").recv().await }, if resizes.is_some() => match RawTerminal::size() {
                Ok(size) => ExecInput::Resize(size),
                Err(_) => continue,
            },
            else => return Ok(()),
        };
        if input.send(&frame).await.is_err() {
            return Ok(());
        }
    }
}

/// Reads standard input on a plain thread: a blocking read cannot be
/// cancelled, and runtime shutdown must not wait for it after the command exits.
/// A read error is sent as the last item; the channel closes at end of input.
fn read_stdin() -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel(4);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = vec![0; 16 * 1024];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => {
                    if sender.blocking_send(Ok(buffer[..read].to_vec())).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let _ = sender.blocking_send(Err(error));
                    return;
                }
            }
        }
    });
    receiver
}

/// The terminal on standard input in raw mode; dropping it restores the original mode.
struct RawTerminal(Termios);

impl RawTerminal {
    fn enable() -> Result<Self> {
        let original = tcgetattr(std::io::stdin())
            .map_err(|error| local_error("read terminal mode", &error))?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(std::io::stdin(), OptionalActions::Now, &raw)
            .map_err(|error| local_error("enable raw terminal mode", &error))?;
        Ok(Self(original))
    }

    fn size() -> Result<TerminalSize> {
        let size = tcgetwinsize(std::io::stdin())
            .map_err(|error| local_error("read terminal size", &error))?;
        Ok(TerminalSize {
            width: size.ws_col,
            height: size.ws_row,
        })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = tcsetattr(std::io::stdin(), OptionalActions::Now, &self.0);
    }
}

fn local_error(action: &str, error: &impl std::fmt::Display) -> CliError {
    CliError::new(ErrorKind::General, format!("could not {action}: {error}"))
}
