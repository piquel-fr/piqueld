//! Transport-independent layout of the dashboard's operation timelines:
//! where each step's bars sit on a shared time axis, where a rollout's Swarm
//! tasks sit on their step's own, and how far the operation has come.

use piqueld_client::observability::Elapsed;
use piqueld_client::timeline::{Stage, Step, StepKey, StepState, TaskLane, TaskSpan, Timeline};
use piqueld_client::{OperationKind, OperationState, TaskState};
use std::collections::HashMap;

/// Intervals the time axis ticks at, in milliseconds; the first giving at
/// most [`MAX_TICKS`] ticks wins.
const TICKS: [i64; 13] = [
    1_000, 2_000, 5_000, 10_000, 15_000, 30_000, 60_000, 120_000, 300_000, 600_000, 900_000,
    1_800_000, 3_600_000,
];
/// Ticks the axis shows at most, unless the operation ran for hours.
const MAX_TICKS: i64 = 6;

/// A step across snapshots of its timeline: its key, and how many steps with
/// the same key come before it, since an operation that tries again runs the
/// same step again as its own.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RowId {
    /// What the step does and on what.
    pub key: StepKey,
    /// Steps with the same key before it.
    pub occurrence: usize,
}

/// A stretch of the time axis, in hundredths of a percent of its length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    /// Where it starts.
    pub left: u32,
    /// How long it is.
    pub width: u32,
}

/// A position on the time axis, in hundredths of a percent, as CSS reads
/// it: `1234` is `12.34%`.
#[must_use]
pub fn percent(position: u32) -> String {
    format!("{}.{:02}%", position / 100, position % 100)
}

/// The time axis a waterfall's bars share: from the operation's start to a
/// whole number of ticks past its end, so it only grows a tick at a time.
/// Times are in milliseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Axis {
    origin: i64,
    length: i64,
    tick: i64,
}

impl Axis {
    /// An axis from `origin_ms` to at least `end_ms`.
    fn new(origin_ms: i64, end_ms: i64) -> Self {
        // Signed integers have no stable `next_multiple_of`.
        let round_up = |value: i64, step: i64| (value + step - 1) / step * step;
        let length = (end_ms - origin_ms).max(1);
        let tick = TICKS
            .into_iter()
            .find(|tick| length <= tick * MAX_TICKS)
            .unwrap_or_else(|| round_up(length / MAX_TICKS, 3_600_000));
        Self {
            origin: origin_ms,
            length: round_up(length, tick),
            tick,
        }
    }

    /// Where `at_ms` falls, clamped to the axis.
    #[must_use]
    pub fn position(&self, at_ms: i64) -> u32 {
        let offset = (at_ms - self.origin).clamp(0, self.length);
        u32::try_from(offset * 10_000 / self.length).unwrap_or(10_000)
    }

    /// The stretch from `start_ms` to `end_ms`.
    fn span(&self, start_ms: i64, end_ms: i64) -> Span {
        let left = self.position(start_ms);
        Span {
            left,
            width: self.position(end_ms).saturating_sub(left),
        }
    }

    /// The ticks after the origin: where each falls, and its label, short
    /// enough for ticks close together, such as "30s" or "1m 30s".
    pub fn ticks(&self) -> impl Iterator<Item = (u32, String)> + use<> {
        let axis = *self;
        (1..=axis.length / axis.tick).map(move |index| {
            let offset = index * axis.tick;
            let (hours, minutes, seconds) = (
                offset / 3_600_000,
                offset / 60_000 % 60,
                offset / 1_000 % 60,
            );
            let label = match (hours, minutes, seconds) {
                (1.., 0, _) => format!("{hours}h"),
                (1.., _, _) => format!("{hours}h {minutes}m"),
                (0, 1.., 0) => format!("{minutes}m"),
                (0, 1.., _) => format!("{minutes}m {seconds}s"),
                _ => format!("{seconds}s"),
            };
            (axis.position(axis.origin + offset), label)
        })
    }
}

/// One step of a waterfall, placed on its axis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    /// Which step it is.
    pub id: RowId,
    /// The step.
    pub step: Step,
    /// When it ran: from its start to its end, or to now while it runs.
    pub bar: Option<Span>,
    /// How long it usually takes, from its start, or for a step left, from
    /// when it should start.
    pub usual: Option<Span>,
    /// How long it ran, or has run so far; for a step left, how long it
    /// usually takes.
    pub duration_ms: Option<u64>,
    /// Whether it took, or is taking, notably longer than usual.
    pub slow: bool,
    /// When no task of a rolling-out service served requests.
    pub down: Vec<Span>,
    /// The stage it opens, when it is the first step of its stage.
    pub stage: Option<Stage>,
}

