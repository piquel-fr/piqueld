//! One-off commands in running service tasks, recorded in environment history.
use super::{Actor, ApplicationError, ApplicationService};
use crate::docker::{Exec, ExecIo};
use piqueld_core::{
    ApplicationId, EnvironmentId, ServiceName, access::AppPermission, auth::HostOperator,
    exec::ExecRequest,
};

/// A created command whose start is already recorded in history.
pub struct ExecSession {
    owner: ApplicationService,
    /// Resolved when the command starts, so its completion stays in the
    /// application's history even if the environment is deleted meanwhile.
    application: ApplicationId,
    environment: EnvironmentId,
    service: ServiceName,
    exec: Exec,
    /// Account and credential, or host operator and its session, that
    /// started the command, which its completion is attributed to as well.
    actor_user_id: Option<String>,
    actor_credential_id: Option<String>,
    actor_operator: Option<HostOperator>,
}

impl ApplicationService {
    /// Creates a command in one running task of `request.service` and records
    /// `who` started it. The command itself may contain secrets and is
    /// never recorded. `actor` needs `apps:exec` on the environment's
    /// application now, not only when it connected.
    /// # Errors
    /// Returns refusal, not found, a service without running tasks, runtime or
    /// storage errors.
    pub async fn exec(
        &self,
        actor: Actor<'_>,
        environment: &EnvironmentId,
        request: &ExecRequest,
        who: &str,
    ) -> Result<ExecSession, ApplicationError> {
        self.store
            .require_on_environment(actor, AppPermission::Exec, environment)
            .await?;
        let application = self
            .store
            .get(environment)
            .await?
            .environment
            .application_id;
        let exec = self
            .runtime
            .create_exec(environment, request)
            .await?
            .ok_or(ApplicationError::ServiceNotRunning)?;
        let attribution = actor.attribution();
        self.store
            .record_environment_event(
                attribution,
                &application,
                environment,
                "command_started",
                &format!("{who} started a command in task {}", exec.task),
                request.service.as_str(),
            )
            .await?;
        Ok(ExecSession {
            owner: self.clone(),
            application,
            environment: environment.clone(),
            service: request.service.clone(),
            exec,
            actor_user_id: attribution.user_id.map(str::to_owned),
            actor_credential_id: attribution.credential_id.map(str::to_owned),
            actor_operator: attribution.operator,
        })
    }
}

impl ExecSession {
    /// Streams the command until it exits, then records its exit code.
    /// # Errors
    /// Returns runtime errors, including a disconnected client.
    pub async fn run(self, io: ExecIo) -> Result<i64, ApplicationError> {
        let result = self.owner.runtime.run_exec(&self.exec, io).await;
        let message = match &result {
            Ok(code) => format!("Command in task {} exited with code {code}", self.exec.task),
            Err(_) => format!("Command stream in task {} failed", self.exec.task),
        };
        if let Err(error) = self
            .owner
            .store
            .record_environment_event(
                crate::store::Attribution {
                    user_id: self.actor_user_id.as_deref(),
                    credential_id: self.actor_credential_id.as_deref(),
                    operator: self.actor_operator,
                    system: None,
                },
                &self.application,
                &self.environment,
                "command_finished",
                &message,
                self.service.as_str(),
            )
            .await
        {
            tracing::error!(?error, task = %self.exec.task, "could not record command completion");
        }
        Ok(result?)
    }
}
