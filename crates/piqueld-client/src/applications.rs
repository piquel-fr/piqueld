pub use piqueld_core::api::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationSummary,
    ApplicationView, ApplyApplicationRequest, DeploymentView, DiagnosticView,
    MAX_APPLICATION_PAGE_SIZE, ObservedApplicationView, ObservedServiceView, PlanView,
    RenameApplicationRequest, RenamedApplication, SavedApplication,
};

use crate::{Client, ClientError, Page, client::invalid_request, generated};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Cursor and page-size options for listing applications.
pub struct ListApplicationsOptions {
    /// Cursor returned by a previous page.
    pub cursor: Option<String>,
    /// Maximum number of items to return, from 1 through 100.
    pub limit: Option<u16>,
}

impl Client {
    /// Lists the first page of applications.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn applications(&self) -> Result<Page<ApplicationSummary>, ClientError> {
        self.applications_with(&ListApplicationsOptions::default())
            .await
    }

    /// Lists a page of applications using cursor and limit options.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn applications_with(
        &self,
        options: &ListApplicationsOptions,
    ) -> Result<Page<ApplicationSummary>, ClientError> {
        if options
            .limit
            .is_some_and(|limit| !(1..=MAX_APPLICATION_PAGE_SIZE).contains(&limit))
        {
            return Err(invalid_request(format!(
                "application list limit must be between 1 and {MAX_APPLICATION_PAGE_SIZE}"
            )));
        }
        generated::ListApplications {
            cursor: options.cursor.clone(),
            limit: options.limit.map(u32::from),
        }
        .send(self)
        .await
        .map(|response| response.data)
    }

    /// Fetches one application by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application(&self, id: &str) -> Result<ApplicationView, ClientError> {
        generated::GetApplication { id: id.into() }
            .send(self)
            .await
            .map(|response| response.data)
    }

    /// Fetches desired, observed, operation, and diagnostic state for an application.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application_detail(&self, id: &str) -> Result<ApplicationDetailView, ClientError> {
        generated::GetApplicationDetail { id: id.into() }
            .send(self)
            .await
            .map(|response| response.data)
    }

    /// Saves application configuration without deploying.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn apply_application(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<SavedApplication, ClientError> {
        self.apply_application_with_force(request, false).await
    }

    /// Applies a manifest, optionally overriding identity and revision preconditions.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn apply_application_with_force(
        &self,
        request: &ApplyApplicationRequest,
        force: bool,
    ) -> Result<SavedApplication, ClientError> {
        self.apply_application_with_options(request, force, false)
            .await
    }

    /// Saves configuration and optionally creates a deployment in one transaction.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn apply_application_with_options(
        &self,
        request: &ApplyApplicationRequest,
        force: bool,
        deploy: bool,
    ) -> Result<SavedApplication, ClientError> {
        generated::ApplyApplication {
            force: force.then_some(true),
            deploy: Some(deploy),
            ..Default::default()
        }
        .send(
            self,
            generated::ApplyApplicationBody::ApplicationJson(request),
        )
        .await
        .map(|response| response.data)
    }

    /// Deletes only if the supplied intent revision still matches; absence is rejected.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn delete_application_with_generation(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        self.delete_application_with_preconditions(id, expected, false)
            .await
    }

    /// Deletes with a revision precondition or an explicit force override.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn delete_application_with_preconditions(
        &self,
        id: &str,
        expected: Option<u64>,
        force: bool,
    ) -> Result<AcceptedOperation, ClientError> {
        generated::DeleteApplication {
            id: id.into(),
            expected_generation: expected,
            force: force.then_some(true),
            ..Default::default()
        }
        .send(self)
        .await
        .map(|response| response.data)
    }

    /// Previews applying an application without mutating runtime state.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_application(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<PlanView, ClientError> {
        generated::PlanApplication {
            ..Default::default()
        }
        .send(
            self,
            generated::PlanApplicationBody::ApplicationJson(request),
        )
        .await
        .map(|response| response.data)
    }

    /// Fetches current reconciliation status for an application.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application_status(&self, id: &str) -> Result<ApplicationStatusView, ClientError> {
        generated::ApplicationStatus { id: id.into() }
            .send(self)
            .await
            .map(|response| response.data)
    }

    /// Creates an application from TOML, requiring its name to be absent.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn apply_application_toml(
        &self,
        manifest: &str,
    ) -> Result<SavedApplication, ClientError> {
        self.apply_application_toml_with_generation(manifest, Some(0))
            .await
    }

    /// Applies TOML with a revision; existing names also require identity via the full method.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn apply_application_toml_with_generation(
        &self,
        manifest: &str,
        expected: Option<u64>,
    ) -> Result<SavedApplication, ClientError> {
        self.apply_application_toml_with_preconditions(manifest, expected, None, false, false)
            .await
    }

    /// Applies TOML to the inspected identity and revision, unless explicitly forced.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn apply_application_toml_with_preconditions(
        &self,
        manifest: &str,
        expected: Option<u64>,
        expected_id: Option<&str>,
        force: bool,
        deploy: bool,
    ) -> Result<SavedApplication, ClientError> {
        generated::ApplyApplication {
            force: force.then_some(true),
            deploy: Some(deploy),
            x_expected_generation: expected,
            x_expected_application_id: expected_id.map(str::to_owned),
            ..Default::default()
        }
        .send(
            self,
            generated::ApplyApplicationBody::ApplicationToml(manifest),
        )
        .await
        .map(|response| response.data)
    }

    /// Previews applying an application from a TOML manifest.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_application_toml(&self, manifest: &str) -> Result<PlanView, ClientError> {
        self.plan_application_toml_with_generation(manifest, None)
            .await
    }

    /// Previews TOML conditioned on the optional current generation.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn plan_application_toml_with_generation(
        &self,
        manifest: &str,
        expected: Option<u64>,
    ) -> Result<PlanView, ClientError> {
        generated::PlanApplication {
            x_expected_generation: expected,
            ..Default::default()
        }
        .send(
            self,
            generated::PlanApplicationBody::ApplicationToml(manifest),
        )
        .await
        .map(|response| response.data)
    }

    /// Repairs the latest accepted intent using its already resolved digests.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn reconcile_application(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        generated::ReconcileApplication {
            id: id.into(),
            expected_generation: expected,
            ..Default::default()
        }
        .send(self)
        .await
        .map(|response| response.data)
    }

    /// Renames an idle application without touching its runtime resources.
    /// # Errors
    /// Returns transport, API, decoding, name, busy, or generation errors.
    pub async fn rename_application(
        &self,
        id: &str,
        request: &RenameApplicationRequest,
    ) -> Result<RenamedApplication, ClientError> {
        self.rename_application_with_force(id, request, false).await
    }

    /// Renames with an explicit override of the revision precondition.
    /// # Errors
    /// Returns transport, API, decoding, name, or busy errors.
    pub async fn rename_application_with_force(
        &self,
        id: &str,
        request: &RenameApplicationRequest,
        force: bool,
    ) -> Result<RenamedApplication, ClientError> {
        generated::RenameApplication {
            id: id.into(),
            force: force.then_some(true),
            ..Default::default()
        }
        .send(
            self,
            generated::RenameApplicationBody::ApplicationJson(request),
        )
        .await
        .map(|response| response.data)
    }

    /// Reads one page of informational events, including history of deleted applications.
    /// # Errors
    /// Returns transport, API, decoding, or pagination errors.
    pub async fn events(
        &self,
        application_id: Option<&str>,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<Page<piqueld_core::Event>, ClientError> {
        if !(1..=100).contains(&limit) {
            return Err(invalid_request("event limit must be between 1 and 100"));
        }
        generated::ListEvents {
            application_id: application_id.map(str::to_owned),
            cursor: cursor.map(str::to_owned),
            limit: Some(u32::from(limit)),
        }
        .send(self)
        .await
        .map(|response| response.data)
    }
}