impl Row {
    /// Where the step stands, in a few words: its state, then how many
    /// requests it retried, or why it failed.
    #[must_use]
    pub fn summary(&self) -> String {
        let step = &self.step;
        let mut summary = step.state.as_str().to_owned();
        if let Some(diagnostic) = step
            .diagnostic
            .as_ref()
            .filter(|_| step.state == StepState::Failed)
        {
            summary = format!("{summary} · {}", diagnostic.code);
        } else if step.retries > 0 {
            let plural = if step.retries == 1 {
                "retry"
            } else {
                "retries"
            };
            summary = format!("{summary} · {} {plural}", step.retries);
        }
        summary
    }

    /// How much of its usual duration a running step has taken, in percent,
    /// at most 100; zero without an expectation.
    #[must_use]
    pub fn fill(&self) -> u64 {
        match (self.duration_ms, self.step.expected_ms) {
            (Some(duration), Some(expected)) if expected > 0 => {
                (duration * 100 / expected).min(100)
            }
            _ => 0,
        }
    }
}

/// An operation's steps laid out on one time axis.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Waterfall {
    /// The axis, once any step has a place in time.
    pub axis: Option<Axis>,
    /// The steps, in their timeline's order.
    pub rows: Vec<Row>,
    /// Where now is, while the operation runs.
    pub now: Option<u32>,
}

impl Waterfall {
    /// Lays `steps` out. While their operation runs, `now_ms` is the current
    /// time: running steps reach it, and each step left is placed when it
    /// should start, after the steps before it, as they usually take. Without
    /// it, steps left have no place in time. A clock behind the daemon's is
    /// taken to be at least at the newest time the steps record.
    #[must_use]
    pub fn new(steps: &[Step], now_ms: Option<i64>) -> Self {
        let latest = steps
            .iter()
            .flat_map(|step| [step.started_at_ms, step.finished_at_ms])
            .flatten()
            .max();
        let now = now_ms.map(|now| latest.map_or(now, |latest| now.max(latest)));
        let expected = |step: &Step| i64::try_from(step.expected_ms.unwrap_or(0)).unwrap_or(0);
        // When each step started or should start, and when it ended, or
        // should end.
        let mut cursor = now;
        let mut group = None;
        let times: Vec<_> = steps
            .iter()
            .map(|step| {
                if let Some(start) = step.started_at_ms {
                    let end = step.finished_at_ms.or(now).or(latest).unwrap_or(start);
                    if !step.state.finished() {
                        cursor = cursor.map(|cursor| cursor.max(start + expected(step)));
                    }
                    return Some((start, end));
                }
                let cursor = cursor.as_mut()?;
                let start = if step.key.phase.concurrent() {
                    *group.get_or_insert(*cursor)
                } else {
                    group = None;
                    *cursor
                };
                *cursor = (*cursor).max(start + expected(step));
                Some((start, start))
            })
            .collect();
        let origin = steps.iter().filter_map(|step| step.started_at_ms).min();
        let axis = origin.map(|origin| {
            let end = times
                .iter()
                .flatten()
                .zip(steps)
                .map(|(&(start, end), step)| end.max(start + expected(step)))
                .chain(now)
                .max()
                .unwrap_or(origin);
            Axis::new(origin, end)
        });
        let mut occurrences = HashMap::<&StepKey, usize>::new();
        let mut stage = None;
        let rows = steps
            .iter()
            .zip(times)
            .map(|(step, time)| {
                let occurrence = occurrences.entry(&step.key).or_default();
                let id = RowId {
                    key: step.key.clone(),
                    occurrence: *occurrence,
                };
                *occurrence += 1;
                let opens = step.key.phase.stage();
                let started = step.state.started();
                let duration_ms = if started {
                    time.map(|(start, end)| u64::try_from(end - start).unwrap_or(0))
                } else {
                    step.expected_ms
                };
                let slow = started
                    && matches!(
                        (duration_ms, step.expected_ms),
                        (Some(duration), Some(usual)) if duration > usual + (usual / 4).max(1_000)
                    );
                Row {
                    id,
                    bar: axis
                        .zip(time)
                        .filter(|_| started)
                        .map(|(axis, (start, end))| axis.span(start, end)),
                    usual: axis
                        .zip(time)
                        .filter(|_| step.expected_ms.is_some())
                        .map(|(axis, (start, _))| axis.span(start, start + expected(step))),
                    down: axis
                        .zip(time)
                        .map(|(axis, (_, end))| {
                            step.gaps()
                                .into_iter()
                                .map(|gap| axis.span(gap.from_ms, gap.until(end)))
                                .collect()
                        })
                        .unwrap_or_default(),
                    duration_ms,
                    slow,
                    stage: (stage.replace(opens) != Some(opens)).then_some(opens),
                    step: step.clone(),
                }
            })
            .collect();
        Self {
            now: axis.zip(now).map(|(axis, now)| axis.position(now)),
            axis,
            rows,
        }
    }

