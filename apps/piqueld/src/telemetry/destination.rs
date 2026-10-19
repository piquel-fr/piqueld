//! One OTLP/gRPC collector and the bounded queues feeding it.
use crate::config::{Credential, OtlpEndpoint};
use crate::store::now_ms;
use opentelemetry::InstrumentationScope;
use opentelemetry_proto::tonic::collector::{
    logs::v1::{ExportLogsServiceRequest, logs_service_client::LogsServiceClient},
    metrics::v1::{ExportMetricsServiceRequest, metrics_service_client::MetricsServiceClient},
    trace::v1::{ExportTraceServiceRequest, trace_service_client::TraceServiceClient},
};
use opentelemetry_proto::transform::{
    common::tonic::ResourceAttributesWithSchema, logs::tonic::group_logs_by_resource_and_scope,
    trace::tonic::group_spans_by_resource_and_scope,
};
use opentelemetry_sdk::{Resource, logs::SdkLogRecord, trace::SpanData};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue, MetadataMap};
use tonic::transport::{Channel, ClientTlsConfig};

/// Records exported in one request at most.
const BATCH: usize = 512;
/// How long a batch may gather after its first record.
const BATCH_WINDOW: Duration = Duration::from_secs(1);
/// How long exports may fail before the destination counts as failing, so a
/// collector restart does not raise a diagnostic. A signal whose last failure
/// is older, because it has sent nothing since, no longer counts.
const FAILING_AFTER_MS: i64 = 60_000;
/// The longest one export may take, response included.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// A log record and the scope that emitted it, as queued for export.
pub(super) type LogItem = (SdkLogRecord, InstrumentationScope);

/// gRPC metadata sent with every export, parsed from `headers_file`: one
/// `name: value` per line, ignoring blank lines.
///
/// ```text
/// authorization: Bearer 0123456789
/// x-scope-orgid: piqueld
/// ```
#[derive(Clone, Default)]
pub struct Headers(MetadataMap);

/// Shows how many headers there are, never their values.
impl std::fmt::Debug for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Headers({} redacted)", self.0.len())
    }
}

/// A line of a headers file is not valid ASCII gRPC metadata. Never contains
/// the value.
#[derive(Debug, Error)]
#[error("telemetry headers line {line} must be `name: value` with an ASCII name and value")]
pub struct HeadersError {
    /// One-based line number.
    line: usize,
}

impl Headers {
    /// Parses a headers file's contents.
    ///
    /// # Errors
    /// Every non-blank line must be an ASCII `name: value` pair; binary
    /// (`-bin`) metadata is not supported.
    pub fn parse(source: &str) -> Result<Self, HeadersError> {
        let mut map = MetadataMap::new();
        for (index, line) in source.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let error = || HeadersError { line: index + 1 };
            let (name, value) = line.split_once(':').ok_or_else(error)?;
            let name = AsciiMetadataKey::from_bytes(name.trim().as_bytes()).map_err(|_| error())?;
            let mut value = Some(value.trim())
                .filter(|value| value.is_ascii())
                .and_then(|value| AsciiMetadataValue::try_from(value).ok())
                .ok_or_else(error)?;
            value.set_sensitive(true);
            map.append(name, value);
        }
        Ok(Self(map))
    }
}

/// Export health of one destination, shared with its worker.
#[derive(Debug, Default)]
struct Health {
    /// Unix milliseconds of the last accepted export; 0 before the first.
    last_success_ms: AtomicI64,
    /// Per signal (see [`Signal::INDEX`]), when its exports started failing
    /// without a success since; 0 while healthy. Signals fail separately: a
    /// collector may accept logs yet reject every span.
    failing_since_ms: [AtomicI64; 3],
    /// Per signal, when its last export failed.
    failed_ms: [AtomicI64; 3],
    /// Records discarded: the queue was full, or the collector failed or
    /// rejected them.
    dropped: AtomicU64,
}

impl Health {
    fn drop_records(&self, count: u64) {
        self.dropped.fetch_add(count, Ordering::Relaxed);
    }

