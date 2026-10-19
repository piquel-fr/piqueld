//! Export of the output and resource usage of the containers piqueld runs.
//!
//! Unlike the daemon's own records, each container's records carry their own
//! resource, so they are built as OTLP messages directly rather than through
//! the SDK, and queued on the same bounded queues.
use super::Telemetry;
use crate::docker::ContainerUsage;
use opentelemetry_proto::tonic::{
    collector::metrics::v1::ExportMetricsServiceRequest,
    common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    metrics::v1::{
        AggregationTemporality, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
        metric, number_data_point,
    },
    resource::v1::Resource,
};
use piqueld_core::api::LogStream;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

/// Instrumentation scope of every container record.
const SCOPE: &str = "piqueld.containers";
/// Longest exported line, in bytes; longer lines are cut.
const MAX_LINE_BYTES: usize = 16 * 1024;
/// Lines a container may export a second, sustained.
const LINES_PER_SECOND: f64 = 200.0;
/// Lines a container may export at once after a quiet spell.
const LINE_BURST: f64 = 2_000.0;

/// Where log export stopped reading each container, saved so that it resumes
/// there after a restart.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContainerCursors {
    /// When the discovery these cursors cover started, in Unix nanoseconds:
    /// a container without a cursor was created after it. `None` before the
    /// first save, when export starts from the present.
    pub watermark: Option<i64>,
    /// Cursors by container ID.
    pub containers: BTreeMap<String, ContainerCursor>,
}

/// Where log export stopped reading one container.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContainerCursor {
    /// Docker's timestamp of the last line read, or where reading started, in
    /// Unix nanoseconds. Resuming skips lines before it but not at it, so a
    /// line may be sent twice and none is missed.
    pub at: i64,
    /// Whether the container stopped and its output was read to the end.
    pub done: bool,
}

/// The current time in Unix nanoseconds.
pub(crate) fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or_default()
}

/// The resource of one container's records: who runs it and what it runs.
#[derive(Clone, Debug)]
pub(crate) struct ContainerResource(Arc<Resource>);

impl ContainerResource {
    /// A resource with `attributes`, such as `service.name`.
    pub(crate) fn new(attributes: impl IntoIterator<Item = (&'static str, String)>) -> Self {
        Self(Arc::new(Resource {
            attributes: attributes
                .into_iter()
                .map(|(key, value)| attribute(key, any_value::Value::StringValue(value)))
                .collect(),
            ..Default::default()
        }))
    }
}

/// Limits the lines one container exports: [`LINES_PER_SECOND`], in bursts
/// of up to [`LINE_BURST`].
#[derive(Debug)]
pub(crate) struct LineLimit {
    tokens: f64,
    refilled: Instant,
}

impl Default for LineLimit {
    fn default() -> Self {
        Self {
            tokens: LINE_BURST,
            refilled: Instant::now(),
        }
    }
}

impl LineLimit {
    /// Takes room for one line now, if there is any.
    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = elapsed
            .mul_add(LINES_PER_SECOND, self.tokens)
            .min(LINE_BURST);
        self.refilled = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

/// One container's resource usage at a poll.
#[derive(Debug)]
pub(crate) struct ContainerSample {
    pub(crate) resource: ContainerResource,
    /// When the container was created, in Unix nanoseconds: the start of its
    /// cumulative counters.
    pub(crate) created: i64,
    /// When it was polled, in Unix nanoseconds.
    pub(crate) time: i64,
    pub(crate) usage: ContainerUsage,
    /// The previous poll's time and CPU time, from which CPU usage is derived.
    pub(crate) previous: Option<(i64, u64)>,
}

impl ContainerSample {
    /// CPUs used on average since the previous poll; 1 is one full core.
    fn cpus(&self) -> Option<f64> {
        let (time, cpu) = self.previous?;
        let elapsed = self.time.checked_sub(time).filter(|elapsed| *elapsed > 0)?;
        #[expect(
            clippy::cast_precision_loss,
            reason = "nanosecond counts lose precision only past 2^53, about 104 days of CPU time per poll"
        )]
        Some(self.usage.cpu_ns?.saturating_sub(cpu) as f64 / elapsed as f64)
    }

