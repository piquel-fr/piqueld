//! An operation's progress as one ordered list of steps: what it did, what it
//! is doing, and what is left.
//!
//! The event journal is the only record. A [`Timeline`] is never stored: the
//! daemon folds an operation's events into one with [`Timeline::apply`], and
//! keeps applying new events to stream it live. Steps not yet started come
//! from the operation's latest recorded plan ([`EventDetail::Planned`]), or,
//! before the operation planned that far, from its environment's previous
//! successful operation ([`StepState::Forecast`]).

use crate::observability::Diagnostic;
use crate::{Event, EventDetail, Operation, OperationState, Phase, names::string_enum};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What a step does and on what: a journaled action's identity.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, ToSchema)]
pub struct StepKey {
    /// What the step does.
    pub phase: Phase,
    /// The logical or Docker resource it acts on, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// The step as people read it: its phase's label, then its resource, such
/// as "Build web".
impl std::fmt::Display for StepKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.phase.label())?;
        match &self.resource {
            Some(resource) => write!(formatter, " {resource}"),
            None => Ok(()),
        }
    }
}

string_enum! {
    #[serde]
    /// How far an operation has stepped through its work, in the order the
    /// steps of a deployment run.
    pub enum Stage {
        /// Fetching the manifest, building and resolving sources, planning.
        Prepare => "prepare",
        /// Before-rollout jobs and the cleanup of earlier runs.
        Jobs => "jobs",
        /// Creating, updating, waiting for and removing runtime resources.
        Rollout => "rollout",
        /// Shared infrastructure outside any one environment.
        Daemon => "daemon",
    }
}

impl Stage {
    /// The stage as a heading, such as "Roll out".
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Prepare => "Prepare",
            Self::Jobs => "Jobs",
            Self::Rollout => "Roll out",
            Self::Daemon => "Daemon",
        }
    }
}

impl Phase {
    /// What a step of this phase does, as people read it: "Build" for
    /// `building_git`. Followed by the step's resource, it names the step.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preparing => "Prepare",
            Self::FetchManifest => "Fetch manifest",
            Self::BuildGit => "Build",
            Self::ResolveImage => "Resolve image",
            Self::Planning => "Plan",
            Self::RunJob => "Run job",
            Self::RemoveJobs => "Remove earlier jobs",
            Self::EnsureNetwork => "Ensure network",
            Self::EnsureVolume => "Ensure volume",
            Self::EnsureService => "Update service",
            Self::WaitForService => "Wait for service",
            Self::WaitForServiceRemoval => "Wait for service removal",
            Self::RemoveService => "Remove service",
            Self::RemoveNetwork => "Remove network",
            Self::RetainVolume => "Keep volume",
            Self::DetachNetwork => "Detach network",
            Self::RemoveVolumes => "Remove volumes",
            Self::RemoveSecrets => "Remove secrets",
            Self::Routing => "Apply routes",
            Self::Observation => "Observe",
            Self::EnsureSwarm => "Ensure Swarm",
            Self::RemoveImage => "Remove image",
            Self::IngressCreateNetwork => "Create ingress network",
            Self::IngressStartGateway => "Start gateway",
            Self::IngressStopGateway => "Stop gateway",
            Self::IngressRemoveContainer => "Remove ingress container",
            Self::IngressPullImage => "Pull gateway image",
            Self::IngressReplaceGateway => "Replace gateway",
            Self::IngressStartTunnel => "Start tunnel",
            Self::IngressStartTailnet => "Start tailnet node",
            Self::IngressRecoverGateway => "Recover gateway",
            Self::IngressConfigureRoutes => "Configure gateway routes",
            Self::IngressDnsCreate => "Create DNS records",
            Self::IngressDnsUpdate => "Update DNS records",
            Self::IngressDnsDelete => "Delete DNS records",
            Self::IngressCertificateIssue => "Issue certificate",
            Self::IngressCertificateRenew => "Renew certificate",
            Self::IngressCertificateDelete => "Delete certificate",
            Self::IngressCertificateChallengeDelete => "Delete certificate challenge",
        }
    }

    /// The stage of an operation this phase belongs to.
    #[must_use]
    pub const fn stage(self) -> Stage {
        match self {
            Self::Preparing
            | Self::FetchManifest
            | Self::BuildGit
            | Self::ResolveImage
            | Self::Planning => Stage::Prepare,
            Self::RunJob | Self::RemoveJobs => Stage::Jobs,
            Self::EnsureNetwork
            | Self::EnsureVolume
            | Self::EnsureService
            | Self::WaitForService
            | Self::WaitForServiceRemoval
            | Self::RemoveService
            | Self::RemoveNetwork
            | Self::RetainVolume
            | Self::DetachNetwork
            | Self::RemoveVolumes
            | Self::RemoveSecrets
            | Self::Routing
            | Self::Observation => Stage::Rollout,
            Self::EnsureSwarm
            | Self::RemoveImage
            | Self::IngressCreateNetwork
            | Self::IngressStartGateway
            | Self::IngressStopGateway
            | Self::IngressRemoveContainer
            | Self::IngressPullImage
            | Self::IngressReplaceGateway
            | Self::IngressStartTunnel
            | Self::IngressStartTailnet
            | Self::IngressRecoverGateway
            | Self::IngressConfigureRoutes
            | Self::IngressDnsCreate
            | Self::IngressDnsUpdate
            | Self::IngressDnsDelete
            | Self::IngressCertificateIssue
            | Self::IngressCertificateRenew
            | Self::IngressCertificateDelete
            | Self::IngressCertificateChallengeDelete => Stage::Daemon,
        }
    }

    /// Whether steps of this phase run concurrently with each other:
    /// sources are prepared several at a time.
    #[must_use]
    pub const fn concurrent(self) -> bool {
        matches!(self, Self::BuildGit | Self::ResolveImage)
    }
}