    /// Records an export's outcome, logging only when the destination starts
    /// failing or recovers, so an unreachable collector cannot flood the logs
    /// that are themselves exported.
    fn record<T: Signal>(
        &self,
        endpoint: &OtlpEndpoint,
        result: Result<u64, tonic::Status>,
        records: u64,
    ) {
        let failing_since = &self.failing_since_ms[T::INDEX];
        match result {
            Ok(rejected) => {
                self.drop_records(rejected);
                self.last_success_ms.store(now_ms(), Ordering::Relaxed);
                if failing_since.swap(0, Ordering::Relaxed) != 0 {
                    tracing::info!(%endpoint, signal = T::NAME, "telemetry export recovered");
                }
            }
            Err(status) => {
                self.drop_records(records);
                let now = now_ms();
                let previous = self.failed_ms[T::INDEX].swap(now, Ordering::Relaxed);
                // A failure after a quiet minute starts a new failing window.
                if now.saturating_sub(previous) >= FAILING_AFTER_MS {
                    failing_since.store(0, Ordering::Relaxed);
                }
                if failing_since
                    .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    tracing::warn!(%endpoint, signal = T::NAME, code = ?status.code(), error = status.message(), "telemetry export failed; records are dropped until the collector accepts them");
                }
            }
        }
    }
}

/// A bounded queue of one signal. Pushing never waits: a full queue drops the
/// record and counts it.
struct Queue<T> {
    sender: mpsc::Sender<T>,
    /// Taken by the worker when it starts.
    receiver: Mutex<Option<mpsc::Receiver<T>>>,
}

impl<T> Queue<T> {
    fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    }

    fn take(&self) -> Option<mpsc::Receiver<T>> {
        self.receiver.lock().take()
    }
}

/// One collector receiving every signal over a lazily connected gRPC channel.
/// Each signal has its own queue, so a burst of logs cannot crowd out spans.
pub(super) struct Destination {
    endpoint: OtlpEndpoint,
    /// Shown by its source only.
    headers_source: Option<Credential>,
    headers: Headers,
    channel: Channel,
    spans: Queue<SpanData>,
    logs: Queue<LogItem>,
    /// Whole metric collections; each counts as one record.
    metrics: Queue<ExportMetricsServiceRequest>,
    health: Health,
    /// Cancelled to send what is queued and stop.
    closing: tokio_util::sync::CancellationToken,
}

impl Destination {
    /// Prepares a destination without connecting; the channel connects on
    /// first export and reconnects after failures. Must run inside Tokio.
    ///
    /// # Errors
    /// Returns invalid headers or TLS configuration.
    pub(super) fn new(
        endpoint: &OtlpEndpoint,
        headers: Option<&Credential>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let uri = endpoint.to_string();
        let mut builder = tonic::transport::Endpoint::from_shared(uri)
            .context("invalid telemetry endpoint")?
            .connect_timeout(Duration::from_secs(5))
            .timeout(EXPORT_TIMEOUT);
        if matches!(endpoint, OtlpEndpoint::Tcp(url) if url.scheme() == "https") {
            builder = builder
                .tls_config(ClientTlsConfig::new().with_webpki_roots())
                .context("invalid telemetry TLS configuration")?;
        }
        Ok(Self {
            endpoint: endpoint.clone(),
            headers_source: headers.cloned(),
            headers: headers
                .map(|headers| Headers::parse(headers.expose()))
                .transpose()?
                .unwrap_or_default(),
            channel: builder.connect_lazy(),
            spans: Queue::new(4096),
            logs: Queue::new(4096),
            metrics: Queue::new(16),
            health: Health::default(),
            closing: tokio_util::sync::CancellationToken::new(),
        })
    }

    /// Queues `item` on `queue`, counting it as dropped when the queue is full.
    fn push<T>(&self, queue: &Queue<T>, item: T) {
        if queue.sender.try_send(item).is_err() {
            self.health.drop_records(1);
        }
    }

    pub(super) fn push_span(&self, span: SpanData) {
        self.push(&self.spans, span);
    }

    pub(super) fn push_log(&self, log: LogItem) {
        self.push(&self.logs, log);
    }

    pub(super) fn push_metrics(&self, metrics: ExportMetricsServiceRequest) {
        self.push(&self.metrics, metrics);
    }

    /// Whether some signal's exports have failed for a minute without a
    /// success, and still did within the last minute.
    pub(super) fn failing(&self) -> bool {
        let now = now_ms();
        let health = &self.health;
        health
            .failing_since_ms
            .iter()
            .zip(&health.failed_ms)
            .any(|(since, failed)| {
                let since = since.load(Ordering::Relaxed);
                since != 0
                    && now.saturating_sub(since) >= FAILING_AFTER_MS
                    && now.saturating_sub(failed.load(Ordering::Relaxed)) < FAILING_AFTER_MS
            })
    }

