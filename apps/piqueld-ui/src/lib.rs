//! The piqueld web dashboard: its browser views (built for `wasm32` only),
//! and the transport-independent state, form, and log helpers they use.

pub mod editor;
pub mod log_output;
pub mod state;

#[cfg(target_arch = "wasm32")]
mod browser;

/// Mounts the dashboard into the current document.
#[cfg(target_arch = "wasm32")]
pub fn mount() {
    browser::mount();
}
