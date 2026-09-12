//! Browser entry point for the piqueld read-only dashboard.

#[cfg(target_arch = "wasm32")]
fn main() {
    piqueld_ui::mount();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!(
        "piqueld-ui {} (build for wasm32-unknown-unknown to run)",
        piqueld_client::version()
    );
}