    /// Current state for the status API. Headers appear only as their file.
    pub(super) fn status(&self) -> piqueld_core::api::TelemetryDestinationStatus {
        let last_success_ms = self.health.last_success_ms.load(Ordering::Relaxed);
        piqueld_core::api::TelemetryDestinationStatus {
            endpoint: self.endpoint.to_string(),
            headers: self.headers_source.as_ref().map(ToString::to_string),
            last_success_ms: (last_success_ms != 0).then_some(last_success_ms),
            dropped: self.health.dropped.load(Ordering::Relaxed),
            failing: self.failing(),
        }
    }

    /// Wraps `message` in a request carrying the configured headers.
    fn request<M>(&self, message: M) -> tonic::Request<M> {
        let mut request = tonic::Request::new(message);
        *request.metadata_mut() = self.headers.0.clone();
        request
    }

    /// Exports every queue until [`Self::close`], labelling records with
    /// `resource`. Spawned once; later calls return `None`.
    pub(super) fn spawn(
        self: &Arc<Self>,
        resource: &Resource,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let (spans, logs, metrics) = (self.spans.take()?, self.logs.take()?, self.metrics.take()?);
        let this = Arc::clone(self);
        let resource = ResourceAttributesWithSchema::from(resource);
        Some(tokio::spawn(async move {
            tokio::join!(
                this.drain(spans, &resource),
                this.drain(logs, &resource),
                this.drain(metrics, &resource),
            );
        }))
    }

    /// Sends what is queued, then stops the worker.
    pub(super) fn close(&self) {
        self.closing.cancel();
    }

    /// Sends batches from `queue` until closed: waits for a record, lets a
    /// batch gather for [`BATCH_WINDOW`], then exports it once. A failed batch
    /// is dropped, never retried, so a dead collector costs at most one
    /// request per window. A batch the collector rejects entirely counts as
    /// a failure. Once closed, takes no new records and sends those already
    /// queued without waiting, so producers cannot stretch the shutdown.
    async fn drain<T: Signal>(
        &self,
        mut queue: mpsc::Receiver<T>,
        resource: &ResourceAttributesWithSchema,
    ) {
        let mut batch = Vec::with_capacity(BATCH);
        loop {
            tokio::select! {
                received = queue.recv_many(&mut batch, BATCH) => if received == 0 {
                    return;
                },
                () = self.closing.cancelled() => queue.close(),
            }
            tokio::select! {
                () = tokio::time::sleep(BATCH_WINDOW) => {}
                () = self.closing.cancelled() => {}
            }
            while batch.len() < BATCH
                && let Ok(item) = queue.try_recv()
            {
                batch.push(item);
            }
            if batch.is_empty() {
                return;
            }
            let records = batch.len() as u64;
            let export = T::export(self, std::mem::take(&mut batch), resource);
            // The channel's own timeout ends once response headers arrive.
            let result = match tokio::time::timeout(EXPORT_TIMEOUT, export)
                .await
                .unwrap_or_else(|_| Err(tonic::Status::deadline_exceeded("export timed out")))
            {
                Ok(rejected) => match u64::try_from(rejected).unwrap_or(0) {
                    rejected if rejected >= records => Err(tonic::Status::invalid_argument(
                        "the collector rejected every record",
                    )),
                    rejected => Ok(rejected),
                },
                Err(status) => Err(status),
            };
            self.health.record::<T>(&self.endpoint, result, records);
        }
    }
}

/// A kind of queued record and how a batch of it is sent.
trait Signal: Sized + Send + 'static {
    /// Index of the signal's health.
    const INDEX: usize;
    /// Name in logs.
    const NAME: &str;

    /// Sends `batch` to `destination`, returning how many records the
    /// collector rejected.
    fn export(
        destination: &Destination,
        batch: Vec<Self>,
        resource: &ResourceAttributesWithSchema,
    ) -> impl Future<Output = Result<i64, tonic::Status>> + Send;
}

impl Signal for SpanData {
    const INDEX: usize = 0;
    const NAME: &str = "traces";

