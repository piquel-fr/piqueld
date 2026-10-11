//! One-off commands in running service tasks, recorded in environment history.
use super::{Actor, ApplicationError, ApplicationService};
use crate::docker::{Exec, ExecIo};
use piqueld_core::{
    ApplicationId, EnvironmentId, ServiceName, access::AppPermission, auth::HostOperator,
    exec::ExecRequest,
};

/// A created command, recorded in history once it runs.
pub struct ExecSession {
    owner: ApplicationService,
    /// Resolved when the command starts, so its completion stays in the
    /// application's history even if the environment is deleted meanwhile.
    application: ApplicationId,
    environment: EnvironmentId,
    service: ServiceName,
    exec: Exec,
    /// Who started the command, as history names them.
    who: String,
    /// Account and credential, or host operator and its session, that
    /// started the command, which its completion is attributed to as well.
    actor_user_id: Option<String>,
    actor_credential_id: Option<String>,
    actor_operator: Option<HostOperator>,
}

impl ApplicationService {
    /// Creates a command in one running task of `request.service`, which
    /// records `who` started it once it runs. The command itself may contain
    /// secrets and is never recorded. `actor` needs `apps:exec` on the
    /// environment's application now, not only when it connected.
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
        self.may_exec(actor, environment).await?;
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
        Ok(ExecSession {
            owner: self.clone(),
            application,
            environment: environment.clone(),
            service: request.service.clone(),
            exec,
            who: who.to_owned(),
            actor_user_id: attribution.user_id.map(str::to_owned),
            actor_credential_id: attribution.credential_id.map(str::to_owned),
            actor_operator: attribution.operator,
        })
    }

    /// Checks that `actor` may still run commands in `environment`, which a
    /// running command rechecks while it lasts.
    /// # Errors
    /// Returns refusal, a revoked credential, or storage errors.
    pub async fn may_exec(
        &self,
        actor: Actor<'_>,
        environment: &EnvironmentId,
    ) -> Result<(), ApplicationError> {
        Ok(self
            .store
            .require_on_environment(actor, AppPermission::Exec, environment)
            .await?)
    }
}

impl ExecSession {
    /// Records the command's start, streams it until it exits, then records
    /// its exit code.
    /// # Errors
    /// Returns storage errors recording the start, and runtime errors,
    /// including a disconnected client.
    pub async fn run(self, io: ExecIo) -> Result<i64, ApplicationError> {
        self.record(
            "command_started",
            &format!("{} started a command in task {}", self.who, self.exec.task),
        )
        .await?;
        let result = self.owner.runtime.run_exec(&self.exec, io).await;
        let message = match &result {
            Ok(code) => format!("Command in task {} exited with code {code}", self.exec.task),
            Err(_) => format!("Command stream in task {} failed", self.exec.task),
        };
        if let Err(error) = self.record("command_finished", &message).await {
            tracing::error!(?error, task = %self.exec.task, "could not record command completion");
        }
        Ok(result?)
    }

    /// Records `kind` with `message` in the environment's history, attributed
    /// to whoever started the command.
    async fn record(&self, kind: &str, message: &str) -> Result<(), ApplicationError> {
        Ok(self
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
                kind,
                message,
                self.service.as_str(),
            )
            .await?)
    }
}
