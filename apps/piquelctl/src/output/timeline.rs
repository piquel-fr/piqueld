//! An operation's steps as it runs: a tree redrawn on terminals, a line per
//! step that starts or changes in logs and pipes, and a `step` event per
//! change with `--json`.
//!
//! ```text
//! ⠹ Deploying operation-01  ▕████████░░░░▏ 6/9  48.2 s  ~30.0 s left
//!   ✓ Fetch manifest  412 ms
//!   ✓ Build api       38.2 s  (usual 25.0 s)
//!   ⠹ Update api      3.2 s
//!   ○ Wait for api    ~30.0 s
//! ```

use super::{
    Escaped, Event, HumanWriter,
    progress::{Backend, Output, Progress, ProgressOutput, TaskOutcome},
};
use crate::support::now_ms;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use parking_lot::Mutex;
use piqueld_client::{
    OperationKind, OperationState,
    observability::Elapsed,
    timeline::{Step, StepState, Timeline},
};
use std::{collections::HashMap, fmt, io, sync::Arc, time::Duration};

/// Step rows a terminal shows at most: beyond them, the earliest steps that
/// succeeded fold into one row, then the last steps left into another.
const MAX_ROWS: usize = 15;
/// Frames of a spinner, one per redraw.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Cells of the header's bar of steps done.
const BAR_CELLS: usize = 12;

/// The live view of one operation's timeline. Callers [`Self::update`] it
/// with each timeline received; the terminal row redraws itself every
/// 100 ms from the latest, so running steps' times advance between them.
pub(crate) struct TimelineProgress {
    output: Arc<Output>,
    /// What the terminal row draws, shared with its draw callback.
    view: Arc<Mutex<View>>,
    /// The redrawn row, on terminals only.
    bar: Option<ProgressBar>,
    /// In line modes, the operation's state last printed.
    printed_state: Option<OperationState>,
    /// In line modes, the state and retries each started step, by its
    /// action, was last printed with.
    printed: HashMap<String, (StepState, u64)>,
    finished: bool,
}

impl TimelineProgress {
    /// Starts showing the operation `label` names; until its first timeline,
    /// a terminal shows a spinner and the label.
    pub(super) fn new(progress: &ProgressOutput, label: &str) -> Self {
        let output = Arc::clone(&progress.inner);
        let view = Arc::new(Mutex::new(View {
            label: label.to_owned(),
            timeline: None,
            waiting: None,
            color: anstream::AutoStream::choice(&io::stderr()) != anstream::ColorChoice::Never,
        }));
        let bar = match &output.backend {
            Backend::Terminal(multi) => {
                let bar = multi.add(ProgressBar::new_spinner());
                let drawn = Arc::clone(&view);
                let draw = move |state: &ProgressState, out: &mut dyn fmt::Write| {
                    let frame = usize::try_from(state.elapsed().as_millis() / 100).unwrap_or(0);
                    // A failed write only cuts this frame short.
                    let _ = drawn.lock().draw(out, now_ms(), frame);
                };
                bar.set_style(
                    ProgressStyle::with_template("{timeline}")
                        .expect("the literal template uses only its own key")
                        .with_key("timeline", draw),
                );
                bar.enable_steady_tick(Duration::from_millis(100));
                Some(bar)
            }
            _ => None,
        };
        Self {
            output,
            view,
            bar,
            printed_state: None,
            printed: HashMap::new(),
            finished: false,
        }
    }

    /// Shows the operation's latest timeline. In line modes, prints the
    /// operation's state when it changed and each step that started or
    /// changed since last printed.
    pub(crate) fn update(&mut self, timeline: &Timeline) {
        if self.finished {
            return;
        }
        if matches!(self.output.backend, Backend::Plain | Backend::Json) {
            let label = self.view.lock().label.clone();
            let state = timeline.operation.state;
            if self.printed_state != Some(state) && !state.terminal() {
                self.printed_state = Some(state);
                self.output.line(&label, state.as_str(), None);
            }
            for step in &timeline.steps {
                let Some(action) = &step.action_id else {
                    continue;
                };
                let shown = (step.state, step.retries);
                if self.printed.get(action) != Some(&shown) {
                    self.printed.insert(action.clone(), shown);
                    self.print(&label, step);
                }
            }
        }
        let mut view = self.view.lock();
        // A running operation's timeline supersedes what was shown under
        // it, such as a failure the wait rode out.
        if !timeline.operation.state.terminal() {
            view.waiting = None;
        }
        view.timeline = Some(timeline.clone());
    }

    /// Prints a step that started or changed, as one line or `step` event.
    ///
    /// ```text
    /// operation-01: Build api · succeeded · 38.2 s  (usual 25.0 s)
    /// ```
    fn print(&self, label: &str, step: &Step) {
        self.output.write(|writer| {
            if matches!(self.output.backend, Backend::Json) {
                return Event::Step { task: label, step }.write(writer);
            }
            let name = StepLabel(step);
            let detail = StepDetail { step, now_ms: None }.to_string();
            let gap = if detail.is_empty() { "" } else { " · " };
            HumanWriter::new(writer).line(format_args!(
                "{label}: {name} · {}{gap}{detail}",
                step.state
            ))
        });
    }

