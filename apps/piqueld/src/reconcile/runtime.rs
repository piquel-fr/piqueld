use super::{
    Arc, BoundaryError, DockerApi, DockerError, InstanceId, NormalizedApplication, Notify,
    PreparedApplication, ResolutionSet, ResolvedSource, RuntimeBoundary, RuntimeLogQuery, Source,
    StoredApplication, compile_application,
};
use crate::api::{PreparedBuild, RuntimeReadiness};
use crate::build::SourceBuilder;
use crate::proxy::{InfrastructureState, IngressApi, IngressSpec};
use crate::secrets::{SecretError, SecretService};
use async_trait::async_trait;
use std::time::Duration;

const PREPARE_TIMEOUT: Duration = Duration::from_mins(5);
const DOCKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
use piqueld_client::{ApplicationLogsOptions, ContainerLogView, ServiceStatusView};
use piqueld_core::resource::{Convergence, TaskDiagnostic, TaskState};

/// Runtime boundary backed by Docker.
pub struct DockerRuntime<D> {
    docker: Arc<D>,
    instance_id: InstanceId,
    wake: Arc<Notify>,
    secret_service: Option<Arc<SecretService>>,
    source_builder: Option<Arc<dyn SourceBuilder>>,
    ingress: Option<(Arc<dyn IngressApi>, IngressSpec)>,
}

impl<D> DockerRuntime<D> {
    /// Creates a Docker runtime adapter.
    #[must_use]
    pub fn new(docker: Arc<D>, instance_id: InstanceId, wake: Arc<Notify>) -> Self {
        Self {
            docker,
            instance_id,
            wake,
            secret_service: None,
            source_builder: None,
            ingress: None,
        }
    }

    /// Enables encrypted logical-secret resolution during preparation.
    #[must_use]
    pub fn with_secret_service(mut self, service: Arc<SecretService>) -> Self {
        self.secret_service = Some(service);
        self
    }

    /// Enables Git/Dockerfile source preparation.
    #[must_use]
    pub fn with_source_builder(mut self, builder: Arc<dyn SourceBuilder>) -> Self {
        self.source_builder = Some(builder);
        self
    }

    /// Enables managed Traefik lifecycle and routed-runtime status.
    #[must_use]
    pub fn with_ingress(mut self, controller: Arc<dyn IngressApi>, spec: IngressSpec) -> Self {
        self.ingress = Some((controller, spec));
        self
    }
}

#[async_trait]
impl<D: DockerApi> RuntimeBoundary for DockerRuntime<D> {
    fn trigger_reconciliation(&self) {
        self.wake.notify_one();
    }

    async fn readiness(&self) -> RuntimeReadiness {
        let swarm = self.docker.ensure_swarm(false).await;
        let (docker, swarm_manager) = match swarm {
            Ok(_) => (true, true),
            Err(_) => (false, false),
        };
        if !docker {
            return RuntimeReadiness {
                docker,
                swarm_manager,
                infrastructure: false,
                reason: Some("Docker Engine or Swarm manager is unavailable".into()),
            };
        }
        let ingress_ready = match &self.ingress {
            Some((controller, spec)) => {
                matches!(
                    controller.status(spec).await,
                    Ok(InfrastructureState::Ready)
                )
            }
            None => true,
        };
        let registry_ready = match &self.source_builder {
            Some(builder) => builder.registry_ready().await.is_ok(),
            None => true,
        };
        let infrastructure = ingress_ready && registry_ready;
        RuntimeReadiness {
            docker,
            swarm_manager,
            infrastructure,
            reason: (!infrastructure)
                .then_some("required registry or ingress infrastructure is not ready".into()),
        }
    }

    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError> {
        tokio::time::timeout(PREPARE_TIMEOUT, async {
            let mut resolutions = ResolutionSet::default();
            let mut builds = Vec::new();
            for service in &application.spec.services {
                let resolved = match &service.source {
                    Source::Image { image } => {
                        let digest_reference = tokio::time::timeout(
                            DOCKER_REQUEST_TIMEOUT,
                            self.docker.resolve_image(image),
                        )
                        .await
                        .map_err(|_| {
                            BoundaryError::Runtime(DockerError::Unavailable("resolve image"))
                        })??;
                        ResolvedSource::Image {
                            requested: image.clone(),
                            digest_reference,
                        }
                    }
                    Source::Git {
                        repository,
                        reference,
                        context,
                        dockerfile,
                    } => {
                        let builder = self
                            .source_builder
                            .as_ref()
                            .ok_or(crate::build::BuildError::Git)?;
                        let built = builder
                            .build_source(
                                application.id.as_str(),
                                &service.name,
                                repository,
                                reference,
                                context,
                                dockerfile,
                            )
                            .await?;
                        builds.push(PreparedBuild {
                            service_name: service.name.clone(),
                            source_commit: built.commit.clone(),
                            image_reference: built.registry_reference.clone(),
                            image_digest: built.digest_reference.clone(),
                            build_key: built.build_key.clone(),
                            context_hash: built.context_hash.clone(),
                            logs: built.logs.clone(),
                        });
                        ResolvedSource::Git {
                            repository: repository.clone(),
                            requested_reference: reference.clone(),
                            commit: built.commit,
                            context: context.clone(),
                            dockerfile: dockerfile.clone(),
                            registry_reference: built.registry_reference,
                            digest_reference: built.digest_reference,
                        }
                    }
                };
                resolutions.sources.insert(service.name.clone(), resolved);
            }

            let names = application
                .logical_secret_references()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !names.is_empty() {
                let service = self
                    .secret_service
                    .as_ref()
                    .ok_or(BoundaryError::Secrets(SecretError::KeyUnavailable))?;
                for generation in service.current_generations(names, &application.id).await? {
                    resolutions
                        .secrets
                        .insert(generation.logical_name.clone(), generation);
                }
            }

            let resolved = compile_application(application, self.instance_id.clone(), &resolutions)
                .map_err(BoundaryError::Compilation)?;
            let observed =
                tokio::time::timeout(DOCKER_REQUEST_TIMEOUT, self.docker.observe(&application.id))
                    .await
                    .map_err(|_| {
                        BoundaryError::Runtime(DockerError::Unavailable("observe application"))
                    })??;
            Ok(PreparedApplication {
                resolved,
                observed,
                builds,
            })
        })
        .await
        .map_err(|_| BoundaryError::Runtime(DockerError::Unavailable("prepare application")))?
    }

    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<piqueld_core::ObservedApplication, BoundaryError> {
        Ok(self.docker.observe(&application.application.id).await?)
    }

