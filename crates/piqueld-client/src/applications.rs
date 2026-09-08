use http::Method;

pub use piqueld_core::api::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationView,
    ApplyApplicationRequest, DiagnosticView, ObservedApplicationView, ObservedServiceView,
    PlanView,
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
        self.send::<_, ()>(
            Method::DELETE,
            &format!("{}/applications/{}", crate::API_PREFIX, path_segment(id)),
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
        self.send_text(
            Method::POST,
            &format!("{}/applications/apply", crate::API_PREFIX),
            manifest,
            &[("content-type", "application/toml")],
        )
        .await
    }

    /// Previews applying an application from a TOML manifest.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_application_toml(&self, manifest: &str) -> Result<PlanView, ClientError> {
        self.send_text(
            Method::POST,
            &format!("{}/applications/plan", crate::API_PREFIX),
            manifest,
            &[("content-type", "application/toml")],
        )
        .await
    }
}