    /// The row of `id`.
    #[must_use]
    pub fn row(&self, id: &RowId) -> Option<&Row> {
        self.rows.iter().find(|row| row.id == *id)
    }

    /// The step the operation is at: the first running, else the last
    /// started.
    #[must_use]
    pub fn current(&self) -> Option<&RowId> {
        self.rows
            .iter()
            .find(|row| row.step.state.running())
            .or_else(|| self.rows.iter().rfind(|row| row.step.state.started()))
            .map(|row| &row.id)
    }
}

/// How a Swarm task looks over a stretch of its lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskLook {
    /// Waiting to start: scheduled, pulling its image, preparing.
    Preparing,
    /// Its container started, waiting on its health check.
    Starting,
    /// Serving requests.
    Running,
    /// Serving requests while Swarm watches it for failure.
    Monitor,
    /// Running, but its health check fails.
    Unhealthy,
    /// Shut down or completed.
    Stopped,
    /// Failed or rejected.
    Failed,
}

impl TaskLook {
    /// How a task in `span` looks.
    fn of(span: TaskSpan) -> Self {
        match span.state {
            TaskState::Starting => Self::Starting,
            TaskState::Running if span.serving() => Self::Running,
            TaskState::Running => Self::Unhealthy,
            TaskState::Complete | TaskState::Shutdown => Self::Stopped,
            TaskState::Failed | TaskState::Rejected => Self::Failed,
            TaskState::New
            | TaskState::Pending
            | TaskState::Assigned
            | TaskState::Accepted
            | TaskState::Preparing
            | TaskState::Unknown => Self::Preparing,
        }
    }

    /// Its name, which CSS colors it by and its stretch shows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Monitor => "monitor",
            Self::Unhealthy => "unhealthy",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// A stretch of a task's lane, in hundredths of a percent of its axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskStretch {
    /// How the task looks over it.
    pub look: TaskLook,
    /// Where it starts.
    pub left: u32,
    /// How long it is; `None` while it lasts, up to now.
    pub width: Option<u32>,
}

/// One Swarm task of a rollout, as one lane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lane {
    /// The task as people tell it apart: its service and slot, such as
    /// "web.1".
    pub name: String,
    /// Whether it runs the new specification.
    pub current: bool,
    /// Its states in order.
    pub stretches: Vec<TaskStretch>,
}

/// A rollout's Swarm tasks on their step's own time axis, a lane each, old
/// and new tasks of a slot together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lanes {
    /// From when the step started to its end, or past now while it runs.
    pub axis: Axis,
    /// The lanes.
    pub lanes: Vec<Lane>,
}

impl Lanes {
    /// Lays `step`'s tasks out at `now_ms`; `None` when it recorded none.
    /// While the step runs, each task's last state lasts up to now, which
    /// the layout leaves open so it changes only with the step, its axis,
    /// or a monitor window closing.
    #[must_use]
    pub fn new(step: &Step, now_ms: i64) -> Option<Self> {
        let spans = step.tasks.iter().flat_map(|lane| &lane.spans);
        let first = spans.clone().map(|span| span.from_ms).min()?;
        let latest = spans.map(|span| span.from_ms).max().unwrap_or(first);
        let origin = step.started_at_ms.unwrap_or(first).min(first);
        let now_ms = now_ms.max(latest);
        let end = step.finished_at_ms;
        let axis = Axis::new(origin, end.unwrap_or(now_ms));
        let service = step.key.resource.as_deref().unwrap_or("task");
        let mut tasks: Vec<_> = step.tasks.iter().collect();
        tasks.sort_by_key(|lane| (lane.task.slot, lane.task.current));
        let lanes = tasks
            .into_iter()
            .map(|lane| Lane {
                name: match lane.task.slot {
                    Some(slot) => format!("{service}.{slot}"),
                    None => lane.task.id.chars().take(12).collect(),
                },
                current: lane.task.current,
                stretches: Self::stretches(lane, step.monitor_seconds, axis, end, now_ms),
            })
            .collect();
        Some(Self { axis, lanes })
    }