impl Client {
    /// Deploys exactly the inspected saved configuration revision.
    /// # Errors
    /// Returns transport, API, or revision conflict errors.
    pub async fn deploy_application(
        &self,
        id: &str,
        expected: u64,
    ) -> Result<AcceptedOperation, ClientError> {
        generated::DeployApplication {
            id: id.into(),
            expected_generation: Some(expected),
            ..Default::default()
        }
        .send(self)
        .await
        .map(|response| response.data)
    }

    /// Lists deployment snapshots newest first, three per page.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn deployments(
        &self,
        id: &str,
        cursor: Option<&str>,
    ) -> Result<Page<DeploymentView>, ClientError> {
        generated::ListDeployments {
            id: id.into(),
            cursor: cursor.map(str::to_owned),
        }
        .send(self)
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
        generated::ListDeploymentAttempts {
            id: id.into(),
            deployment: deployment.into(),
            cursor: cursor.map(str::to_owned),
        }
        .send(self)
        .await
        .map(|response| response.data)
    }
}

impl Client {
    /// Reads a bounded historical Docker log window.
    /// # Errors
    /// Returns transport, validation or Docker errors.
    pub async fn application_logs(
        &self,
        id: &str,
        service: Option<&str>,
        tail: u16,
        since_seconds: u32,
    ) -> Result<piqueld_core::api::ApplicationLogs, ClientError> {
        self.filtered_application_logs(id, service, tail, since_seconds, None)
            .await
    }
    /// Reads a bounded log window filtered by service and stream in the daemon.
    /// # Errors
    /// Returns transport, validation or Docker errors.
    pub async fn filtered_application_logs(
        &self,
        id: &str,
        service: Option<&str>,
        tail: u16,
        since_seconds: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, ClientError> {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("tail", &tail.to_string())
            .append_pair("since_seconds", &since_seconds.to_string());
        if let Some(service) = service {
            query.append_pair("service", service);
        }
        if let Some(stream) = stream {
            query.append_pair("stream", stream.as_str());
        }
        self.send::<_, ()>(
            http::Method::GET,
            &format!(
                "{}/applications/{}/logs?{}",
                crate::API_PREFIX,
                id,
                query.finish()
            ),
            None,
            &[],
        )
        .await
    }
}