    async fn export(
        destination: &Destination,
        batch: Vec<Self>,
        resource: &ResourceAttributesWithSchema,
    ) -> Result<i64, tonic::Status> {
        let request = ExportTraceServiceRequest {
            resource_spans: group_spans_by_resource_and_scope(batch, resource),
        };
        let response = TraceServiceClient::new(destination.channel.clone())
            .export(destination.request(request))
            .await?;
        Ok(response
            .into_inner()
            .partial_success
            .map_or(0, |partial| partial.rejected_spans))
    }
}

impl Signal for LogItem {
    const INDEX: usize = 1;
    const NAME: &str = "logs";

    async fn export(
        destination: &Destination,
        batch: Vec<Self>,
        resource: &ResourceAttributesWithSchema,
    ) -> Result<i64, tonic::Status> {
        let records: Vec<_> = batch
            .iter()
            .map(|(record, scope)| (record, scope))
            .collect();
        let request = ExportLogsServiceRequest {
            resource_logs: group_logs_by_resource_and_scope(
                &opentelemetry_sdk::logs::LogBatch::new(&records),
                resource,
            ),
        };
        let response = LogsServiceClient::new(destination.channel.clone())
            .export(destination.request(request))
            .await?;
        Ok(response
            .into_inner()
            .partial_success
            .map_or(0, |partial| partial.rejected_log_records))
    }
}

/// Collections already carry the meter provider's resource.
impl Signal for ExportMetricsServiceRequest {
    const INDEX: usize = 2;
    const NAME: &str = "metrics";

    async fn export(
        destination: &Destination,
        batch: Vec<Self>,
        _resource: &ResourceAttributesWithSchema,
    ) -> Result<i64, tonic::Status> {
        let collections = i64::try_from(batch.len()).unwrap_or(i64::MAX);
        let request = ExportMetricsServiceRequest {
            resource_metrics: batch
                .into_iter()
                .flat_map(|request| request.resource_metrics)
                .collect(),
        };
        let response = MetricsServiceClient::new(destination.channel.clone())
            .export(destination.request(request))
            .await?;
        // Rejections count data points, but drops count collections: the
        // batch, usually one collection, is lost if any point was rejected.
        Ok(response.into_inner().partial_success.map_or(0, |partial| {
            if partial.rejected_data_points > 0 {
                collections
            } else {
                0
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full queue drops without waiting, and an unreachable collector
    /// costs only the records it was sent, all counted.
    #[tokio::test]
    async fn unreachable_collectors_drop_and_count_records() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = OtlpEndpoint::Unix(directory.path().join("absent.sock"));
        let destination = Arc::new(Destination::new(&endpoint, None).unwrap());
        for _ in 0..17 {
            destination.push_metrics(ExportMetricsServiceRequest::default());
        }
        assert_eq!(destination.status().dropped, 1);
        let worker = destination
            .spawn(&Resource::builder_empty().build())
            .unwrap();
        destination.close();
        worker.await.unwrap();
        let status = destination.status();
        assert_eq!((status.dropped, status.last_success_ms), (17, None));
    }

    /// Closing stops taking records, so a producer still pushing cannot
    /// keep the worker sending.
    #[tokio::test(flavor = "multi_thread")]
    async fn closing_sends_only_what_is_queued() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = OtlpEndpoint::Unix(directory.path().join("absent.sock"));
        let destination = Arc::new(Destination::new(&endpoint, None).unwrap());
        let worker = destination
            .spawn(&Resource::builder_empty().build())
            .unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pushing = std::thread::spawn({
            let (destination, stop) = (Arc::clone(&destination), Arc::clone(&stop));
            move || {
                while !stop.load(Ordering::Relaxed) {
                    destination.push_metrics(ExportMetricsServiceRequest::default());
                }
            }
        });
        destination.close();
        let stopped = tokio::time::timeout(Duration::from_secs(5), worker).await;
        stop.store(true, Ordering::Relaxed);
        pushing.join().unwrap();
        stopped.expect("the worker stops").unwrap();
    }

    #[test]
    fn headers_are_ascii_name_value_lines() {
        let headers =
            Headers::parse("Authorization: Bearer token\n\nx-scope-orgid:piqueld\n").unwrap();
        assert_eq!(headers.0.get("authorization").unwrap(), "Bearer token");
        assert_eq!(headers.0.get("x-scope-orgid").unwrap(), "piqueld");
        for invalid in [
            "no separator",
            "bad name: x",
            "key-bin: x",
            "key: caf\u{e9}",
        ] {
            assert!(Headers::parse(invalid).is_err(), "{invalid}");
        }
    }
}
