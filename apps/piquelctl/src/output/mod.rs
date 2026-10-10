//! Streaming CLI presentation. Configuration is resolved once at construction.
//!
//! Results use stdout; every other role uses stderr. Quiet suppresses human
//! results, info and progress, but preserves JSON, warnings, prompts and errors.
//! With `--json`, every stderr event is one [`Event`] line. Each event is rendered and flushed before returning,
//! coordinated with active progress. `main` reports clap's usage errors itself.

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
/// Stderr shared between the console and progress tasks.
type SharedWriter = Arc<Mutex<Writer>>;

/// One result event, with independently typed machine data and human rendering.
pub(crate) trait Report {
    /// Machine schema written by `--json`.
    type Json: Serialize + ?Sized;
    fn json(&self) -> &Self::Json;
    /// Human rendering on stdout; values must go through the escaping writer.
    fn render_human(&self, output: &mut HumanWriter<'_>) -> io::Result<()>;
}

/// Stderr events that can carry context without changing stdout's schema.
pub(crate) trait DiagnosticReport {
    fn render(&self, output: &mut HumanWriter<'_>) -> io::Result<()>;
    /// The event written instead with `--json`.
    fn event(&self) -> Event<'_>;
}

/// One stderr event with `--json`: a single-line object whose only key names
/// its kind, e.g. `{"warning":{"message":"…"}}` or `{"error":{"code":…}}`.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Event<'a> {
    /// Routine context; hidden in quiet mode.
    Info { message: String },
    /// A degraded, incomplete, or surprising result.
    Warning { message: String },
    /// An interactive question, answered on stdin.
    Prompt { message: String },
    /// A task's progress; `outcome` is set once it finished.
    Progress {
        task: &'a str,
        message: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<&'static str>,
    },
    /// The command's failure, shaped like the API's error envelope.
    Error(crate::error::ErrorBody<'a>),
}

impl Event<'_> {
    /// Writes the event as one JSON line.
    pub(crate) fn write(&self, writer: &mut dyn Write) -> io::Result<()> {
        serde_json::to_writer(&mut *writer, self).map_err(io::Error::other)?;
        writeln!(writer)
    }
}

/// Destination for results, chosen once from `--json` and `--quiet`.
enum ResultChannel {
    Human(Writer),
    Json(Writer),
    Hidden,
}

/// Owns output policy. Only progress handles may be cloned across tasks.
pub(crate) struct Console {
    result: ResultChannel,
    stderr: SharedWriter,
    /// Whether stderr events are written as JSON lines.
    json: bool,
    /// Whether info messages are shown (off in quiet mode).
    info: bool,
    progress: ProgressOutput,
}

impl Console {
    /// Real stdout/stderr console. Progress animates only when stderr is a
    /// terminal with a non-`dumb` `TERM`; JSON output bypasses ANSI stripping.
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

    /// Builds a console over arbitrary writers; `new` and tests share this.
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
            progress: ProgressOutput::new(Arc::clone(&stderr), terminal, quiet, json),
            stderr,
            json,
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
            self.message("Info", message, |message| Event::Info { message })?;
        }
        Ok(())
    }

    /// Degraded, incomplete or surprising results on stderr, including quiet mode.
    pub(crate) fn warning(&mut self, message: impl fmt::Display) -> Result<()> {
        self.message("Warning", message, |message| Event::Warning { message })
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

    /// Writes a diagnostic report to stderr as one event.
    fn diagnostic(&mut self, report: &impl DiagnosticReport) -> Result<()> {
        if self.json {
            self.write_stderr(|writer| report.event().write(writer))
        } else {
            self.write_human(|out| report.render(out))
        }
    }

    /// Writes a labelled one-line message, e.g. `Warning: ...`, to stderr, or
    /// its `event` with `--json`.
    fn message(
        &mut self,
        role: &'static str,
        message: impl fmt::Display,
        event: impl FnOnce(String) -> Event<'static>,
    ) -> Result<()> {
        if self.json {
            self.write_stderr(|writer| event(message.to_string()).write(writer))
        } else {
            self.write_human(|out| out.label(role, message))
        }
    }

    /// Renders one human stderr event through the escaping writer.
    fn write_human(
        &mut self,
        render: impl FnOnce(&mut HumanWriter<'_>) -> io::Result<()>,
    ) -> Result<()> {
        self.write_stderr(|writer| render(&mut HumanWriter::new(writer)))
    }

    /// Writes one stderr event under the stderr lock with progress rows suspended,
    /// then flushes so the event is visible before returning.
    fn write_stderr(&mut self, write: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> Result<()> {
        self.progress.suspend(|| {
            let mut writer = self
                .stderr
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            write(writer.as_mut())?;
            writer.flush()?;
            Ok(())
        })
    }

    /// Interactive questions on stderr, always flushed and never quieted;
    /// a `prompt` event with `--json`.
    /// The caller enforces --yes/noninteractive policy before invoking this.
    pub(crate) fn prompt(&mut self, message: &str) -> Result<()> {
        if self.json {
            let message = message.trim_end().to_owned();
            return self.write_stderr(|writer| Event::Prompt { message }.write(writer));
        }
        self.write_human(|out| out.value(message))
    }

    /// Multi-line instructions for the operator, never quieted; values
    /// remain terminal-escaped. With `--json`, one `info` event.
    pub(crate) fn prompt_lines(&mut self, lines: &[String]) -> Result<()> {
        if self.json {
            let message = lines.join("\n");
            return self.write_stderr(|writer| Event::Info { message }.write(writer));
        }
        self.write_human(|out| {
            for line in lines {
                out.line(line)?;
            }
            Ok(())
        })
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

    /// Writes an escaped value without a newline.
    pub(crate) fn value(&mut self, value: impl fmt::Display) -> io::Result<()> {
        write!(self.writer, "{}", Escaped(value))
    }

    /// Writes an escaped value followed by a newline.
    pub(crate) fn line(&mut self, value: impl fmt::Display) -> io::Result<()> {
        self.value(value)?;
        writeln!(self.writer)
    }

    pub(crate) fn blank(&mut self) -> io::Result<()> {
        writeln!(self.writer)
    }

    /// Writes a bold cyan `Label:` prefix and an escaped value line.
    pub(crate) fn label(&mut self, label: &str, value: impl fmt::Display) -> io::Result<()> {
        write!(self.writer, "\x1b[1;36m{}:\x1b[0m ", Escaped(label))?;
        self.line(value)
    }

    /// Writes a bold heading line.
    pub(crate) fn heading(&mut self, heading: &str) -> io::Result<()> {
        writeln!(self.writer, "\x1b[1m{}\x1b[0m", Escaped(heading))
    }

    /// Writes one plan change line, colored by marker: `+` green, `-` red, anything
    /// else (`~`) yellow.
    ///
    /// ```text
    ///   ~ services.web.replicas   1 → 3
    /// ```
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

    /// Writes captured log text, keeping newlines and tabs but escaping every other
    /// control character so logs cannot drive the terminal.
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
