use axum::{Extension, response::IntoResponse};
use http::header;
use piqueld_core::api::{Envelope, ErrorBody, SecretMetadata};
use piqueld_core::{edit::RoutesValue, manifest::Route};
use serde_json::Value;
use std::sync::Arc;
use utoipa::{OpenApi, ToResponse};

// Base document: API metadata plus explicitly registered component schemas.
// Paths are merged in by `documented_router`.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "piqueld API",
        version = "v1",
        description = "piqueld control-plane API. Mutation responses identify durable operations; named volumes are retained on deletion.",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    components(schemas(
        ErrorBody,
        piqueld_core::auth::User,
        piqueld_core::auth::AuthStatus,
        piqueld_core::auth::RegistrationStart,
        piqueld_core::auth::Ceremony,
        piqueld_core::auth::CeremonyFinish,
        piqueld_core::auth::PasskeyView,
        piqueld_core::auth::CredentialView,
        piqueld_core::auth::InvitationView,
        piqueld_core::auth::Directory,
        piqueld_core::auth::Manage,
        piqueld_core::auth::Managed,
        piqueld_core::auth::DeviceStart,
        piqueld_core::auth::DevicePoll,
        piqueld_core::auth::DeviceApprove,
        piqueld_core::auth::DeviceToken,
        SecretMetadata,
        piqueld_core::api::SecretKeyRecovery,
        Envelope<Vec<SecretMetadata>>,
        Envelope<SecretMetadata>,
        Envelope<bool>,
        Route,
        RoutesValue
    ))
)]
struct ApiDoc;

/// Returns the path-less base document that `documented_router` extends.
pub(super) fn base_document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

// Utoipa's declarative response schema uses this tuple field as a body type;
// no runtime value reads the field. Keep the schema-only wrapper rather than
// replacing the derive with a handwritten ToResponse implementation.
#[allow(dead_code)]
#[derive(ToResponse)]
#[response(description = "Structured, sanitized error")]
pub(super) struct ApiErrorResponse(ErrorBody);

// Serves the precomputed `OpenAPI` 3.0 document installed by `finish_router`.
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    operation_id = "openApiDocument",
    summary = "Get the `OpenAPI` document",
    responses(
        (status = 200, description = "OpenAPI 3.0 document", body = Object, content_type = "application/vnd.oai.openapi+json")
    )
)]
pub(super) async fn openapi(Extension(document): Extension<Arc<Value>>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.0",
        )],
        document.to_string(),
    )
}

/// Generates the `OpenAPI` contract from Utoipa endpoint metadata.
///
/// # Panics
///
/// Panics if Utoipa's generated document cannot be serialized.
#[must_use]
pub fn openapi_document() -> Value {
    openapi_30_document(&super::documented_router().into_openapi())
}

/// Serializes Utoipa's `OpenAPI` 3.1 output as the published 3.0.3 contract.
///
/// Besides schema downgrading, it drops the license `identifier` (3.1 only),
/// declares bearer-token and session-cookie security for every operation,
/// clears security on public endpoints (see `auth::is_public`), strips
/// `nullable` from optional parameters, and documents the TCP 403 response.
pub(super) fn openapi_30_document(document: &utoipa::openapi::OpenApi) -> Value {
    let mut document = serde_json::to_value(document).expect("OpenAPI serialization cannot fail");
    convert_to_openapi_30(&mut document);
    document["openapi"] = Value::String("3.0.3".into());
    document["info"]["license"]
        .as_object_mut()
        .expect("license is an object")
        .remove("identifier");
    document["components"]["securitySchemes"] = serde_json::json!({
        "bearerAuth": {"type":"http", "scheme":"bearer"},
        "browserSession": {"type":"apiKey", "in":"cookie", "name":"piqueld_session"}
    });
    document["security"] = serde_json::json!([{"bearerAuth":[]},{"browserSession":[]}]);
    if let Some(paths) = document["paths"].as_object_mut() {
        for (path, item) in paths {
            if let Some(operations) = item.as_object_mut() {
                for operation in operations.values_mut() {
                    if super::auth::is_public(path) {
                        operation["security"] = serde_json::json!([]);
                    }
                }
            }
        }
    }
    remove_nullable_parameters(&mut document);
    complete_http_contract(&mut document);
    document
}

