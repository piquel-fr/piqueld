//! Task presentation only: callers supply domain-independent labels and outcomes.
use super::{Escaped, Event, HumanWriter, SharedWriter};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Final state printed on a task's completion line.
#[derive(Clone, Copy)]
pub(crate) enum TaskOutcome {
    Succeeded,
    Failed,
    Skipped,
}
impl TaskOutcome {
    fn text(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// How progress is presented, chosen once per console.
enum Backend {
    /// Animated spinner rows on an interactive stderr.
    Terminal(MultiProgress),
    /// One permanent line per start, change, and finish (logs, pipes, `TERM=dumb`).
    Plain,
    /// One `progress` event line per start, change, and finish (`--json`).
    Json,
    /// Nothing is printed (quiet mode).
    Hidden,
}

/// Console-owned progress renderer; tasks keep their own reference to the output.
pub(super) struct ProgressOutput {
    inner: Arc<Output>,
}
/// Progress backend plus the shared stderr writer used for permanent lines.
struct Output {
    backend: Backend,
    writer: SharedWriter,
}

impl ProgressOutput {
    pub(super) fn new(writer: SharedWriter, terminal: bool, quiet: bool, json: bool) -> Self {
        let backend = if quiet {
            Backend::Hidden
        } else if json {
            Backend::Json
        } else if terminal {
            Backend::Terminal(MultiProgress::with_draw_target(ProgressDrawTarget::stderr()))
        } else {
            Backend::Plain
        };
        Self {
            inner: Arc::new(Output { backend, writer }),
        }
    }

    /// Runs `event` with spinner rows hidden, so other stderr/stdout writes never
    /// interleave with a redraw.
    pub(super) fn suspend<T>(&self, event: impl FnOnce() -> T) -> T {
        self.inner.suspend(event)
    }

    #[cfg(test)]
    pub(super) fn set_draw_target(&self, target: ProgressDrawTarget) {
        if let Backend::Terminal(multi) = &self.inner.backend {
            multi.set_draw_target(target);
        }
    }

    /// Starts a task: a ticking spinner on terminals, a `label: started` line in plain
    /// mode, or nothing when hidden.
    pub(super) fn start(&self, label: &str) -> ProgressTask {
        let bar = match &self.inner.backend {
            Backend::Terminal(multi) => {
                let bar = multi.add(ProgressBar::new_spinner());
                bar.set_style(
                    ProgressStyle::with_template("{spinner} {prefix}: {msg} [{elapsed}]")
                        .expect("static progress template"),
                );
                bar.set_prefix(Escaped(label).to_string());
                bar.enable_steady_tick(Duration::from_millis(100));
                Some(bar)
            }
            Backend::Plain | Backend::Json => {
                self.inner.line(label, "started", None);
                None
            }
            Backend::Hidden => None,
        };
        ProgressTask {
            state: Arc::new(Mutex::new(Task {
                output: Arc::clone(&self.inner),
                label: label.to_owned(),
                bar,
                started: Instant::now(),
                last: None,
                finished: false,
            })),
        }
    }
}

impl Output {
    fn suspend<T>(&self, event: impl FnOnce() -> T) -> T {
        match &self.backend {
            Backend::Terminal(multi) => multi.suspend(event),
            _ => event(),
        }
    }

    /// Prints a permanent `label: message` line, or a `progress` event with
    /// `--json` (best effort, skipped when hidden).
    fn line(&self, label: &str, message: &str, outcome: Option<TaskOutcome>) {
        if matches!(self.backend, Backend::Hidden) {
            return;
        }
        // Completion is a permanent ordinary line, not an indicatif zombie row.
        // This survives later task drops and progress redraws.
        let _ = self.suspend(|| -> io::Result<()> {
            let mut writer = self
                .writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(self.backend, Backend::Json) {
                Event::Progress {
                    task: label,
                    message,
                    outcome: outcome.map(TaskOutcome::text),
                }
                .write(writer.as_mut())?;
            } else {
                let outcome =
                    outcome.map_or_else(String::new, |outcome| format!("{} · ", outcome.text()));
                HumanWriter::new(writer.as_mut())
                    .line(format_args!("{label}: {outcome}{message}"))?;
            }
            writer.flush()
        });
    }
}

/// Clones share one task lifecycle. Only the last unfinished handle clears its
/// transient row; explicit finish is idempotent across all clones.
#[derive(Clone)]
pub(crate) struct ProgressTask {
    state: Arc<Mutex<Task>>,
}

impl ProgressTask {
    /// Sets the task's current message; unchanged messages are skipped so plain
    /// mode prints only transitions. No-op after finish or when hidden.
    pub(crate) fn update(&self, message: &str) {
        let mut task = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if task.finished || matches!(task.output.backend, Backend::Hidden) {
            return;
        }
        if task.last.as_deref() == Some(message) {
            return;
        }
        if let Some(bar) = &task.bar {
            bar.set_message(Escaped(message).to_string());
        } else {
            task.output.line(&task.label, message, None);
        }
        task.last = Some(message.to_owned());
    }

    /// Clears the spinner and prints the permanent completion line. Idempotent.
    ///
    /// ```text
    /// op-123: succeeded · deleted [4s]
    /// ```
    pub(crate) fn finish(&self, outcome: TaskOutcome, message: &str) {
        let mut task = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if task.finished {
            return;
        }
        task.finished = true;
        if let Some(bar) = &task.bar {
            bar.finish_and_clear();
        }
        task.output.line(
            &task.label,
            &format!("{message} [{}s]", task.started.elapsed().as_secs()),
            Some(outcome),
        );
    }
}

/// Shared state behind every clone of one `ProgressTask`.
struct Task {
    output: Arc<Output>,
    /// Task label, escaped when rendered for humans.
    label: String,
    /// Spinner row, present only on the terminal backend.
    bar: Option<ProgressBar>,
    started: Instant,
    /// Last message, used to suppress duplicate updates.
    last: Option<String>,
    finished: bool,
}
impl Drop for Task {
    fn drop(&mut self) {
        if !self.finished
            && let Some(bar) = &self.bar
        {
            bar.finish_and_clear();
        }
    }
}
