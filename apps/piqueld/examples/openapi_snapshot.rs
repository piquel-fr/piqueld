//! Regenerates the checked-in Plan 05 `OpenAPI` snapshot.

use piqueld::api::openapi_document;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi-v1.json");
    let mut document = serde_json::to_string_pretty(&openapi_document())?;
    document.push('\n');
    std::fs::write(path, document)?;
    Ok(())
}
