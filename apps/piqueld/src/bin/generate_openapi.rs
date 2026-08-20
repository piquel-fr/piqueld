//! Generates or checks the checked-in API `OpenAPI` snapshot.

use piqueld::api::openapi_document;

/// Generates the OpenAPI JSON snapshot or verifies that the checked-in snapshot is current.
///
/// Pass `--check` to compare the generated document with the existing snapshot without modifying it.
///
/// # Examples
///
/// ```text
/// cargo run --
/// cargo run -- --check
/// ```
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi-v1.json");
    let mut document = serde_json::to_string_pretty(&openapi_document())?;
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
