//! Streaming CLI presentation. Configuration is resolved once at construction.
//!
//! Results use stdout; every other role uses stderr. Quiet suppresses human
//! results, info and progress, but preserves JSON, warnings, prompts and errors.
//! Each event is rendered and flushed before returning, coordinated with active
//! progress. Clap owns help/version/parse errors before this boundary exists.

mod progress;
pub(crate) mod reports;
#[cfg(test)]
mod tests;

use crate::{cli::Cli, error::Result};
use progress::ProgressOutput;
pub(crate) use progress::{ProgressTask, TaskOutcome};
use serde::Serialize;
use std::{
    fmt,
    io::{self, IsTerminal, Write},
    sync::{Arc, Mutex},
};

type Writer = Box<dyn Write + Send>;
type SharedWriter = Arc<Mutex<Writer>>;

/// One result event, with independently typed machine data and human rendering.
pub(crate) trait Report {
    type Json: Serialize + ?Sized;
    fn json(&self) -> &Self::Json;
    fn render_human(&self, output: &mut HumanWriter<'_>) -> io::Result<()>;
}

/// Human-only stderr events can carry context without changing stdout's schema.
pub(crate) trait DiagnosticReport {
    fn render(&self, output: &mut HumanWriter<'_>) -> io::Result<()>;
}

enum ResultChannel {
    Human(Writer),
    Json(Writer),
    Hidden,
}

/// Owns output policy. Only progress handles may be cloned across tasks.
pub(crate) struct Console {
    result: ResultChannel,
    stderr: SharedWriter,
    info: bool,
    progress: ProgressOutput,
}

impl Console {
    pub(crate) fn new(cli: &Cli) -> Self {
        let terminal =
            io::stderr().is_terminal() && std::env::var("TERM").is_ok_and(|term| term != "dumb");
        let stdout: Writer = if cli.json {
            Box::new(io::stdout())
        } else {
            Box::new(anstream::stdout())
        };
        Self::with_writers(
            cli.json,
            cli.quiet,
            terminal,
            stdout,
            Box::new(anstream::stderr()),
        )
    }

    fn with_writers(
        json: bool,
        quiet: bool,
        terminal: bool,
        stdout: Writer,
        stderr: Writer,
    ) -> Self {
        let stderr = Arc::new(Mutex::new(stderr));
        Self {
            result: if json {
                ResultChannel::Json(stdout)
            } else if quiet {
                ResultChannel::Hidden
            } else {
                ResultChannel::Human(stdout)
            },
            progress: ProgressOutput::new(Arc::clone(&stderr), terminal, quiet),
            stderr,
            info: !quiet,
        }
    }

    /// Command data on stdout. Quiet hides human results, never explicit JSON.
    pub(crate) fn emit(&mut self, report: &impl Report) -> Result<()> {
        self.progress.suspend(|| {
            match &mut self.result {
                ResultChannel::Human(writer) => {
                    report.render_human(&mut HumanWriter::new(writer.as_mut()))?;
                    writer.flush()?;
                }
                ResultChannel::Json(writer) => {
                    serde_json::to_writer(&mut **writer, report.json())
                        .map_err(io::Error::other)?;
                    writeln!(writer)?;
                    writer.flush()?;
                }
                ResultChannel::Hidden => {}
            }
            Ok(())
        })
    }

    /// Routine context/advice on stderr; discarded in quiet mode.
    pub(crate) fn info(&mut self, message: impl fmt::Display) -> Result<()> {
        if self.info {
            self.message("Info", message)?;
        }
        Ok(())
    }

    /// Degraded, incomplete or surprising results on stderr, including quiet mode.
    pub(crate) fn warning(&mut self, message: impl fmt::Display) -> Result<()> {
        self.message("Warning", message)
    }

    /// A contextual warning is one indivisible, fallible stderr event.
    pub(crate) fn warning_report(&mut self, report: &impl DiagnosticReport) -> Result<()> {
        self.diagnostic(report)
    }

