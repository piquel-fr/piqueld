//! Feeds container telemetry: follows the output of every container piqueld
//! runs and polls their resource usage.
//!
//! Discovery lists the containers every [`DISCOVERY_INTERVAL`], and as soon
//! as Docker reports one starting. Each container with output to read gets a
//! follower, which reads from its cursor until the container stops. The
//! cursors saved for restarts only pass a line once every copy queued for
//! export was sent or dropped, so a line still queued when the daemon dies is
//! read again after the restart rather than lost.
use super::ApplicationService;
use super::telemetry::{Names, interval, step};
use crate::docker::{
    BollardDocker, ContainerState, DockerError, LimitedDocker, ManagedContainer, Workload,
};
use crate::store::StoreError;
use crate::telemetry::{ContainerCursors, ContainerResource, ContainerSample, LineLimit, now_ns};
use futures_util::StreamExt as _;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How often containers are listed and cursors saved.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);
/// The least time between two discoveries a container start triggers.
const DISCOVERY_SPACING: Duration = Duration::from_secs(1);
/// How often resource usage is polled.
const USAGE_INTERVAL: Duration = Duration::from_secs(30);
/// Containers whose usage is read at once.
const USAGE_CONCURRENCY: usize = 2;

/// The Docker client container telemetry reads through.
pub(super) type Docker = Arc<LimitedDocker<BollardDocker>>;

/// Every container being exported, by ID, shared with the usage poll.
type Exports = Arc<Mutex<HashMap<String, Exported>>>;

/// A container's resource, shared with its follower and replaced when its
/// environment is renamed.
type SharedResource = Arc<Mutex<ContainerResource>>;

/// One container being exported.
struct Exported {
    container: ManagedContainer,
    resource: SharedResource,
    /// Docker's timestamp of the last line read, or where reading starts, in
    /// Unix nanoseconds; shared with its follower.
    read: Arc<AtomicI64>,
    /// The last line every copy of which was sent or dropped, or where
    /// reading started: where reading resumes after a restart.
    handled: Arc<AtomicI64>,
    /// Whether it stopped and its output was read to the end. Not saved: a
    /// restart reads stopped containers once more from their cursor, since a
    /// restarted ingress container keeps its ID.
    done: bool,
    /// Whether Docker reported it starting since its follower started, so
    /// output may follow what that follower read even once it is done.
    restarted: bool,
    /// Reads its output until it stops.
    follower: Option<JoinHandle<Result<(), DockerError>>>,
    /// Whether a failure to read its output was logged, so a container whose
    /// output cannot be read warns once, not every discovery.
    warned: bool,
}

impl Drop for Exported {
    fn drop(&mut self) {
        if let Some(follower) = &self.follower {
            follower.abort();
        }
    }
}

impl Exported {
    /// Notes how its follower ended, if it did, and starts one when there
    /// may be output to read.
    fn keep_following(&mut self, service: &ApplicationService, docker: &Docker) {
        self.reap();
        if self.needs_follower() {
            self.follower = Some(tokio::spawn(follow(
                Arc::clone(docker),
                service.telemetry.clone(),
                self.container.id.clone(),
                Arc::clone(&self.resource),
                (Arc::clone(&self.read), Arc::clone(&self.handled)),
            )));
        }
    }

    /// Notes that Docker reported it starting.
    fn started(&mut self) {
        self.restarted = true;
        self.done = false;
    }

    /// Notes how its follower ended, if it did.
    fn reap(&mut self) {
        if let Some(result) = self
            .follower
            .as_mut()
            .and_then(futures_util::FutureExt::now_or_never)
        {
            self.follower = None;
            match result {
                // A follower ends once its container stops, having read all
                // its output unless it started again since.
                Ok(Ok(())) => {
                    self.done = self.container.state == ContainerState::Stopped && !self.restarted;
                }
                Ok(Err(error)) if !self.warned => {
                    self.warned = true;
                    tracing::warn!(container = %self.container.name, ?error, "container output could not be exported; retrying");
                }
                Ok(Err(_)) => {}
                Err(error) => {
                    tracing::error!(container = %self.container.name, ?error, "container output follower failed");
                }
            }
        }
    }

    /// Whether a follower should start now: it has none, and it has output
    /// to read. Starting one covers any restart reported so far.
    fn needs_follower(&mut self) -> bool {
        match self.container.state {
            ContainerState::Created => return false,
            ContainerState::Running => self.done = false,
            ContainerState::Stopped => {}
        }
        let needed = self.follower.is_none() && !self.done;
        if needed {
            self.restarted = false;
        }
        needed
    }
}

