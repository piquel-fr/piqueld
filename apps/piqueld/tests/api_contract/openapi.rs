//! The served `OpenAPI` document against the generated snapshot and the routes it documents.

use super::*;

#[tokio::test]
async fn served_openapi_document_matches_the_generated_snapshot_and_resolves_refs() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;

    let response = server
        .send(Method::GET, "/api/v1/openapi.json", &[], Vec::new())
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let generated =
        serde_json::to_value(piqueld::api::http::openapi_document()).expect("document serializes");
    assert_eq!(response.json(), generated);
    for schema in ["ImageReference", "RepositoryDigest", "ImmutableImage"] {
        assert!(
            generated
                .pointer(&format!("/components/schemas/{schema}/pattern"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|pattern| !pattern.is_empty()),
            "{schema} must expose its validation pattern"
        );
    }

    let text = serde_json::to_string(&generated).expect("document stringifies");
    assert!(
        !text.contains("\"propertyNames\""),
        "OpenAPI 3.0 does not support propertyNames"
    );
    for (pointer, reference) in [
        (
            "/components/schemas/EnvironmentDetailView/properties/latest_operation",
            "#/components/schemas/Operation",
        ),
        (
            "/components/schemas/ApplicationSpec/properties/manifest",
            "#/components/schemas/RepositoryManifest",
        ),
        (
            "/components/schemas/DesiredService/properties/healthcheck",
            "#/components/schemas/ValidatedHealthCheck",
        ),
    ] {
        assert_nullable_reference(
            generated.pointer(pointer).expect("documented schema"),
            reference,
        );
    }
    let mut unresolved = Vec::new();
    collect_unresolved_refs(&generated, &generated, &mut unresolved);
    assert!(unresolved.is_empty(), "unresolved refs: {unresolved:?}");
}

fn assert_nullable_reference(schema: &serde_json::Value, reference: &str) {
    let variants = schema["oneOf"].as_array().expect("nullable oneOf variants");
    assert!(variants.iter().any(|variant| {
        variant
            .get("$ref")
            .or_else(|| variant.pointer("/allOf/0/$ref"))
            == Some(&serde_json::Value::String(reference.to_owned()))
    }));
    assert!(variants.iter().any(|variant| {
        variant["nullable"] == true
            && variant["enum"] == serde_json::json!([null])
            && variant["type"] == "string"
    }));
}

/// Collects into `out` every `$ref` in `value` that resolves to nothing in
/// `document`.
fn collect_unresolved_refs(
    value: &serde_json::Value,
    document: &serde_json::Value,
    out: &mut Vec<String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(serde_json::Value::as_str) {
                let pointer = reference.strip_prefix("#").unwrap_or(reference);
                if document.pointer(pointer).is_none() {
                    out.push(reference.to_owned());
                }
            }
            for child in map.values() {
                collect_unresolved_refs(child, document, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_unresolved_refs(child, document, out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn routed_statuses_and_media_types_are_documented_in_openapi() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let document = serde_json::to_value(piqueld::api::http::openapi_document()).unwrap();
    let cases = [
        (Method::GET, "/system/status", 200),
        (Method::GET, "/system/configuration", 503),
        (Method::GET, "/system/checks", 503),
        (Method::GET, "/applications", 200),
        (Method::GET, "/events", 200),
        (Method::GET, "/applications/{id}", 404),
        (Method::GET, "/environments/{id}/detail", 404),
        (Method::GET, "/environments/{id}/status", 404),
        (Method::GET, "/environments/{id}/deployments", 404),
        (Method::GET, "/operations/{id}", 404),
        (Method::POST, "/applications/apply", 400),
        (Method::POST, "/applications/plan", 400),
        (Method::GET, "/environments/{id}/exec", 426),
    ];
    for (method, suffix, status) in cases {
        let path = format!("/api/v1{suffix}");
        let response = server
            .send(
                method.clone(),
                &path.replace("{id}", "app-missing"),
                &[("content-type", "application/json")],
                if method == Method::POST {
                    b"{}".to_vec()
                } else {
                    Vec::new()
                },
            )
            .await;
        assert_eq!(response.status.as_u16(), status, "{method} {path}");
        response.assert_documented(&document, &method, &path);
    }
}
