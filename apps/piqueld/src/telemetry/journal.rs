//! Journal events as traces, log records and deployment and build metrics.
//!
//! Each operation is one trace: a root span for the operation, a child span
//! per execution attempt, and below that a span per journaled action (builds,
//! jobs, rollouts, waits, …) timed by the duration the journal records.
//! Actions outside any operation are traces of their own.
//!
//! ```text
//! operation apply                         created → finished
//! └── attempt 2                           operation_started → operation_succeeded
//!     ├── building_git                    action_started → action_succeeded
//!     └── ensure_service
//! ```
//!
//! Trace and span IDs are hashes of the operation, attempt and action IDs, so
//! a span ending after a daemon restart still joins its trace, and every
//! event's log record links to the span it belongs to. Spans are only sent
//! once they end. An operation that is retried later sends its root span again,
//! extended to its new end.
use super::Telemetry;
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, Severity};
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
};
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_sdk::trace::{SpanData, SpanEvents, SpanLinks};
use piqueld_core::observability::DiagnosticCode;
use piqueld_core::operation::OperationKind;
use piqueld_core::{Event, Operation};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// Open attempts remembered at most; beyond it, the oldest are forgotten and
/// lose their attempt span.
const OPEN_ATTEMPTS: usize = 10_000;

/// What the exporter looked up about an event.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EventContext<'a> {
    /// Application and environment names, for metric labels.
    pub names: Option<(&'a str, &'a str)>,
    /// The event's operation as stored now; absent once pruned.
    pub operation: Option<&'a Operation>,
}

/// Turns journal events into telemetry, in journal order.
pub(crate) struct Journal {
    telemetry: Telemetry,
    logger: opentelemetry_sdk::logs::SdkLogger,
    /// Start times of attempts still running, by operation and attempt.
    open: HashMap<(String, u64), i64>,
}

impl Journal {
    /// Starts after `running`: the operation ID, attempt and start time of
    /// every attempt running now, whose end is still to come.
    pub(super) fn new(
        telemetry: Telemetry,
        logger: opentelemetry_sdk::logs::SdkLogger,
        running: impl IntoIterator<Item = (String, u64, i64)>,
    ) -> Self {
        Self {
            telemetry,
            logger,
            open: running
                .into_iter()
                .map(|(operation, attempt, started)| ((operation, attempt), started))
                .collect(),
        }
    }

    /// Whether recording `event` needs its operation as stored now.
    pub(crate) fn needs_operation(event: &Event) -> bool {
        Self::ENDS_ATTEMPTS.contains(&event.kind.as_str())
    }

    /// Event kinds that may end an attempt and, with it, the operation.
    const ENDS_ATTEMPTS: [&str; 6] = [
        "operation_succeeded",
        "operation_failed",
        "operation_cancelled",
        "operation_superseded",
        "operation_interrupted",
        "reconciliation_requested",
    ];

    /// Exports `event` as a log record, plus any span it ends and the
    /// deployment or build it finishes.
    pub(crate) fn record(&mut self, event: &Event, context: EventContext<'_>) {
        self.log(event, context);
        for span in self.spans(event, context) {
            self.telemetry.export_span(&span);
        }
    }

    /// Builds the spans `event` ends, recording deployment and build metrics
    /// along the way.
    fn spans(&mut self, event: &Event, context: EventContext<'_>) -> Vec<SpanData> {
        match event.kind.as_str() {
            "operation_started" => {
                if let (Some(operation), Some(attempt)) = (&event.operation_id, event.attempt) {
                    if self.open.len() >= OPEN_ATTEMPTS {
                        self.open.clear();
                    }
                    self.open
                        .insert((operation.clone(), attempt), event.created_at_ms);
                }
                Vec::new()
            }
            "action_succeeded" | "action_failed" | "action_outcome_unknown" => {
                self.action(event, context).into_iter().collect()
            }
            kind if Self::ENDS_ATTEMPTS.contains(&kind) => self.operation(event, context),
            _ => Vec::new(),
        }
    }

