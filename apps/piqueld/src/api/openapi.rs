use axum::{Extension, response::IntoResponse};
use http::header;
use piqueld_client::ErrorBody;
use serde_json::Value;
use std::sync::Arc;
use utoipa::{OpenApi, ToResponse};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "piqueld API",
        version = "v1",
        description = "piqueld control-plane API. Mutation responses identify durable operations; named volumes are retained on deletion.",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    servers(
        (url = "http://127.0.0.1:7845", description = "Default loopback TCP endpoint; clients may also use the configured Unix socket.")
    ),
    components(schemas(ErrorBody)),
)]
struct ApiDoc;

pub(super) fn base_document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

// Endpoint response attributes reference this type through Utoipa's proc macro.
// The Rust compiler cannot see that generated reference when checking dead code.
#[allow(dead_code)]
#[derive(ToResponse)]
#[response(description = "Structured, sanitized error")]
pub(super) struct ApiErrorResponse(ErrorBody);

#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    operation_id = "openApiDocument",
    summary = "Get the OpenAPI document",
    responses(
        (status = 200, description = "OpenAPI 3.1 document", body = Object, content_type = "application/vnd.oai.openapi+json")
    )
)]
pub(super) async fn openapi(
    Extension(document): Extension<Arc<utoipa::openapi::OpenApi>>,
) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.1",
        )],
        serde_json::to_string(document.as_ref()).expect("OpenAPI serialization cannot fail"),
    )
}

/// Generates the `OpenAPI` contract from Utoipa endpoint metadata.
///
/// # Panics
///
/// Panics if Utoipa's generated document cannot be serialized.
#[must_use]
pub fn openapi_document() -> Value {
    serde_json::to_value(super::documented_router().into_openapi())
        .expect("OpenAPI serialization cannot fail")
}
