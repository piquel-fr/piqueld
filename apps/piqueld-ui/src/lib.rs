//! The piqueld browser dashboard: applications, environments, deployments,
//! logs, accounts, and observability.

pub mod application;
pub mod auth;
pub mod deployments;
pub mod editor;
pub mod environments;
pub mod log_output;
pub mod overview;
pub mod secrets;
pub mod state;
pub mod terminal;

#[cfg(target_arch = "wasm32")]
mod browser;

/// Mounts the dashboard into the current document.
#[cfg(target_arch = "wasm32")]
pub fn mount() {
    browser::mount();
}
