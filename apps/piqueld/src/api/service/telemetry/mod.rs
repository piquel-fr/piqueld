//! Feeds telemetry export from the journal, the store and the runtime.
mod containers;

use super::ApplicationService;
use crate::store::{JournalPosition, StoreError, Visibility, now_ms};
use crate::telemetry::{EventContext, Journal, ServiceSample};
use piqueld_core::EnvironmentId;
use piqueld_core::access::Scope;
use piqueld_core::api::TelemetryStatus;
use piqueld_core::observability::EventFilter;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The diagnostic and condition key of a collector that keeps failing.
const TELEMETRY_EXPORT_FAILED: &str = "telemetry_export_failed";

/// Events read from the journal at once.
const EVENT_PAGE: usize = 100;

/// How long names stay cached, so renames reach labels.
const NAMES_TTL: Duration = Duration::from_secs(300);

/// How often receivers are refreshed, so one deployed or removed reaches
/// export within seconds. Grants and deletions refresh them at once.
const RECEIVERS_INTERVAL: Duration = Duration::from_secs(5);

/// Names labelling an environment's telemetry, looked up when first needed
/// and forgotten every [`NAMES_TTL`].
struct Names {
    names: HashMap<EnvironmentId, EnvironmentNames>,
    since: std::time::Instant,
}

/// The names labelling one environment's telemetry.
#[derive(Clone)]
struct EnvironmentNames {
    application: String,
    environment: String,
    /// The preview's slug, for previews.
    preview: Option<String>,
}

/// Where the journal exporter is: its exporter, the last event it read, and
/// the names labelling what it exports.
struct Tail {
    journal: Journal,
    cursor: i64,
    names: Names,
}

impl ApplicationService {
    /// Exports journal events after `start` every second, samples daemon and
    /// service gauges every 30 seconds, refreshes receivers every
    /// [`RECEIVERS_INTERVAL`], and exports the containers' output and usage
    /// while enabled and anything receives it, until cancelled. Each runs on
    /// its own, so a slow Docker observation never holds up the journal or
    /// export health, and none holds up reconciliation or requests. Without
    /// container export, forgets where it stopped reading, so enabling it
    /// again starts from the present.
    pub(super) async fn export_telemetry(
        &self,
        start: JournalPosition,
        cancellation: CancellationToken,
    ) {
        if !self.telemetry.containers()
            && let Err(error) = self.store.clear_container_cursors().await
        {
            tracing::error!(?error, "container log cursors could not be cleared");
        }
        let Some(journal) = self.telemetry.journal(start.running) else {
            return;
        };
        let mut tail = Tail {
            journal,
            cursor: start.event,
            names: Names::default(),
        };
        let mut journal_tick = interval(Duration::from_secs(1));
        let mut daemon_tick = interval(Duration::from_secs(30));
        let mut services_tick = interval(Duration::from_secs(30));
        let mut receivers_tick = interval(RECEIVERS_INTERVAL);
        // A failure left open while an endpoint was configured closes.
        let mut exported = HashSet::from([None]);
        tokio::join!(
            async {
                while step(
                    &mut journal_tick,
                    &cancellation,
                    self.export_events(&mut tail),
                )
                .await
                {}
            },
            async {
                while step(
                    &mut daemon_tick,
                    &cancellation,
                    self.sample_daemon(&mut exported),
                )
                .await
                {}
            },
            async {
                while step(&mut receivers_tick, &cancellation, self.receive_telemetry()).await {}
            },
            async {
                while step(&mut services_tick, &cancellation, self.sample_services()).await {}
            },
            async {
                if self.telemetry.containers() {
                    self.export_received_containers(&cancellation).await;
                }
            },
        );
    }

    /// Exports every event journaled since the last call.
    async fn export_events(&self, tail: &mut Tail) -> Result<(), StoreError> {
        loop {
            let (events, checkpoint) = match self
                .store
                .stream_events(
                    &EventFilter::default(),
                    &Visibility::ALL,
                    tail.cursor,
                    EVENT_PAGE,
                )
                .await
            {
                // Pruning overtook the exporter; carry on from the present.
                Err(StoreError::HistoryExpired) => {
                    tail.cursor = self.store.journal_position().await?.event;
                    return Ok(());
                }
                result => result?,
            };
            for event in &events {
                let operation = match &event.operation_id {
                    Some(id) if Journal::needs_operation(event) => {
                        match self.store.operation(id).await {
                            Ok(operation) => Some(operation),
                            Err(StoreError::NotFound) => None,
                            Err(error) => return Err(error),
                        }
                    }
                    _ => None,
                };
                let labels = match &event.environment_id {
                    Some(id) => tail.names.get(self, id).await,
                    None => None,
                };
                tail.journal.record(
                    event,
                    EventContext {
                        names: labels
                            .as_ref()
                            .map(|names| (names.application.as_str(), names.environment.as_str())),
                        operation: operation.as_ref(),
                    },
                );
                tail.cursor = event.id;
            }
            tail.cursor = tail.cursor.max(checkpoint);
            if events.len() < EVENT_PAGE {
                return Ok(());
            }
        }
    }

    /// Exports to the receiving service of every granted environment that
    /// deploys one, and to no other.
    pub(super) async fn receive_telemetry(&self) -> Result<(), StoreError> {
        self.telemetry
            .receive(self.store.telemetry_receivers())
            .await
    }

