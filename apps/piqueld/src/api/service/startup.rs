//! Production service construction and reconciliation startup.

use super::ApplicationService;
use crate::{
    config::DaemonConfig,
    docker::{BollardDocker, DockerApi},
    reconcile::Controller,
    store::{Store, StoreError},
};
use anyhow::Context;
use piqueld_core::manifest::Hostname;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::info;

impl ApplicationService {
    /// Opens persistence, initializes authentication, connects Docker, and starts
    /// reconciliation and ingress.
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
    /// 3. Connect Docker, interrupt stale actions, and ensure a Swarm manager
    ///    (journaled as an `ensure_swarm` action).
    /// 4. Build the ingress, reconciliation controller, and service.
    /// 5. Bind the webhook listener, then record the daemon start, which
    ///    notifies as `daemon_started`.
    /// 6. Spawn one task joining reconciliation, image cleanup, ingress,
    ///    notification observation, notification webhook delivery, sync, the
    ///    listener receiving GitHub webhooks, and telemetry export from the
    ///    journal.
    /// # Errors
    /// Returns contextual storage, Docker connection, or Swarm initialization errors.
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
        let docker = Arc::new(
            BollardDocker::connect(&config.docker.socket)
                .context("failed to connect to Docker Engine")?,
        );
        Self::ensure_swarm(&store, &docker, config).await?;
        store.configure_deliveries().await?;
        // Webhooks never follow redirects, so a target cannot bounce requests elsewhere.
        let webhook_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to initialize webhook client")?;
        info!(
            socket = %config.docker.socket.display(),
            auto_initialize_swarm = config.docker.auto_initialize_swarm,
            "connected to Docker Engine as a single-node Swarm manager"
        );
        let ingress = Arc::new(crate::ingress::Ingress::from_config(
            config,
            Arc::clone(&store),
        )?);
        let wake = Arc::new(Notify::new());
        let reconciler = Controller::new(docker, Arc::clone(&store))
            .with_config(&config.reconciliation)
            .with_images(&config.images)
            .with_ingress(Arc::clone(&ingress))
            .with_telemetry(telemetry.sockets().cloned());
        let service = Self::new(store, reconciler.runtime(Arc::clone(&wake)))
            .with_image_status(reconciler.image_status())
            .with_configuration(config.view())
            .with_ingress(Arc::clone(&ingress))
            .with_webhook_hostname(config.ingress.webhook_hostname.clone())
            .with_telemetry(telemetry)
            .with_shutdown(cancellation.clone());
        let webhook_listener = ingress
            .bind_webhooks()
            .await
            .context("failed to bind the webhook socket")?;
        // Every listener is bound and Swarm is ready, so the daemon can serve.
        service
            .store
            .record_daemon_started(env!("CARGO_PKG_VERSION"))
            .await
            .context("failed to record the daemon start")?;
        let scan_interval = Duration::from_secs(config.reconciliation.scan_interval_seconds);
        let finished_operation_days = config.retention.finished_operation_days;
        let event_days = config.retention.event_days;
        let background = service.clone();
        let controller = tokio::spawn(async move {
            // Also cancel on panic so callers cannot wait forever on a failed worker.
            let _cancel_on_drop = cancellation.clone().drop_guard();
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
            let (result, (), (), (), (), (), (), ()) = tokio::join!(
                reconcile,
                reconciler.run_image_cleanup(cancellation.child_token()),
                ingress.run(cancellation.child_token()),
                background
                    .observe_notifications(scan_interval.as_secs(), cancellation.child_token()),
                background.deliver_notifications(webhook_client, cancellation.child_token()),
                background.sync(cancellation.child_token()),
                background.serve_webhooks(webhook_listener, cancellation.child_token()),
                background.export_telemetry(journaled, cancellation.child_token())
            );
            result
        });
        Ok((service, auth, controller))
    }

    /// Closes actions a previous process left open, which can never finish,
    /// then ensures a Swarm manager, journaled as an `ensure_swarm` action.
    async fn ensure_swarm(
        store: &Store,
        docker: &BollardDocker,
        config: &DaemonConfig,
    ) -> anyhow::Result<()> {
        store.interrupt_actions(None).await?;
        let bootstrap = store.begin_action(None, "ensure_swarm", None).await?;
        store.action_request(&bootstrap, 1).await?;
        let result = docker
            .ensure_swarm(config.docker.auto_initialize_swarm)
            .await;
        store
            .finish_action(
                &bootstrap,
                result.as_ref().err().map(|error| {
                    piqueld_core::observability::Diagnostic::new(
                        crate::operations::diagnostic_id(),
                        error.diagnostic_code(),
                        error.to_string(),
                    )
                }),
            )
            .await?;
        result.context("failed to establish single-node Docker Swarm readiness")?;
        Ok(())
    }

    /// Opens the control-plane state with the daemon's settings, starts
    /// `telemetry` export now that it names the installation, so even a
    /// start that fails later is exported, and reads where the journal stands
    /// before this start changes it.
    async fn open_store(
        config: &DaemonConfig,
        telemetry: &crate::telemetry::Telemetry,
    ) -> anyhow::Result<(Arc<Store>, crate::store::JournalPosition)> {
        let store = Store::open(config.server.database_path())
            .await
            .context("failed to open control-plane state")?
            .with_build_history(config.build_history.clone())
            .with_observability(config)
            .with_previews(config.previews.clone())
            .with_snapshot_uploads(config.snapshots.max_upload_bytes)
            .with_checkouts(config.checkouts.max_bytes);
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
        let website = crate::auth::Auth::validate_origin(config.public_url())?
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