impl crate::manifest::ValidatedSource {
    /// The phase preparing this source is journaled as.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        if matches!(self, Self::Git { .. }) {
            Phase::BuildGit
        } else {
            Phase::ResolveImage
        }
    }
}

string_enum! {
    #[serde]
    /// Where one step stands.
    pub enum StepState {
        /// Expected from the environment's previous successful operation;
        /// the operation has not planned that far yet.
        Forecast => "forecast",
        /// In the operation's latest plan, not started yet.
        Planned => "planned",
        /// Running.
        Running => "running",
        /// Failed a request and waiting to retry it.
        Retrying => "retrying",
        /// Finished successfully.
        Succeeded => "succeeded",
        /// Failed.
        Failed => "failed",
        /// Interrupted before its outcome was recorded, e.g. by a restart.
        Interrupted => "interrupted",
    }
}

impl StepState {
    /// Whether the step has started, so it has a place in time.
    #[must_use]
    pub const fn started(self) -> bool {
        !matches!(self, Self::Forecast | Self::Planned)
    }

    /// Whether the step has ended.
    #[must_use]
    pub const fn finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }

    /// Whether the step started and has not ended: running or retrying.
    #[must_use]
    pub const fn running(self) -> bool {
        matches!(self, Self::Running | Self::Retrying)
    }
}

/// One step of an operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Step {
    /// What it does and on what.
    #[serde(flatten)]
    pub key: StepKey,
    /// Where it stands.
    pub state: StepState,
    /// The journaled action running it, once started; filter events by it
    /// for its full history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    /// The operation attempt that ran it, once started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    /// When it started, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    /// When it ended, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
    /// Requests retried after a retryable failure.
    #[serde(default)]
    pub retries: u64,
    /// How long it usually takes: the median of the same step in the
    /// environment's recent successful operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_ms: Option<u64>,
    /// Why it failed, or why its latest request did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
}