    /// Export state of the configured endpoint and of the receivers of
    /// `readable` applications, named.
    pub(super) async fn telemetry_status(&self, readable: &Scope) -> TelemetryStatus {
        let mut status = self.telemetry.status();
        let mut destinations = Vec::new();
        for mut destination in status.destinations {
            if let Some(receiver) = &mut destination.receiver {
                let Ok(stored) = self.store.get(&receiver.environment_id).await else {
                    continue;
                };
                if !readable.contains(stored.application.application.id()) {
                    continue;
                }
                receiver.application = Some(stored.application.application.metadata().name.clone());
                receiver.environment = Some(stored.environment.name);
            }
            destinations.push(destination);
        }
        status.destinations = destinations;
        status
    }

    /// Records whether export to `receiver`'s receiving service, or the
    /// configured endpoint, is `failing`, as its `telemetry_export_failed`
    /// condition.
    async fn observe_export(
        &self,
        receiver: Option<&EnvironmentId>,
        failing: bool,
    ) -> Result<(), StoreError> {
        let key = receiver.map_or_else(
            || TELEMETRY_EXPORT_FAILED.to_owned(),
            |id| format!("{TELEMETRY_EXPORT_FAILED}:{id}"),
        );
        let environment = receiver.map(EnvironmentId::as_str);
        self.store
            .observe_condition(&key, environment, failing, now_ms(), 90_000)
            .await
    }

    /// Samples daemon statistics for the gauges, and reports whether each
    /// collector keeps failing. One `exported` to at the last sample but no
    /// longer counts as recovered; `exported` is updated in place.
    async fn sample_daemon(
        &self,
        exported: &mut HashSet<Option<EnvironmentId>>,
    ) -> Result<(), StoreError> {
        let failures = self.telemetry.failures();
        for gone in exported
            .iter()
            .filter(|known| !failures.iter().any(|(receiver, _)| receiver == *known))
        {
            self.observe_export(gone.as_ref(), false).await?;
        }
        exported.clear();
        for (receiver, failed) in failures {
            self.observe_export(receiver.as_ref(), failed).await?;
            exported.insert(receiver);
        }
        let stats = self
            .store
            .daemon_stats()
            .await
            .inspect_err(|error| tracing::warn!(?error, "daemon statistics are unavailable"))
            .ok();
        self.telemetry.sample(|snapshot| snapshot.stats = stats);
        Ok(())
    }

    /// Samples every deployed service's replicas for the gauges. Separate
    /// from [`Self::sample_daemon`], since slow Docker observations must not
    /// delay it.
    async fn sample_services(&self) -> Result<(), StoreError> {
        if !self.telemetry.exporting() {
            return Ok(());
        }
        let mut services = Vec::new();
        for stored in self.store.every_environment().await? {
            if stored.resolved.is_none() || stored.delete_intent() {
                continue;
            }
            // An environment the runtime cannot observe reports no replicas.
            let Ok(observed) = self.runtime.observe(&stored).await else {
                continue;
            };
            let application = stored.application.application.metadata().name.to_string();
            let environment = stored.environment.name.to_string();
            let observed = super::views::observed_view(&stored, &observed, false);
            services.extend(observed.services.into_iter().map(|service| ServiceSample {
                application: application.clone(),
                environment: environment.clone(),
                service: service.name,
                desired: service.desired_replicas,
                running: service.observed_replicas,
                healthy: service.healthy_replicas,
            }));
        }
        self.telemetry
            .sample(|snapshot| snapshot.services = services);
        Ok(())
    }
}

impl Default for Names {
    fn default() -> Self {
        Self {
            names: HashMap::new(),
            since: std::time::Instant::now(),
        }
    }
}

impl Names {
    /// The names of environment `id`, looked up when unknown; `None` once
    /// it is deleted.
    async fn get(
        &mut self,
        service: &ApplicationService,
        id: &EnvironmentId,
    ) -> Option<EnvironmentNames> {
        if self.since.elapsed() > NAMES_TTL {
            self.names.clear();
            self.since = std::time::Instant::now();
        }
        if let Some(names) = self.names.get(id) {
            return Some(names.clone());
        }
        let stored = service.store.get(id).await.ok()?;
        let names = EnvironmentNames {
            application: stored.application.application.metadata().name.to_string(),
            environment: stored.environment.name.to_string(),
            preview: match &stored.environment.kind {
                piqueld_core::EnvironmentKind::Preview(preview) => Some(preview.slug.to_string()),
                piqueld_core::EnvironmentKind::Environment => None,
            },
        };
        self.names.insert(id.clone(), names.clone());
        Some(names)
    }
}

/// A ticker that skips ticks missed while work ran.
fn interval(period: Duration) -> tokio::time::Interval {
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick
}

/// Runs `work` at the next `tick`, logging its failure, which is retried the
/// next tick. Returns `false` once cancelled, abandoning `work`.
async fn step(
    tick: &mut tokio::time::Interval,
    cancellation: &CancellationToken,
    work: impl Future<Output = Result<(), StoreError>>,
) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => return false,
        _ = tick.tick() => {}
    }
    tokio::select! {
        () = cancellation.cancelled() => false,
        result = work => {
            if let Err(error) = result {
                tracing::error!(?error, "telemetry export failed");
            }
            true
        }
    }
}
