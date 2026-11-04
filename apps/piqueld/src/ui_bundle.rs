//! Compile-time dashboard bundle supplied by the build script.
//!
//! The build script writes [`BUNDLE`] into `OUT_DIR` and sets the
//! `embedded_dashboard` cfg when it embeds a dashboard: one it compiles under
//! the `embedded-ui` feature, or the prebuilt one a published crate ships.
//! Without either, this module is empty.

#[cfg(embedded_dashboard)]
include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));
