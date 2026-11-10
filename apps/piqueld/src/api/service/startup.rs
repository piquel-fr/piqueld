//! Production service construction and reconciliation startup.

use super::ApplicationService;
use crate::{operations::OperationError, reconcile::Controller};
use anyhow::Context;
use piqueld_config::DaemonConfig;
use piqueld_core::manifest::Hostname;
use piqueld_docker::{BollardDocker, DockerApi};
use piqueld_store::{Store, StoreError};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Longest wait between attempts to reach Docker as a Swarm manager, and so
/// how long readiness, which probes Docker directly, can report ready before
/// reconciliation starts.
const MAX_SWARM_RETRY_DELAY: Duration = Duration::from_secs(10);

impl ApplicationService {
    /// Opens persistence, initializes authentication, and starts
    /// reconciliation and ingress once Docker is a Swarm manager, serving
    /// without it until then.
    ///
    /// The process must hold its data-directory lock and bind its listeners first.
    /// Cancelling the supplied token stops both workers; the returned task must
    /// be joined before releasing the lock. Caddy keeps serving after normal shutdown.
    /// A controller failure cancels the token so all transports shut down together.
    ///
    /// Steps, in order:
    /// 1. Open the store, start `telemetry` export, noting where the journal
    ///    stands so it exports what this start journals, and initialize
    ///    authentication.
    /// 2. Reserve the website's and webhooks' hostnames so applications
    ///    cannot route them.
    /// 3. Create the Docker client, which needs neither Docker nor its socket
    ///    yet, register the build machine if one is configured, and
    ///    interrupt stale actions.
    /// 4. Build the ingress, reconciliation controller, and service.
    /// 5. Bind the webhook listener, then record the daemon start, which
    ///    notifies as `daemon_started`.
    /// 6. Spawn one task joining notification observation, notification
    ///    webhook delivery, sync, the listener receiving GitHub webhooks,
    ///    telemetry export from the journal, and, once `establish_swarm`
    ///    reaches a Swarm manager, reconciliation, image cleanup and ingress.
    ///    Until then requested operations wait. Readiness probes Docker
    ///    directly, so it reports ready up to `MAX_SWARM_RETRY_DELAY` before
    ///    the next attempt starts them.
    /// # Errors
    /// Returns contextual storage, authentication, or client initialization errors.
    pub async fn start(
        config: &DaemonConfig,
        telemetry: crate::telemetry::Telemetry,
        cancellation: CancellationToken,
    ) -> anyhow::Result<(Self, crate::auth::Auth, JoinHandle<Result<(), StoreError>>)> {
        let (store, journaled) = Self::open_store(config, &telemetry).await?;
        let auth = crate::auth::Auth::initialize(&store, config).await?;
        let reserved = Self::installation_hostnames(config)?;
        for hostname in store.reserve_installation_hostnames(&reserved).await? {
            tracing::error!(%hostname, "route hostname is reserved for the piqueld website or its webhooks; the gateway will not publish it");
        }
        let docker = Arc::new(Self::connect_docker(config).await?);
        // Actions left open by a previous process can never finish; close them first.
        store.interrupt_actions(None).await?;
        store.restart_failure_windows().await?;
        let ingress = Arc::new(crate::ingress::Ingress::from_config(
            config,
            Arc::clone(&store),
        )?);
        let sources = Arc::new(
            crate::git::Sources::open(&config.server.data_dir)
                .await
                .context("failed to prepare the Git checkouts and snapshots directories")?
                .configured(config),
        );
        let wake = Arc::new(Notify::new());
        let reconciler = Controller::new(
            Arc::clone(&docker),
            Arc::clone(&store),
            Arc::clone(&sources),
        )
        .with_config(&config.reconciliation)
        .with_images(&config.images)
        .with_ingress(Arc::clone(&ingress))
        .with_telemetry(telemetry.sockets().cloned());
        let service = Self::new(store, reconciler.runtime(Arc::clone(&wake)), sources)?
            .with_image_status(reconciler.image_status())
            .with_builder(docker.builder().cloned())
            .with_configuration(config)
            .with_ingress(Arc::clone(&ingress))
            .with_webhook_hostname(config.ingress.webhook_hostname.clone())
            .with_public_url(config.public_url())
            .with_telemetry(telemetry)
            .with_shutdown(cancellation.clone());
        let webhook_listener = ingress
            .bind_webhooks()
            .await
            .context("failed to bind the webhook socket")?;
        // Every listener is bound, so the daemon can serve.
        service
            .store
            .record_daemon_started(env!("CARGO_PKG_VERSION"))
            .await
            .context("failed to record the daemon start")?;
        let scan_interval = Duration::from_secs(config.reconciliation.scan_interval_seconds);
        let finished_operation_days = config.retention.finished_operation_days;
        let event_days = config.retention.event_days;
        let auto_initialize_swarm = config.docker.auto_initialize_swarm;
        let background = service.clone();
        let controller = tokio::spawn(async move {
            // Also cancel on panic so callers cannot wait forever on a failed worker.
            let _cancel_on_drop = cancellation.clone().drop_guard();
            // Workers driving Docker start once it is a Swarm manager; until
            // then the API, notifications and sync already serve.
            let runtime = async {
                let ready = background
                    .establish_swarm(&docker, auto_initialize_swarm, &cancellation)
                    .await
                    .inspect_err(|_| cancellation.cancel())?;
                if !ready {
                    return Ok(());
                }
                let reconcile = async {
                    let result = reconciler
                        .run(
                            wake,
                            scan_interval,
                            finished_operation_days,
                            event_days,
                            cancellation.child_token(),
                        )
                        .await;
                    cancellation.cancel();
                    result
                };
                let (result, (), ()) = tokio::join!(
                    reconcile,
                    reconciler.run_image_cleanup(cancellation.child_token()),
                    ingress.run(cancellation.child_token()),
                );
                result
            };
            let (result, (), (), (), (), ()) = tokio::join!(
                runtime,
                background
                    .observe_notifications(scan_interval.as_secs(), cancellation.child_token()),
                background.deliver_notifications(cancellation.child_token()),
                background.sync(cancellation.child_token()),
                background.serve_webhooks(webhook_listener, cancellation.child_token()),
                background.export_telemetry(journaled, cancellation.child_token())
            );
            result
        });
        Ok((service, auth, controller))
    }

