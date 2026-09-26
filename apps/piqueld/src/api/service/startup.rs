//! Production service construction and reconciliation startup.

use super::ApplicationService;
use crate::{
    config::DaemonConfig,
    docker::{BollardDocker, DockerApi},
    reconcile::Controller,
    store::{Store, StoreError},
};
use anyhow::Context;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::info;

impl ApplicationService {
    /// Opens persistence, connects Docker, and starts reconciliation and ingress.
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
    ) -> anyhow::Result<(Self, JoinHandle<Result<(), StoreError>>)> {
        let store = Arc::new(
            Store::open(config.server.database_path())
                .await
                .context("failed to open control-plane state")?
                .with_build_history(config.build_history.clone()),
        );
        info!(path = %config.server.database_path().display(), "opened control-plane state");
        let docker = Arc::new(
            BollardDocker::connect(&config.docker.socket)
                .context("failed to connect to Docker Engine")?,
        );
        docker
            .ensure_swarm(config.docker.auto_initialize_swarm)
            .await
            .context("Docker Engine is not an active single-node Swarm manager")?;
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
        let controller = tokio::spawn(async move {
            let reconciliation = async {
                // Also cancel on panic so all workers and transports shut down together.
                let _cancel_on_drop = cancellation.clone().drop_guard();
                reconciler
                    .run(
                        wake,
                        scan_interval,
                        finished_operation_days,
                        event_days,
                        cancellation.child_token(),
                    )
                    .await
            };
            let (result, ()) =
                tokio::join!(reconciliation, ingress.run(cancellation.child_token()));
            result
        });
        Ok((service, controller))
    }
}
