use axum::response::IntoResponse;
use http::header;
use piqueld_client::{
    AcceptedOperation, ApplicationStatusView, ApplicationView, CreateApplicationRequest,
    DeleteApplicationRequest, Envelope, ErrorBody, ExpectedGeneration, OperationView, Page,
    PlanApplicationRequest, PlanView, ReplaceApplicationRequest, SystemCapabilities, SystemStatus,
};
use schemars::{JsonSchema, schema_for};
use serde_json::{Value, json};
use utoipa::{OpenApi, ToResponse, ToSchema};

use super::{applications, operations, system};

#[derive(OpenApi)]
#[openapi(
    paths(
        system::status,
        system::capabilities,
        openapi,
        applications::list,
        applications::create,
        applications::plan_create,
        applications::get,
        applications::replace,
        applications::delete,
        applications::plan_replace,
        applications::reconcile,
        applications::status,
        applications::events,
        operations::get,
        operations::events,
    ),
    info(
        title = "piqueld API",
        version = "v1",
        description = "Plan 05 control-plane API. Mutation responses identify durable operations; named volumes are retained on deletion.",
        license(name = "MIT", identifier = "MIT")
    ),
    servers(
        (url = "http://127.0.0.1:7845", description = "Default loopback TCP endpoint; clients may also use the configured Unix socket.")
    ),
)]
struct ApiDoc;

trait SchemaSource {
    type Source: JsonSchema;
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

        impl SchemaSource for $name {
            type Source = Envelope<$body>;
        }
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
pub(super) async fn openapi() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.1",
        )],
        openapi_document().to_string(),
    )
}

/// Generates the `OpenAPI` contract from Utoipa endpoint metadata.
///
/// Core domain schemas are bridged from Schemars as JSON objects so the domain
/// crate does not need to depend on the HTTP-facing Utoipa crate.
///
/// # Panics
///
/// Panics if Utoipa produces an `OpenAPI` value with an invalid root or components
/// shape, or if an in-memory schema cannot be serialized.
#[must_use]
pub fn openapi_document() -> Value {
    let mut document =
        serde_json::to_value(ApiDoc::openapi()).expect("OpenAPI serialization cannot fail");
    let components = document
        .as_object_mut()
        .expect("OpenAPI document must be an object")
        .entry("components")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("OpenAPI components must be an object");
    let schemas = components
        .entry("schemas")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("OpenAPI schemas must be an object");
    // Schemars remains authoritative for schema bodies while Utoipa provides
    // compile-time-safe component names in endpoint annotations.
    schemas.clear();
    add_schema::<ErrorBody>(schemas);
    add_schema_as::<SystemStatusEnvelope>(schemas);
    add_schema_as::<SystemCapabilitiesEnvelope>(schemas);
    add_schema_as::<ApplicationPageEnvelope>(schemas);
    add_schema_as::<ApplicationEnvelope>(schemas);
    add_schema::<CreateApplicationRequest>(schemas);
    add_schema::<ReplaceApplicationRequest>(schemas);
    add_schema::<PlanApplicationRequest>(schemas);
    add_schema::<piqueld_client::ReplacePlanRequest>(schemas);
    add_schema::<DeleteApplicationRequest>(schemas);
    add_schema::<ExpectedGeneration>(schemas);
    add_schema_as::<AcceptedOperationEnvelope>(schemas);
    add_schema_as::<PlanEnvelope>(schemas);
    add_schema_as::<ApplicationStatusEnvelope>(schemas);
    add_schema_as::<OperationEnvelope>(schemas);
    document
}

fn add_schema<T: JsonSchema + ToSchema>(schemas: &mut serde_json::Map<String, Value>) {
    add_schema_named::<T>(schemas, T::name().as_ref());
}

fn add_schema_as<Name: SchemaSource + ToSchema>(schemas: &mut serde_json::Map<String, Value>) {
    add_schema_named::<Name::Source>(schemas, Name::name().as_ref());
}

fn add_schema_named<T: JsonSchema>(schemas: &mut serde_json::Map<String, Value>, name: &str) {
    let root = schema_for!(T);
    let mut schema = serde_json::to_value(root.schema).expect("schema serialization cannot fail");
    rewrite_schema_refs(&mut schema);
    schemas.insert(name.into(), schema);
    for (definition_name, definition) in root.definitions {
        let mut definition =
            serde_json::to_value(definition).expect("schema serialization cannot fail");
        rewrite_schema_refs(&mut definition);
        schemas.entry(definition_name).or_insert(definition);
    }
}

fn rewrite_schema_refs(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get_mut("$ref")
                && let Some(name) = reference.strip_prefix("#/definitions/")
            {
                *reference = format!("#/components/schemas/{name}");
            }
            for value in map.values_mut() {
                rewrite_schema_refs(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_schema_refs(value);
            }
        }
        _ => {}
    }
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
