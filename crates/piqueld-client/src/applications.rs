pub use piqueld_core::api::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationSummary,
    ApplicationView, ApplyApplicationRequest, DeploymentView, DiagnosticView,
    MAX_APPLICATION_PAGE_SIZE, ObservedApplicationView, ObservedServiceView, PlanView,
    RenameApplicationRequest, RenamedApplication, SavedApplication,
};

use crate::{
    Client, ClientError, Envelope, Page,
    client::{generated_result, invalid_request},
};

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
        generated_result(
            self.generated
                .list_applications(options.cursor.as_deref(), options.limit.map(u32::from))
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Fetches one application by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application(&self, id: &str) -> Result<ApplicationView, ClientError> {
        generated_result(self.generated.get_application(id).await)
            .await
            .map(|response| response.data)
    }

    /// Fetches desired, observed, operation, and diagnostic state for an application.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application_detail(&self, id: &str) -> Result<ApplicationDetailView, ClientError> {
        generated_result(self.generated.get_application_detail(id).await)
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
        generated_result(
            self.generated
                .apply_application(
                    Some(deploy),
                    force.then_some(true),
                    None,
                    None,
                    None,
                    request,
                )
                .await,
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
        generated_result(
            self.generated
                .delete_application(id, expected, force.then_some(true), None)
                .await,
        )
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
        generated_result(self.generated.plan_application(None, None, request).await)
            .await
            .map(|response| response.data)
    }

    /// Fetches current reconciliation status for an application.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application_status(&self, id: &str) -> Result<ApplicationStatusView, ClientError> {
        generated_result(self.generated.application_status(id).await)
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
        let mut query = Vec::with_capacity(2);
        if force {
            query.push(("force", true.to_string()));
        }
        query.push(("deploy", deploy.to_string()));
        let mut headers = Vec::new();
        if let Some(expected) = expected {
            headers.push(("X-Expected-Generation", expected.to_string()));
        }
        if let Some(expected_id) = expected_id {
            headers.push(("X-Expected-Application-Id", expected_id.to_owned()));
        }
        self.send_toml::<Envelope<SavedApplication>>(
            "/api/v1/applications/apply",
            &query,
            &headers,
            manifest,
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
        let headers = expected
            .map(|expected| vec![("X-Expected-Generation", expected.to_string())])
            .unwrap_or_default();
        self.send_toml::<Envelope<PlanView>>("/api/v1/applications/plan", &[], &headers, manifest)
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
        generated_result(
            self.generated
                .reconcile_application(id, expected, None, None)
                .await,
        )
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
        generated_result(
            self.generated
                .rename_application(id, force.then_some(true), None, request)
                .await,
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
        generated_result(
            self.generated
                .list_events(application_id, cursor, Some(i64::from(limit)))
                .await,
        )
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
        generated_result(
            self.generated
                .deploy_application(id, Some(expected), None, None)
                .await,
        )
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
}

impl Client {
    /// Downloads the saved application manifest as TOML.
    /// # Errors
    /// Returns transport, API, or UTF-8 decoding errors.
    pub async fn application_manifest(&self, id: &str) -> Result<String, ClientError> {
        let stream =
            generated_result(self.generated.download_application_manifest(id).await).await?;
        let bytes = crate::client::collect_byte_stream(stream).await?;
        String::from_utf8(bytes).map_err(|source| ClientError::TextDecode { source })
    }

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
