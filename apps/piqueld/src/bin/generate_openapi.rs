//! Generates or checks the checked-in API `OpenAPI` snapshot.

use piqueld::api::openapi_document;
use serde_json::Value;

fn validate_api_paths(document: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .ok_or("OpenAPI document has no paths object")?;
    for path in paths.keys() {
        if !path.starts_with("/api/") {
            return Err(format!("OpenAPI path is outside the API namespace: {path}").into());
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi-v1.json");
    let value = openapi_document();
    validate_api_paths(&value)?;
    let mut document = serde_json::to_string_pretty(&value)?;
    document.push('\n');
    if std::env::args().any(|argument| argument == "--check") {
        let existing = std::fs::read_to_string(&path)?;
        if existing != document {
            return Err(format!("{} is out of date", path.display()).into());
        }
        println!("{} is up to date", path.display());
    } else {
        std::fs::write(&path, document)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}
