//! Production service construction and reconciliation startup.

use super::ApplicationService;
use crate::{
    config::DaemonConfig,
    docker::{BollardDocker, DockerApi},
    reconcile::{Controller, RetryPolicy},
    store::{Store, StoreError},
};
use anyhow::Context;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::info;

impl ApplicationService {
    /// Opens persistence, initializes authentication, connects Docker, and starts reconciliation.
    ///
    /// The process must hold its data-directory lock and bind its listeners first.
    /// Cancelling the supplied token stops reconciliation; the returned task must
    /// be joined before releasing the lock. A controller failure cancels the token
    /// so all transports shut down together.
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
                .with_build_history(config.build_history.clone()),
        );
        info!(path = %config.server.database_path().display(), "opened control-plane state");
        let auth = crate::auth::Auth::initialize(&store, config).await?;
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
        let wake = Arc::new(Notify::new());
        let reconciler = Controller::new(docker, Arc::clone(&store))
            .with_retry_policy(RetryPolicy {
                convergence_timeout: Duration::from_secs(
                    config.reconciliation.convergence_timeout_seconds,
                ),
                ..RetryPolicy::default()
            })
            .with_prepare_timeout(Duration::from_secs(
                config.reconciliation.prepare_timeout_seconds,
            ));
        let service = Self::new(store, reconciler.runtime(Arc::clone(&wake)))
            .with_configuration(config.view());
        let scan_interval = Duration::from_secs(config.reconciliation.scan_interval_seconds);
        let finished_operation_days = config.retention.finished_operation_days;
        let event_days = config.retention.event_days;
        let controller = tokio::spawn(async move {
            // Also cancel on panic so callers cannot wait forever on a failed worker.
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
        });
        Ok((service, auth, controller))
    }
}