    /// `lane`'s stretches on `axis`: a new task's first serving stretch
    /// splits at the end of its monitor window. The last lasts to `end`,
    /// or stays open while the step runs; a task that is over ends where it
    /// stopped.
    fn stretches(
        lane: &TaskLane,
        monitor_seconds: Option<u32>,
        axis: Axis,
        end: Option<i64>,
        now_ms: i64,
    ) -> Vec<TaskStretch> {
        let window = monitor_seconds.and_then(|seconds| lane.monitor(seconds));
        // Each look, from when, and until when if it ended; changes the
        // lane doesn't show, such as a first health verdict, merge.
        let mut times: Vec<(TaskLook, i64, Option<i64>)> = Vec::new();
        for (index, span) in lane.spans.iter().enumerate() {
            let look = TaskLook::of(*span);
            let until = if span.state.ended() {
                Some(span.from_ms)
            } else {
                lane.spans.get(index + 1).map(|next| next.from_ms).or(end)
            };
            match times.last_mut() {
                Some(last) if last.0 == look => last.2 = until,
                _ => times.push((look, span.from_ms, until)),
            }
        }
        // Watched from when it first served until the window closed, then
        // running.
        let watched = window.and_then(|window| {
            let index = times
                .iter()
                .position(|&(look, from, _)| look == TaskLook::Running && from == window.from_ms)?;
            Some((window, index))
        });
        if let Some((window, index)) = watched {
            let until = times[index].2;
            times[index].0 = TaskLook::Monitor;
            if until.unwrap_or(now_ms) >= window.to_ms {
                times[index].2 = Some(window.to_ms);
                times.insert(index + 1, (TaskLook::Running, window.to_ms, until));
            }
        }
        times
            .into_iter()
            .map(|(look, from, until)| {
                let left = axis.position(from);
                TaskStretch {
                    look,
                    left,
                    width: until.map(|until| axis.position(until).saturating_sub(left)),
                }
            })
            .collect()
    }
}

/// How far an operation has come, for its header and tab title.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Progress {
    kind: OperationKind,
    /// Where the operation stands.
    pub state: OperationState,
    /// Steps finished.
    pub done: usize,
    /// Steps altogether, done and left.
    pub total: usize,
    /// The steps running, such as "Build web".
    pub running: Vec<String>,
    /// How long it has run, or ran.
    pub elapsed_ms: u64,
    /// How much longer it should take, while it runs.
    pub left_ms: Option<u64>,
    /// How long it usually takes.
    pub usual_ms: Option<u64>,
}

impl Progress {
    /// Where `timeline` stands at `now_ms`.
    #[must_use]
    pub fn new(timeline: &Timeline, now_ms: i64) -> Self {
        let operation = &timeline.operation;
        let started = timeline
            .steps
            .iter()
            .filter_map(|step| step.started_at_ms)
            .chain(operation.started_at_ms)
            .min();
        let end = operation.finished_at_ms.unwrap_or(now_ms);
        Self {
            kind: operation.kind,
            state: operation.state,
            done: timeline
                .steps
                .iter()
                .filter(|step| step.state.finished())
                .count(),
            total: timeline.steps.len(),
            running: timeline
                .steps
                .iter()
                .filter(|step| step.state.running())
                .map(|step| step.key.to_string())
                .collect(),
            elapsed_ms: started.map_or(0, |started| u64::try_from(end - started).unwrap_or(0)),
            left_ms: timeline.remaining_ms(now_ms),
            usual_ms: timeline.usual_ms(),
        }
    }

    /// Steps done, in percent.
    #[must_use]
    pub fn percent(&self) -> usize {
        (self.done * 100).checked_div(self.total).unwrap_or(0)
    }