    async fn runtime_status(
        &self,
        application: &StoredApplication,
    ) -> Result<Vec<ServiceStatusView>, BoundaryError> {
        let observed = self.docker.observe(&application.application.id).await?;
        Ok(application
            .resolved
            .services
            .iter()
            .map(|desired| {
                let found = observed.services.iter().find(|service| {
                    service.name
                        == piqueld_core::docker_resource_name(
                            &application.application.id,
                            piqueld_core::ResourceKind::Service,
                            Some(&desired.logical_name),
                        )
                });
                let running_replicas = found
                    .map(|service| {
                        service
                            .tasks
                            .iter()
                            .filter(|task| {
                                task.desired_running
                                    && task.state == TaskState::Running
                                    && task.healthy != Some(false)
                            })
                            .count()
                    })
                    .and_then(|count| u16::try_from(count).ok())
                    .unwrap_or_default();
                let state = found.map_or_else(
                    || "missing".into(),
                    |service| match service.convergence {
                        Convergence::Converged => "converged".into(),
                        Convergence::Updating => "updating".into(),
                        Convergence::Degraded => "degraded".into(),
                        Convergence::Failed => "failed".into(),
                    },
                );
                let diagnostic = found.and_then(|service| {
                    service.tasks.iter().find_map(|task| {
                        task.diagnostic.as_ref().map(|diagnostic| match diagnostic {
                            TaskDiagnostic::Failed { exit_code } => exit_code.map_or_else(
                                || "task failed".into(),
                                |code| format!("task failed with exit code {code}"),
                            ),
                            TaskDiagnostic::Rejected => "task rejected by Docker".into(),
                        })
                    })
                });
                ServiceStatusView {
                    service: desired.logical_name.clone(),
                    desired_replicas: desired.replicas,
                    running_replicas,
                    state,
                    diagnostic,
                }
            })
            .collect())
    }

    async fn infrastructure_status(
        &self,
        application: &StoredApplication,
    ) -> Result<Option<String>, BoundaryError> {
        if application.application.spec.routes.is_empty() {
            return Ok(None);
        }
        let Some((controller, spec)) = &self.ingress else {
            return Ok(Some("unavailable".into()));
        };
        Ok(Some(match controller.status(spec).await? {
            InfrastructureState::Ready => "ready".into(),
            InfrastructureState::Degraded { .. } => "degraded".into(),
        }))
    }

    async fn application_logs(
        &self,
        application: &StoredApplication,
        options: &ApplicationLogsOptions,
    ) -> Result<Vec<ContainerLogView>, BoundaryError> {
        let since_seconds = i64::try_from(options.since_seconds.unwrap_or(300)).unwrap_or(i64::MAX);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            });
        let query = RuntimeLogQuery {
            since_seconds: now.saturating_sub(since_seconds),
            tail: usize::from(options.tail.unwrap_or(200)).clamp(1, 1_000),
            max_bytes: usize::try_from(options.max_bytes.unwrap_or(262_144))
                .unwrap_or(262_144)
                .clamp(1, 1_048_576),
        };
        Ok(self
            .docker
            .application_logs(&self.instance_id, &application.application.id, &query)
            .await?
            .into_iter()
            .map(|record| ContainerLogView {
                service: record.service,
                task_id: record.task_id,
                container_id: record.container_id,
                timestamp: record.timestamp,
                stream: record.stream,
                message: record.message,
                display_message: record.display_message,
            })
            .collect())
    }
}
