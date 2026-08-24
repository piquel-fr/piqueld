//! Compile-time dashboard bundle supplied by the build script.
//!
//! With the `embedded-ui` feature, the build script runs Tailwind and Trunk
//! and writes [`BUNDLE`] into `OUT_DIR`. Without it this module is empty.

#[cfg(feature = "embedded-ui")]
include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));
