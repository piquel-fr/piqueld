//! Standalone JSON Schema documents, published for editor completion and
//! inline errors in TOML files (through Taplo, for example).

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

/// The draft-7 JSON Schema of `T` titled `title`, with every schema `T`
/// refers to under `definitions`.
#[must_use]
pub fn standalone<T: ToSchema>(title: &str) -> Value {
    let mut definitions = Vec::new();
    T::schemas(&mut definitions);
    let mut schema = json!(T::schema());
    schema["$schema"] = "http://json-schema.org/draft-07/schema#".into();
    schema["title"] = title.into();
    schema["definitions"] = json!(definitions.into_iter().collect::<BTreeMap<_, _>>());
    relocate_refs(&mut schema);
    let mut combined = BTreeSet::new();
    combined_definitions(&schema, &mut combined);
    for name in combined {
        if let Some(definition) = schema["definitions"][&name].as_object_mut() {
            definition.remove("additionalProperties");
        }
    }
    schema
}

/// Collects the definitions an `allOf` combines with other schemas, such as
/// a serde tag's or a flattened structure's properties. Draft 7 checks
/// `additionalProperties` against its own schema's properties only, so these
/// must not forbid other properties, as utoipa makes `deny_unknown_fields`
/// structures do.
fn combined_definitions(value: &Value, names: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for member in object
                .get("allOf")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = member["$ref"]
                    .as_str()
                    .and_then(|reference| reference.strip_prefix("#/definitions/"))
                {
                    names.insert(name.to_owned());
                }
            }
            for value in object.values() {
                combined_definitions(value, names);
            }
        }
        Value::Array(values) => {
            for value in values {
                combined_definitions(value, names);
            }
        }
        _ => {}
    }
}

/// Rewrites utoipa's `OpenAPI` component refs to draft-7 `definitions` refs.
fn relocate_refs(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(reference)) = object.get_mut("$ref")
                && let Some(name) = reference.strip_prefix("#/components/schemas/")
            {
                *reference = format!("#/definitions/{name}");
            }
            for value in object.values_mut() {
                relocate_refs(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                relocate_refs(value);
            }
        }
        _ => {}
    }
}
