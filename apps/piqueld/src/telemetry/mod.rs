//! Export of the daemon's own traces, metrics and logs over OTLP/gRPC.
//!
//! piqueld pushes telemetry to every destination: the `[telemetry]`
//! endpoint, if set, and the receiving service of each environment granted
//! telemetry, over its socket (see [`Sockets`]). Every record carries the
//! resource attributes `service.name = piqueld`, `service.version` and
//! `service.instance.id`, the installation's ID.
//!
//! - Traces: each journaled operation, see [`journal`], and API requests,
//!   through a `tracing-opentelemetry` layer on their `tracing` spans.
//! - Metrics: deployments, builds, API latency, access denials, and gauges
//!   of the daemon process, its queues and every deployed service.
//! - Logs: the daemon's own `tracing` events and every journal event.
//!
//! With `[telemetry] containers`, the output and resource usage of every
//! container piqueld runs are exported too, see [`containers`], each under
//! its own resource naming the manifest service.
//!
//! Records flow through the OpenTelemetry SDK into [`Fanout`], which copies
//! them onto each destination's bounded queues. Nothing waits on a collector:
//! a full queue or a failed export drops records and counts them, and one
//! that keeps failing raises a single `telemetry_export_failed` diagnostic,
//! attributed to the receiving environment for a receiver. Revoking a
//! receiver's grant drops its destination at once, with what it had queued.

mod containers;
mod destination;
mod journal;
mod metrics;
mod sockets;

pub(crate) use containers::{ContainerCursors, ContainerExport, ContainerSample};
use destination::{Destination, LogItem};
pub use destination::{Headers, HeadersError};
pub(crate) use journal::{EventContext, Journal};
pub use metrics::{ServiceSample, Snapshot};
pub use sockets::Sockets;

use crate::config::{OtlpEndpoint, TelemetryConfig};
use opentelemetry::KeyValue;
use opentelemetry::logs::LoggerProvider as _;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord, SdkLoggerProvider};
use opentelemetry_sdk::metrics::{
    PeriodicReader, SdkMeterProvider, Temporality, data::ResourceMetrics,
    exporter::PushMetricExporter,
};
use opentelemetry_sdk::trace::{SdkTracerProvider, Span, SpanData, SpanProcessor};
use parking_lot::RwLock;
use piqueld_core::api::TelemetryDestinationStatus;
use piqueld_core::{EnvironmentId, ServiceName};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing_subscriber::{Layer, filter::filter_fn, registry::LookupSpan};

/// How often metrics are collected and queued.
const METRICS_INTERVAL: Duration = Duration::from_secs(30);
/// Log targets never exported, since exporting itself logs through them,
/// including its own reports of failing collectors.
const EXPORT_TARGETS: [&str; 6] = [
    "opentelemetry",
    "tonic",
    "h2",
    "hyper",
    "tower",
    "piqueld::telemetry",
];

/// Cheap handle to telemetry export. The default, used by tests, is
/// disabled and every method does nothing; the daemon's is always enabled,
/// as receivers may be granted at any time, and exports nothing while it has
/// no destination.
#[derive(Clone, Default)]
pub struct Telemetry(Option<Arc<Hub>>);

/// Providers and destinations of enabled telemetry.
struct Hub {
    destinations: Arc<Destinations>,
    tracer: SdkTracerProvider,
    logger: SdkLoggerProvider,
    /// Whether container output and resource usage are exported too.
    containers: bool,
    /// Set once the installation is known, see [`Telemetry::start`].
    started: OnceLock<Started>,
    /// Where receivers' sockets are.
    sockets: Sockets,
    /// Serializes refreshes of the receivers, reads included, so a stale
    /// read never restores a revoked one; see [`Telemetry::receive`].
    receiving: tokio::sync::Mutex<()>,
}

/// What exists once the installation's ID is known.
struct Started {
    resource: Resource,
    meter: SdkMeterProvider,
    instruments: metrics::Instruments,
}

/// Every destination, shared with the SDK processors feeding them: the
/// configured endpoint, then receivers.
#[derive(Default)]
struct Destinations(RwLock<Vec<Arc<Destination>>>);

impl Hub {
    /// Environment `id`'s receiver.
    fn destination(&self, id: &EnvironmentId) -> Option<Arc<Destination>> {
        self.destinations
            .0
            .read()
            .iter()
            .find(|destination| destination.receiver() == Some(id))
            .cloned()
    }
}