/// Exports a container's output from its `read` cursor until it stops,
/// moving the cursor past each line read; the queued lines move `handled`.
async fn follow(
    docker: Docker,
    telemetry: crate::telemetry::Telemetry,
    id: String,
    resource: SharedResource,
    (read, handled): (Arc<AtomicI64>, Arc<AtomicI64>),
) -> Result<(), DockerError> {
    let resume = read.load(Ordering::Relaxed);
    // Docker reads from whole seconds; lines before the cursor are skipped.
    let since = i32::try_from(resume.div_euclid(1_000_000_000).max(0)).unwrap_or(i32::MAX);
    let mut limit = LineLimit::default();
    let mut logs = std::pin::pin!(docker.container_logs(&id, since));
    while let Some(chunk) = logs.next().await {
        let chunk = chunk?;
        let Some(stream) = chunk.stream else {
            continue;
        };
        for (timestamp, line) in chunk.lines {
            if timestamp < resume {
                continue;
            }
            let line = piqueld_core::api::LogRecord::clean_message(&line);
            let resource = resource.lock().clone();
            telemetry
                .export_container_line(&resource, &mut limit, &handled, stream, timestamp, line);
            read.fetch_max(timestamp, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Discovery's state between runs.
struct Discovery {
    /// Cursors saved by the previous process, used once per container.
    saved: BTreeMap<String, i64>,
    /// Where containers the previous process did not know start reading.
    start: i64,
    /// When the last successful discovery started, in Unix nanoseconds.
    scanned: Option<i64>,
    /// When the last discovery ran.
    ran: Option<Instant>,
    names: Names,
}

impl ApplicationService {
    /// Exports the output and resource usage of every container piqueld
    /// runs, until cancelled.
    pub(super) async fn export_containers(&self, docker: Docker, cancellation: &CancellationToken) {
        let saved = self
            .store
            .container_cursors()
            .await
            .inspect_err(|error| {
                tracing::error!(
                    ?error,
                    "container log cursors are unreadable; exporting from the present"
                );
            })
            .unwrap_or_default();
        let mut discovery = Discovery {
            start: saved.watermark.unwrap_or_else(now_ns),
            saved: saved.containers,
            scanned: None,
            ran: None,
            names: Names::default(),
        };
        let exports = Exports::default();
        let mut usage_tick = interval(USAGE_INTERVAL);
        let mut previous = HashMap::new();
        tokio::join!(
            self.discover_containers(&docker, &exports, &mut discovery, cancellation),
            async {
                while step(
                    &mut usage_tick,
                    cancellation,
                    self.poll_usage(&docker, &exports, &mut previous),
                )
                .await
                {}
            },
        );
    }

    /// Discovers containers every [`DISCOVERY_INTERVAL`], saving their
    /// cursors, and when one starts, until cancelled.
    async fn discover_containers(
        &self,
        docker: &Docker,
        exports: &Exports,
        discovery: &mut Discovery,
        cancellation: &CancellationToken,
    ) {
        let mut tick = interval(DISCOVERY_INTERVAL);
        let mut starts = None;
        loop {
            let started = async {
                match &mut starts {
                    Some(starts) => futures_util::StreamExt::next(starts).await,
                    None => std::future::pending().await,
                }
            };
            let interval = tokio::select! {
                () = cancellation.cancelled() => return,
                _ = tick.tick() => true,
                event = started => {
                    match event {
                        // It may have stopped already, with new output.
                        Some(Ok(Some(id))) => {
                            if let Some(exported) = exports.lock().get_mut(&id) {
                                exported.started();
                            }
                        }
                        Some(Ok(None)) => {}
                        // Resubscribed at the next interval.
                        Some(Err(_)) | None => starts = None,
                    }
                    false
                }
            };
            if interval && starts.is_none() {
                // Starts while unsubscribed went unseen, so every container
                // may have new output; Docker replays those from now on.
                let since = now_ns().div_euclid(1_000_000_000);
                exports.lock().values_mut().for_each(Exported::started);
                starts = Some(Box::pin(docker.container_starts(since)));
            }
            if !interval
                && discovery
                    .ran
                    .is_some_and(|ran| ran.elapsed() < DISCOVERY_SPACING)
            {
                continue;
            }
            if let Err(error) = self.discover(docker, exports, discovery).await {
                tracing::warn!(?error, "containers could not be listed for telemetry");
                continue;
            }
            if !interval {
                continue;
            }
            let Some(scanned) = discovery.scanned else {
                continue;
            };
            let cursors = ContainerCursors {
                watermark: Some(scanned),
                containers: exports
                    .lock()
                    .iter()
                    .map(|(id, exported)| (id.clone(), exported.handled.load(Ordering::Relaxed)))
                    .collect(),
            };
            if let Err(error) = self.store.save_container_cursors(&cursors).await {
                tracing::error!(?error, "container log cursors could not be saved");
            }
        }
    }

    /// Lists the containers, starts following new ones, forgets removed ones,
    /// and refreshes their resources.
    async fn discover(
        &self,
        docker: &Docker,
        exports: &Exports,
        discovery: &mut Discovery,
    ) -> Result<(), DockerError> {
        discovery.ran = Some(Instant::now());
        let scanned = now_ns();
        let containers = docker.managed_containers(self.store.instance_id()).await?;
        let mut resources = Vec::with_capacity(containers.len());
        for container in &containers {
            resources.push(
                self.container_resource(container, &mut discovery.names)
                    .await,
            );
        }
        let mut exports = exports.lock();
        exports.retain(|id, _| containers.iter().any(|container| &container.id == id));
        for (container, resource) in containers.into_iter().zip(resources) {
            let exported = exports.entry(container.id.clone()).or_insert_with(|| {
                let cursor = discovery
                    .saved
                    .remove(&container.id)
                    .unwrap_or(discovery.start);
                Exported {
                    resource: Arc::new(Mutex::new(resource.clone())),
                    read: Arc::new(AtomicI64::new(cursor)),
                    handled: Arc::new(AtomicI64::new(cursor)),
                    done: false,
                    restarted: false,
                    follower: None,
                    warned: false,
                    container: container.clone(),
                }
            });
            {
                let mut current = exported.resource.lock();
                if *current != resource {
                    *current = resource;
                }
            }
            exported.container = container;
            exported.keep_following(self, docker);
        }
        discovery.scanned = Some(scanned);
        Ok(())
    }

    /// The resource of a container's records: the manifest service or job,
    /// or the infrastructure component, as `service.name`, what kind of
    /// workload it is, the container and task, and its environment's names.
    async fn container_resource(
        &self,
        container: &ManagedContainer,
        names: &mut Names,
    ) -> ContainerResource {
        let (service, workload, environment) = match &container.workload {
            Workload::Service { environment, name } => (name.clone(), "service", Some(environment)),
            Workload::Job { environment, name } => (name.clone(), "job", Some(environment)),
            Workload::Infrastructure(name) => ((*name).to_owned(), "infrastructure", None),
        };
        let mut attributes = vec![
            ("service.name", service),
            ("piqueld.workload", workload.to_owned()),
            ("container.id", container.id.clone()),
            ("container.name", container.name.clone()),
        ];
        attributes.extend(container.task.clone().map(|task| ("piqueld.task.id", task)));
        if let Some(environment) = environment {
            attributes.push(("piqueld.environment.id", environment.to_string()));
            if let Some(names) = names.get(self, environment).await {
                attributes.push(("piqueld.application", names.application));
                attributes.push(("piqueld.environment", names.environment));
                attributes.extend(names.preview.map(|slug| ("piqueld.preview", slug)));
            }
        }
        ContainerResource::new(attributes)
    }

    /// Reads the resource usage of every running container and queues it as
    /// one collection. `previous` holds each container's last poll, from
    /// which CPU usage is derived. A container that cannot be read is
    /// skipped until the next poll.
    async fn poll_usage(
        &self,
        docker: &Docker,
        exports: &Exports,
        previous: &mut HashMap<String, (i64, u64)>,
    ) -> Result<(), StoreError> {
        let running: Vec<_> = exports
            .lock()
            .values()
            .filter(|exported| exported.container.state == ContainerState::Running)
            .map(|exported| {
                (
                    exported.container.id.clone(),
                    exported.container.created.saturating_mul(1_000_000_000),
                    exported.resource.lock().clone(),
                )
            })
            .collect();
        let polled: Vec<_> = futures_util::stream::iter(running)
            .map(|(id, created, resource)| async move {
                let usage = docker.container_usage(&id).await;
                (id, created, resource, now_ns(), usage)
            })
            .buffer_unordered(USAGE_CONCURRENCY)
            .collect()
            .await;
        let mut samples = Vec::new();
        let mut seen = HashMap::new();
        for (id, created, resource, time, usage) in polled {
            let usage = match usage {
                Ok(Some(usage)) => usage,
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(container = %id, ?error, "container stats could not be read");
                    continue;
                }
            };
            if let Some(cpu) = usage.cpu_ns {
                seen.insert(id.clone(), (time, cpu));
            }
            samples.push(ContainerSample {
                resource,
                created,
                time,
                usage,
                previous: previous.get(&id).copied(),
            });
        }
        *previous = seen;
        if !samples.is_empty() {
            self.telemetry.export_container_usage(&samples);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A container that stopped, started and stopped again before its first
    /// follower was reaped gets a new follower for the second run's output.
    #[tokio::test]
    async fn a_restart_reported_before_reaping_reads_the_new_output() {
        let follower = tokio::spawn(async { Ok(()) });
        while !follower.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut exported = Exported {
            container: ManagedContainer {
                id: "gateway-id".into(),
                name: "piqueld-ingress-0123".into(),
                state: ContainerState::Stopped,
                created: 0,
                task: None,
                workload: Workload::Infrastructure("piqueld-gateway"),
            },
            resource: Arc::new(Mutex::new(ContainerResource::new([]))),
            read: Arc::default(),
            handled: Arc::default(),
            done: false,
            restarted: false,
            follower: Some(follower),
            warned: false,
        };
        exported.started();
        exported.reap();
        assert!(exported.needs_follower());
        exported.follower = Some(tokio::spawn(async { Ok(()) }));
        while !exported.follower.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        exported.reap();
        assert!(
            !exported.needs_follower(),
            "the second run was read to the end"
        );
    }
}
