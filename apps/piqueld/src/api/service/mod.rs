//! Shared daemon operations, independent of transport adapters.

mod admission;
mod checks;
mod exec;
mod history;
mod notification_message;
mod notification_worker;
mod observability;
mod previews;
mod promotions;
mod queries;
mod restores;
mod startup;
mod sync;
mod system;
mod telemetry;
mod views;
mod webhooks;

use crate::application::{BoundaryError, RuntimeBoundary};
pub use admission::Expensive;
pub use exec::ExecSession;
pub use history::ManifestExport;
use piqueld_core::{
    ApplicationId, EnvironmentId,
    access::Grants,
    api::{SecretAccess, SecretMetadata, StoredSecret},
};
use piqueld_store::{Store, StoreError};
use std::sync::Arc;
pub use webhooks::{Delivery, WEBHOOK_BODY_LIMIT, WEBHOOK_PATH, WebhookError};

/// Errors returned by transport-independent daemon operations.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    /// A mutation needs an inspected revision and identity or an explicit force override.
    #[error("mutation preconditions are required")]
    PreconditionRequired,
    /// Application pagination is outside its supported bounds or has an invalid cursor.
    #[error("pagination parameters are invalid")]
    InvalidPagination,
    /// Workload log bounds or service filter are invalid.
    #[error("invalid log query")]
    InvalidLogQuery,
    /// The requested service has no running task to execute a command in.
    #[error("service has no running task")]
    ServiceNotRunning,
    /// No effective host configuration was attached to this service.
    #[error("effective host configuration is unavailable")]
    ConfigurationUnavailable,
    /// Saved configuration could not be rendered as TOML.
    #[error("could not render saved configuration")]
    ManifestSerialization(#[source] toml::ser::Error),
    /// Persistence failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The runtime plan contains a known conflict.
    #[error("runtime plan contains blocking conflicts")]
    PlanBlocked(Vec<piqueld_core::PlanDiagnostic>),
    /// Image resolution or compilation failed.
    #[error(transparent)]
    Runtime(#[from] BoundaryError),
    /// The manifest repository could not be read, so no branch is known gone.
    #[error("the manifest repository could not be read")]
    RepositoryUnavailable(#[source] anyhow::Error),
    /// An uploaded local snapshot was refused.
    #[error(transparent)]
    Snapshot(#[from] crate::git::snapshot::SnapshotError),
    /// As much of this kind of expensive work already runs as it may.
    #[error("too much {0:?} work is running")]
    Busy(Expensive),
}

pub use piqueld_store::{
    Actor, ListedHead, Mutation, MutationResponse, PreviewMutation, PromotionMutation, Restore,
    SourceChoice, SyncOutcome,
};

/// Cheaply clonable entry point for all daemon operations.
///
/// HTTP, MCP, and scheduled jobs share this service; adapters only decode inputs
/// and encode results. Runtime and persistence stay private to this layer.
///
/// This service validates request IDs and names, commits intent and the replay
/// receipt atomically through the store, then wakes reconciliation. Keeping this
/// here makes those callers share the same acceptance rules without requiring
/// the database layer to know about the controller.
#[derive(Clone)]
pub struct ApplicationService {
    /// Managed ingress state, reported by readiness and used by plan previews.
    ingress: Option<Arc<crate::ingress::Ingress>>,
    tailnet: Option<tokio::sync::watch::Receiver<piqueld_core::api::TailnetStatus>>,
    /// The built images as of the last cleanup; `None` without a controller.
    images: Option<tokio::sync::watch::Receiver<piqueld_core::api::ImageStatus>>,
    /// Effective host settings; `None` unless attached at startup.
    configuration: Option<Arc<Configuration>>,
    store: Arc<Store>,
    /// Receives local snapshots and reports checkout limits.
    sources: Arc<crate::git::Sources>,
    /// Docker/Swarm adapter used for runtime requests and reconciliation wakeups.
    runtime: Arc<dyn RuntimeBoundary>,
    /// Export of the daemon's traces, metrics and logs; disabled by default.
    telemetry: crate::telemetry::Telemetry,
    /// Slots for audit records waiting to be written.
    audit_backlog: Arc<tokio::sync::Semaphore>,
    /// The share of `audit_backlog` anonymous records may hold.
    anonymous_backlog: Arc<tokio::sync::Semaphore>,
    /// Audit records dropped or not written since the last one written (see
    /// [`Self::record_audit`]).
    audit_lost: Arc<std::sync::atomic::AtomicU64>,
    /// Credential and network pairs already noted (see `observe_address`).
    known_addresses: Arc<observability::KnownAddresses>,
    /// Applications a webhook said moved, waiting for the sync worker.
    hints: Arc<sync::Hints>,
    /// Public hostname serving webhook deliveries, `ingress.webhook_hostname`.
    webhook_hostname: Option<piqueld_core::manifest::Hostname>,
    /// Cancelled when the daemon shuts down, ending long-lived connections.
    shutdown: tokio_util::sync::CancellationToken,
    /// How those long-lived connections are paced.
    connection_timing: crate::api::http::ConnectionTiming,
    /// Branch listings shared by concurrent preview reads.
    listings: Arc<crate::git::Listings>,
    /// Budgets of expensive work (see [`Self::admit`]).
    admission: Arc<admission::Admission>,
    /// Sends notifications, queued or tests.
    notifier: notification_worker::Notifier,
    /// The website's origin, `auth.public_url`, which notifications link to.
    public_url: String,
}

/// Effective host settings, as attached by
/// [`ApplicationService::with_configuration`].
struct Configuration {
    /// Read-only settings for the API.
    view: piqueld_core::api::HostConfiguration,
    /// What installation checks inspect.
    checks: checks::CheckSettings,
}

/// Audit records allowed to wait for the writer at once.
pub(crate) const AUDIT_BACKLOG: usize = 1024;

impl ApplicationService {
    /// Checks what submitting `mutation` needs before any state is read or
    /// work is done for it: the revision precondition unless `force`d (see
    /// `lacks_precondition`), a well-formed idempotency key `request_id`, and
    /// a valid name for a rename.
    ///
    /// # Errors
    /// `PreconditionRequired`, or invalid input.
    fn check_submission(
        mutation: &Mutation,
        expected_generation: Option<u64>,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<(), ApplicationError> {
        if !force && mutation.lacks_precondition(expected_generation) {
            return Err(ApplicationError::PreconditionRequired);
        }
        // Request IDs are idempotency keys: 1–128 chars of `[A-Za-z0-9-_.:]`.
        if request_id.is_some_and(|id| {
            id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        }) {
            return Err(StoreError::InvalidInput.into());
        }
        if let Mutation::Rename { name, .. } = mutation
            && !piqueld_core::valid_logical_name(name)
        {
            return Err(StoreError::InvalidInput.into());
        }
        Ok(())
    }

    /// Creates a service over supplied storage and runtime adapters.
    /// No background work is started; use [`Self::start`] for daemon startup.
    ///
    /// # Errors
    /// Returns the webhook HTTP client's initialization error with context.
    pub fn new(
        store: Arc<Store>,
        runtime: Arc<dyn RuntimeBoundary>,
        sources: Arc<crate::git::Sources>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context;

        Ok(Self {
            store,
            runtime,
            sources,
            configuration: None,
            ingress: None,
            tailnet: None,
            images: None,
            telemetry: crate::telemetry::Telemetry::default(),
            audit_backlog: Arc::new(tokio::sync::Semaphore::new(AUDIT_BACKLOG)),
            anonymous_backlog: Arc::new(tokio::sync::Semaphore::new(AUDIT_BACKLOG / 2)),
            audit_lost: Arc::default(),
            known_addresses: Arc::default(),
            hints: Arc::default(),
            webhook_hostname: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            connection_timing: crate::api::http::ConnectionTiming::default(),
            listings: Arc::default(),
            admission: Arc::default(),
            notifier: notification_worker::Notifier::new()
                .context("initialize the notification webhook client")?,
            public_url: String::new(),
        })
    }

    /// Links notifications to the dashboard at `public_url`, the website's origin.
    #[must_use]
    pub fn with_public_url(mut self, public_url: &str) -> Self {
        public_url
            .trim_end_matches('/')
            .clone_into(&mut self.public_url);
        self
    }

    /// Ends long-lived connections, like event streams and commands, once
    /// `shutdown` is cancelled.
    #[must_use]
    pub fn with_shutdown(mut self, shutdown: tokio_util::sync::CancellationToken) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Cancelled when the daemon shuts down.
    #[must_use]
    pub fn shutdown(&self) -> &tokio_util::sync::CancellationToken {
        &self.shutdown
    }

    /// Paces long-lived connections with `timing` instead of the defaults.
    #[must_use]
    pub fn with_connection_timing(mut self, timing: crate::api::http::ConnectionTiming) -> Self {
        self.connection_timing = timing;
        self
    }

    /// How long-lived connections are paced.
    #[must_use]
    pub(crate) fn connection_timing(&self) -> crate::api::http::ConnectionTiming {
        self.connection_timing
    }

    /// Notifies after every change to accounts, credentials, or grants.
    #[must_use]
    pub fn access_changes(&self) -> tokio::sync::watch::Receiver<()> {
        self.store.access_changes()
    }

    /// `actor`'s current grants, for connections that outlive a request.
    /// # Errors
    /// Returns a revoked credential or storage errors.
    pub async fn grants(&self, actor: Actor<'_>) -> Result<Grants, ApplicationError> {
        Ok(self.store.grants(actor).await?)
    }

    /// Names the public hostname webhook deliveries arrive on, for the
    /// payload URLs shown to users.
    #[must_use]
    pub fn with_webhook_hostname(
        mut self,
        hostname: Option<piqueld_core::manifest::Hostname>,
    ) -> Self {
        self.webhook_hostname = hostname;
        self
    }

    /// Reports the tailnet node's latest status, when the daemon runs one.
    #[must_use]
    pub fn with_tailnet(
        mut self,
        status: Option<tokio::sync::watch::Receiver<piqueld_core::api::TailnetStatus>>,
    ) -> Self {
        self.tailnet = status;
        self
    }

    /// Exports through `telemetry`, already started, and reports its state.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: crate::telemetry::Telemetry) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Telemetry export, which records API latency.
    #[must_use]
    pub fn telemetry(&self) -> &crate::telemetry::Telemetry {
        &self.telemetry
    }

    /// Reports image cleanup's latest status.
    #[must_use]
    pub fn with_image_status(
        mut self,
        status: tokio::sync::watch::Receiver<piqueld_core::api::ImageStatus>,
    ) -> Self {
        self.images = Some(status);
        self
    }

    /// Shares managed ingress state with transport-independent daemon operations.
    #[must_use]
    pub fn with_ingress(mut self, ingress: Arc<crate::ingress::Ingress>) -> Self {
        self.ingress = Some(ingress);
        self
    }

    /// Attaches the effective host configuration for read-only API
    /// inspection and installation checks.
    #[must_use]
    pub fn with_configuration(mut self, config: &piqueld_config::DaemonConfig) -> Self {
        self.configuration = Some(Arc::new(Configuration {
            view: config.view(),
            checks: checks::CheckSettings::from(config),
        }));
        self
    }

    /// Recovers from a lost master key by discarding every stored and generated
    /// secret value and every webhook secret; see `Store::recover_secret_key`.
    /// # Errors
    /// Returns an error if the current key still works or is not lost, or
    /// storage errors.
    pub async fn recover_secret_key(
        &self,
        actor: Actor<'_>,
    ) -> Result<piqueld_core::api::SecretKeyRecovery, ApplicationError> {
        Ok(self.store.recover_secret_key(actor).await?)
    }

    /// Lists an environment's generated secrets without exposing their values.
    ///
    /// # Errors
    /// Returns a storage error when the environment or its metadata cannot be read.
    pub async fn secrets(
        &self,
        environment: &EnvironmentId,
    ) -> Result<Vec<SecretMetadata>, ApplicationError> {
        Ok(self.store.secrets(environment).await?)
    }

    /// Lists an application's stored secrets and their access, never values.
    ///
    /// # Errors
    /// Returns a storage error when the application or its metadata cannot be read.
    pub async fn stored_secrets(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<StoredSecret>, ApplicationError> {
        Ok(self.store.stored_secrets(application).await?)
    }

    /// Stores a new version of an application's secret after checking the
    /// inspected generation; `access` replaces its access list.
    ///
    /// # Errors
    /// Returns a validation, generation conflict, or storage error.
    pub async fn put_stored_secret(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        expected_generation: i64,
        value: Vec<u8>,
        access: Option<&SecretAccess>,
    ) -> Result<StoredSecret, ApplicationError> {
        Ok(self
            .store
            .put_stored_secret(actor, application, name, expected_generation, value, access)
            .await?)
    }

    /// Replaces the access list of an application's secret.
    ///
    /// # Errors
    /// Returns absence, unknown environments, or storage errors.
    pub async fn set_secret_access(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        access: &SecretAccess,
    ) -> Result<StoredSecret, ApplicationError> {
        Ok(self
            .store
            .set_secret_access(actor, application, name, access)
            .await?)
    }

    /// Generates a new version of an environment's generated secret after
    /// checking the inspected generation; see `Store::regenerate_secret`.
    ///
    /// # Errors
    /// Returns absence, generation conflict, or storage errors.
    pub async fn regenerate_secret(
        &self,
        actor: Actor<'_>,
        environment: &EnvironmentId,
        name: &str,
        expected_generation: i64,
    ) -> Result<SecretMetadata, ApplicationError> {
        Ok(self
            .store
            .regenerate_secret(actor, environment, name, expected_generation)
            .await?)
    }

    /// Removes an unreferenced generated secret and all of its runtime versions,
    /// holding off the controller's Docker mutations throughout (see
    /// `RuntimeBoundary::lock_mutations`). See `remove_secret_versions`.
    ///
    /// # Errors
    /// Returns when the secret is referenced or storage or runtime cleanup fails.
    pub async fn delete_secret(
        &self,
        actor: Actor<'_>,
        environment: &EnvironmentId,
        name: &str,
        expected_generation: i64,
    ) -> Result<(), ApplicationError> {
        let _mutations = self.runtime.lock_mutations().await;
        let deletion = self
            .store
            .begin_secret_deletion(actor, environment, name, expected_generation)
            .await?;
        self.remove_secret_versions(actor, name, &deletion).await?;
        self.store
            .finish_secret_deletion(actor.attribution(), environment, name, &deletion.id)
            .await?;
        Ok(())
    }

    /// Removes an application's secret that no environment uses, and every
    /// environment's Docker secrets for it, holding off the controller's
    /// Docker mutations as `delete_secret` does. See `remove_secret_versions`.
    ///
    /// # Errors
    /// Returns when the secret is referenced or storage or runtime cleanup fails.
    pub async fn delete_stored_secret(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        expected_generation: i64,
    ) -> Result<(), ApplicationError> {
        let _mutations = self.runtime.lock_mutations().await;
        let deletion = self
            .store
            .begin_stored_secret_deletion(actor, application, name, expected_generation)
            .await?;
        self.remove_secret_versions(actor, name, &deletion).await?;
        self.store
            .finish_stored_secret_deletion(actor.attribution(), application, name, &deletion.id)
            .await?;
        Ok(())
    }

    /// Removes the Docker secrets of a reserved deletion, environment by
    /// environment:
    ///
    /// 1. Journals a `remove_secrets` action by `actor` and removes the Swarm
    ///    secret versions.
    /// 2. Records the action outcome.
    ///
    /// A failed runtime cleanup leaves the reservation in place, so retrying the
    /// deletion resumes it (see `secret_deleting`).
    async fn remove_secret_versions(
        &self,
        actor: Actor<'_>,
        name: &str,
        deletion: &piqueld_store::SecretDeletion,
    ) -> Result<(), ApplicationError> {
        for (environment, versions) in &deletion.versions {
            let journal = self
                .store
                .begin_application_action(
                    actor.attribution(),
                    environment,
                    "remove_secrets",
                    Some(name),
                )
                .await?;
            let result = match self.store.action_request(&journal, 1).await {
                Ok(()) => self.runtime.remove_secrets(environment, versions).await,
                Err(error) => Err(error.into()),
            };
            self.store
                .finish_action(
                    &journal,
                    result.as_ref().err().map(BoundaryError::diagnostic),
                )
                .await?;
            result?;
        }
        Ok(())
    }

    /// Accepts a mutation from `actor` and records its receipt in the same
    /// transaction.
    /// `expected_generation` is the last inspected intent revision: zero requires
    /// absence. Mutations other than reconcile require a revision; saves
    /// also require the inspected identity when the revision is nonzero. An
    /// explicit force override bypasses revision and name-based identity checks.
    /// `request_id` is the caller's idempotency key, not an operation ID: replay
    /// returns the original response, including for rename which has no operation.
    /// # Errors
    /// Returns validation, conflict, or persistence errors.
    pub async fn accept(
        &self,
        actor: Actor<'_>,
        mutation: Mutation,
        expected_generation: Option<u64>,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<MutationResponse, ApplicationError> {
        Self::check_submission(&mutation, expected_generation, force, request_id)?;
        let receivers = mutation.changes_receivers();
        let stops = matches!(mutation, Mutation::Stop { .. });
        let started = match &mutation {
            Mutation::Reconcile { id } => Some(id.clone()),
            _ => None,
        };
        let (response, wake) =
            Box::pin(
                self.store
                    .accept(actor, mutation, expected_generation, force, request_id),
            )
            .await?;
        if stops && let Some(ingress) = &self.ingress {
            // Withdraws its routes before answering, so they no longer
            // reach services about to stop; the gateway retries failures.
            if let Err(error) = ingress.refresh().await {
                tracing::warn!(?error, "withdrawing a stopped environment's routes failed");
            }
        }
        if wake {
            self.runtime.trigger_reconciliation();
        }
        // Sync skips a stopped environment, and webhooks don't come again:
        // list its repository now, so a start deploys the head it missed.
        // The worker coalesces hints, so reconciling an environment that
        // wasn't stopped costs at most one listing. Best effort: it was
        // accepted already.
        if let Some(id) = started
            && let Ok(stored) = self.store.get(&id).await
        {
            self.hints.hint(stored.environment.application_id);
        }
        if receivers {
            // Stops export to a revoked or deleted receiver before
            // answering, then lets the controller mount or unmount sockets.
            self.receive_telemetry().await?;
            self.runtime.trigger_full_scan();
        }
        Ok(response)
    }
}

#[cfg(test)]
impl ApplicationService {
    /// A service over `store`, with its Git sources in `dir`, whose runtime
    /// never reaches Docker: its socket in `dir` does not exist, so any
    /// Docker call fails at once.
    pub(crate) async fn unconnected(store: Arc<Store>, dir: &std::path::Path) -> Self {
        let docker =
            Arc::new(crate::docker::BollardDocker::connect(&dir.join("docker.sock")).unwrap());
        let sources = Arc::new(crate::git::Sources::open(dir).await.unwrap());
        let runtime =
            crate::reconcile::Controller::new(docker, Arc::clone(&store), Arc::clone(&sources))
                .runtime(Arc::new(tokio::sync::Notify::new()));
        Self::new(store, runtime, sources).unwrap()
    }
}