impl Destinations {
    fn each(&self, mut visit: impl FnMut(&Arc<Destination>)) {
        for destination in self.0.read().iter() {
            visit(destination);
        }
    }
}

/// The span and log processor and metric exporter that copy each record
/// onto every destination's queue.
#[derive(Clone)]
struct Fanout(Arc<Destinations>);

impl std::fmt::Debug for Fanout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Fanout")
    }
}

impl SpanProcessor for Fanout {
    fn on_start(&self, _span: &mut Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, span: SpanData) {
        self.0
            .each(|destination| destination.push_span(span.clone()));
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }
}

impl LogProcessor for Fanout {
    fn emit(&self, record: &mut SdkLogRecord, scope: &opentelemetry::InstrumentationScope) {
        self.0.each(|destination| {
            destination.push_log(LogItem::Daemon(Box::new((record.clone(), scope.clone()))));
        });
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }
}

impl PushMetricExporter for Fanout {
    /// Converts a collection once and queues a copy per destination. Called
    /// on the SDK's reader thread; it never waits.
    fn export(&self, metrics: &ResourceMetrics) -> impl Future<Output = OTelSdkResult> + Send {
        let request = ExportMetricsServiceRequest::from(metrics);
        self.0
            .each(|destination| destination.push_metrics(request.clone()));
        std::future::ready(Ok(()))
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}

impl Telemetry {
    /// Prepares export to the configured endpoint, if any, and to the
    /// receivers granted later, listening in `sockets`, so `tracing` layers
    /// can be installed before the daemon opens its state. Records queue
    /// until [`Self::start`]. Must run inside Tokio.
    ///
    /// # Errors
    /// Returns invalid headers or TLS configuration.
    pub fn new(config: &TelemetryConfig, sockets: Sockets) -> anyhow::Result<Self> {
        let destinations = Arc::new(Destinations::default());
        if let Some(endpoint) = &config.endpoint {
            destinations.0.write().push(Arc::new(Destination::new(
                endpoint,
                config.headers.as_ref(),
                None,
            )?));
        }
        let fanout = Fanout(Arc::clone(&destinations));
        Ok(Self(Some(Arc::new(Hub {
            destinations,
            containers: config.containers,
            tracer: SdkTracerProvider::builder()
                .with_span_processor(fanout.clone())
                .build(),
            logger: SdkLoggerProvider::builder()
                .with_log_processor(fanout)
                .build(),
            started: OnceLock::new(),
            sockets,
            receiving: tokio::sync::Mutex::default(),
        }))))
    }

    /// The `tracing` layers exporting API request spans and the daemon's own
    /// log events; `None` when disabled.
    #[must_use]
    pub fn layer<S>(&self) -> Option<Box<dyn Layer<S> + Send + Sync>>
    where
        S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
    {
        let hub = self.0.as_ref()?;
        let spans = tracing_opentelemetry::layer()
            .with_tracer(hub.tracer.tracer("piqueld"))
            .with_filter(filter_fn(|metadata| {
                metadata.is_span() && metadata.target().starts_with("piqueld::api")
            }));
        let logs =
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&hub.logger)
                .with_filter(filter_fn(|metadata| {
                    metadata.is_event()
                        && !EXPORT_TARGETS
                            .iter()
                            .any(|target| metadata.target().starts_with(target))
                }));
        Some(spans.and_then(logs).boxed())
    }

    /// Starts exporting now that `instance_id`, the installation's ID, is
    /// known: labels every record with it, starts each destination's worker,
    /// and starts collecting metrics. Later calls do nothing.
    pub fn start(&self, instance_id: &piqueld_core::InstanceId) {
        let Some(hub) = &self.0 else {
            return;
        };
        let resource = Resource::builder_empty()
            .with_attributes([
                KeyValue::new("service.name", "piqueld"),
                KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                KeyValue::new("service.instance.id", instance_id.to_string()),
            ])
            .build();
        let mut fresh = false;
        let started = hub.started.get_or_init(|| {
            fresh = true;
            let meter = SdkMeterProvider::builder()
                .with_resource(resource.clone())
                .with_reader(
                    PeriodicReader::builder(Fanout(Arc::clone(&hub.destinations)))
                        .with_interval(METRICS_INTERVAL)
                        .build(),
                )
                .build();
            Started {
                instruments: metrics::Instruments::new(&meter.meter("piqueld")),
                resource,
                meter,
            }
        });
        if !fresh {
            return;
        }
        hub.destinations
            .each(|destination| destination.spawn(&started.resource));
    }