    /// Ends the view with a permanent line: the outcome, why waiting failed
    /// if it did, and how long the operation took; on terminals, then its
    /// steps as they ended. Idempotent.
    ///
    /// ```text
    /// operation-01: succeeded · 52.3 s (usual 48.0 s)
    /// ```
    pub(crate) fn finish(&mut self, outcome: TaskOutcome, failure: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        // Cleared before the view is locked: clearing draws the row.
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
        let now_ms = now_ms();
        let (label, total, rows) = {
            let view = self.view.lock();
            let mut rows = String::new();
            if let (Backend::Terminal(_), Some(timeline)) = (&self.output.backend, &view.timeline) {
                // Writing to a `String` cannot fail.
                let _ = view.rows(&mut rows, timeline, now_ms, 0, usize::MAX);
            }
            (view.label.clone(), view.total(now_ms), rows)
        };
        let message = [failure.map(str::to_owned), total]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        self.output.line(&label, &message, Some(outcome));
        if !rows.is_empty() {
            self.output
                .write(|writer| writeln!(writer, "{}", rows.trim_start_matches('\n')));
        }
    }
}

/// Shows what the wait is on under the steps: what the caller still waits
/// for once the operation ended, or a failure the wait goes on through.
impl Progress for TimelineProgress {
    fn show(&self, message: &str) {
        let mut view = self.view.lock();
        if self.finished || view.waiting.as_deref() == Some(message) {
            return;
        }
        view.waiting = Some(message.to_owned());
        let label = view.label.clone();
        drop(view);
        if !matches!(self.output.backend, Backend::Terminal(_)) {
            self.output.line(&label, message, None);
        }
    }
}

impl Drop for TimelineProgress {
    fn drop(&mut self) {
        if !self.finished
            && let Some(bar) = &self.bar
        {
            bar.finish_and_clear();
        }
    }
}

/// What the terminal row draws.
struct View {
    /// Names the operation, escaped when drawn.
    label: String,
    /// The latest timeline received.
    timeline: Option<Timeline>,
    /// What the caller still waits for once the operation ended.
    waiting: Option<String>,
    /// Whether stderr takes colors.
    color: bool,
}

impl View {
    /// Draws the row at `now_ms`: a header with the steps done and the time
    /// left, then the steps; `frame` turns the spinners.
    fn draw(&self, out: &mut dyn fmt::Write, now_ms: i64, frame: usize) -> fmt::Result {
        self.paint(out, "36", SPINNER[frame % SPINNER.len()])?;
        let Some(timeline) = &self.timeline else {
            return write!(out, " {}", Escaped(&self.label));
        };
        let verb = match timeline.operation.kind {
            OperationKind::Delete => "Deleting",
            OperationKind::Apply | OperationKind::Refresh => "Deploying",
        };
        write!(out, " {verb} {}  ▕", Escaped(&self.label))?;
        let total = timeline.steps.len();
        let done = timeline
            .steps
            .iter()
            .filter(|step| step.state.finished())
            .count();
        let filled = (done * BAR_CELLS).checked_div(total).unwrap_or(0);
        for cell in 0..BAR_CELLS {
            out.write_char(if cell < filled { '█' } else { '░' })?;
        }
        write!(out, "▏ {done}/{total}")?;
        match timeline.operation.started_at_ms {
            Some(started) => write!(out, "  {}", Elapsed::from_millis(now_ms - started))?,
            None => out.write_str("  queued")?,
        }
        if let Some(left) = timeline.remaining_ms(now_ms) {
            write!(out, "  ~{} left", Elapsed(Duration::from_millis(left)))?;
        }
        self.rows(out, timeline, now_ms, frame, MAX_ROWS)?;
        if let Some(waiting) = &self.waiting {
            write!(out, "\n  … {}", Escaped(waiting))?;
        }
        Ok(())
    }

    /// Writes a row per step, each on a new line, with at most `max_rows`
    /// rows.
    fn rows(
        &self,
        out: &mut dyn fmt::Write,
        timeline: &Timeline,
        now_ms: i64,
        frame: usize,
        max_rows: usize,
    ) -> fmt::Result {
        let steps = &timeline.steps;
        let succeeded = steps
            .iter()
            .take_while(|step| step.state == StepState::Succeeded)
            .count();
        let folded = match steps.len().checked_sub(max_rows) {
            Some(over) if over > 0 => succeeded.min(over + 1),
            _ => 0,
        };
        let folded = if folded > 1 { folded } else { 0 };
        if folded > 0 {
            out.write_str("\n  ")?;
            self.paint(out, "32", '✓')?;
            write!(out, " {folded} steps done")?;
        }
        let budget = max_rows - usize::from(folded > 0);
        let mut shown = &steps[folded..];
        let more = if shown.len() > budget {
            shown = &shown[..budget - 1];
            steps.len() - folded - shown.len()
        } else {
            0
        };
        let width = shown
            .iter()
            .map(|step| StepLabel(step).width())
            .max()
            .unwrap_or(0);
        for step in shown {
            self.row(out, step, width, now_ms, frame)?;
        }
        if more > 0 {
            out.write_str("\n  ")?;
            self.paint(out, "2", '○')?;
            write!(out, " {more} more steps")?;
        }
        Ok(())
    }

