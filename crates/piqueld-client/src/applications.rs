use http::Method;
use piqueld_core::manifest::ApplicationManifest;
use piqueld_core::{NormalizedApplication, Plan};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Client, ClientError, OperationView, Page, path_segment};

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Public application state returned by the API.
pub struct ApplicationView {
    /// Normalized application manifest.
    pub application: NormalizedApplication,
    /// Monotonic application generation.
    #[schema(minimum = 1)]
    pub generation: u64,
    /// Hash of the normalized desired specification.
    pub spec_hash: String,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
/// Request to create an application.
pub struct CreateApplicationRequest {
    /// Application manifest to store and reconcile.
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
/// Request to replace an application at an expected generation.
pub struct ReplaceApplicationRequest {
    /// Generation that must still be current.
    #[schema(minimum = 1)]
    pub expected_generation: u64,
    /// Replacement application manifest.
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
/// Request to preview creation of an application.
pub struct PlanApplicationRequest {
    /// Application manifest to plan.
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
/// Request to preview replacing an application.
pub struct ReplacePlanRequest {
    /// Generation that the plan is based on.
    #[schema(minimum = 1)]
    pub expected_generation: u64,
    /// Replacement application manifest.
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
/// Request carrying an optimistic-concurrency generation.
pub struct ExpectedGeneration {
    /// Generation that must still be current.
    #[schema(minimum = 1)]
    pub expected_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
/// Request to mark an application for deletion.
pub struct DeleteApplicationRequest {
    /// Generation that must still be current.
    #[schema(minimum = 1)]
    pub expected_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Operation accepted by an application mutation endpoint.
pub struct AcceptedOperation {
    /// Asynchronous operation identifier.
    pub operation_id: String,
    /// Stable application identifier.
    pub application_id: String,
    /// Generation created by the mutation.
    #[schema(minimum = 1)]
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Dry-run plan returned by the API.
pub struct PlanView {
    /// Stable application identifier.
    pub application_id: String,
    /// Generation that would be created.
    #[schema(minimum = 1)]
    pub proposed_generation: u64,
    /// Ordered runtime plan.
    pub plan: Plan,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Current application reconciliation status.
pub struct ApplicationStatusView {
    /// Stable application identifier.
    pub application_id: String,
    /// Machine-readable lifecycle state.
    pub state: String,
    /// Last observed application generation.
    #[schema(minimum = 1)]
    pub observed_generation: Option<u64>,
    /// Optional safe status message.
    pub message: Option<String>,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Sanitized runtime diagnostic shown by the read-only dashboard.
pub struct DiagnosticView {
    /// Stable diagnostic category.
    pub code: String,
    /// Bounded, actionable message safe for a browser.
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Observed service state summarized for browser and operator clients.
pub struct ObservedServiceView {
    /// Logical service name from the desired application.
    pub name: String,
    /// Observed immutable image reference, when the service exists.
    pub image: Option<String>,
    /// Desired replica count from the application manifest.
    pub desired_replicas: u16,
    /// Replicas currently reported by the runtime.
    pub observed_replicas: u16,
    /// Replicas that are running and healthy enough to serve traffic.
    pub healthy_replicas: u16,
    /// Runtime convergence category.
    pub convergence: String,
    /// Sanitized task and service diagnostics.
    pub diagnostics: Vec<DiagnosticView>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
/// Bounded observed runtime state for one application.
pub struct ObservedApplicationView {
    /// Observed services in desired service order.
    pub services: Vec<ObservedServiceView>,
    /// Number of owned networks observed.
    pub network_count: u32,
    /// Number of owned volumes observed.
    pub volume_count: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Read-only application detail composed at the API boundary.
pub struct ApplicationDetailView {
    /// Desired application and generation.
    pub application: ApplicationView,
    /// Durable application lifecycle status.
    pub status: ApplicationStatusView,
    /// Sanitized runtime observation.
    pub observed: ObservedApplicationView,
    /// Most recent durable operation, when one exists.
    pub latest_operation: Option<OperationView>,
    /// Bounded diagnostics from status, runtime, and the latest operation.
    pub diagnostics: Vec<DiagnosticView>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Cursor and page-size options for listing applications.
pub struct ListApplicationsOptions {
    /// Cursor returned by a previous page.
    pub cursor: Option<String>,
    /// Maximum number of items to return.
    pub limit: Option<u16>,
}

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
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(cursor) = &options.cursor {
            query.append_pair("cursor", cursor);
        }
        if let Some(limit) = options.limit {
            query.append_pair("limit", &limit.to_string());
        }
        let query = query.finish();
        let path = if query.is_empty() {
            "/api/v1/applications".to_owned()
        } else {
            format!("/api/v1/applications?{query}")
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
            &format!("/api/v1/applications/{}", path_segment(id)),
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
            &format!("/api/v1/applications/{}/detail", path_segment(id)),
            None,
            &[],
        )
        .await
    }

