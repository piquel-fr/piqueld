use http::Method;

pub use piqueld_core::api::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationView,
    ApplyApplicationRequest, DiagnosticView, ObservedApplicationView, ObservedServiceView,
    PlanView, RenameApplicationRequest, RenamedApplication,
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
    /// Maximum number of items to return, from 1 through 3.
    pub limit: Option<u16>,
}

const APPLICATION_PAGE_SIZE: u16 = 3;

impl Client {
    /// Lists the first page of applications.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn applications(&self) -> Result<Page<ApplicationView>, ClientError> {
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
    ) -> Result<Page<ApplicationView>, ClientError> {
        if options
            .limit
            .is_some_and(|limit| !(1..=APPLICATION_PAGE_SIZE).contains(&limit))
        {
            return Err(invalid_request(format!(
                "application list limit must be between 1 and {APPLICATION_PAGE_SIZE}"
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

    /// Applies desired application state and starts asynchronous reconciliation.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn apply_application(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::POST,
            &format!("{}/applications/apply", crate::API_PREFIX),
            Some(request),
            &[],
        )
        .await
    }

    /// Marks an application for deletion.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn delete_application(&self, id: &str) -> Result<AcceptedOperation, ClientError> {
        self.delete_application_with_generation(id, None).await
    }

    /// Deletes only if the optional intent revision still matches.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn delete_application_with_generation(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send::<_, ()>(
            Method::DELETE,
            &Self::mutation_path(id, "", expected),
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

    /// Applies desired application state from a TOML manifest.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn apply_application_toml(
        &self,
        manifest: &str,
    ) -> Result<AcceptedOperation, ClientError> {
        self.apply_application_toml_with_generation(manifest, None)
            .await
    }

    /// Applies TOML with an optional intent revision precondition.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn apply_application_toml_with_generation(
        &self,
        manifest: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        self.apply_application_toml_with_preconditions(manifest, expected, None)
            .await
    }

    /// Applies TOML only to the inspected identity and revision when supplied.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn apply_application_toml_with_preconditions(
        &self,
        manifest: &str,
        expected: Option<u64>,
        expected_id: Option<&str>,
    ) -> Result<AcceptedOperation, ClientError> {
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
            &format!("{}/applications/apply", crate::API_PREFIX),
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

    /// Explicitly resolves the current manifest again.
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn refresh_application(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send::<_, ()>(
            Method::POST,
            &Self::mutation_path(id, "/refresh", expected),
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
        self.send(
            Method::POST,
            &Self::mutation_path(id, "/rename", None),
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
