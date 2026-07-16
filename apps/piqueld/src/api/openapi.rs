use axum::response::IntoResponse;
use http::header;
use piqueld_client::{
    AcceptedOperation, ApplicationStatusView, ApplicationView, CreateApplicationRequest,
    DeleteApplicationRequest, Envelope, ErrorBody, ExpectedGeneration, OperationView, Page,
    PlanApplicationRequest, PlanView, ReplaceApplicationRequest, SystemCapabilities, SystemStatus,
};
use schemars::{JsonSchema, schema_for};
use serde_json::{Value, json};

pub(super) async fn openapi() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.1",
        )],
        openapi_document().to_string(),
    )
}

/// Generated `OpenAPI` contract. Core domain schemas are linked as explicit JSON objects to avoid coupling core to Utoipa.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn openapi_document() -> Value {
    let mut schemas = serde_json::Map::new();
    add_schema::<ErrorBody>(&mut schemas, "ErrorBody");
    add_schema::<Envelope<SystemStatus>>(&mut schemas, "SystemStatusEnvelope");
    add_schema::<Envelope<SystemCapabilities>>(&mut schemas, "SystemCapabilitiesEnvelope");
    add_schema::<Envelope<Page<ApplicationView>>>(&mut schemas, "ApplicationPageEnvelope");
    add_schema::<Envelope<ApplicationView>>(&mut schemas, "ApplicationEnvelope");
    add_schema::<CreateApplicationRequest>(&mut schemas, "CreateApplicationRequest");
    add_schema::<ReplaceApplicationRequest>(&mut schemas, "ReplaceApplicationRequest");
    add_schema::<PlanApplicationRequest>(&mut schemas, "PlanApplicationRequest");
    add_schema::<piqueld_client::ReplacePlanRequest>(&mut schemas, "ReplacePlanRequest");
    add_schema::<DeleteApplicationRequest>(&mut schemas, "DeleteApplicationRequest");
    if let Some(force) = schemas
        .get_mut("DeleteApplicationRequest")
        .and_then(|schema| schema.pointer_mut("/properties/force"))
    {
        force["enum"] = json!([false]);
        force["description"] =
            json!("Must be false. Force deletion is unsupported and named volumes are retained.");
    }
    add_schema::<ExpectedGeneration>(&mut schemas, "ExpectedGeneration");
    add_schema::<Envelope<AcceptedOperation>>(&mut schemas, "AcceptedOperationEnvelope");
    add_schema::<Envelope<PlanView>>(&mut schemas, "PlanEnvelope");
    add_schema::<Envelope<ApplicationStatusView>>(&mut schemas, "ApplicationStatusEnvelope");
    add_schema::<Envelope<OperationView>>(&mut schemas, "OperationEnvelope");

    let id = json!({"name":"id","in":"path","required":true,"schema":{"type":"string","minLength":8,"maxLength":64}});
    let last_event_id = json!({"name":"Last-Event-ID","in":"header","required":false,"schema":{"type":"string"},"description":"Last durable/current-state event ID received by the client."});
    let expected_generation = json!({"name":"X-Expected-Generation","in":"header","required":false,"schema":{"type":"integer","format":"uint64","minimum":1},"description":"Required for application/toml replacement and replacement planning."});
    let json_toml = |schema: &str| {
        json!({
            "required":true,
            "content":{
                "application/json":{"schema":{"$ref":format!("#/components/schemas/{schema}")}},
                "application/toml":{"schema":{"type":"string"}},
                "text/toml":{"schema":{"type":"string"}}
            }
        })
    };
    let json_body = |schema: &str| json!({"required":true,"content":{"application/json":{"schema":{"$ref":format!("#/components/schemas/{schema}")}}}});
    let response = |description: &str, schema: &str| json!({"description":description,"content":{"application/json":{"schema":{"$ref":format!("#/components/schemas/{schema}")}}}});
    let errors = |statuses: &[&str]| {
        let mut map = serde_json::Map::new();
        for status in statuses {
            map.insert(
                (*status).into(),
                response("Structured, sanitized error", "ErrorBody"),
            );
        }
        map
    };
    let operation = |operation_id: &str,
                     success_status: &str,
                     success_schema: &str,
                     statuses: &[&str]| {
        let mut responses = errors(statuses);
        responses.insert(success_status.into(), response("Success", success_schema));
        json!({"operationId":operation_id,"summary":operation_summary(operation_id),"responses":responses})
    };
    let mut paths = serde_json::Map::new();
    paths.insert(
        "/api/v1/system/status".into(),
        json!({"get":operation("systemStatus","200","SystemStatusEnvelope", &["500","503"])}),
    );
    paths.insert(
        "/api/v1/system/capabilities".into(),
        json!({"get":operation("systemCapabilities","200","SystemCapabilitiesEnvelope", &["500"])}),
    );
    paths.insert("/api/v1/openapi.json".into(), json!({"get":{"operationId":"openApiDocument","summary":operation_summary("openApiDocument"),"responses":{"200":{"description":"OpenAPI 3.1 document","content":{"application/vnd.oai.openapi+json":{"schema":{"type":"object"}}}}}}}));

    let mut list = operation(
        "listApplications",
        "200",
        "ApplicationPageEnvelope",
        &["400", "500", "503"],
    );
    list["parameters"] = json!([
        {"name":"cursor","in":"query","required":false,"schema":{"type":"string"}},
        {"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":100,"default":50}}
    ]);
    let mut create = operation(
        "createApplication",
        "202",
        "AcceptedOperationEnvelope",
        &["400", "409", "415", "422", "500", "502", "503"],
    );
    create["parameters"] = json!([{"name":"Idempotency-Key","in":"header","required":true,"schema":{"type":"string","minLength":1,"maxLength":128}}]);
    create["requestBody"] = json_toml("CreateApplicationRequest");
    paths.insert(
        "/api/v1/applications".into(),
        json!({"get":list,"post":create}),
    );

    let mut create_plan = operation(
        "planApplicationCreate",
        "200",
        "PlanEnvelope",
        &["400", "409", "415", "422", "500", "503"],
    );
    create_plan["requestBody"] = json_toml("PlanApplicationRequest");
    paths.insert(
        "/api/v1/applications/plan".into(),
        json!({"post":create_plan}),
    );

    let mut get_app = operation(
        "getApplication",
        "200",
        "ApplicationEnvelope",
        &["400", "404", "500", "503"],
    );
    get_app["parameters"] = json!([id.clone()]);
    let mut replace = operation(
        "replaceApplication",
        "202",
        "AcceptedOperationEnvelope",
        &["400", "404", "409", "415", "422", "500", "502", "503"],
    );
    replace["parameters"] = json!([id.clone(), expected_generation.clone()]);
    replace["requestBody"] = json_toml("ReplaceApplicationRequest");
    let mut delete = operation(
        "deleteApplication",
        "202",
        "AcceptedOperationEnvelope",
        &["400", "404", "409", "415", "500", "502", "503"],
    );
    delete["parameters"] = json!([id.clone()]);
    delete["requestBody"] = json_body("DeleteApplicationRequest");
    paths.insert(
        "/api/v1/applications/{id}".into(),
        json!({"get":get_app,"put":replace,"delete":delete}),
    );

    let mut replace_plan = operation(
        "planApplicationReplace",
        "200",
        "PlanEnvelope",
        &["400", "404", "409", "415", "422", "500", "502", "503"],
    );
    replace_plan["parameters"] = json!([id.clone(), expected_generation]);
    replace_plan["requestBody"] = json_toml("ReplacePlanRequest");
    paths.insert(
        "/api/v1/applications/{id}/plan".into(),
        json!({"post":replace_plan}),
    );

    let mut reconcile = operation(
        "reconcileApplication",
        "202",
        "AcceptedOperationEnvelope",
        &["400", "404", "409", "415", "500", "502", "503"],
    );
    reconcile["parameters"] = json!([id.clone()]);
    reconcile["requestBody"] = json_body("ExpectedGeneration");
    paths.insert(
        "/api/v1/applications/{id}/reconcile".into(),
        json!({"post":reconcile}),
    );

    let mut status = operation(
        "applicationStatus",
        "200",
        "ApplicationStatusEnvelope",
        &["400", "404", "500", "503"],
    );
    status["parameters"] = json!([id.clone()]);
    paths.insert(
        "/api/v1/applications/{id}/status".into(),
        json!({"get":status}),
    );

    let event_response = json!({"description":"Server-Sent Events with durable/current-state IDs and bounded replay reset events.","content":{"text/event-stream":{"schema":{"type":"string"}}}});
    let mut app_events = json!({"operationId":"watchApplication","summary":operation_summary("watchApplication"),"parameters":[id.clone(),last_event_id.clone()],"responses":{"200":event_response.clone(),"400":response("Structured, sanitized error","ErrorBody"),"404":response("Structured, sanitized error","ErrorBody"),"500":response("Structured, sanitized error","ErrorBody"),"503":response("Structured, sanitized error","ErrorBody")}});
    app_events["x-sse-keepalive-seconds"] = json!(15);
    paths.insert(
        "/api/v1/applications/{id}/events".into(),
        json!({"get":app_events}),
    );

    let mut get_operation = operation(
        "getOperation",
        "200",
        "OperationEnvelope",
        &["404", "500", "503"],
    );
    get_operation["parameters"] = json!([id.clone()]);
    paths.insert(
        "/api/v1/operations/{id}".into(),
        json!({"get":get_operation}),
    );
    let mut operation_events = json!({"operationId":"watchOperation","summary":operation_summary("watchOperation"),"parameters":[id,last_event_id],"responses":{"200":event_response,"404":response("Structured, sanitized error","ErrorBody"),"500":response("Structured, sanitized error","ErrorBody"),"503":response("Structured, sanitized error","ErrorBody")}});
    operation_events["x-sse-terminal-closes"] = json!(true);
    operation_events["x-sse-keepalive-seconds"] = json!(15);
    paths.insert(
        "/api/v1/operations/{id}/events".into(),
        json!({"get":operation_events}),
    );

    json!({
        "openapi":"3.1.0",
        "info":{"title":"piqueld API","version":"v1","description":"Plan 05 control-plane API. Mutation responses identify durable operations; named volumes are retained on deletion.","license":{"name":"MIT","identifier":"MIT"}},
        "servers":[{"url":"http://127.0.0.1:7845","description":"Default loopback TCP endpoint; clients may also use the configured Unix socket."}],
        "security":[],
        "paths":paths,
        "components":{"schemas":schemas}
    })
}

fn add_schema<T: JsonSchema>(schemas: &mut serde_json::Map<String, Value>, name: &str) {
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

fn operation_summary(operation_id: &str) -> &'static str {
    match operation_id {
        "systemStatus" => "Get daemon status",
        "systemCapabilities" => "Get daemon capabilities",
        "openApiDocument" => "Get the OpenAPI document",
        "listApplications" => "List applications",
        "createApplication" => "Create an application",
        "planApplicationCreate" => "Preview application creation",
        "getApplication" => "Get an application",
        "replaceApplication" => "Replace an application",
        "deleteApplication" => "Request application deletion",
        "planApplicationReplace" => "Preview application replacement",
        "reconcileApplication" => "Request application reconciliation",
        "applicationStatus" => "Get application status",
        "watchApplication" => "Watch application status events",
        "getOperation" => "Get an operation",
        "watchOperation" => "Watch operation events",
        _ => "piqueld API operation",
    }
}