    /// Creates an application and starts its asynchronous reconciliation.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn create_application(
        &self,
        request: &CreateApplicationRequest,
        idempotency_key: &str,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::POST,
            "/api/v1/applications",
            Some(request),
            &[("idempotency-key", idempotency_key)],
        )
        .await
    }

    /// Replaces an application at an expected generation.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn replace_application(
        &self,
        id: &str,
        request: &ReplaceApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError> {
        self.replace_application_with_key(id, request, None).await
    }

    /// Replaces an application and optionally binds the mutation to an
    /// idempotency key for safe transport retries.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn replace_application_with_key(
        &self,
        id: &str,
        request: &ReplaceApplicationRequest,
        idempotency_key: Option<&str>,
    ) -> Result<AcceptedOperation, ClientError> {
        let headers = idempotency_key
            .map(|key| vec![("idempotency-key", key)])
            .unwrap_or_default();
        self.send(
            Method::PUT,
            &format!("/api/v1/applications/{}", path_segment(id)),
            Some(request),
            &headers,
        )
        .await
    }

    /// Marks an application for deletion.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn delete_application(
        &self,
        id: &str,
        request: &DeleteApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError> {
        self.delete_application_with_key(id, request, None).await
    }

    /// Marks an application for deletion and optionally binds the mutation to
    /// an idempotency key for safe transport retries.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn delete_application_with_key(
        &self,
        id: &str,
        request: &DeleteApplicationRequest,
        idempotency_key: Option<&str>,
    ) -> Result<AcceptedOperation, ClientError> {
        let headers = idempotency_key
            .map(|key| vec![("idempotency-key", key)])
            .unwrap_or_default();
        self.send(
            Method::DELETE,
            &format!("/api/v1/applications/{}", path_segment(id)),
            Some(request),
            &headers,
        )
        .await
    }

    /// Plans creating an application without mutating runtime state.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_create(
        &self,
        request: &PlanApplicationRequest,
    ) -> Result<PlanView, ClientError> {
        self.send(
            Method::POST,
            "/api/v1/applications/plan",
            Some(request),
            &[],
        )
        .await
    }

    /// Plans replacing an application without mutating runtime state.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_replace(
        &self,
        id: &str,
        request: &ReplacePlanRequest,
    ) -> Result<PlanView, ClientError> {
        self.send(
            Method::POST,
            &format!("/api/v1/applications/{}/plan", path_segment(id)),
            Some(request),
            &[],
        )
        .await
    }

    /// Requests reconciliation at an expected generation.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn reconcile(
        &self,
        id: &str,
        expected_generation: u64,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::POST,
            &format!("/api/v1/applications/{}/reconcile", path_segment(id)),
            Some(&ExpectedGeneration {
                expected_generation,
            }),
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
            &format!("/api/v1/applications/{}/status", path_segment(id)),
            None,
            &[],
        )
        .await
    }

    /// Creates an application from a TOML manifest.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn create_application_toml(
        &self,
        manifest: &str,
        idempotency_key: &str,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send_text(
            Method::POST,
            "/api/v1/applications",
            manifest,
            &[
                ("content-type", "application/toml"),
                ("idempotency-key", idempotency_key),
            ],
        )
        .await
    }

    /// Replaces an application using a TOML manifest.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn replace_application_toml(
        &self,
        id: &str,
        manifest: &str,
        expected_generation: u64,
    ) -> Result<AcceptedOperation, ClientError> {
        self.replace_application_toml_with_key(id, manifest, expected_generation, None)
            .await
    }

    /// Replaces an application from TOML and optionally binds the mutation to
    /// an idempotency key for safe transport retries.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn replace_application_toml_with_key(
        &self,
        id: &str,
        manifest: &str,
        expected_generation: u64,
        idempotency_key: Option<&str>,
    ) -> Result<AcceptedOperation, ClientError> {
        let generation = expected_generation.to_string();
        let mut headers = vec![
            ("content-type", "application/toml"),
            ("x-expected-generation", generation.as_str()),
        ];
        if let Some(key) = idempotency_key {
            headers.push(("idempotency-key", key));
        }
        self.send_text(
            Method::PUT,
            &format!("/api/v1/applications/{}", path_segment(id)),
            manifest,
            &headers,
        )
        .await
    }

    /// Plans creating an application from a TOML manifest.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_create_toml(&self, manifest: &str) -> Result<PlanView, ClientError> {
        self.send_text(
            Method::POST,
            "/api/v1/applications/plan",
            manifest,
            &[("content-type", "application/toml")],
        )
        .await
    }

    /// Plans replacing an application from a TOML manifest.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn plan_replace_toml(
        &self,
        id: &str,
        manifest: &str,
        expected_generation: u64,
    ) -> Result<PlanView, ClientError> {
        let generation = expected_generation.to_string();
        self.send_text(
            Method::POST,
            &format!("/api/v1/applications/{}/plan", path_segment(id)),
            manifest,
            &[
                ("content-type", "application/toml"),
                ("x-expected-generation", &generation),
            ],
        )
        .await
    }
}
