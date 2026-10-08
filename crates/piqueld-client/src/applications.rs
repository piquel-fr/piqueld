pub use piqueld_core::api::{
    AcceptedOperation, ApplicationSummary, ApplicationView, ApplyApplicationRequest,
    DeletedApplication, DeploymentView, DiagnosticView, MAX_APPLICATION_PAGE_SIZE,
    ObservedApplicationView, ObservedServiceView, PlanView, ReleaseView, RenameApplicationRequest,
    RenamedApplication, SavedApplication,
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

    /// Fetches one application and its environments by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application(&self, id: &str) -> Result<ApplicationView, ClientError> {
        generated_result(self.generated.get_application(id).await)
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

    /// Deletes an application and all its environments only if the supplied
    /// intent revision still matches; absence is rejected.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn delete_application_with_generation(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<DeletedApplication, ClientError> {
        self.delete_application_with_preconditions(id, expected, false, &[])
            .await
    }

    /// Deletes an application and all its environments with a revision
    /// precondition or an explicit force override. `environments` must name
    /// every environment when there are several; forcing never skips that.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn delete_application_with_preconditions(
        &self,
        id: &str,
        expected: Option<u64>,
        force: bool,
        environments: &[&str],
    ) -> Result<DeletedApplication, ClientError> {
        let environments = (!environments.is_empty()).then(|| environments.join(","));
        generated_result(
            self.generated
                .delete_application(
                    id,
                    environments.as_deref(),
                    expected,
                    force.then_some(true),
                    None,
                )
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Previews applying an application without mutating runtime state, against
    /// `environment` (by default the application's only environment).
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_application(
        &self,
        request: &ApplyApplicationRequest,
        environment: Option<&str>,
    ) -> Result<PlanView, ClientError> {
        generated_result(
            self.generated
                .plan_application(environment, None, None, request)
                .await,
        )
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
    ///
    /// Progenitor generates only one request media type per operation. The generated
    /// apply endpoint uses JSON, so this TOML variant uses the shared TOML adapter.
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
        self.plan_application_toml_with_generation(manifest, None, None)
            .await
    }

    /// Previews TOML conditioned on the optional current generation, against
    /// `environment` (by default the application's only environment).
    ///
    /// Progenitor generates only one request media type per operation. The generated
    /// plan endpoint uses JSON, so this TOML variant uses the shared TOML adapter.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn plan_application_toml_with_generation(
        &self,
        manifest: &str,
        expected: Option<u64>,
        environment: Option<&str>,
    ) -> Result<PlanView, ClientError> {
        let headers = expected
            .map(|expected| vec![("X-Expected-Generation", expected.to_string())])
            .unwrap_or_default();
        let query = environment
            .map(|environment| vec![("environment", environment.to_owned())])
            .unwrap_or_default();
        self.send_toml::<Envelope<PlanView>>(
            "/api/v1/applications/plan",
            &query,
            &headers,
            manifest,
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
}

impl Client {
    /// Lists an application's releases newest first, twenty per page.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn releases(
        &self,
        id: &str,
        cursor: Option<&str>,
    ) -> Result<Page<ReleaseView>, ClientError> {
        generated_result(self.generated.list_releases(id, cursor).await)
            .await
            .map(|response| response.data)
    }

    /// Downloads the saved application manifest as TOML.
    /// # Errors
    /// Returns transport, API, or UTF-8 decoding errors.
    pub async fn application_manifest(&self, id: &str) -> Result<String, ClientError> {
        let stream =
            generated_result(self.generated.download_application_manifest(id).await).await?;
        let bytes = crate::client::collect_byte_stream(stream).await?;
        String::from_utf8(bytes).map_err(|source| ClientError::TextDecode { source })
    }
}
