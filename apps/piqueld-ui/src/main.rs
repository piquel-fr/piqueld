//! Browser entry point for the piqueld dashboard.

/// Mounts the dashboard when running in the browser.
#[cfg(target_arch = "wasm32")]
fn main() {
    piqueld_ui::mount();
}

/// Native builds only print the version; the dashboard runs in `wasm32`.
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!(
        "piqueld-ui {} (build for wasm32-unknown-unknown to run)",
        piqueld_client::version()
    );
}
