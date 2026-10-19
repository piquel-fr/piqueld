//! The daemon's metric instruments.
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use parking_lot::Mutex;
use piqueld_core::observability::DaemonStats;
use std::sync::Arc;

/// Latest daemon and service measurements. The service samples them every
/// 30 seconds; observable gauges read the latest at each collection.
#[derive(Debug, Default)]
pub struct Snapshot {
    /// Cached daemon statistics, when they could be collected.
    pub stats: Option<DaemonStats>,
    /// Replica counts of every deployed service.
    pub services: Vec<ServiceSample>,
}

/// Replica counts of one deployed service.
#[derive(Debug)]
pub struct ServiceSample {
    /// Application name.
    pub application: String,
    /// Environment name.
    pub environment: String,
    /// Logical service name.
    pub service: String,
    /// Replicas the manifest asks for.
    pub desired: u16,
    /// Replicas the runtime reports.
    pub running: u16,
    /// Running replicas healthy enough to serve.
    pub healthy: u16,
}

/// Reads one gauge's points from a snapshot.
type Read = fn(&Snapshot) -> Vec<(u64, Vec<KeyValue>)>;

/// Integer gauges: name, unit, description, and how to read them.
const GAUGES: &[(&str, &str, &str, Read)] = &[
    ("piqueld.uptime", "s", "Daemon process uptime", |s| {
        stat(s, |stats| vec![(stats.uptime_seconds, vec![])])
    }),
    (
        "process.memory.usage",
        "By",
        "Resident memory of the daemon process",
        |s| {
            stat(s, |stats| {
                stats
                    .memory_bytes
                    .map(|bytes| (bytes, vec![]))
                    .into_iter()
                    .collect()
            })
        },
    ),
    (
        "piqueld.database.size",
        "By",
        "SQLite database and write-ahead log sizes",
        |s| {
            stat(s, |stats| {
                vec![
                    (
                        stats.database_bytes,
                        vec![KeyValue::new("file", "database")],
                    ),
                    (stats.wal_bytes, vec![KeyValue::new("file", "wal")]),
                ]
            })
        },
    ),
    (
        "piqueld.disk.available",
        "By",
        "Disk space available to the daemon on its data filesystem",
        |s| {
            stat(s, |stats| {
                stats
                    .available_disk_bytes
                    .map(|bytes| (bytes, vec![]))
                    .into_iter()
                    .collect()
            })
        },
    ),
    (
        "piqueld.events",
        "{event}",
        "Retained history events",
        |s| stat(s, |stats| vec![(count(stats.events), vec![])]),
    ),
    (
        "piqueld.diagnostics",
        "{diagnostic}",
        "Retained distinct diagnostic occurrences",
        |s| stat(s, |stats| vec![(count(stats.diagnostics), vec![])]),
    ),
    (
        "piqueld.build_output.size",
        "By",
        "Retained build output",
        |s| stat(s, |stats| vec![(count(stats.build_output_bytes), vec![])]),
    ),
    (
        "piqueld.operations",
        "{operation}",
        "Operations running or queued to run",
        |s| {
            stat(s, |stats| {
                vec![
                    (
                        count(stats.running_operations),
                        vec![KeyValue::new("state", "running")],
                    ),
                    (
                        count(stats.queued_operations),
                        vec![KeyValue::new("state", "queued")],
                    ),
                ]
            })
        },
    ),
    (
        "piqueld.notification.deliveries",
        "{delivery}",
        "Webhook deliveries awaiting a retry or failed for good",
        |s| {
            stat(s, |stats| {
                vec![
                    (
                        count(stats.pending_deliveries),
                        vec![KeyValue::new("state", "pending")],
                    ),
                    (
                        count(stats.failed_deliveries),
                        vec![KeyValue::new("state", "failed")],
                    ),
                ]
            })
        },
    ),
    (
        "piqueld.service.replicas",
        "{replica}",
        "Replicas of each deployed service: desired by the manifest, running, and healthy",
        |s| {
            s.services
                .iter()
                .flat_map(|service| {
                    [
                        ("desired", service.desired),
                        ("running", service.running),
                        ("healthy", service.healthy),
                    ]
                    .map(|(state, replicas)| {
                        (
                            u64::from(replicas),
                            vec![
                                KeyValue::new("application", service.application.clone()),
                                KeyValue::new("environment", service.environment.clone()),
                                KeyValue::new("service", service.service.clone()),
                                KeyValue::new("state", state),
                            ],
                        )
                    })
                })
                .collect()
        },
    ),
];