    /// Where the operation stands, in a word: "Deploying" or "Deployed".
    #[must_use]
    pub const fn status(&self) -> &'static str {
        let delete = matches!(self.kind, OperationKind::Delete);
        match self.state {
            OperationState::Requested => "Queued",
            OperationState::Running if delete => "Deleting",
            OperationState::Running => "Deploying",
            OperationState::Succeeded if delete => "Deleted",
            OperationState::Succeeded => "Deployed",
            OperationState::Failed => "Failed",
            OperationState::Cancelled => "Cancelled",
            OperationState::Superseded => "Superseded",
        }
    }

    /// The browser tab's title while showing the operation of `subject`:
    /// its progress while it runs, then its outcome.
    #[must_use]
    pub fn title(&self, subject: &str) -> String {
        let status = self.status();
        match self.state {
            OperationState::Requested | OperationState::Running => {
                format!("{}% · {status} {subject}", self.percent())
            }
            OperationState::Succeeded => format!("✓ {status} {subject}"),
            OperationState::Failed => format!("✗ {status} · {subject}"),
            OperationState::Cancelled | OperationState::Superseded => {
                format!("{status} · {subject}")
            }
        }
    }

    /// Steps done of all, such as "4/9".
    #[must_use]
    pub fn steps(&self) -> String {
        format!("{}/{}", self.done, self.total)
    }

    /// How long a running operation should still take, such as "~48.0 s
    /// left", or how a finished one compared with its usual duration.
    #[must_use]
    pub fn outlook(&self) -> Option<String> {
        self.against_usual().or_else(|| {
            let left = self.left_ms.filter(|_| !self.state.terminal())?;
            Some(format!(
                "~{} left",
                Elapsed(std::time::Duration::from_millis(left))
            ))
        })
    }

    /// How a finished operation compared with its usual duration, such as
    /// "6.0 s faster than usual".
    fn against_usual(&self) -> Option<String> {
        let usual = self.usual_ms.filter(|_| self.state.terminal())?;
        let difference = self.elapsed_ms.abs_diff(usual);
        Some(if difference < 1_000 {
            "as long as usual".to_owned()
        } else {
            let faster = if self.elapsed_ms < usual {
                "faster"
            } else {
                "slower"
            };
            format!(
                "{} {faster} than usual",
                Elapsed(std::time::Duration::from_millis(difference))
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_client::Phase;

    fn step(phase: Phase, resource: &str, state: StepState) -> Step {
        let key = StepKey {
            phase,
            resource: Some(resource.into()),
        };
        Step::pending(key, state, None)
    }

    /// A step that ran from `start` to `end` (seconds), usually taking
    /// `usual` seconds.
    fn ran(phase: Phase, resource: &str, start: i64, end: Option<i64>, usual: u64) -> Step {
        let state = if end.is_some() {
            StepState::Succeeded
        } else {
            StepState::Running
        };
        Step {
            started_at_ms: Some(start * 1_000),
            finished_at_ms: end.map(|end| end * 1_000),
            expected_ms: Some(usual * 1_000),
            ..step(phase, resource, state)
        }
    }

    fn left(phase: Phase, resource: &str, usual: u64) -> Step {
        Step {
            expected_ms: Some(usual * 1_000),
            ..step(phase, resource, StepState::Planned)
        }
    }

    #[test]
    fn steps_left_are_placed_after_those_running_as_they_usually_take() {
        let steps = [
            // Sources run at once; web is over its usual 20 s.
            ran(Phase::BuildGit, "api", 0, Some(10), 12),
            ran(Phase::BuildGit, "web", 0, None, 20),
            left(Phase::RunJob, "migrate", 5),
            left(Phase::EnsureService, "api", 10),
        ];
        let waterfall = Waterfall::new(&steps, Some(30_000));
        // 45 s of work fit a 50 s axis ticking every 10 s.
        let axis = waterfall.axis.unwrap();
        assert_eq!(
            axis.ticks().map(|(_, label)| label).collect::<Vec<_>>(),
            ["10s", "20s", "30s", "40s", "50s"]
        );
        let spans: Vec<_> = waterfall
            .rows
            .iter()
            .map(|row| (row.bar, row.usual, row.slow))
            .collect();
        let span = |left, width| Some(Span { left, width });
        assert_eq!(
            spans,
            [
                (span(0, 2000), span(0, 2400), false),
                // Running up to now, past its usual duration.
                (span(0, 6000), span(0, 4000), true),
                // The job starts now, the service after it.
                (None, span(6000, 1000), false),
                (None, span(7000, 2000), false),
            ]
        );
        assert_eq!(waterfall.now, Some(6000));
        assert_eq!(waterfall.current(), Some(&waterfall.rows[1].id));
        // Each stage opens once.
        let stages: Vec<_> = waterfall.rows.iter().map(|row| row.stage).collect();
        assert_eq!(
            stages,
            [
                Some(Stage::Prepare),
                None,
                Some(Stage::Jobs),
                Some(Stage::Rollout)
            ]
        );
    }

    #[test]
    fn a_step_run_again_is_told_apart_and_a_plan_has_no_place_in_time() {
        let failed = Step {
            state: StepState::Failed,
            ..ran(Phase::EnsureService, "web", 0, Some(1), 1)
        };
        let again = ran(Phase::EnsureService, "web", 2, None, 1);
        let waterfall = Waterfall::new(&[failed, again], None);
        let occurrences: Vec<_> = waterfall.rows.iter().map(|row| row.id.occurrence).collect();
        assert_eq!(occurrences, [0, 1]);

        let plan = Waterfall::new(&[left(Phase::EnsureService, "web", 3)], None);
        assert_eq!(plan.axis, None);
        assert_eq!((plan.rows[0].bar, plan.rows[0].usual), (None, None));
    }

    #[test]
    fn progress_tells_the_time_left_then_against_the_usual() {
        let mut progress = Progress {
            kind: OperationKind::Apply,
            state: OperationState::Running,
            done: 4,
            total: 9,
            running: vec!["Build web".into()],
            elapsed_ms: 30_000,
            left_ms: Some(48_000),
            usual_ms: Some(80_000),
        };
        assert_eq!(progress.steps(), "4/9");
        assert_eq!(progress.outlook().as_deref(), Some("~48.0 s left"));
        progress.state = OperationState::Succeeded;
        progress.elapsed_ms = 74_000;
        assert_eq!(
            progress.outlook().as_deref(),
            Some("6.0 s faster than usual")
        );
    }

    #[test]
    fn rollout_lanes_leave_the_last_state_open_and_mark_the_monitor_window() {
        use piqueld_client::timeline::{RolloutTask, TaskLane};
        let lane = |id: &str, current: bool, spans: &[(TaskState, i64)]| TaskLane {
            task: RolloutTask {
                id: id.into(),
                slot: Some(1),
                current,
                state: spans.last().unwrap().0,
                healthy: None,
            },
            spans: spans
                .iter()
                .map(|&(state, from_ms)| TaskSpan {
                    state,
                    healthy: None,
                    from_ms,
                })
                .collect(),
        };
        // Stop-first: the old task stops at 2 s, the new one runs at 4 s.
        let step = Step {
            tasks: vec![
                lane(
                    "new",
                    true,
                    &[(TaskState::Preparing, 2_000), (TaskState::Running, 4_000)],
                ),
                lane(
                    "old",
                    false,
                    // Its health verdict at 1 s changes nothing the lane shows.
                    &[
                        (TaskState::Running, 0),
                        (TaskState::Running, 1_000),
                        (TaskState::Shutdown, 2_000),
                    ],
                ),
            ],
            monitor_seconds: Some(5),
            ..ran(Phase::WaitForService, "web", 0, None, 10)
        };
        let looks = |now_ms| {
            Lanes::new(&step, now_ms)
                .unwrap()
                .lanes
                .into_iter()
                .map(|lane| {
                    let stretches = lane.stretches.iter().map(|s| (s.look, s.left, s.width));
                    (lane.name, lane.current, stretches.collect::<Vec<_>>())
                })
                .collect::<Vec<_>>()
        };
        // At 6 s, on a 6 s axis, the new task is in its monitor window.
        assert_eq!(
            looks(6_000),
            [
                (
                    "web.1".into(),
                    false,
                    vec![
                        (TaskLook::Running, 0, Some(3333)),
                        (TaskLook::Stopped, 3333, Some(0)),
                    ]
                ),
                (
                    "web.1".into(),
                    true,
                    vec![
                        (TaskLook::Preparing, 3333, Some(3333)),
                        (TaskLook::Monitor, 6666, None),
                    ]
                ),
            ]
        );
        // At 10 s, on a 10 s axis, the window closed at 9 s.
        assert_eq!(
            looks(10_000)[1].2,
            [
                (TaskLook::Preparing, 2000, Some(2000)),
                (TaskLook::Monitor, 4000, Some(5000)),
                (TaskLook::Running, 9000, None),
            ]
        );
        // No task served from 2 s to 4 s.
        let waterfall = Waterfall::new(std::slice::from_ref(&step), Some(10_000));
        assert_eq!(
            waterfall.rows[0].down,
            [Span {
                left: 2000,
                width: 2000
            }]
        );
    }

    #[test]
    fn hours_long_operations_tick_in_whole_hours() {
        let axis = Axis::new(0, 9 * 3_600_000);
        assert_eq!(axis.ticks().count(), 5);
        assert_eq!(axis.ticks().last().unwrap(), (10_000, "10h".to_owned()));
    }
}