    /// The span of a finished action, and its build metrics.
    fn action(&self, event: &Event, context: EventContext<'_>) -> Option<SpanData> {
        let action = event.action_id.as_deref()?;
        let outcome = match event.kind.as_str() {
            "action_succeeded" => "succeeded",
            "action_failed" => "failed",
            _ => "unknown",
        };
        let duration = event.duration_ms.map(Duration::from_millis);
        let phase = event.phase.as_deref().unwrap_or("action");
        if phase == "building_git" {
            let labels = labels(context, outcome);
            self.telemetry.with_instruments(|instruments| {
                instruments.builds.add(1, &labels);
                if let Some(duration) = duration {
                    instruments
                        .build_duration
                        .record(duration.as_secs_f64(), &labels);
                }
            });
        }
        let (trace, parent) = match (&event.operation_id, event.attempt) {
            (Some(operation), Some(attempt)) => (
                trace_id(&["operation", operation]),
                span_id(&["attempt", operation, &attempt.to_string()]),
            ),
            (Some(operation), None) => (
                trace_id(&["operation", operation]),
                span_id(&["operation", operation]),
            ),
            (None, _) => (trace_id(&["action", action]), SpanId::INVALID),
        };
        let end = time(event.created_at_ms);
        let mut attributes = common_attributes(event, context);
        attributes.extend([
            KeyValue::new("piqueld.action.id", action.to_owned()),
            KeyValue::new("piqueld.outcome", outcome),
        ]);
        attributes.extend(
            event
                .resource
                .clone()
                .map(|resource| KeyValue::new("piqueld.resource", resource)),
        );
        attributes.extend(
            event
                .retry
                .and_then(|retry| i64::try_from(retry).ok())
                .map(|retry| KeyValue::new("piqueld.retry", retry)),
        );
        Some(span(
            trace,
            span_id(&["action", action]),
            parent,
            // Names stay few; the resource, often a Docker name, is an attribute.
            phase.to_owned(),
            (end - duration.unwrap_or_default(), end),
            attributes,
            failure(event),
        ))
    }

    /// The spans of an ended attempt and, once the operation finished, its
    /// root span, with deployment metrics.
    fn operation(&mut self, event: &Event, context: EventContext<'_>) -> Vec<SpanData> {
        let Some(id) = &event.operation_id else {
            return Vec::new();
        };
        // An operation goes with its environment, as a finished deletion's
        // does: its success still ends it, though its kind is unknown.
        let (kind, created_at_ms, attempts) = match context.operation {
            Some(operation) => (
                Some(operation.kind),
                Some(operation.created_at_ms),
                operation.attempt,
            ),
            None if event.kind == "operation_succeeded" => {
                (None, None, event.attempt.unwrap_or_default())
            }
            None => return Vec::new(),
        };
        let attempt = event.attempt.unwrap_or(attempts);
        // Deletions never fail: they record each error and keep their attempt
        // running until it is retried.
        if event.kind == "operation_failed" && kind == Some(OperationKind::Delete) {
            return Vec::new();
        }
        let outcome = match event.kind.as_str() {
            "operation_succeeded" => "succeeded",
            "operation_failed" => "failed",
            "operation_cancelled" => "cancelled",
            "operation_superseded" => "superseded",
            "operation_interrupted" => "interrupted",
            _ => "requeued",
        };
        let end = time(event.created_at_ms);
        let trace = trace_id(&["operation", id]);
        let root = span_id(&["operation", id]);
        let status = failure(event);
        let mut spans = Vec::new();
        let started = self.open.remove(&(id.clone(), attempt));
        if let Some(started) = started {
            let mut attributes = common_attributes(event, context);
            attributes.push(KeyValue::new("piqueld.outcome", outcome));
            spans.push(span(
                trace,
                span_id(&["attempt", id, &attempt.to_string()]),
                root,
                format!("attempt {attempt}"),
                (time(started), end),
                attributes,
                status.clone(),
            ));
        }
        if !matches!(outcome, "succeeded" | "failed" | "cancelled" | "superseded") {
            return spans;
        }
        if kind.is_some_and(|kind| kind != OperationKind::Delete) {
            let labels = labels(context, outcome);
            self.telemetry.with_instruments(|instruments| {
                instruments.deployments.add(1, &labels);
                if let Some(started) = started {
                    let duration = event.created_at_ms.saturating_sub(started);
                    instruments.deployment_duration.record(
                        Duration::from_millis(u64::try_from(duration).unwrap_or(0)).as_secs_f64(),
                        &labels,
                    );
                }
            });
        }
        let mut attributes = common_attributes(event, context);
        attributes.extend(kind.map(|kind| KeyValue::new("piqueld.operation.kind", kind.as_str())));
        attributes.extend([
            KeyValue::new("piqueld.outcome", outcome),
            KeyValue::new(
                "piqueld.attempts",
                i64::try_from(attempts).unwrap_or(i64::MAX),
            ),
        ]);
        spans.push(span(
            trace,
            root,
            SpanId::INVALID,
            kind.map_or_else(
                || "operation".to_owned(),
                |kind| format!("operation {kind}"),
            ),
            // A removed operation's creation is unknown; its last attempt began it.
            (
                time(created_at_ms.or(started).unwrap_or(event.created_at_ms)),
                end,
            ),
            attributes,
            status,
        ));
        spans
    }