impl Step {
    /// A step not started yet, expected to take `expected_ms`.
    #[must_use]
    pub const fn pending(key: StepKey, state: StepState, expected_ms: Option<u64>) -> Self {
        Self {
            key,
            state,
            action_id: None,
            attempt: None,
            started_at_ms: None,
            finished_at_ms: None,
            retries: 0,
            expected_ms,
            diagnostic: None,
        }
    }

    /// How much longer it should take at `now_ms`: its expected duration
    /// less the time it has run, at least zero; zero once it ended, and
    /// `None` without an expectation.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: i64) -> Option<u64> {
        if self.state.finished() {
            return Some(0);
        }
        let expected = self.expected_ms?;
        Some(expected.saturating_sub(self.duration_ms(now_ms).unwrap_or(0)))
    }

    /// How long it ran, or has been running at `now_ms`; `None` before it
    /// started.
    #[must_use]
    pub fn duration_ms(&self, now_ms: i64) -> Option<u64> {
        let started = self.started_at_ms?;
        let ended = self.finished_at_ms.unwrap_or(now_ms);
        Some(u64::try_from(ended - started).unwrap_or(0))
    }

    /// How long it usually takes, when it succeeded in noticeably more or
    /// less time than that: half of it, and a second at least.
    #[must_use]
    pub fn unusual_ms(&self) -> Option<u64> {
        if self.state != StepState::Succeeded {
            return None;
        }
        let expected = self.expected_ms?;
        let took = self.duration_ms(self.finished_at_ms?)?;
        (took.abs_diff(expected) >= (expected / 2).max(1_000)).then_some(expected)
    }
}

/// How long a step usually takes, from an environment's recent successful
/// operations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ExpectedStep {
    /// The step.
    #[serde(flatten)]
    pub key: StepKey,
    /// Its median duration in milliseconds.
    pub duration_ms: u64,
}

/// An operation's steps, folded from its events.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Timeline {
    /// The operation, as of the newest event applied.
    pub operation: Operation,
    /// Its steps: those started, in the order they started, then those left,
    /// in the order they are expected to run.
    pub steps: Vec<Step>,
    /// When the operation's target was promoted, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_at_ms: Option<i64>,
    /// The newest event applied: the stream resumes after it.
    pub cursor: i64,
    /// Expected durations, which plans and started steps look up.
    #[serde(skip)]
    expected: Vec<ExpectedStep>,
}

impl Timeline {
    /// An operation's timeline before any of its events: `forecast`, the
    /// steps of the environment's previous successful operation in their
    /// order, with how long each step usually takes from `expected`.
    #[must_use]
    pub fn new(operation: Operation, forecast: Vec<StepKey>, expected: Vec<ExpectedStep>) -> Self {
        let mut timeline = Self {
            operation,
            steps: Vec::new(),
            promoted_at_ms: None,
            cursor: 0,
            expected,
        };
        timeline.steps = forecast
            .into_iter()
            .map(|key| {
                let expected = timeline.expected_ms(&key);
                Step::pending(key, StepState::Forecast, expected)
            })
            .collect();
        timeline
    }