    /// Waits until Docker Engine answers as a single-node Swarm manager,
    /// initializing one when `auto_initialize`, however long that takes.
    /// Attempts are journaled under one daemon-scoped `ensure_swarm` action,
    /// with failures recorded as retries backing off up to
    /// `MAX_SWARM_RETRY_DELAY`. Returns whether Swarm is ready: `false` once
    /// `cancellation` fires.
    async fn establish_swarm(
        &self,
        docker: &BollardDocker,
        auto_initialize: bool,
        cancellation: &CancellationToken,
    ) -> Result<bool, StoreError> {
        let action = self.store.begin_action(None, "ensure_swarm", None).await?;
        let mut delay = Duration::from_secs(1);
        let mut attempt = 0_u32;
        let error = loop {
            attempt = attempt.saturating_add(1);
            self.store.action_request(&action, attempt).await?;
            let result = tokio::select! {
                () = cancellation.cancelled() => break OperationError::Cancelled,
                result = docker.ensure_swarm(auto_initialize) => result,
            };
            let diagnostic = match result {
                Ok(_) => {
                    self.store.finish_action(&action, None).await?;
                    info!(
                        auto_initialize_swarm = auto_initialize,
                        "connected to Docker Engine as a single-node Swarm manager"
                    );
                    return Ok(true);
                }
                Err(error) => OperationError::from(error).diagnostic(),
            };
            self.store
                .action_retry(&action, attempt, delay, &diagnostic)
                .await?;
            tracing::warn!(
                diagnostic_id = %diagnostic.id,
                attempt,
                retry_delay_ms = delay.as_secs_f64() * 1000.0,
                "Docker Swarm is not ready; deployments wait for it"
            );
            tokio::select! {
                () = cancellation.cancelled() => break OperationError::Cancelled,
                () = tokio::time::sleep(delay) => {}
            }
            delay = delay.saturating_mul(2).min(MAX_SWARM_RETRY_DELAY);
        };
        self.store
            .finish_action(&action, Some(error.diagnostic()))
            .await?;
        Ok(false)
    }

    /// Configures the Docker Engine client, which builds on the build machine
    /// when one is configured. Neither need be reachable yet:
    /// `establish_swarm` waits for Docker and its socket, and registering the
    /// build machine only records its address.
    async fn connect_docker(config: &DaemonConfig) -> anyhow::Result<BollardDocker> {
        let docker = BollardDocker::connect(&config.docker.socket)
            .context("failed to configure the Docker Engine client")?;
        let Some(remote) = &config.build.remote else {
            return Ok(docker);
        };
        let builder = piqueld_docker::RemoteBuilder::register(remote, &config.server.data_dir)
            .await
            .context("failed to register the build machine with Docker Buildx")?;
        Ok(docker.with_builder(builder))
    }

    /// Opens the control-plane state with the daemon's settings, starts
    /// `telemetry` export now that it names the installation, so even a
    /// start that fails later is exported, and reads where the journal stands
    /// before this start changes it.
    async fn open_store(
        config: &DaemonConfig,
        telemetry: &crate::telemetry::Telemetry,
    ) -> anyhow::Result<(Arc<Store>, piqueld_store::JournalPosition)> {
        let store = Store::open(config.server.database_path())
            .await
            .context("failed to open control-plane state")?
            .with_build_history(config.build_history.clone())
            .with_observability(config)
            .with_previews(config.previews.clone());
        info!(path = %config.server.database_path().display(), "opened control-plane state");
        telemetry.start(store.instance_id());
        let journaled = store.journal_position().await?;
        // A daemon stopped as a deletion ended may have left the deleted
        // environment's socket directories behind.
        if let Some(sockets) = telemetry.sockets() {
            for id in sockets.environments() {
                if matches!(store.get(&id).await, Err(StoreError::NotFound)) {
                    sockets.remove(&id);
                }
            }
        }
        telemetry.receive(store.telemetry_receivers()).await?;
        Ok((Arc::new(store), journaled))
    }

    /// The hostnames the installation serves itself, which application routes
    /// may not claim, nor their subdomains: the website's, which could
    /// otherwise act on its passkeys or cookies, the dashboard's, and the
    /// webhooks'.
    fn installation_hostnames(config: &DaemonConfig) -> anyhow::Result<Vec<Hostname>> {
        let website = piqueld_config::AuthConfig::validate_origin(config.public_url())?
            .domain()
            .and_then(|host| Hostname::parse(host.trim_end_matches('.')).ok());
        let dashboard = config.ingress.private.dashboard_hostname.clone();
        let webhooks = config.ingress.webhook_hostname.clone();
        // The website is usually the dashboard hostname too.
        let hostnames: std::collections::BTreeSet<_> = website
            .into_iter()
            .chain(dashboard)
            .chain(webhooks)
            .collect();
        Ok(hostnames.into_iter().collect())
    }
}