/// Converts Utoipa's JSON Schema output to its `OpenAPI` 3.0 equivalent.
///
/// Recursively rewrites nullability, since 3.0 has no `null` type:
///
/// ```text
/// {"type": ["string", "null"]}              -> {"type": "string", "nullable": true}
/// {"oneOf": [{"$ref": R}, {"type": "null"}]} -> {"oneOf": [{"$ref": R}, <nullable null enum>]}
/// {"oneOf": [{inline}, {"type": "null"}]}   -> {inline..., "nullable": true}
/// ```
///
/// References cannot carry `nullable` in 3.0 (siblings of `$ref` are ignored),
/// so they keep a `oneOf` with a null-only alternative.
fn convert_to_openapi_30(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                convert_to_openapi_30(value);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                convert_to_openapi_30(value);
            }
            // OpenAPI 3.0 supports `additionalProperties`, but not JSON
            // Schema's separate constraints on property names.
            object.remove("propertyNames");

            if let Some(Value::Array(types)) = object.get("type") {
                let non_null = types
                    .iter()
                    .filter(|value| value.as_str() != Some("null"))
                    .cloned()
                    .collect::<Vec<_>>();
                if non_null.len() == 1 && non_null.len() != types.len() {
                    object.insert("type".into(), non_null[0].clone());
                    object.insert("nullable".into(), Value::Bool(true));
                }
            }

            let nullable_schema =
                object
                    .get("oneOf")
                    .and_then(Value::as_array)
                    .and_then(|schemas| {
                        let non_null = schemas
                            .iter()
                            .filter(|schema| {
                                schema.get("type").and_then(Value::as_str) != Some("null")
                            })
                            .collect::<Vec<_>>();
                        (non_null.len() == 1 && non_null.len() != schemas.len())
                            .then(|| non_null[0].clone())
                    });
            if let Some(mut schema) = nullable_schema {
                object.remove("oneOf");
                if let Some(schema) = schema.as_object_mut()
                    && let Some(reference) = schema.remove("$ref")
                {
                    let mut referenced = serde_json::json!({ "$ref": reference });
                    if !schema.is_empty() {
                        schema.insert("allOf".into(), serde_json::json!([referenced]));
                        referenced = Value::Object(schema.clone());
                    }
                    object.insert(
                        "oneOf".into(),
                        serde_json::json!([
                            referenced,
                            { "type": "string", "nullable": true, "enum": [null] }
                        ]),
                    );
                } else if let Some(schema) = schema.as_object() {
                    object.extend(schema.clone());
                    object.insert("nullable".into(), Value::Bool(true));
                }
            }
        }
        _ => {}
    }
}

/// Adds the 403 returned by the TCP browser trust middleware
/// (`browser::BrowserPolicy`) to every documented operation.
fn complete_http_contract(document: &mut Value) {
    let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) else {
        return;
    };
    for item in paths.values_mut().filter_map(Value::as_object_mut) {
        for operation in item.values_mut().filter_map(Value::as_object_mut) {
            if let Some(responses) = operation
                .get_mut("responses")
                .and_then(Value::as_object_mut)
            {
                responses.insert("403".into(), serde_json::json!({
                    "description": "TCP authority or browser origin is not trusted",
                    "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorBody"}}}
                }));
            }
        }
    }
}

/// Optional HTTP parameters are absent rather than represented as JSON null.
fn remove_nullable_parameters(document: &mut Value) {
    let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) else {
        return;
    };
    for item in paths.values_mut().filter_map(Value::as_object_mut) {
        for operation in item.values_mut().filter_map(Value::as_object_mut) {
            let Some(parameters) = operation
                .get_mut("parameters")
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            for parameter in parameters.iter_mut().filter_map(Value::as_object_mut) {
                if parameter.get("required").and_then(Value::as_bool) != Some(true)
                    && let Some(schema) = parameter.get_mut("schema").and_then(Value::as_object_mut)
                {
                    schema.remove("nullable");
                }
            }
        }
    }
}