    /// The sample's metrics, named after OpenTelemetry's container
    /// conventions; `container.memory.usage.limit` follows the collector's
    /// Docker receiver, since the conventions name no limit.
    fn metrics(&self) -> Vec<Metric> {
        let usage = &self.usage;
        let point = |value, attributes: Vec<KeyValue>| NumberDataPoint {
            attributes,
            start_time_unix_nano: unsigned(self.created),
            time_unix_nano: unsigned(self.time),
            value: Some(value),
            ..Default::default()
        };
        let integer = |value: u64| number_data_point::Value::AsInt(signed(value));
        let direction =
            |key, value: &str| attribute(key, any_value::Value::StringValue(value.into()));
        let mut metrics = Vec::new();
        #[expect(
            clippy::cast_precision_loss,
            reason = "nanosecond counts lose precision only past 2^53, about 104 days of CPU time"
        )]
        let cpu_seconds = usage.cpu_ns.map(|cpu| cpu as f64 / 1e9);
        if let Some(seconds) = cpu_seconds {
            metrics.push(sum(
                ("container.cpu.time", "s", "CPU time used by the container"),
                vec![point(number_data_point::Value::AsDouble(seconds), vec![])],
            ));
        }
        if let Some(cpus) = self.cpus() {
            metrics.push(gauge(
                (
                    "container.cpu.usage",
                    "{cpu}",
                    "CPUs used by the container between polls; 1 is one full core",
                ),
                vec![point(number_data_point::Value::AsDouble(cpus), vec![])],
            ));
        }
        if let Some(memory) = usage.memory {
            metrics.push(gauge(
                (
                    "container.memory.usage",
                    "By",
                    "Memory used by the container, page cache included",
                ),
                vec![point(integer(memory), vec![])],
            ));
        }
        if let Some(limit) = usage.memory_limit {
            metrics.push(gauge(
                (
                    "container.memory.usage.limit",
                    "By",
                    "Memory limit of the container, or the host's memory without one",
                ),
                vec![point(integer(limit), vec![])],
            ));
        }
        if !usage.networks.is_empty() {
            let points = usage
                .networks
                .iter()
                .flat_map(|(interface, received, sent)| {
                    [("receive", received), ("transmit", sent)].map(|(way, bytes)| {
                        point(
                            integer(*bytes),
                            vec![
                                direction("network.interface.name", interface),
                                direction("network.io.direction", way),
                            ],
                        )
                    })
                })
                .collect();
            metrics.push(sum(
                (
                    "container.network.io",
                    "By",
                    "Network bytes received and sent",
                ),
                points,
            ));
        }
        if let Some((read, written)) = usage.disk {
            let points = [("read", read), ("write", written)]
                .map(|(way, bytes)| {
                    point(integer(bytes), vec![direction("disk.io.direction", way)])
                })
                .into();
            metrics.push(sum(
                (
                    "container.disk.io",
                    "By",
                    "Block device bytes read and written",
                ),
                points,
            ));
        }
        metrics
    }
}

/// A metric's name, unit and description.
type Describe = (&'static str, &'static str, &'static str);

/// A cumulative, monotonic sum.
fn sum((name, unit, description): Describe, data_points: Vec<NumberDataPoint>) -> Metric {
    Metric {
        name: name.into(),
        unit: unit.into(),
        description: description.into(),
        data: Some(metric::Data::Sum(Sum {
            data_points,
            aggregation_temporality: AggregationTemporality::Cumulative.into(),
            is_monotonic: true,
        })),
        ..Default::default()
    }
}

/// A gauge.
fn gauge((name, unit, description): Describe, data_points: Vec<NumberDataPoint>) -> Metric {
    Metric {
        name: name.into(),
        unit: unit.into(),
        description: description.into(),
        data: Some(metric::Data::Gauge(Gauge { data_points })),
        ..Default::default()
    }
}

/// An attribute named `key`.
fn attribute(key: &str, value: any_value::Value) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue { value: Some(value) }),
        ..Default::default()
    }
}

/// The scope of every container record.
fn scope() -> InstrumentationScope {
    InstrumentationScope {
        name: SCOPE.into(),
        ..Default::default()
    }
}

/// Unix nanoseconds as OTLP carries them; none are before 1970.
fn unsigned(nanos: i64) -> u64 {
    u64::try_from(nanos).unwrap_or_default()
}

/// A counter as OTLP's signed integers carry it.
fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Cuts `line` to [`MAX_LINE_BYTES`] at a character boundary.
fn bounded(mut line: String) -> String {
    if line.len() > MAX_LINE_BYTES {
        line.truncate(line.floor_char_boundary(MAX_LINE_BYTES));
    }
    line
}

/// A queued container log record with its resource.
pub(super) type ContainerLog = (ContainerResource, LogRecord);

/// Groups queued container records by resource, in queue order.
pub(super) fn group_logs(logs: Vec<ContainerLog>) -> Vec<ResourceLogs> {
    let mut grouped: Vec<(ContainerResource, Vec<LogRecord>)> = Vec::new();
    for (resource, record) in logs {
        match grouped
            .iter_mut()
            .find(|(seen, _)| Arc::ptr_eq(&seen.0, &resource.0))
        {
            Some((_, records)) => records.push(record),
            None => grouped.push((resource, vec![record])),
        }
    }
    grouped
        .into_iter()
        .map(|(resource, log_records)| ResourceLogs {
            resource: Some((*resource.0).clone()),
            scope_logs: vec![ScopeLogs {
                scope: Some(scope()),
                log_records,
                ..Default::default()
            }],
            ..Default::default()
        })
        .collect()
}