    /// Applies one of the operation's events, newer than [`Self::cursor`];
    /// older events, and those of other operations, are ignored.
    pub fn apply(&mut self, event: &Event) {
        if event.id <= self.cursor || event.operation_id.as_deref() != Some(&self.operation.id) {
            return;
        }
        self.cursor = event.id;
        if event
            .attempt
            .is_some_and(|attempt| !self.current_attempt(attempt))
        {
            return;
        }
        if let Some(EventDetail::Planned { steps, coverage }) = &event.detail {
            self.plan(steps, *coverage);
            return;
        }
        match event.kind.as_str() {
            "target_promoted" => self.promoted_at_ms = Some(event.created_at_ms),
            "action_started" => self.start(event),
            kind => {
                let Some(step) = self.action(event) else {
                    return;
                };
                match kind {
                    "action_requested" => {
                        step.retries = event.retry.unwrap_or(1).saturating_sub(1);
                        step.state = StepState::Running;
                    }
                    "action_retry" => {
                        step.state = StepState::Retrying;
                        step.diagnostic.clone_from(&event.diagnostic);
                    }
                    "action_succeeded" | "action_failed" | "action_outcome_unknown" => {
                        step.state = match kind {
                            "action_succeeded" => StepState::Succeeded,
                            "action_failed" => StepState::Failed,
                            _ => StepState::Interrupted,
                        };
                        step.finished_at_ms = Some(event.created_at_ms);
                        step.diagnostic.clone_from(&event.diagnostic);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Updates the operation as read after its events changed it. Once it is
    /// requested or runs again, the steps earlier attempts started are
    /// dropped.
    pub fn set_operation(&mut self, operation: Operation) {
        self.operation = operation;
        let steps = std::mem::take(&mut self.steps);
        self.steps = steps
            .into_iter()
            .filter(|step| {
                step.attempt
                    .is_none_or(|attempt| self.current_attempt(attempt))
            })
            .collect();
    }

    /// Whether `attempt` is the operation's current run. A timeline shows
    /// only that: running an operation again, e.g. to start a stopped
    /// environment, starts over, and while it is requested again its latest
    /// attempt is the run before.
    fn current_attempt(&self, attempt: u64) -> bool {
        attempt > self.operation.attempt
            || (attempt == self.operation.attempt
                && self.operation.state != OperationState::Requested)
    }

    /// Whether the operation ended and none of its steps still runs, so the
    /// timeline stays as it is unless the operation is requested again.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.operation.state.terminal() && !self.steps.iter().any(|step| step.state.running())
    }

    /// How much longer the operation should take at `now_ms`: the steps left
    /// run one after another, except sources, which are prepared at once.
    /// `None` once it ended, or when no step left has an expectation.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: i64) -> Option<u64> {
        if self.operation.state.terminal() {
            return None;
        }
        Self::chain(
            self.steps
                .iter()
                .filter(|step| !step.state.finished())
                .map(|step| (step.key.phase, step.remaining_ms(now_ms))),
        )
    }

    /// How long the operation's steps usually take altogether, run as
    /// [`Self::remaining_ms`] runs them. `None` when no step has an
    /// expectation.
    #[must_use]
    pub fn usual_ms(&self) -> Option<u64> {
        Self::chain(
            self.steps
                .iter()
                .map(|step| (step.key.phase, step.expected_ms)),
        )
    }

    /// How long steps of the given phases and durations take: one after
    /// another, except sources, which are prepared at once. Steps without a
    /// duration are skipped; `None` when none has one.
    fn chain(steps: impl Iterator<Item = (Phase, Option<u64>)>) -> Option<u64> {
        let mut known = false;
        let mut sequential = 0u64;
        let mut concurrent = 0u64;
        for (phase, duration) in steps {
            let Some(duration) = duration else {
                continue;
            };
            known = true;
            if phase.concurrent() {
                concurrent = concurrent.max(duration);
            } else {
                sequential += duration;
            }
        }
        known.then_some(sequential + concurrent)
    }

    /// The step an event of a started action belongs to.
    fn action(&mut self, event: &Event) -> Option<&mut Step> {
        let action = event.action_id.as_deref()?;
        self.steps
            .iter_mut()
            .rev()
            .find(|step| step.action_id.as_deref() == Some(action))
    }

    /// Starts the step `event` names: the first one left with its key, or a
    /// new one after those started.
    fn start(&mut self, event: &Event) {
        let Some(phase) = event.phase else {
            return;
        };
        let key = StepKey {
            phase,
            resource: event.resource.clone(),
        };
        let index = self
            .steps
            .iter()
            .position(|step| !step.state.started() && step.key == key)
            .unwrap_or_else(|| {
                let expected = self.expected_ms(&key);
                self.steps
                    .push(Step::pending(key, StepState::Planned, expected));
                self.steps.len() - 1
            });
        let step = &mut self.steps[index];
        step.state = StepState::Running;
        step.action_id.clone_from(&event.action_id);
        step.attempt = event.attempt;
        step.started_at_ms = Some(event.created_at_ms);
        self.order();
    }

    /// Replaces the steps left that `coverage` plans with `planned`.
    fn plan(&mut self, planned: &[StepKey], coverage: PlanCoverage) {
        self.steps
            .retain(|step| step.state.started() || !coverage.covers(step.key.phase.stage()));
        for key in planned {
            let running = self
                .steps
                .iter()
                .any(|step| step.key == *key && step.state.running());
            if !running {
                let expected = self.expected_ms(key);
                self.steps
                    .push(Step::pending(key.clone(), StepState::Planned, expected));
            }
        }
        self.order();
    }

    /// Keeps the started steps first, in the order they started, then the
    /// steps left by stage, each stage's in the order it was planned.
    fn order(&mut self) {
        self.steps.sort_by_key(|step| {
            if step.state.started() {
                (false, None)
            } else {
                (true, Some(step.key.phase.stage() as u8))
            }
        });
    }

    /// How long `key` usually takes.
    fn expected_ms(&self, key: &StepKey) -> Option<u64> {
        self.expected
            .iter()
            .find(|step| step.key == *key)
            .map(|step| step.duration_ms)
    }
}

string_enum! {
    #[serde]
    /// Which of an operation's steps a recorded plan lists.
    pub enum PlanCoverage {
        /// Preparing sources and running jobs, before the runtime is planned.
        Preparation => "preparation",
        /// Everything left: preparation is done by the time the runtime is
        /// planned.
        Runtime => "runtime",
    }
}

impl PlanCoverage {
    /// Whether a plan of this coverage replaces the steps left of `stage`.
    const fn covers(self, stage: Stage) -> bool {
        match self {
            Self::Preparation => matches!(stage, Stage::Prepare | Stage::Jobs),
            Self::Runtime => !matches!(stage, Stage::Daemon),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EnvironmentId, OperationKind};

    fn key(phase: Phase, resource: &str) -> StepKey {
        StepKey {
            phase,
            resource: Some(resource.into()),
        }
    }

    fn timeline(expected: &[(Phase, &str, u64)]) -> Timeline {
        let operation = Operation {
            id: "op".into(),
            environment_id: EnvironmentId::parse("env-web-01").unwrap(),
            kind: OperationKind::Refresh,
            generation: 1,
            attempt: 1,
            consecutive_failures: 0,
            state: OperationState::Running,
            phase: None,
            resource: None,
            error_code: None,
            error_message: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            started_at_ms: Some(0),
            finished_at_ms: None,
        };
        let expected: Vec<_> = expected
            .iter()
            .map(|(phase, resource, duration_ms)| ExpectedStep {
                key: key(*phase, resource),
                duration_ms: *duration_ms,
            })
            .collect();
        let forecast = expected.iter().map(|step| step.key.clone()).collect();
        Timeline::new(operation, forecast, expected)
    }

    /// Applies events numbered from the timeline's cursor.
    fn apply(timeline: &mut Timeline, events: Vec<Event>) {
        for (offset, mut event) in events.into_iter().enumerate() {
            event.id = timeline.cursor + 1 + i64::try_from(offset).unwrap();
            event.operation_id = Some("op".into());
            event.attempt = Some(1);
            timeline.apply(&event);
        }
    }

    fn action(kind: &str, id: &str, phase: Phase, resource: &str, at: i64) -> Event {
        Event {
            kind: kind.into(),
            action_id: Some(id.into()),
            phase: Some(phase),
            resource: Some(resource.into()),
            created_at_ms: at,
            ..Event::default()
        }
    }

    fn planned(coverage: PlanCoverage, steps: &[StepKey]) -> Event {
        Event {
            kind: "plan_recorded".into(),
            detail: Some(EventDetail::Planned {
                steps: steps.to_vec(),
                coverage,
            }),
            ..Event::default()
        }
    }

    fn states(timeline: &Timeline) -> Vec<(Phase, StepState)> {
        timeline
            .steps
            .iter()
            .map(|step| (step.key.phase, step.state))
            .collect()
    }

    #[test]
    fn plans_replace_the_forecast_and_actions_fill_them_in() {
        let mut timeline = timeline(&[
            (Phase::BuildGit, "web", 30_000),
            (Phase::EnsureService, "web", 2_000),
            (Phase::WaitForService, "web", 10_000),
        ]);
        assert_eq!(
            states(&timeline),
            [
                (Phase::BuildGit, StepState::Forecast),
                (Phase::EnsureService, StepState::Forecast),
                (Phase::WaitForService, StepState::Forecast),
            ]
        );
        // Preparation replaces only its own stages' forecast.
        apply(
            &mut timeline,
            vec![
                planned(PlanCoverage::Preparation, &[key(Phase::BuildGit, "web")]),
                action("action_started", "a1", Phase::BuildGit, "web", 100),
                action("action_succeeded", "a1", Phase::BuildGit, "web", 25_100),
                planned(
                    PlanCoverage::Runtime,
                    &[
                        key(Phase::EnsureVolume, "data"),
                        key(Phase::EnsureService, "web"),
                    ],
                ),
                action("action_started", "a2", Phase::EnsureVolume, "data", 25_200),
            ],
        );
        assert_eq!(
            states(&timeline),
            [
                (Phase::BuildGit, StepState::Succeeded),
                (Phase::EnsureVolume, StepState::Running),
                (Phase::EnsureService, StepState::Planned),
            ]
        );
        let build = &timeline.steps[0];
        assert_eq!(
            (build.started_at_ms, build.finished_at_ms, build.expected_ms),
            (Some(100), Some(25_100), Some(30_000))
        );
        assert_eq!(timeline.steps[2].expected_ms, Some(2_000));
    }

    #[test]
    fn retries_and_failures_stay_on_their_step() {
        let mut timeline = timeline(&[]);
        let mut retry = action("action_retry", "a1", Phase::EnsureService, "web", 200);
        retry.diagnostic = Some(Diagnostic::from_recorded_code(
            "d1".into(),
            "docker_unavailable",
            "Docker is unavailable".into(),
        ));
        let mut requested = action("action_requested", "a1", Phase::EnsureService, "web", 300);
        requested.retry = Some(3);
        apply(
            &mut timeline,
            vec![
                action("action_started", "a1", Phase::EnsureService, "web", 100),
                retry,
            ],
        );
        assert_eq!(timeline.steps[0].state, StepState::Retrying);
        apply(
            &mut timeline,
            vec![
                requested,
                action("action_failed", "a1", Phase::EnsureService, "web", 400),
                // A new attempt runs the same step again as its own.
                action("action_started", "a2", Phase::EnsureService, "web", 500),
            ],
        );
        assert_eq!(
            states(&timeline),
            [
                (Phase::EnsureService, StepState::Failed),
                (Phase::EnsureService, StepState::Running),
            ]
        );
        assert_eq!(timeline.steps[0].retries, 2);
    }

    #[test]
    fn sources_count_once_towards_the_time_left() {
        let mut timeline = timeline(&[
            (Phase::BuildGit, "api", 30_000),
            (Phase::BuildGit, "web", 20_000),
            (Phase::RunJob, "migrate", 5_000),
        ]);
        apply(
            &mut timeline,
            vec![
                action("action_started", "a1", Phase::BuildGit, "api", 0),
                action("action_started", "a2", Phase::BuildGit, "web", 0),
            ],
        );
        // Usually the longest source's 30 s, then the job's 5 s.
        assert_eq!(timeline.usual_ms(), Some(35_000));
        // The longest source has 20 s left, then the job's 5 s.
        assert_eq!(timeline.remaining_ms(10_000), Some(25_000));
        // A step running over its expectation counts as about to end.
        assert_eq!(timeline.remaining_ms(40_000), Some(5_000));
    }

    #[test]
    fn another_attempt_starts_over() {
        let mut timeline = timeline(&[]);
        apply(
            &mut timeline,
            vec![
                action("action_started", "a1", Phase::BuildGit, "web", 1),
                action("action_failed", "a1", Phase::BuildGit, "web", 2),
            ],
        );
        // Requested again, its latest attempt is the run before.
        let mut requested = timeline.operation.clone();
        requested.state = OperationState::Requested;
        timeline.set_operation(requested.clone());
        assert_eq!(timeline.steps.len(), 0, "{:?}", timeline.steps);
        let mut retried = requested;
        retried.state = OperationState::Running;
        retried.attempt = 2;
        timeline.set_operation(retried);
        // Events of the earlier attempt no longer apply.
        let mut late = action("action_started", "a3", Phase::EnsureService, "web", 3);
        late.id = timeline.cursor + 1;
        late.operation_id = Some("op".into());
        late.attempt = Some(1);
        timeline.apply(&late);
        assert_eq!(timeline.steps.len(), 0);
    }

    #[test]
    fn a_requested_operation_shows_none_of_its_previous_run() {
        let mut timeline = timeline(&[(Phase::EnsureService, "web", 2_000)]);
        timeline.operation.state = OperationState::Requested;
        apply(
            &mut timeline,
            vec![
                action("action_started", "a1", Phase::EnsureService, "web", 1),
                action("action_succeeded", "a1", Phase::EnsureService, "web", 2),
            ],
        );
        assert_eq!(
            states(&timeline),
            [(Phase::EnsureService, StepState::Forecast)]
        );
    }

    #[test]
    fn only_noticeably_unusual_durations_are_flagged() {
        let mut timeline = timeline(&[
            (Phase::BuildGit, "api", 30_000),
            (Phase::BuildGit, "web", 20_000),
            (Phase::EnsureService, "api", 1_000),
        ]);
        apply(
            &mut timeline,
            vec![
                action("action_started", "a1", Phase::BuildGit, "api", 0),
                action("action_started", "a2", Phase::BuildGit, "web", 0),
                action("action_succeeded", "a1", Phase::BuildGit, "api", 50_000),
                action("action_succeeded", "a2", Phase::BuildGit, "web", 21_000),
                action("action_started", "a3", Phase::EnsureService, "api", 50_000),
            ],
        );
        let unusual: Vec<_> = timeline.steps.iter().map(Step::unusual_ms).collect();
        // 20 s over its 30 s is unusual; 1 s over 20 s is not.
        assert_eq!(unusual, [Some(30_000), None, None]);
        // The sources build at once, then the service updates.
        assert_eq!(timeline.usual_ms(), Some(31_000));
    }

    #[test]
    fn events_already_applied_or_of_other_operations_are_ignored() {
        let mut timeline = timeline(&[]);
        apply(
            &mut timeline,
            vec![action(
                "action_started",
                "a1",
                Phase::EnsureService,
                "web",
                1,
            )],
        );
        let mut replayed = action("action_succeeded", "a1", Phase::EnsureService, "web", 2);
        replayed.id = timeline.cursor;
        replayed.operation_id = Some("op".into());
        timeline.apply(&replayed);
        let mut other = action("action_succeeded", "a1", Phase::EnsureService, "web", 2);
        other.id = timeline.cursor + 1;
        other.operation_id = Some("other".into());
        timeline.apply(&other);
        assert_eq!(timeline.steps[0].state, StepState::Running);
    }
}