    /// Sends `event` as a log record linked to the span it belongs to.
    fn log(&self, event: &Event, context: EventContext<'_>) {
        let mut record = self.logger.create_log_record();
        record.set_timestamp(time(event.created_at_ms));
        record.set_observed_timestamp(SystemTime::now());
        let (severity, text) = if failure(event) == Status::Unset {
            (Severity::Info, "INFO")
        } else {
            (Severity::Warn, "WARN")
        };
        record.set_severity_number(severity);
        record.set_severity_text(text);
        record.set_body(AnyValue::from(
            event.message.clone().unwrap_or_else(|| event.kind.clone()),
        ));
        record.add_attributes(
            common_attributes(event, context)
                .into_iter()
                .map(|attribute| (attribute.key, any(attribute.value))),
        );
        record.add_attributes(
            [
                ("piqueld.event.id", Some(event.id.to_string())),
                ("piqueld.event.kind", Some(event.kind.clone())),
                ("piqueld.event.scope", Some(event.scope.as_str().to_owned())),
                ("piqueld.action.id", event.action_id.clone()),
                ("piqueld.phase", event.phase.clone()),
                ("piqueld.resource", event.resource.clone()),
                (
                    "piqueld.diagnostic.id",
                    event
                        .diagnostic
                        .as_ref()
                        .map(|diagnostic| diagnostic.id.clone()),
                ),
                ("piqueld.request.id", event.request_id.clone()),
            ]
            .into_iter()
            .filter_map(|(key, value)| Some((key, AnyValue::from(value?)))),
        );
        if let Some(operation) = &event.operation_id {
            let span = match (&event.action_id, event.attempt) {
                (Some(action), _) => span_id(&["action", action]),
                (None, Some(attempt)) => span_id(&["attempt", operation, &attempt.to_string()]),
                (None, None) => span_id(&["operation", operation]),
            };
            record.set_trace_context(
                trace_id(&["operation", operation]),
                span,
                Some(TraceFlags::SAMPLED),
            );
        } else if let Some(action) = &event.action_id {
            record.set_trace_context(
                trace_id(&["action", action]),
                span_id(&["action", action]),
                Some(TraceFlags::SAMPLED),
            );
        }
        self.logger.emit(record);
    }
}

/// A span attribute's value as a log attribute's.
fn any(value: opentelemetry::Value) -> AnyValue {
    match value {
        opentelemetry::Value::I64(value) => value.into(),
        opentelemetry::Value::F64(value) => value.into(),
        opentelemetry::Value::Bool(value) => value.into(),
        value => value.as_str().into_owned().into(),
    }
}