impl Telemetry {
    /// Whether container output and resource usage are exported.
    #[must_use]
    pub fn containers(&self) -> bool {
        self.0.as_ref().is_some_and(|hub| hub.containers)
    }

    /// Queues one line of a container's output, taken from `limit`, on every
    /// destination; a line past the limit is dropped and counted. The line is
    /// cut to [`MAX_LINE_BYTES`] and its severity left unset.
    pub(crate) fn export_container_line(
        &self,
        resource: &ContainerResource,
        limit: &mut LineLimit,
        stream: LogStream,
        timestamp: i64,
        line: String,
    ) {
        let Some(hub) = &self.0 else {
            return;
        };
        if !limit.take(Instant::now()) {
            hub.destinations
                .each(|destination| destination.drop_records(1));
            return;
        }
        let record = LogRecord {
            time_unix_nano: unsigned(timestamp),
            observed_time_unix_nano: unsigned(now_ns()),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(bounded(line))),
            }),
            attributes: vec![attribute(
                "log.iostream",
                any_value::Value::StringValue(stream.as_str().into()),
            )],
            ..Default::default()
        };
        hub.destinations.each(|destination| {
            destination.push_log(super::LogItem::Container((
                resource.clone(),
                record.clone(),
            )));
        });
    }

    /// Queues one collection of container resource usage on every destination.
    pub(crate) fn export_container_usage(&self, samples: &[ContainerSample]) {
        let Some(hub) = &self.0 else {
            return;
        };
        let request = ExportMetricsServiceRequest {
            resource_metrics: samples
                .iter()
                .map(|sample| ResourceMetrics {
                    resource: Some((*sample.resource.0).clone()),
                    scope_metrics: vec![ScopeMetrics {
                        scope: Some(scope()),
                        metrics: sample.metrics(),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
        };
        hub.destinations
            .each(|destination| destination.push_metrics(request.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn line_limit_allows_bursts_then_the_sustained_rate() {
        let mut limit = LineLimit::default();
        let start = limit.refilled;
        let taken = (0..3_000).filter(|_| limit.take(start)).count();
        assert_eq!(taken, 2_000);
        let later = start + Duration::from_millis(100);
        let taken = (0..100).filter(|_| limit.take(later)).count();
        assert_eq!(taken, 20);
    }

    #[test]
    fn long_lines_are_cut_at_a_character_boundary() {
        let line = bounded("é".repeat(MAX_LINE_BYTES));
        assert_eq!(line.len(), MAX_LINE_BYTES);
        assert_eq!(bounded("short".into()), "short");
    }

    /// Every counter starts at the container's creation, and CPU usage needs
    /// a previous poll.
    #[test]
    fn samples_become_container_metrics() {
        let mut sample = ContainerSample {
            resource: ContainerResource::new([("service.name", "web".into())]),
            created: 1_000,
            time: 31_000_000_000,
            usage: ContainerUsage {
                cpu_ns: Some(45_000_000_000),
                memory: Some(64),
                memory_limit: Some(128),
                networks: vec![("eth0".into(), 10, 20)],
                disk: Some((1, 2)),
            },
            previous: None,
        };
        let names = |sample: &ContainerSample| -> Vec<String> {
            sample
                .metrics()
                .into_iter()
                .map(|metric| metric.name)
                .collect()
        };
        assert_eq!(
            names(&sample),
            [
                "container.cpu.time",
                "container.memory.usage",
                "container.memory.usage.limit",
                "container.network.io",
                "container.disk.io",
            ]
        );
        sample.previous = Some((1_000_000_000, 30_000_000_000));
        let metrics = sample.metrics();
        let Some(metric::Data::Gauge(cpus)) = &metrics[1].data else {
            panic!("CPU usage is a gauge");
        };
        assert_eq!(metrics[1].name, "container.cpu.usage");
        assert_eq!(
            cpus.data_points[0].value,
            Some(number_data_point::Value::AsDouble(0.5))
        );
        let Some(metric::Data::Sum(network)) = &metrics[4].data else {
            panic!("network I/O is a sum");
        };
        assert_eq!(network.data_points.len(), 2);
        assert!(
            network
                .data_points
                .iter()
                .all(|point| point.start_time_unix_nano == 1_000)
        );
    }

    #[test]
    fn container_logs_are_grouped_by_resource() {
        let web = ContainerResource::new([("service.name", "web".into())]);
        let worker = ContainerResource::new([("service.name", "worker".into())]);
        let grouped = group_logs(vec![
            (web.clone(), LogRecord::default()),
            (worker, LogRecord::default()),
            (web, LogRecord::default()),
        ]);
        let counts: Vec<_> = grouped
            .iter()
            .map(|logs| logs.scope_logs[0].log_records.len())
            .collect();
        assert_eq!(counts, [2, 1]);
    }
}