    /// Writes one step's row on a new line, its label padded to `width`.
    fn row(
        &self,
        out: &mut dyn fmt::Write,
        step: &Step,
        width: usize,
        now_ms: i64,
        frame: usize,
    ) -> fmt::Result {
        let (glyph, color) = match step.state {
            StepState::Succeeded => ('✓', "32"),
            StepState::Failed => ('✗', "31"),
            StepState::Interrupted => ('!', "33"),
            StepState::Retrying => ('↻', "33"),
            StepState::Running => (SPINNER[frame % SPINNER.len()], "36"),
            StepState::Forecast | StepState::Planned => ('○', "2"),
        };
        out.write_str("\n  ")?;
        self.paint(out, color, glyph)?;
        let name = StepLabel(step);
        let pad = width - name.width();
        write!(out, " {}{:pad$}  ", Escaped(name), "")?;
        let now_ms = Some(now_ms);
        let detail = Escaped(StepDetail { step, now_ms });
        if step.state.started() {
            write!(out, "{detail}")
        } else {
            self.paint(out, "2", detail)
        }
    }

    /// Writes `text` in the ANSI `color`, when stderr takes colors.
    fn paint(&self, out: &mut dyn fmt::Write, color: &str, text: impl fmt::Display) -> fmt::Result {
        if self.color {
            write!(out, "\x1b[{color}m{text}\x1b[0m")
        } else {
            write!(out, "{text}")
        }
    }

    /// How long the operation took, or has run at `now_ms`, and, once every
    /// step succeeded, how long its steps usually take: `52.3 s (usual
    /// 48.0 s)`. The steps of a failed run are not the usual ones.
    fn total(&self, now_ms: i64) -> Option<String> {
        let timeline = self.timeline.as_ref()?;
        let operation = &timeline.operation;
        let took = Elapsed::from_millis(
            operation.finished_at_ms.unwrap_or(now_ms) - operation.started_at_ms?,
        );
        let succeeded = timeline
            .steps
            .iter()
            .all(|step| step.state == StepState::Succeeded);
        Some(match timeline.usual_ms().filter(|_| succeeded) {
            Some(usual) => format!("{took} (usual {})", Elapsed(Duration::from_millis(usual))),
            None => took.to_string(),
        })
    }
}

/// What a step does and on what: `Build api`.
#[derive(Clone, Copy)]
struct StepLabel<'a>(&'a Step);

impl StepLabel<'_> {
    /// Its width in characters.
    fn width(self) -> usize {
        self.0.key.phase.label().chars().count()
            + self
                .0
                .key
                .resource
                .as_ref()
                .map_or(0, |resource| 1 + resource.chars().count())
    }
}

impl fmt::Display for StepLabel<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.key.fmt(formatter)
    }
}

/// How long a step took, or should take before it started, and how it
/// went: `38.2 s  (usual 25.0 s)`, `retry 2: Docker is unavailable` or
/// `~30.0 s`. With `now_ms`, also how long a step still running has run.
struct StepDetail<'a> {
    step: &'a Step,
    now_ms: Option<i64>,
}

impl fmt::Display for StepDetail<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let step = self.step;
        if !step.state.started() {
            return match step.expected_ms {
                Some(expected) => {
                    write!(formatter, "~{}", Elapsed(Duration::from_millis(expected)))
                }
                None => Ok(()),
            };
        }
        // Separates the parts written.
        let mut gap = "";
        let now_ms = self.now_ms.or(step.finished_at_ms);
        if let Some(took) = now_ms.and_then(|now_ms| step.duration_ms(now_ms)) {
            write!(formatter, "{}", Elapsed(Duration::from_millis(took)))?;
            gap = "  ";
        }
        if let Some(usual) = step.unusual_ms() {
            write!(
                formatter,
                "{gap}(usual {})",
                Elapsed(Duration::from_millis(usual))
            )?;
        }
        let summary = step
            .diagnostic
            .as_ref()
            .map(|diagnostic| &diagnostic.summary);
        match (step.state, step.retries) {
            (StepState::Retrying, retries) => {
                write!(formatter, "{gap}retry {}", retries + 1)?;
                if let Some(summary) = summary {
                    write!(formatter, ": {summary}")?;
                }
                return Ok(());
            }
            (StepState::Running, 0) => {}
            (StepState::Running, retries) => write!(formatter, "{gap}retry {retries}")?,
            (_, 0) => {}
            (_, 1) => write!(formatter, "{gap}after 1 retry")?,
            (_, retries) => write!(formatter, "{gap}after {retries} retries")?,
        }
        match (step.state, summary) {
            (StepState::Failed, Some(summary)) => write!(formatter, "  {summary}"),
            (StepState::Interrupted, _) => formatter.write_str("  interrupted"),
            _ => Ok(()),
        }
    }
}