/// Labels of a deployment or build metric. Unknown names, of an environment
/// deleted meanwhile, are empty.
fn labels(context: EventContext<'_>, outcome: &'static str) -> [KeyValue; 3] {
    let (application, environment) = context.names.unwrap_or_default();
    [
        KeyValue::new("application", application.to_owned()),
        KeyValue::new("environment", environment.to_owned()),
        KeyValue::new("outcome", outcome),
    ]
}

/// Attributes every span and log record of `event` carries.
fn common_attributes(event: &Event, context: EventContext<'_>) -> Vec<KeyValue> {
    let mut attributes = Vec::new();
    if let Some((application, environment)) = context.names {
        attributes.extend([
            KeyValue::new("piqueld.application", application.to_owned()),
            KeyValue::new("piqueld.environment", environment.to_owned()),
        ]);
    }
    attributes.extend(
        event
            .application_id
            .as_ref()
            .map(|id| KeyValue::new("piqueld.application.id", id.to_string())),
    );
    attributes.extend(
        event
            .environment_id
            .as_ref()
            .map(|id| KeyValue::new("piqueld.environment.id", id.to_string())),
    );
    attributes.extend(
        event
            .operation_id
            .clone()
            .map(|id| KeyValue::new("piqueld.operation.id", id)),
    );
    attributes.extend(
        event
            .attempt
            .and_then(|attempt| i64::try_from(attempt).ok())
            .map(|attempt| KeyValue::new("piqueld.attempt", attempt)),
    );
    attributes.extend(
        event
            .error_code
            .clone()
            .map(|code| KeyValue::new("error.type", code)),
    );
    attributes
}

/// An error status for events recording a failure, other than being
/// cancelled or superseded.
fn failure(event: &Event) -> Status {
    match event.error_code.as_deref().map(DiagnosticCode::parse) {
        None | Some(Some(DiagnosticCode::Cancelled | DiagnosticCode::Superseded)) => Status::Unset,
        Some(_) => Status::error(event.message.clone().unwrap_or_default()),
    }
}

/// The first 16 bytes of the SHA-256 of `parts`.
fn trace_id(parts: &[&str]) -> TraceId {
    TraceId::from_bytes(digest(parts)[..16].try_into().expect("16 bytes"))
}

/// Bytes 16..24 of the SHA-256 of `parts`, so a trace's root span never
/// shares its trace ID's bytes.
fn span_id(parts: &[&str]) -> SpanId {
    SpanId::from_bytes(digest(parts)[16..24].try_into().expect("8 bytes"))
}

/// SHA-256 of `parts` separated by NUL bytes, which IDs never contain.
fn digest(parts: &[&str]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update("piqueld");
    for part in parts {
        hash.update([0]);
        hash.update(part);
    }
    hash.finalize().into()
}

