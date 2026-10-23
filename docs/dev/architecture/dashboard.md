# Dashboard

The dashboard is a Leptos application compiled to WebAssembly and embedded in the
daemon binary. See the [dashboard](../../user/dashboard.md) page of the user guide
for what it does.

## Development

Building the dashboard needs only Cargo and the `wasm32-unknown-unknown` target,
which `rust-toolchain.toml` installs. `nix develop` provides the same toolchain
plus `just` and Nextest. Docker must be accessible to your user and support
a single-node Swarm. Start the development workflow:

```console
just dev
```

This watches the daemon and dashboard sources, rebuilds the embedded bundle, and
restarts this worktree's isolated development instance, served on its own
localhost port. Refresh the browser after a rebuild. See
[development instances](../development.md).

Compile the browser client and dashboard without building the bundle with:

```console
just ui-check
```

The browser modules separate dashboard navigation and lists, editor state,
configuration forms, deployment history, navigation guards, and shared
presentation primitives (`browser/ui.rs` for icons, badges, notices, dialogs,
and tabs; `browser/format.rs` for relative times, durations, and sizes).
Styles are plain CSS: a base reset adapted from Tailwind's preflight, theme
tokens, shell layout, and component rules, in `apps/piqueld-ui/styles/`. To
format view macros as well as Rust, run `leptosfmt` from `apps/piqueld-ui` (it
reads the local `leptosfmt.toml`), followed by `cargo fmt --all`.

## Production assets

The release dashboard ships inside the daemon binary itself, and Cargo builds
it. With the `embedded-ui` feature, the daemon build script:

1. compiles `piqueld-ui` for `wasm32-unknown-unknown` with the size-optimized
   `dashboard` profile, using a nested Cargo with its own target directory;
2. generates the JavaScript bindings in-process with `wasm-bindgen-cli-support`,
   pinned to the exact `wasm-bindgen` version;
3. concatenates `styles/` in cascade order, and writes a small loader module;
4. fills the placeholders in `apps/piqueld-ui/index.html` with the
   content-hashed file names, and embeds every file.

Setting `PIQUELD_DASHBOARD_WASM` to a compiled `piqueld-ui.wasm` skips step 1
and embeds that module instead, and the build warns that changes under
`apps/piqueld-ui` are not included. The Nix packages pass their own cached
`dashboard` build this way, so daemon-only changes don't recompile the UI.

No Trunk, wasm-bindgen CLI, binaryen, Tailwind, or Node is involved. The shell
has no inline scripts, so the Content-Security-Policy is a constant. See
[ADR 0002](adr/0002-cargo-built-dashboard.md) for the reasoning and
the planned move to Cargo artifact dependencies, which would remove the nested
Cargo invocation.

```console
cargo build --release --package piqueld --bin piqueld --features embedded-ui --locked
# Build both the embedded daemon and CLI: just build-embedded
```

There is no runtime UI configuration: the dashboard exists exactly when the
binary was built with the feature, and binaries built without it are API-only.
The combined Nix package (`.#combined`, the default `.#` on Linux) includes
`piquelctl` and a daemon with the release dashboard embedded, built by the same
build script. The `.#daemon` output contains only the daemon without the
feature, and the `.#cli` output (the default on macOS) contains only `piquelctl`.

The TCP router serves bundle files below `/dashboard/` and uses `index.html`
only for extensionless dashboard paths. API, health, and unknown paths never
receive the SPA shell. Content-hashed asset filenames are served with
immutable caching; the shell is always revalidated.

The dashboard performs one initial refresh, then bounded pagination and
environment-status and recent-deployment reads. Background polls run every 15 seconds after success and back off
to at most 120 seconds after failures. Polls pause while the document is
hidden, never overlap, and a manual refresh remains available. A failed refresh
keeps the last successful view visible and marks it stale.
## Browser tests

The [Playwright suite](https://github.com/piquel-fr/piqueld/blob/main/tests/playwright/README.md) exercises the embedded dashboard against
an isolated API fixture. Run `just setup-playwright` once, then `just test-playwright`. Chromium
is provided by a pinned Docker image; the tests need no system browser, Python,
or ChromeDriver. The `browser-playwright` CI job retains failure traces and screenshots.
