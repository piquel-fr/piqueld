//! Read-only browser dashboard for the Plan 06 daemon.

pub mod state;

#[cfg(target_arch = "wasm32")]
mod browser;

/// Mounts the dashboard into the current document.
#[cfg(target_arch = "wasm32")]
pub fn mount() {
    browser::mount();
}