/// Unix milliseconds as a timestamp.
fn time(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

/// A finished internal span of the journal's instrumentation scope.
fn span(
    trace: TraceId,
    id: SpanId,
    parent: SpanId,
    name: String,
    (start, end): (SystemTime, SystemTime),
    attributes: Vec<KeyValue>,
    status: Status,
) -> SpanData {
    SpanData {
        span_context: SpanContext::new(trace, id, TraceFlags::SAMPLED, false, TraceState::NONE),
        parent_span_id: parent,
        parent_span_is_remote: false,
        span_kind: SpanKind::Internal,
        name: name.into(),
        start_time: start,
        end_time: end,
        attributes,
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links: SpanLinks::default(),
        status,
        instrumentation_scope: InstrumentationScope::builder("piqueld.journal").build(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::logs::LoggerProvider as _;
    use piqueld_core::operation::OperationState;

    /// A journal whose exporter started while attempt 1 of `op` ran.
    fn journal() -> Journal {
        let logger = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
            .build()
            .logger("test");
        Journal::new(Telemetry::default(), logger, [("op".into(), 1, 1_000)])
    }

    /// Operation `op`, created at 500 ms, in `state` after two attempts.
    fn operation(kind: OperationKind, state: OperationState) -> Operation {
        Operation {
            id: "op".into(),
            environment_id: piqueld_core::EnvironmentId::parse("app-notes").unwrap(),
            kind,
            generation: 1,
            attempt: 2,
            consecutive_failures: 0,
            state,
            phase: None,
            resource: None,
            error_code: None,
            error_message: None,
            created_at_ms: 500,
            updated_at_ms: 6_000,
            started_at_ms: Some(3_000),
            finished_at_ms: None,
        }
    }

    /// An event of operation `op` at `at` ms, during `attempt`.
    fn event(kind: &str, attempt: u64, at: i64) -> Event {
        Event {
            id: at,
            application_id: None,
            environment_id: None,
            operation_id: Some("op".into()),
            generation: Some(1),
            attempt: Some(attempt),
            kind: kind.into(),
            message: None,
            error_code: None,
            phase: None,
            resource: None,
            scope: piqueld_core::observability::EventScope::Application,
            action_id: None,
            retry: None,
            retry_delay_ms: None,
            duration_ms: None,
            request_id: None,
            actor: None,
            diagnostic: None,
            created_at_ms: at,
        }
    }

    /// An operation spans attempts across a restart, each with its actions,
    /// under one root, and every span ends exactly once.
    #[test]
    fn an_operation_is_one_trace_of_attempts_and_actions() {
        let mut journal = journal();
        let operation = operation(OperationKind::Apply, OperationState::Succeeded);
        let context = EventContext {
            names: Some(("notes", "production")),
            operation: Some(&operation),
        };
        let interrupted = journal.spans(&event("operation_interrupted", 1, 2_000), context);
        assert_eq!(
            journal.spans(&event("operation_started", 2, 3_000), context),
            []
        );
        let build = journal.spans(
            &Event {
                action_id: Some("build".into()),
                phase: Some("building_git".into()),
                resource: Some("web".into()),
                duration_ms: Some(1_500),
                ..event("action_succeeded", 2, 5_000)
            },
            context,
        );
        let finished = journal.spans(&event("operation_succeeded", 2, 6_000), context);
        let ([first], [build], [second, root]) = (&interrupted[..], &build[..], &finished[..])
        else {
            panic!("{interrupted:?} {build:?} {finished:?}");
        };
        let trace = root.span_context.trace_id();
        for span in [first, build, second] {
            assert_eq!(span.span_context.trace_id(), trace, "{}", span.name);
        }
        assert_eq!(root.parent_span_id, SpanId::INVALID);
        assert_eq!(first.parent_span_id, root.span_context.span_id());
        assert_eq!(second.parent_span_id, root.span_context.span_id());
        assert_eq!(build.parent_span_id, second.span_context.span_id());
        assert_ne!(first.span_context.span_id(), second.span_context.span_id());
        assert_eq!(build.name, "building_git");
        assert_eq!(
            (build.start_time, build.end_time),
            (time(3_500), time(5_000))
        );
        assert_eq!((root.start_time, root.end_time), (time(500), time(6_000)));
        // Repeating the end sends the root again, but no second attempt.
        let repeated = journal.spans(&event("operation_succeeded", 2, 7_000), context);
        assert_eq!(repeated.len(), 1);
        assert_eq!(
            repeated[0].span_context.span_id(),
            root.span_context.span_id()
        );
    }

    /// A deletion's errors leave its attempt running, and it finishes once
    /// its operation is gone with its environment.
    #[test]
    fn deletions_finish_after_their_operation_is_removed() {
        let mut journal = journal();
        let deletion = operation(OperationKind::Delete, OperationState::Running);
        let failing = EventContext {
            names: None,
            operation: Some(&deletion),
        };
        assert_eq!(
            journal.spans(&event("operation_failed", 1, 2_000), failing),
            []
        );
        let finished = journal.spans(
            &event("operation_succeeded", 1, 3_000),
            EventContext::default(),
        );
        let [attempt, root] = &finished[..] else {
            panic!("{finished:?}");
        };
        assert_eq!(root.name, "operation");
        assert_eq!(root.start_time, time(1_000));
        assert_eq!(attempt.parent_span_id, root.span_context.span_id());
    }
}
