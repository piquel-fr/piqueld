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
use piqueld_core::EnvironmentId;
use rustix::fs::{Mode, OFlags};
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

/// A log record as queued for export.
pub(super) enum LogItem {
    /// One of the daemon's own, with the scope that emitted it; boxed, since
    /// it is far larger than a container's.
    Daemon(Box<(SdkLogRecord, InstrumentationScope)>),
    /// A line of a container's output, with the container's resource.
    Container(super::containers::ContainerLog),
}

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
        receiver: Option<&EnvironmentId>,
        result: Result<u64, tonic::Status>,
        records: u64,
    ) {
        let receiver = receiver.map(EnvironmentId::as_str);
        let failing_since = &self.failing_since_ms[T::INDEX];
        match result {
            Ok(rejected) => {
                self.drop_records(rejected);
                self.last_success_ms.store(now_ms(), Ordering::Relaxed);
                if failing_since.swap(0, Ordering::Relaxed) != 0 {
                    tracing::info!(%endpoint, receiver, signal = T::NAME, "telemetry export recovered");
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
                    tracing::warn!(%endpoint, receiver, signal = T::NAME, code = ?status.code(), error = status.message(), "telemetry export failed; records are dropped until the collector accepts them");
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
    /// The environment whose receiving service this is; `None` for the
    /// configured endpoint.
    receiver: Option<EnvironmentId>,
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
    /// The worker exporting the queues, once spawned.
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
        receiver: Option<EnvironmentId>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let uri = match endpoint {
            OtlpEndpoint::Tcp(url) => url.to_string(),
            // The connector below reaches the socket; the URI only names it.
            OtlpEndpoint::Unix(_) => "http://localhost".to_owned(),
        };
        let mut builder = tonic::transport::Endpoint::from_shared(uri)
            .context("invalid telemetry endpoint")?
            .connect_timeout(Duration::from_secs(5))
            .timeout(EXPORT_TIMEOUT);
        if matches!(endpoint, OtlpEndpoint::Tcp(url) if url.scheme() == "https") {
            builder = builder
                .tls_config(ClientTlsConfig::new().with_webpki_roots())
                .context("invalid telemetry TLS configuration")?;
        }
        let channel = match endpoint {
            OtlpEndpoint::Tcp(_) => builder.connect_lazy(),
            OtlpEndpoint::Unix(path) => {
                let path = path.clone();
                let stale = receiver.is_some().then(|| Arc::new(StaleSocket::default()));
                builder.connect_with_connector_lazy(tower::service_fn(move |_| {
                    let (path, stale) = (path.clone(), stale.clone());
                    async move {
                        connect_unix(&path, stale.as_deref())
                            .await
                            .map(hyper_util::rt::TokioIo::new)
                    }
                }))
            }
        };
        Ok(Self {
            endpoint: endpoint.clone(),
            receiver,
            headers_source: headers.cloned(),
            headers: headers
                .map(|headers| Headers::parse(headers.expose()))
                .transpose()?
                .unwrap_or_default(),
            channel,
            spans: Queue::new(4096),
            logs: Queue::new(4096),
            metrics: Queue::new(16),
            health: Health::default(),
            closing: tokio_util::sync::CancellationToken::new(),
            worker: Mutex::default(),
        })
    }

    /// The environment whose receiving service this is.
    pub(super) fn receiver(&self) -> Option<&EnvironmentId> {
        self.receiver.as_ref()
    }

    /// Whether it exports to `endpoint`.
    pub(super) fn exports_to(&self, endpoint: &OtlpEndpoint) -> bool {
        self.endpoint == *endpoint
    }

    /// Queues `item` on `queue`, counting it as dropped when the queue is
    /// full. Returns whether it was queued.
    fn push<T>(&self, queue: &Queue<T>, item: T) -> bool {
        let queued = queue.sender.try_send(item).is_ok();
        if !queued {
            self.health.drop_records(1);
        }
        queued
    }

    /// Counts `count` records dropped before reaching a queue.
    pub(super) fn drop_records(&self, count: u64) {
        self.health.drop_records(count);
    }

    pub(super) fn push_span(&self, span: SpanData) {
        self.push(&self.spans, span);
    }

    pub(super) fn push_log(&self, log: LogItem) -> bool {
        self.push(&self.logs, log)
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

    /// Current state for the status API. Headers appear only as their file;
    /// a receiver's names are left for the caller to fill in.
    pub(super) fn status(&self) -> piqueld_core::api::TelemetryDestinationStatus {
        let health = &self.health;
        let last_success_ms = health.last_success_ms.load(Ordering::Relaxed);
        let last_failure_ms = health
            .failed_ms
            .iter()
            .map(|failed| failed.load(Ordering::Relaxed))
            .max()
            .unwrap_or(0);
        piqueld_core::api::TelemetryDestinationStatus {
            endpoint: self.endpoint.to_string(),
            headers: self.headers_source.as_ref().map(ToString::to_string),
            receiver: self
                .receiver
                .clone()
                .map(piqueld_core::api::TelemetryReceiver::new),
            last_success_ms: (last_success_ms != 0).then_some(last_success_ms),
            dropped: health.dropped.load(Ordering::Relaxed),
            connected: last_success_ms != 0 && last_success_ms >= last_failure_ms,
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
    /// `resource`. Spawned once; later calls do nothing.
    pub(super) fn spawn(self: &Arc<Self>, resource: &Resource) {
        let (Some(spans), Some(logs), Some(metrics)) =
            (self.spans.take(), self.logs.take(), self.metrics.take())
        else {
            return;
        };
        let this = Arc::clone(self);
        let resource = ResourceAttributesWithSchema::from(resource);
        *self.worker.lock() = Some(tokio::spawn(async move {
            tokio::join!(
                this.drain(spans, &resource),
                this.drain(logs, &resource),
                this.drain(metrics, &resource),
            );
        }));
    }

    /// Sends what is queued, then stops the worker, returning it to wait on.
    pub(super) fn close(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.closing.cancel();
        self.worker.lock().take()
    }

    /// How many log records are queued.
    #[cfg(test)]
    pub(super) fn queued_logs(&self) -> usize {
        self.logs.sender.max_capacity() - self.logs.sender.capacity()
    }

    /// Whether its worker runs.
    #[cfg(test)]
    pub(super) fn running(&self) -> bool {
        self.worker.lock().is_some()
    }

    /// Stops the worker at once, dropping what is queued and abandoning an
    /// export in flight.
    pub(super) fn abort(&self) {
        self.closing.cancel();
        if let Some(worker) = self.worker.lock().take() {
            worker.abort();
        }
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
            self.health
                .record::<T>(&self.endpoint, self.receiver.as_ref(), result, records);
        }
    }
}

/// Connects to the Unix socket at `path`. A path longer than a socket
/// address holds (108 bytes), as a deep `data_dir` makes receivers' sockets,
/// is reached through a descriptor of its directory instead, as
/// `/proc/self/fd/<descriptor>/<socket>`.
///
/// With `stale`, refused connections may remove the socket; see
/// [`StaleSocket`]. Only receivers', in piqueld's own directories, are.
async fn connect_unix(
    path: &std::path::Path,
    stale: Option<&StaleSocket>,
) -> std::io::Result<tokio::net::UnixStream> {
    /// Bytes of `sun_path`, its terminating NUL included.
    const SUN_PATH: usize = 108;
    let (Some(directory), Some(socket)) = (path.parent(), path.file_name()) else {
        return tokio::net::UnixStream::connect(path).await;
    };
    let directory = rustix::fs::open(
        directory,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    // `directory` stays open while its socket is used.
    let short = std::path::Path::new("/proc/self/fd")
        .join(std::os::fd::AsRawFd::as_raw_fd(&directory).to_string())
        .join(socket);
    let reachable = if path.as_os_str().len() < SUN_PATH {
        path
    } else {
        &short
    };
    let connected = tokio::net::UnixStream::connect(reachable).await;
    if let Some(stale) = stale {
        let refused = matches!(&connected, Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused);
        stale.observe(&directory, socket, refused);
    }
    connected
}

/// A receiver's socket that nothing listens on. A collector that crashed
/// without removing its socket could never listen again, as the path is
/// taken; but a collector also refuses connections for an instant between
/// binding and listening. So the socket is removed only once connections
/// to the same file were refused for [`Self::AFTER`].
#[derive(Default)]
struct StaleSocket(Mutex<Option<(u64, std::time::Instant)>>);

impl StaleSocket {
    /// How long a socket refuses connections before it counts as stale.
    const AFTER: Duration = Duration::from_secs(1);

    /// Records whether a connection to `socket` in `directory` was
    /// `refused`, removing the socket once it is stale.
    fn observe(&self, directory: &impl std::os::fd::AsFd, socket: &std::ffi::OsStr, refused: bool) {
        let mut since = self.0.lock();
        let inode = rustix::fs::statat(directory, socket, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .map(|stat| stat.st_ino);
        let (true, Some(inode)) = (refused, inode) else {
            *since = None;
            return;
        };
        match *since {
            Some((first, at)) if first == inode => {
                if at.elapsed() >= Self::AFTER {
                    let _ = rustix::fs::unlinkat(directory, socket, rustix::fs::AtFlags::empty());
                    *since = None;
                }
            }
            _ => *since = Some((inode, std::time::Instant::now())),
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
        let mut daemon = Vec::new();
        let mut containers = Vec::new();
        for item in batch {
            match item {
                Self::Daemon(log) => daemon.push(*log),
                Self::Container(log) => containers.push(log),
            }
        }
        let records: Vec<_> = daemon
            .iter()
            .map(|(record, scope)| (record, scope))
            .collect();
        let mut resource_logs = Vec::new();
        if !records.is_empty() {
            resource_logs = group_logs_by_resource_and_scope(
                &opentelemetry_sdk::logs::LogBatch::new(&records),
                resource,
            );
        }
        let (containers, handled) = super::containers::group_logs(containers);
        resource_logs.extend(containers);
        let request = ExportLogsServiceRequest { resource_logs };
        let response = LogsServiceClient::new(destination.channel.clone())
            .export(destination.request(request))
            .await;
        // Container cursors pass these lines only now that they are sent, or
        // dropped by a failed export.
        drop(handled);
        let response = response?;
        Ok(response
            .into_inner()
            .partial_success
            .map_or(0, |partial| partial.rejected_log_records))
    }
}

/// Collections already carry their resources: the meter provider's, or each
/// container's.
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
        let destination = Arc::new(Destination::new(&endpoint, None, None).unwrap());
        for _ in 0..17 {
            destination.push_metrics(ExportMetricsServiceRequest::default());
        }
        assert_eq!(destination.status().dropped, 1);
        destination.spawn(&Resource::builder_empty().build());
        destination.close().unwrap().await.unwrap();
        let status = destination.status();
        assert_eq!((status.dropped, status.last_success_ms), (17, None));
    }

    /// Closing stops taking records, so a producer still pushing cannot
    /// keep the worker sending.
    #[tokio::test(flavor = "multi_thread")]
    async fn closing_sends_only_what_is_queued() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = OtlpEndpoint::Unix(directory.path().join("absent.sock"));
        let destination = Arc::new(Destination::new(&endpoint, None, None).unwrap());
        destination.spawn(&Resource::builder_empty().build());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pushing = std::thread::spawn({
            let (destination, stop) = (Arc::clone(&destination), Arc::clone(&stop));
            move || {
                while !stop.load(Ordering::Relaxed) {
                    destination.push_metrics(ExportMetricsServiceRequest::default());
                }
            }
        });
        let worker = destination.close().unwrap();
        let stopped = tokio::time::timeout(Duration::from_secs(5), worker).await;
        stop.store(true, Ordering::Relaxed);
        pushing.join().unwrap();
        stopped.expect("the worker stops").unwrap();
    }

    /// A socket whose path exceeds `sun_path`, as deep data directories
    /// make receivers', is still reached.
    #[tokio::test]
    async fn sockets_with_paths_too_long_for_an_address_connect() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("d".repeat(120));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("otlp.sock");
        assert!(tokio::net::UnixStream::connect(&socket).await.is_err());
        let held = std::fs::File::open(&directory).unwrap();
        let short = format!(
            "/proc/self/fd/{}/otlp.sock",
            std::os::fd::AsRawFd::as_raw_fd(&held)
        );
        let _listener = std::os::unix::net::UnixListener::bind(short).unwrap();
        connect_unix(&socket, None).await.unwrap();
    }

    /// A receiver's socket that keeps refusing connections is removed, so
    /// its collector can listen again after a crash; one refusal, as while
    /// a collector starts, and another endpoint's socket are left alone.
    #[tokio::test]
    async fn stale_receiver_sockets_are_removed() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("otlp.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert!(connect_unix(&socket, None).await.is_err());
        let stale = StaleSocket::default();
        assert!(connect_unix(&socket, Some(&stale)).await.is_err());
        assert!(socket.exists(), "a single refusal keeps it");
        std::thread::sleep(StaleSocket::AFTER);
        assert!(connect_unix(&socket, Some(&stale)).await.is_err());
        assert!(!socket.exists());
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