    /// Exports to exactly the receiving services `receivers` reads, of the
    /// granted environments that deploy one. Refreshes run one at a time,
    /// from the read on, so one that read before a revocation never applies
    /// after a later one. A receiver no longer listed, or renamed, is
    /// dropped at once, with what it had queued, so a revoked grant receives
    /// nothing more; a new one connects lazily to its socket and retries
    /// until its collector listens.
    ///
    /// # Errors
    /// Returns the read's error, changing nothing.
    pub async fn receive<E>(
        &self,
        receivers: impl Future<Output = Result<Vec<(EnvironmentId, ServiceName)>, E>>,
    ) -> Result<(), E> {
        let Some(hub) = &self.0 else {
            return Ok(());
        };
        let _receiving = hub.receiving.lock().await;
        let receivers: Vec<_> = receivers
            .await?
            .into_iter()
            .map(|(id, service)| {
                let endpoint = OtlpEndpoint::Unix(hub.sockets.socket(&id, &service));
                (id, endpoint)
            })
            .collect();
        hub.destinations.0.write().retain(|destination| {
            let kept = destination.receiver().is_none_or(|id| {
                receivers
                    .iter()
                    .any(|(receiver, endpoint)| receiver == id && destination.exports_to(endpoint))
            });
            if !kept {
                destination.abort();
            }
            kept
        });
        for (id, endpoint) in &receivers {
            if hub.destination(id).is_some() {
                continue;
            }
            let destination = match Destination::new(endpoint, None, Some(id.clone())) {
                Ok(destination) => Arc::new(destination),
                Err(error) => {
                    tracing::error!(?error, environment_id = %id, "telemetry receiver could not be prepared");
                    continue;
                }
            };
            if let Some(started) = hub.started.get() {
                destination.spawn(&started.resource);
            }
            hub.destinations.0.write().push(destination);
        }
        Ok(())
    }

    /// Collects metrics a last time, then gives each destination up to
    /// `deadline` to send what it holds.
    pub async fn shutdown(&self, deadline: Duration) {
        let Some(hub) = &self.0 else {
            return;
        };
        if let Some(started) = hub.started.get() {
            // The reader's final collection waits for its thread.
            let meter = started.meter.clone();
            let _ = tokio::task::spawn_blocking(move || meter.shutdown()).await;
        }
        let mut workers = Vec::new();
        hub.destinations
            .each(|destination| workers.extend(destination.close()));
        if tokio::time::timeout(deadline, futures_util::future::join_all(workers))
            .await
            .is_err()
        {
            tracing::warn!("telemetry still being sent at shutdown was lost");
        }
    }

    /// Where receivers' sockets are; `None` when disabled.
    #[must_use]
    pub fn sockets(&self) -> Option<&Sockets> {
        self.0.as_ref().map(|hub| &hub.sockets)
    }

    /// `target` with its receiver's socket projected as reconciliation
    /// does while its environment is `granted` telemetry; see
    /// [`Sockets::project`].
    #[must_use]
    pub fn project(
        &self,
        target: piqueld_core::ResolvedApplication,
        granted: bool,
    ) -> piqueld_core::ResolvedApplication {
        match self.sockets() {
            Some(sockets) => sockets.project(target, granted),
            None => target,
        }
    }

