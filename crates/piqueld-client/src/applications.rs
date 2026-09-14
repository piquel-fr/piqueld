use http::Method;

pub use piqueld_core::api::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationSummary,
    ApplicationView, ApplyApplicationRequest, DeploymentView, DiagnosticView,
    MAX_APPLICATION_PAGE_SIZE, ObservedApplicationView, ObservedServiceView, PlanView,
    RenameApplicationRequest, RenamedApplication, SavedApplication,
};

use crate::{
    Client, ClientError, Page,
    client::{invalid_request, path_segment},
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
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(cursor) = &options.cursor {
            query.append_pair("cursor", cursor);
        }
        if let Some(limit) = options.limit {
            query.append_pair("limit", &limit.to_string());
        }
        let query = query.finish();
        let path = if query.is_empty() {
            format!("{}/applications", crate::API_PREFIX)
        } else {
            format!("{}/applications?{query}", crate::API_PREFIX)
        };
        self.send::<_, ()>(Method::GET, &path, None, &[]).await
    }

    /// Fetches one application by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application(&self, id: &str) -> Result<ApplicationView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("{}/applications/{}", crate::API_PREFIX, path_segment(id)),
            None,
            &[],
        )
        .await
    }

    /// Fetches desired, observed, operation, and diagnostic state for an application.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application_detail(&self, id: &str) -> Result<ApplicationDetailView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!(
                "{}/applications/{}/detail",
                crate::API_PREFIX,
                path_segment(id)
            ),
            None,
            &[],
        )
        .await
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
        self.send(
            Method::POST,
            &Self::force_path(
                format!("{}/applications/apply?deploy={deploy}", crate::API_PREFIX),
                force,
            ),
            Some(request),
            &[],
        )
        .await
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
        self.send::<_, ()>(
            Method::DELETE,
            &Self::force_path(Self::mutation_path(id, "", expected), force),
            None,
            &[],
        )
        .await
    }

    /// Previews applying an application without mutating runtime state.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_application(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<PlanView, ClientError> {
        self.send(
            Method::POST,
            &format!("{}/applications/plan", crate::API_PREFIX),
            Some(request),
            &[],
        )
        .await
    }

    /// Fetches current reconciliation status for an application.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn application_status(&self, id: &str) -> Result<ApplicationStatusView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!(
                "{}/applications/{}/status",
                crate::API_PREFIX,
                path_segment(id)
            ),
            None,
            &[],
        )
        .await
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
        let generation = expected.map(|value| value.to_string());
        let mut headers = vec![("content-type", "application/toml")];
        if let Some(value) = generation.as_deref() {
            headers.push(("x-expected-generation", value));
        }
        if let Some(id) = expected_id {
            headers.push(("x-expected-application-id", id));
        }
        self.send_text(
            Method::POST,
            &Self::force_path(
                format!("{}/applications/apply?deploy={deploy}", crate::API_PREFIX),
                force,
            ),
            manifest,
            &headers,
        )
        .await
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
        let generation = expected.map(|value| value.to_string());
        let mut headers = vec![("content-type", "application/toml")];
        if let Some(value) = generation.as_deref() {
            headers.push(("x-expected-generation", value));
        }
        self.send_text(
            Method::POST,
            &format!("{}/applications/plan", crate::API_PREFIX),
            manifest,
            &headers,
        )
        .await
    }

    fn force_path(mut path: String, force: bool) -> String {
        if force {
            path.push(if path.contains('?') { '&' } else { '?' });
            path.push_str("force=true");
        }
        path
    }

    fn mutation_path(id: &str, action: &str, expected: Option<u64>) -> String {
        let path = format!(
            "{}/applications/{}{}",
            crate::API_PREFIX,
            path_segment(id),
            action
        );
        expected.map_or_else(
            || path.clone(),
            |generation| format!("{path}?expected_generation={generation}"),
        )
    }

    /// Repairs the latest accepted intent using its already resolved digests.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn reconcile_application(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send::<_, ()>(
            Method::POST,
            &Self::mutation_path(id, "/reconcile", expected),
            None,
            &[],
        )
        .await
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
        self.send(
            Method::POST,
            &Self::force_path(Self::mutation_path(id, "/rename", None), force),
            Some(request),
            &[],
        )
        .await
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
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(id) = application_id {
            query.append_pair("application_id", id);
        }
        if let Some(cursor) = cursor {
            query.append_pair("cursor", cursor);
        }
        query.append_pair("limit", &limit.to_string());
        let path = format!("{}/events?{}", crate::API_PREFIX, query.finish());
        self.send::<_, ()>(Method::GET, &path, None, &[]).await
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
        self.send::<_, ()>(
            Method::POST,
            &Self::mutation_path(id, "/deploy", Some(expected)),
            None,
            &[],
        )
        .await
    }

    /// Lists deployment snapshots newest first, three per page.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn deployments(
        &self,
        id: &str,
        cursor: Option<&str>,
    ) -> Result<Page<DeploymentView>, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &Self::history_path(id, "/deployments", cursor),
            None,
            &[],
        )
        .await
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
        self.send::<_, ()>(
            Method::GET,
            &Self::history_path(
                id,
                &format!("/deployments/{}/attempts", path_segment(deployment)),
                cursor,
            ),
            None,
            &[],
        )
        .await
    }

    fn history_path(id: &str, action: &str, cursor: Option<&str>) -> String {
        let mut path = Self::mutation_path(id, action, None);
        if let Some(cursor) = cursor {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("cursor", cursor)
                .finish();
            path.push('?');
            path.push_str(&query);
        }
        path
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
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("tail", &tail.to_string())
            .append_pair("since_seconds", &since_seconds.to_string());
        if let Some(service) = service {
            query.append_pair("service", service);
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
