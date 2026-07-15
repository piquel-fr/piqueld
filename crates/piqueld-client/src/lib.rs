//! Typed client surface for piqueld.
//!
//! HTTP behavior is introduced with the API increment; this foundation only
//! establishes the dependency boundary.

/// Returns the client crate version embedded at build time.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