    /// Whether anything receives telemetry.
    #[must_use]
    pub fn exporting(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|hub| !hub.destinations.0.read().is_empty())
    }

    /// Export state of every destination, the configured endpoint first,
    /// for the status API. Receivers' names are left unset.
    #[must_use]
    pub fn status(&self) -> piqueld_core::api::TelemetryStatus {
        let mut destinations = Vec::new();
        if let Some(hub) = &self.0 {
            hub.destinations
                .each(|destination| destinations.push(destination.status()));
        }
        piqueld_core::api::TelemetryStatus {
            destinations,
            containers: self.containers(),
        }
    }

    /// Export state of environment `id`'s receiving service, while it
    /// receives telemetry.
    #[must_use]
    pub fn receiver(&self, id: &EnvironmentId) -> Option<TelemetryDestinationStatus> {
        self.0
            .as_ref()?
            .destination(id)
            .map(|destination| destination.status())
    }

    /// Whether each destination, by the environment it is the receiver of,
    /// has failed for a minute without a success.
    #[must_use]
    pub fn failures(&self) -> Vec<(Option<EnvironmentId>, bool)> {
        let mut failures = Vec::new();
        if let Some(hub) = &self.0 {
            hub.destinations.each(|destination| {
                failures.push((destination.receiver().cloned(), destination.failing()));
            });
        }
        failures
    }

    /// A journal exporter starting after `running`, the attempts running now;
    /// `None` when disabled.
    pub(crate) fn journal(
        &self,
        running: impl IntoIterator<Item = (String, u64, i64)>,
    ) -> Option<Journal> {
        let hub = self.0.as_ref()?;
        Some(Journal::new(
            self.clone(),
            hub.logger.logger("piqueld.journal"),
            running,
        ))
    }

    /// Updates the measurements daemon and service gauges report.
    pub(crate) fn sample(&self, update: impl FnOnce(&mut Snapshot)) {
        self.with_instruments(|instruments| {
            update(&mut instruments.snapshot.lock());
        });
    }

    /// Records one API request's latency under its route template; requests
    /// matching no route share `route`'s absence.
    pub(crate) fn record_request(
        &self,
        method: &http::Method,
        route: Option<&str>,
        status: http::StatusCode,
        latency: Duration,
    ) {
        self.with_instruments(|instruments| {
            let mut attributes = vec![
                KeyValue::new("http.request.method", method.as_str().to_owned()),
                KeyValue::new("http.response.status_code", i64::from(status.as_u16())),
            ];
            attributes.extend(route.map(|route| KeyValue::new("http.route", route.to_owned())));
            instruments
                .requests
                .record(latency.as_secs_f64(), &attributes);
        });
    }

    /// Counts one API request refused for missing credentials or permission.
    pub(crate) fn count_denial(&self) {
        self.with_instruments(|instruments| instruments.denials.add(1, &[]));
    }

    /// Queues a finished span on every destination.
    fn export_span(&self, span: &SpanData) {
        if let Some(hub) = &self.0 {
            hub.destinations
                .each(|destination| destination.push_span(span.clone()));
        }
    }

    /// Runs `record` once metrics are collected.
    fn with_instruments(&self, record: impl FnOnce(&metrics::Instruments)) {
        if let Some(started) = self.0.as_ref().and_then(|hub| hub.started.get()) {
            record(&started.instruments);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Granted receivers export to their socket from the moment they are
    /// listed; one no longer listed, or renamed, is dropped at once, its
    /// worker stopped.
    #[tokio::test]
    async fn receivers_follow_the_grants_and_revoked_ones_stop_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let sockets = Sockets::new(directory.path());
        let telemetry = Telemetry::new(&TelemetryConfig::default(), sockets.clone()).unwrap();
        telemetry.start(&piqueld_core::InstanceId::parse("instance-test").unwrap());
        let (kept, revoked) = (
            EnvironmentId::parse("app-kept").unwrap(),
            EnvironmentId::parse("app-revoked").unwrap(),
        );
        let (collector, renamed) = (
            ServiceName::parse("collector").unwrap(),
            ServiceName::parse("otel").unwrap(),
        );
        let listed = |receivers: &[(&EnvironmentId, &ServiceName)]| {
            let receivers = receivers
                .iter()
                .map(|(id, service)| ((*id).clone(), (*service).clone()))
                .collect();
            std::future::ready(Ok::<_, std::convert::Infallible>(receivers))
        };
        telemetry
            .receive(listed(&[(&kept, &collector), (&revoked, &collector)]))
            .await
            .unwrap();
        let hub = telemetry.0.as_ref().unwrap();
        let (dropped, old) = (
            hub.destination(&revoked).unwrap(),
            hub.destination(&kept).unwrap(),
        );
        assert!(dropped.running(), "receivers start exporting");
        assert_eq!(
            telemetry.receiver(&kept).unwrap().endpoint,
            format!("unix://{}", sockets.socket(&kept, &collector).display())
        );

        telemetry
            .receive(listed(&[(&kept, &renamed)]))
            .await
            .unwrap();
        let receivers = telemetry
            .status()
            .destinations
            .into_iter()
            .filter_map(|destination| {
                Some((destination.receiver?.environment_id, destination.endpoint))
            })
            .collect::<Vec<_>>();
        let socket = format!("unix://{}", sockets.socket(&kept, &renamed).display());
        assert_eq!(
            receivers,
            [(kept, socket)],
            "a renamed receiver gets its own socket"
        );
        assert!(!dropped.running(), "its worker was aborted");
        assert!(!old.running(), "and the old socket's too");
    }
}
