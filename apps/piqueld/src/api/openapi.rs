use axum::{Extension, response::IntoResponse};
use http::header;
use piqueld_client::{
    AcceptedOperation, ApplicationStatusView, ApplicationView, Envelope, ErrorBody, OperationView,
    Page, PlanView, SystemCapabilities, SystemStatus,
};
use serde_json::Value;
use std::sync::Arc;
use utoipa::{OpenApi, ToResponse, ToSchema};

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

#[allow(dead_code)]
#[derive(ToResponse)]
#[response(description = "Structured, sanitized error")]
pub(super) struct ApiErrorResponse(ErrorBody);

macro_rules! envelope_schema {
    ($name:ident, $body:ty) => {
        #[allow(dead_code)]
        #[derive(ToSchema)]
        pub(super) struct $name(Envelope<$body>);
    };
}

envelope_schema!(SystemStatusEnvelope, SystemStatus);
envelope_schema!(SystemCapabilitiesEnvelope, SystemCapabilities);
envelope_schema!(ApplicationPageEnvelope, Page<ApplicationView>);
envelope_schema!(ApplicationEnvelope, ApplicationView);
envelope_schema!(AcceptedOperationEnvelope, AcceptedOperation);
envelope_schema!(PlanEnvelope, PlanView);
envelope_schema!(ApplicationStatusEnvelope, ApplicationStatusView);
envelope_schema!(OperationEnvelope, OperationView);

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::openapi_document;

    const OPERATION_IDS: [&str; 15] = [
        "systemStatus",
        "systemCapabilities",
        "openApiDocument",
        "listApplications",
        "createApplication",
        "planApplicationCreate",
        "getApplication",
        "replaceApplication",
        "deleteApplication",
        "planApplicationReplace",
        "reconcileApplication",
        "applicationStatus",
        "watchApplication",
        "getOperation",
        "watchOperation",
    ];

    #[test]
    fn every_openapi_operation_uses_a_registered_id() {
        let document = openapi_document();
        let actual = document["paths"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|path| path.as_object().unwrap().values())
            .map(|operation| operation["operationId"].as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        let expected = OPERATION_IDS
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();

        assert_eq!(actual, expected);
        assert_eq!(actual.len(), OPERATION_IDS.len());
    }
}
