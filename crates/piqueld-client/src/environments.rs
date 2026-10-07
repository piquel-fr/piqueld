//! Environment lifecycle, deployments, status, history, and logs.
pub use piqueld_core::api::{
    CreateEnvironmentRequest, EnvironmentBranchRequest, EnvironmentDetailView, EnvironmentRequest,
    EnvironmentStatusView, EnvironmentView,
};
pub use piqueld_core::{EnvironmentName, EnvironmentSource, TrackedBranch};

use crate::{
    AcceptedOperation, Client, ClientError, DeploymentView, ManifestRevision, Page,
    client::generated_result,
};

impl Client {
    /// Fetches one environment by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn environment(&self, id: &str) -> Result<EnvironmentView, ClientError> {
        generated_result(self.generated.get_environment(id).await)
            .await
            .map(|response| response.data)
    }

    /// Fetches desired, observed, operation, and diagnostic state for an environment.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn environment_detail(&self, id: &str) -> Result<EnvironmentDetailView, ClientError> {
        generated_result(self.generated.get_environment_detail(id).await)
            .await
            .map(|response| response.data)
    }

    /// Fetches current reconciliation status for an environment.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn environment_status(&self, id: &str) -> Result<EnvironmentStatusView, ClientError> {
        generated_result(self.generated.environment_status(id).await)
            .await
            .map(|response| response.data)
    }

    /// Adds an environment to an application, conditioned on the inspected
    /// application revision unless forced.
    /// # Errors
    /// Returns transport, API, decoding, name, branch, or generation errors.
    pub async fn create_environment(
        &self,
        application: &str,
        request: &CreateEnvironmentRequest,
        force: bool,
    ) -> Result<EnvironmentView, ClientError> {
        generated_result(
            self.generated
                .create_environment(application, force.then_some(true), None, request)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Renames an environment without touching its runtime resources.
    /// # Errors
    /// Returns transport, API, decoding, name, or generation errors.
    pub async fn rename_environment(
        &self,
        id: &str,
        request: &EnvironmentRequest,
        force: bool,
    ) -> Result<EnvironmentView, ClientError> {
        generated_result(
            self.generated
                .rename_environment(id, force.then_some(true), None, request)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Points an environment of a repository-backed application at another
    /// branch, or pins or unpins its commit, without redeploying it.
    /// # Errors
    /// Returns transport, API, decoding, branch, or generation errors.
    pub async fn set_environment_branch(
        &self,
        id: &str,
        request: &EnvironmentBranchRequest,
        force: bool,
    ) -> Result<EnvironmentView, ClientError> {
        generated_result(
            self.generated
                .set_environment_branch(id, force.then_some(true), None, request)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Deletes an environment's services and networks, retaining its volumes,
    /// with a revision precondition or an explicit force override.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn delete_environment(
        &self,
        id: &str,
        expected: Option<u64>,
        force: bool,
    ) -> Result<AcceptedOperation, ClientError> {
        generated_result(
            self.generated
                .delete_environment(id, expected, force.then_some(true), None)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Retries the latest operation with its saved inputs once it has ended or failed,
    /// without resolving sources again. An operation still in progress is returned as is.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn reconcile_environment(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        generated_result(
            self.generated
                .reconcile_environment(id, expected, None, None)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Deploys an environment from its source, conditioned on the inspected
    /// revision. `revision` fetches a repository manifest from another branch
    /// or commit than the environment's, once.
    /// # Errors
    /// Returns transport, API, or revision conflict errors.
    pub async fn deploy_environment(
        &self,
        id: &str,
        expected: u64,
        revision: Option<&ManifestRevision>,
    ) -> Result<AcceptedOperation, ClientError> {
        let (branch, commit) = match revision {
            None => (None, None),
            Some(ManifestRevision::Branch(branch)) => (Some(branch.as_str()), None),
            Some(ManifestRevision::Commit(commit)) => (None, Some(commit.as_str())),
        };
        generated_result(
            self.generated
                .deploy_environment(id, branch, commit, Some(expected), None, None)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Lists an environment's deployment snapshots newest first, three per page.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn deployments(
        &self,
        id: &str,
        cursor: Option<&str>,
    ) -> Result<Page<DeploymentView>, ClientError> {
        generated_result(self.generated.list_deployments(id, cursor).await)
            .await
            .map(|response| response.data)
    }

    /// Lists retained attempt outcomes, 100 per page.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn deployment_attempts(
        &self,
        id: &str,
        deployment: &str,
        cursor: Option<&str>,
    ) -> Result<Page<piqueld_core::Operation>, ClientError> {
        generated_result(
            self.generated
                .list_deployment_attempts(id, deployment, cursor)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Reads one page of informational events, oldest first, optionally of one
    /// application (its own and all its environments' events) or environment,
    /// including history of deleted environments.
    /// # Errors
    /// Returns transport, API, decoding, or pagination errors.
    pub async fn events(
        &self,
        application_id: Option<&str>,
        environment_id: Option<&str>,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<Page<piqueld_core::Event>, ClientError> {
        self.filtered_events(
            &piqueld_core::observability::EventFilter {
                application_id: application_id.map(str::to_owned),
                environment_id: environment_id.map(str::to_owned),
                ..Default::default()
            },
            cursor,
            limit,
        )
        .await
    }

    /// Reads a bounded historical Docker log window.
    /// # Errors
    /// Returns transport, validation or Docker errors.
    pub async fn environment_logs(
        &self,
        id: &str,
        service: Option<&str>,
        tail: u16,
        since_seconds: u32,
    ) -> Result<piqueld_core::api::ApplicationLogs, ClientError> {
        self.filtered_environment_logs(id, service, tail, since_seconds, None)
            .await
    }

    /// Reads a bounded log window filtered by service and stream in the daemon.
    /// # Errors
    /// Returns transport, validation or Docker errors.
    pub async fn filtered_environment_logs(
        &self,
        id: &str,
        service: Option<&str>,
        tail: u16,
        since_seconds: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, ClientError> {
        generated_result(
            self.generated
                .environment_logs(
                    id,
                    service,
                    Some(since_seconds),
                    stream.as_ref(),
                    Some(u32::from(tail)),
                )
                .await,
        )
        .await
        .map(|response| response.data)
    }
}
