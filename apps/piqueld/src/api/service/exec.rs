//! One-off commands in running service tasks, recorded in application history.
use super::{ApplicationError, ApplicationService};
use crate::docker::{Exec, ExecIo};
use piqueld_core::{ApplicationId, ServiceName, exec::ExecRequest};

/// A created command whose start is already recorded in history.
pub struct ExecSession {
    owner: ApplicationService,
    application: ApplicationId,
    service: ServiceName,
    exec: Exec,
}

impl ApplicationService {
    /// Creates a command in one running task of `request.service` and records
    /// which account started it. The command itself may contain secrets and is
    /// never recorded.
    /// # Errors
    /// Returns not found, a service without running tasks, runtime or storage errors.
    pub async fn exec(
        &self,
        application: &ApplicationId,
        request: &ExecRequest,
        account: &str,
    ) -> Result<ExecSession, ApplicationError> {
        self.store.get(application).await?;
        let exec = self
            .runtime
            .create_exec(application, request)
            .await?
            .ok_or(ApplicationError::ServiceNotRunning)?;
        self.store
            .record_application_event(
                application,
                "command_started",
                &format!("{account} started a command in task {}", exec.task),
                request.service.as_str(),
            )
            .await?;
        Ok(ExecSession {
            owner: self.clone(),
            application: application.clone(),
            service: request.service.clone(),
            exec,
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
            .record_application_event(
                &self.application,
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
