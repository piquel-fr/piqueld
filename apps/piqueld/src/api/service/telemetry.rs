//! Feeds telemetry export from the journal, the store and the runtime.
use super::ApplicationService;
use crate::store::{JournalPosition, StoreError, Visibility, now_ms};
use crate::telemetry::{EventContext, Journal, ServiceSample};
use piqueld_core::EnvironmentId;
use piqueld_core::observability::EventFilter;
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Events read from the journal at once.
const EVENT_PAGE: usize = 100;

/// How long names stay cached, so renames reach labels.
const NAMES_TTL: Duration = Duration::from_secs(300);

/// Names labelling an environment's telemetry, looked up when first needed
/// and forgotten every [`NAMES_TTL`].
pub(super) struct Names {
    names: HashMap<EnvironmentId, EnvironmentNames>,
    since: std::time::Instant,
}

/// The names labelling one environment's telemetry.
#[derive(Clone)]
pub(super) struct EnvironmentNames {
    pub(super) application: String,
    pub(super) environment: String,
    /// The preview's slug, for previews.
    pub(super) preview: Option<String>,
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
    /// service gauges every 30 seconds, and exports the containers' output
    /// and usage through `docker` when enabled, until cancelled. Each runs on
    /// its own, so a slow Docker observation never holds up the journal or
    /// export health, and none holds up reconciliation or requests. Without
    /// telemetry, only closes a failure left from when it was configured.
    /// Without container export, forgets where it stopped reading, so
    /// enabling it again starts from the present.
    pub(super) async fn export_telemetry(
        &self,
        start: JournalPosition,
        docker: super::containers::Docker,
        cancellation: CancellationToken,
    ) {
        if !self.telemetry.containers()
            && let Err(error) = self.store.clear_container_cursors().await
        {
            tracing::error!(?error, "container log cursors could not be cleared");
        }
        let Some(journal) = self.telemetry.journal(start.running) else {
            // Close a failure left open while export was configured.
            let healthy = self.observe_export(false).await;
            if let Err(error) = healthy {
                tracing::error!(?error, "telemetry export health could not be recorded");
            }
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
            async { while step(&mut daemon_tick, &cancellation, self.sample_daemon()).await {} },
            async {
                while step(&mut services_tick, &cancellation, self.sample_services()).await {}
            },
            async {
                if self.telemetry.containers() {
                    self.export_containers(docker, &cancellation).await;
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

    /// Records whether export is `failing`, as the `telemetry_export_failed`
    /// condition.
    async fn observe_export(&self, failing: bool) -> Result<(), StoreError> {
        self.store
            .observe_condition("telemetry_export_failed", None, failing, now_ms(), 90_000)
            .await
    }

    /// Samples daemon statistics for the gauges, and reports a collector
    /// that keeps failing.
    async fn sample_daemon(&self) -> Result<(), StoreError> {
        self.observe_export(self.telemetry.failing()).await?;
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
    pub(super) async fn get(
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
pub(super) fn interval(period: Duration) -> tokio::time::Interval {
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick
}

/// Runs `work` at the next `tick`, logging its failure, which is retried the
/// next tick. Returns `false` once cancelled, abandoning `work`.
pub(super) async fn step(
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
