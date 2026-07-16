use http::Method;
use piqueld_core::manifest::ApplicationManifest;
use piqueld_core::{NormalizedApplication, Plan};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Client, ClientError, Page, SseEvent, path_segment};

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ApplicationView {
    pub application: NormalizedApplication,
    #[schema(minimum = 1)]
    pub generation: u64,
    pub spec_hash: String,
    pub delete_intent: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateApplicationRequest {
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaceApplicationRequest {
    #[schema(minimum = 1)]
    pub expected_generation: u64,
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanApplicationRequest {
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplacePlanRequest {
    #[schema(minimum = 1)]
    pub expected_generation: u64,
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpectedGeneration {
    #[schema(minimum = 1)]
    pub expected_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteApplicationRequest {
    #[schema(minimum = 1)]
    pub expected_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct AcceptedOperation {
    pub operation_id: String,
    pub application_id: String,
    #[schema(minimum = 1)]
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PlanView {
    pub application_id: String,
    #[schema(minimum = 1)]
    pub proposed_generation: u64,
    pub plan: Plan,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ApplicationStatusView {
    pub application_id: String,
    pub state: String,
    #[schema(minimum = 1)]
    pub observed_generation: Option<u64>,
    pub message: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ListApplicationsOptions {
    pub cursor: Option<String>,
    pub limit: Option<u16>,
}

impl Client {
    pub async fn applications(&self) -> Result<Page<ApplicationView>, ClientError> {
        self.applications_with(&ListApplicationsOptions::default())
            .await
    }

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

    pub async fn application(&self, id: &str) -> Result<ApplicationView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/applications/{}", path_segment(id)),
            None,
            &[],
        )
        .await
    }

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

    pub async fn replace_application(
        &self,
        id: &str,
        request: &ReplaceApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::PUT,
            &format!("/api/v1/applications/{}", path_segment(id)),
            Some(request),
            &[],
        )
        .await
    }

    pub async fn delete_application(
        &self,
        id: &str,
        request: &DeleteApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::DELETE,
            &format!("/api/v1/applications/{}", path_segment(id)),
            Some(request),
            &[],
        )
        .await
    }

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

    pub async fn application_status(&self, id: &str) -> Result<ApplicationStatusView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/applications/{}/status", path_segment(id)),
            None,
            &[],
        )
        .await
    }

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

    pub async fn replace_application_toml(
        &self,
        id: &str,
        manifest: &str,
        expected_generation: u64,
    ) -> Result<AcceptedOperation, ClientError> {
        let generation = expected_generation.to_string();
        self.send_text(
            Method::PUT,
            &format!("/api/v1/applications/{}", path_segment(id)),
            manifest,
            &[
                ("content-type", "application/toml"),
                ("x-expected-generation", &generation),
            ],
        )
        .await
    }

    pub async fn plan_create_toml(&self, manifest: &str) -> Result<PlanView, ClientError> {
        self.send_text(
            Method::POST,
            "/api/v1/applications/plan",
            manifest,
            &[("content-type", "application/toml")],
        )
        .await
    }

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

    #[must_use]
    pub fn watch_application(
        &self,
        id: &str,
        last_event_id: Option<&str>,
    ) -> tokio::sync::mpsc::Receiver<Result<SseEvent, ClientError>> {
        self.watch_events(
            format!("/api/v1/applications/{}/events", path_segment(id)),
            last_event_id,
        )
    }
}