    /// Final failure/context on stderr. Best effort because it cannot report its
    /// own write failure; the caller preserves the original command exit code.
    pub(crate) fn error(&mut self, report: &impl DiagnosticReport) {
        let _ = self.diagnostic(report);
    }

    /// Fatal interaction messages use the same best-effort error boundary.
    pub(crate) fn error_message(&mut self, message: impl fmt::Display) {
        let _ = self.message("Error", message);
    }

    fn diagnostic(&mut self, report: &impl DiagnosticReport) -> Result<()> {
        self.write_stderr(|out| report.render(out))
    }

    fn message(&mut self, role: &'static str, message: impl fmt::Display) -> Result<()> {
        self.write_stderr(|out| out.label(role, message))
    }

    fn write_stderr(
        &mut self,
        render: impl FnOnce(&mut HumanWriter<'_>) -> io::Result<()>,
    ) -> Result<()> {
        self.progress.suspend(|| {
            let mut writer = self
                .stderr
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            render(&mut HumanWriter::new(writer.as_mut()))?;
            writer.flush()?;
            Ok(())
        })
    }

    /// Interactive questions on stderr, always flushed and never quieted.
    /// The caller enforces --yes/noninteractive policy before invoking this.
    pub(crate) fn prompt(&mut self, message: &str) -> Result<()> {
        self.write_stderr(|out| out.value(message))
    }

    /// Transient task state on stderr; hidden in quiet mode. Updates are best effort.
    pub(crate) fn start_task(&mut self, label: &str) -> ProgressTask {
        self.progress.start(label)
    }
}

/// Explicit human styling and escaping. Dynamic values cannot emit controls;
/// only `log_text` preserves newline/tab. No rendered command output is retained.
pub(crate) struct HumanWriter<'a> {
    writer: &'a mut dyn Write,
}

impl<'a> HumanWriter<'a> {
    pub(crate) fn new(writer: &'a mut dyn Write) -> Self {
        Self { writer }
    }

    pub(crate) fn value(&mut self, value: impl fmt::Display) -> io::Result<()> {
        write!(self.writer, "{}", Escaped(value))
    }

    pub(crate) fn line(&mut self, value: impl fmt::Display) -> io::Result<()> {
        self.value(value)?;
        writeln!(self.writer)
    }

    pub(crate) fn blank(&mut self) -> io::Result<()> {
        writeln!(self.writer)
    }

    pub(crate) fn label(&mut self, label: &str, value: impl fmt::Display) -> io::Result<()> {
        write!(self.writer, "\x1b[1;36m{}:\x1b[0m ", Escaped(label))?;
        self.line(value)
    }

    pub(crate) fn heading(&mut self, heading: &str) -> io::Result<()> {
        writeln!(self.writer, "\x1b[1m{}\x1b[0m", Escaped(heading))
    }

    pub(crate) fn change(&mut self, marker: char, field: &str, value: &str) -> io::Result<()> {
        let color = match marker {
            '+' => 32,
            '-' => 31,
            _ => 33,
        };
        write!(
            self.writer,
            "\x1b[{color}m  {marker} {}\x1b[0m   ",
            Escaped(field)
        )?;
        self.line(value)
    }

    pub(crate) fn log_text(&mut self, text: &str) -> io::Result<()> {
        for character in text.chars() {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                write!(self.writer, "{}", character.escape_default())?;
            } else {
                write!(self.writer, "{character}")?;
            }
        }
        Ok(())
    }
}

/// Escape while formatting, without allocating the entire rendered event.
pub(crate) struct Escaped<T>(pub(crate) T);
impl<T: fmt::Display> fmt::Display for Escaped<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Escape<'a, 'b>(&'a mut fmt::Formatter<'b>);
        impl fmt::Write for Escape<'_, '_> {
            fn write_str(&mut self, value: &str) -> fmt::Result {
                for character in value.chars() {
                    if character.is_control() {
                        write!(self.0, "{}", character.escape_default())?;
                    } else {
                        write!(self.0, "{character}")?;
                    }
                }
                Ok(())
            }
        }
        fmt::write(&mut Escape(formatter), format_args!("{}", self.0))
    }
}
