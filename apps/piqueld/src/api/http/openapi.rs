use axum::{Extension, response::IntoResponse};
use http::header;
use piqueld_core::api::ErrorBody;
use piqueld_core::{edit::RoutesValue, manifest::Route};
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
    components(schemas(ErrorBody, Route, RoutesValue))
)]
struct ApiDoc;

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

pub(super) fn openapi_30_document(document: &utoipa::openapi::OpenApi) -> Value {
    let mut document = serde_json::to_value(document).expect("OpenAPI serialization cannot fail");
    convert_to_openapi_30(&mut document);
    document["openapi"] = Value::String("3.0.3".into());
    document["info"]["license"]
        .as_object_mut()
        .expect("license is an object")
        .remove("identifier");
    remove_nullable_parameters(&mut document);
    document
}

/// Converts Utoipa's JSON Schema output to its `OpenAPI` 3.0 equivalent.
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
