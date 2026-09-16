//! Task presentation only: callers supply domain-independent labels and outcomes.
use super::{Escaped, HumanWriter, SharedWriter};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

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

enum Backend {
    Terminal(MultiProgress),
    Plain,
    Hidden,
}

pub(super) struct ProgressOutput {
    inner: Arc<Output>,
}
struct Output {
    backend: Backend,
    writer: SharedWriter,
}

impl ProgressOutput {
    pub(super) fn new(writer: SharedWriter, terminal: bool, quiet: bool) -> Self {
        let backend = if quiet {
            Backend::Hidden
        } else if terminal {
            Backend::Terminal(MultiProgress::with_draw_target(ProgressDrawTarget::stderr()))
        } else {
            Backend::Plain
        };
        Self {
            inner: Arc::new(Output { backend, writer }),
        }
    }

    pub(super) fn suspend<T>(&self, event: impl FnOnce() -> T) -> T {
        self.inner.suspend(event)
    }

    #[cfg(test)]
    pub(super) fn set_draw_target(&self, target: ProgressDrawTarget) {
        if let Backend::Terminal(multi) = &self.inner.backend {
            multi.set_draw_target(target);
        }
    }

    pub(super) fn start(&self, label: &str) -> ProgressTask {
        let label = Escaped(label).to_string();
        let bar = match &self.inner.backend {
            Backend::Terminal(multi) => {
                let bar = multi.add(ProgressBar::new_spinner());
                bar.set_style(
                    ProgressStyle::with_template("{spinner} {prefix}: {msg} [{elapsed}]")
                        .expect("static progress template"),
                );
                bar.set_prefix(label.clone());
                bar.enable_steady_tick(Duration::from_millis(100));
                Some(bar)
            }
            Backend::Plain => {
                self.inner.line(&label, "started");
                None
            }
            Backend::Hidden => None,
        };
        ProgressTask {
            state: Arc::new(Mutex::new(Task {
                output: Arc::clone(&self.inner),
                label,
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

    fn line(&self, label: &str, message: &str) {
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
            HumanWriter::new(writer.as_mut()).line(format_args!("{label}: {message}"))?;
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
    pub(crate) fn update(&self, message: &str) {
        let mut task = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if task.finished || matches!(task.output.backend, Backend::Hidden) {
            return;
        }
        let message = Escaped(message).to_string();
        if task.last.as_ref() == Some(&message) {
            return;
        }
        if let Some(bar) = &task.bar {
            bar.set_message(message.clone());
        } else {
            task.output.line(&task.label, &message);
        }
        task.last = Some(message);
    }

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
            &format!(
                "{} · {} [{}s]",
                outcome.text(),
                Escaped(message),
                task.started.elapsed().as_secs()
            ),
        );
    }
}

struct Task {
    output: Arc<Output>,
    label: String,
    bar: Option<ProgressBar>,
    started: Instant,
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
