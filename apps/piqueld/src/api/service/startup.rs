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
    /// # Errors
    /// Returns contextual storage, Docker connection, or Swarm initialization errors.
    pub async fn start(
        config: &DaemonConfig,
        cancellation: CancellationToken,
    ) -> anyhow::Result<(Self, crate::auth::Auth, JoinHandle<Result<(), StoreError>>)> {
        let store = Arc::new(
            Store::open(config.server.database_path())
                .await
                .context("failed to open control-plane state")?
                .with_build_history(config.build_history.clone())
                .with_observability(config),
        );
        info!(path = %config.server.database_path().display(), "opened control-plane state");
        let auth = crate::auth::Auth::initialize(&store, config).await?;
        // Application routes must never serve the website origin or its subdomains,
        // which could otherwise act on its passkeys or cookies.
        let website = crate::auth::Auth::validate_origin(&config.auth.public_url)?
            .domain()
            .and_then(|host| Hostname::parse(host.trim_end_matches('.')).ok());
        for hostname in store
            .reserve_installation_hostnames(website.as_slice())
            .await?
        {
            tracing::error!(%hostname, "route hostname is reserved for the piqueld website; the gateway will not publish it");
        }
        let docker = Arc::new(
            BollardDocker::connect(&config.docker.socket)
                .context("failed to connect to Docker Engine")?,
        );
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
                        format!("diagnostic-{}", uuid::Uuid::now_v7().simple()),
                        error.diagnostic_code(),
                        error.to_string(),
                    )
                }),
            )
            .await?;
        result.context("failed to establish single-node Docker Swarm readiness")?;
        store.configure_deliveries().await?;
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
        let ingress = Arc::new(crate::ingress::Ingress::new(
            config.ingress.enabled,
            &config.docker.socket,
            &config.server.data_dir,
            Arc::clone(&store),
        )?);
        let wake = Arc::new(Notify::new());
        let reconciler = Controller::new(docker, Arc::clone(&store))
            .with_config(&config.reconciliation)
            .with_ingress(Arc::clone(&ingress));
        let service = Self::new(store, reconciler.runtime(Arc::clone(&wake)))
            .with_configuration(config.view())
            .with_ingress(Arc::clone(&ingress));
        let scan_interval = Duration::from_secs(config.reconciliation.scan_interval_seconds);
        let finished_operation_days = config.retention.finished_operation_days;
        let event_days = config.retention.event_days;
        let background = service.clone();
        let scan_seconds = config.reconciliation.scan_interval_seconds;
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
            let (result, (), (), ()) = tokio::join!(
                reconcile,
                ingress.run(cancellation.child_token()),
                background.observe_notifications(scan_seconds, cancellation.child_token()),
                background.deliver_notifications(webhook_client, cancellation.child_token())
            );
            result
        });
        Ok((service, auth, controller))
    }
}
