//! Environment lifecycle, deployments, status, history, and logs.
pub use piqueld_core::api::{
    AcceptedPromotion, CreateEnvironmentRequest, DeploymentOrigin, EnvironmentBranchRequest,
    EnvironmentDetailView, EnvironmentRequest, EnvironmentSourceRequest, EnvironmentStatusView,
    EnvironmentTelemetryRequest, EnvironmentView, PromoteRequest, ReleasePlan, ReleasePromotion,
    RouteUrl, RunState, SecretProblem, TelemetryReport, UrlCondition, UrlState,
};
pub use piqueld_core::{EnvironmentName, EnvironmentSource, PromotedFrom, TrackedBranch};

use crate::{
    AcceptedOperation, ApplicationLogs, Client, ClientError, DeploymentView, Event, LogStream,
    ManifestRevision, Operation, Page, PlanView, client::EnvelopedCall, observability::EventFilter,
};

impl Client {
    /// Fetches one environment by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn environment(&self, id: &str) -> Result<EnvironmentView, ClientError> {
        self.generated.get_environment(id).data().await
    }

    /// Fetches desired, observed, operation, and diagnostic state for an environment.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn environment_detail(&self, id: &str) -> Result<EnvironmentDetailView, ClientError> {
        self.generated.get_environment_detail(id).data().await
    }

    /// Grants or revokes an environment's telemetry, which its receiving
    /// service then gets from every application.
    ///
    /// # Errors
    /// Returns transport, API, or decoding errors; previews are not found.
    pub async fn set_environment_telemetry(
        &self,
        id: &str,
        granted: bool,
    ) -> Result<EnvironmentView, ClientError> {
        let request = EnvironmentTelemetryRequest { granted };
        self.generated
            .set_environment_telemetry(id, None, &request)
            .data()
            .await
    }

    /// Fetches current reconciliation status for an environment.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn environment_status(&self, id: &str) -> Result<EnvironmentStatusView, ClientError> {
        self.generated.environment_status(id).data().await
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
        self.generated
            .create_environment(application, force.then_some(true), None, request)
            .data()
            .await
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
        self.generated
            .rename_environment(id, force.then_some(true), None, request)
            .data()
            .await
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
        self.generated
            .set_environment_branch(id, force.then_some(true), None, request)
            .data()
            .await
    }

    /// Makes an environment promoted from another, or returns it to tracking
    /// its own source, without deploying it.
    /// # Errors
    /// Returns transport, API, decoding, source, or generation errors.
    pub async fn set_environment_source(
        &self,
        id: &str,
        request: &EnvironmentSourceRequest,
        force: bool,
    ) -> Result<EnvironmentView, ClientError> {
        self.generated
            .set_environment_source(id, force.then_some(true), None, request)
            .data()
            .await
    }

    /// Promotes a release into a promoted environment, pinning it at acceptance.
    /// # Errors
    /// Returns transport, API, decoding, precondition, or generation errors.
    pub async fn promote_environment(
        &self,
        id: &str,
        request: &PromoteRequest,
        force: bool,
    ) -> Result<AcceptedPromotion, ClientError> {
        self.generated
            .promote_environment(id, force.then_some(true), None, request)
            .data()
            .await
    }

    /// Plans promoting a release into an environment without changing anything.
    /// # Errors
    /// Returns transport, API, decoding, or precondition errors.
    pub async fn plan_promotion(
        &self,
        id: &str,
        request: &PromoteRequest,
    ) -> Result<PlanView, ClientError> {
        self.generated.plan_promotion(id, request).data().await
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
        self.generated
            .delete_environment(id, expected, force.then_some(true), None)
            .data()
            .await
    }

    /// Retries the latest operation with its saved inputs once it has ended or failed,
    /// without resolving sources again, starting the environment if it is stopped.
    /// An operation still in progress is returned as is.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn reconcile_environment(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        self.generated
            .reconcile_environment(id, expected, None, None)
            .data()
            .await
    }

    /// Stops an environment: its services scale to zero and the gateway
    /// withdraws its routes, until it is started or deployed again. Returns
    /// its latest operation, which runs again when it had rolled out.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn stop_environment(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        self.generated
            .stop_environment(id, expected, None, None)
            .data()
            .await
    }

    /// Starts a stopped environment, reconciling it like
    /// [`Self::reconcile_environment`].
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn start_environment(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        self.generated
            .start_environment(id, expected, None, None)
            .data()
            .await
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
        self.generated
            .deploy_environment(id, branch, commit, Some(expected), None, None)
            .data()
            .await
    }

    /// Lists an environment's deployment snapshots newest first, three per page.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn deployments(
        &self,
        id: &str,
        cursor: Option<&str>,
    ) -> Result<Page<DeploymentView>, ClientError> {
        self.generated.list_deployments(id, cursor).data().await
    }

    /// Lists retained attempt outcomes, 100 per page.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn deployment_attempts(
        &self,
        id: &str,
        deployment: &str,
        cursor: Option<&str>,
    ) -> Result<Page<Operation>, ClientError> {
        self.generated
            .list_deployment_attempts(id, deployment, cursor)
            .data()
            .await
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
    ) -> Result<Page<Event>, ClientError> {
        self.filtered_events(
            &EventFilter {
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
    ) -> Result<ApplicationLogs, ClientError> {
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
        stream: Option<LogStream>,
    ) -> Result<ApplicationLogs, ClientError> {
        self.generated
            .environment_logs(
                id,
                service,
                Some(since_seconds),
                stream.as_ref(),
                Some(u32::from(tail)),
            )
            .data()
            .await
    }
}
