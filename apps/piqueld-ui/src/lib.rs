//! Browser application management and deployment history.

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