/// Points read from the daemon statistics, none before the first sample.
fn stat(
    snapshot: &Snapshot,
    read: impl Fn(&DaemonStats) -> Vec<(u64, Vec<KeyValue>)>,
) -> Vec<(u64, Vec<KeyValue>)> {
    snapshot.stats.as_ref().map(read).unwrap_or_default()
}

/// A stored count, which is never negative.
fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Boundaries for deployment and build durations, in seconds.
const OPERATION_SECONDS: [f64; 11] = [
    1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0,
];
/// Boundaries for API latency, in seconds, as OpenTelemetry's HTTP conventions advise.
const REQUEST_SECONDS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Instruments recorded as things happen, and the snapshot gauges read.
pub(super) struct Instruments {
    pub(super) snapshot: Arc<Mutex<Snapshot>>,
    pub(super) deployments: Counter<u64>,
    pub(super) deployment_duration: Histogram<f64>,
    pub(super) builds: Counter<u64>,
    pub(super) build_duration: Histogram<f64>,
    pub(super) requests: Histogram<f64>,
    pub(super) denials: Counter<u64>,
}

impl Instruments {
    /// Registers every instrument on `meter`.
    pub(super) fn new(meter: &Meter) -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        for &(name, unit, description, read) in GAUGES {
            let snapshot = Arc::clone(&snapshot);
            meter
                .u64_observable_gauge(name)
                .with_unit(unit)
                .with_description(description)
                .with_callback(move |observer| {
                    for (value, attributes) in read(&snapshot.lock()) {
                        observer.observe(value, &attributes);
                    }
                })
                .build();
        }
        let cpu = |read: fn(&DaemonStats) -> Option<f64>| {
            let snapshot = Arc::clone(&snapshot);
            move |observer: &dyn opentelemetry::metrics::AsyncInstrument<f64>| {
                if let Some(value) = snapshot.lock().stats.as_ref().and_then(read) {
                    observer.observe(value, &[]);
                }
            }
        };
        meter
            .f64_observable_counter("process.cpu.time")
            .with_unit("s")
            .with_description("CPU time used by the daemon process")
            .with_callback(cpu(|stats| stats.cpu_seconds))
            .build();
        meter
            .f64_observable_gauge("process.cpu.utilization")
            .with_unit("1")
            .with_description("CPU used by the daemon process between samples; 1 is one full core")
            .with_callback(cpu(|stats| {
                stats.cpu_percent.map(|percent| percent / 100.0)
            }))
            .build();
        let histogram = |name: &'static str, description: &'static str, boundaries: &[f64]| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_description(description)
                .with_boundaries(boundaries.to_vec())
                .build()
        };
        Self {
            deployments: meter
                .u64_counter("piqueld.deployments")
                .with_unit("{deployment}")
                .with_description("Finished deployment attempts by outcome")
                .build(),
            deployment_duration: histogram(
                "piqueld.deployment.duration",
                "Duration of finished deployment attempts",
                &OPERATION_SECONDS,
            ),
            builds: meter
                .u64_counter("piqueld.builds")
                .with_unit("{build}")
                .with_description("Finished builds by outcome")
                .build(),
            build_duration: histogram(
                "piqueld.build.duration",
                "Duration of finished builds",
                &OPERATION_SECONDS,
            ),
            requests: histogram(
                "http.server.request.duration",
                "Duration of API requests",
                &REQUEST_SECONDS,
            ),
            denials: meter
                .u64_counter("piqueld.access.denials")
                .with_unit("{request}")
                .with_description("API requests refused for missing credentials or permission")
                .build(),
            snapshot,
        }
    }
}
