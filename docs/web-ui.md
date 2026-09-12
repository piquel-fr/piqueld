# Read-only web dashboard

piqueld ships a small client-side-rendered Leptos dashboard. It answers four
questions: whether the daemon is reachable, which applications exist, what
their desired and observed state is, and whether each application is converged,
degraded, or failed. It has no mutation controls; the visible operator
direction is to use `piquelctl` for plan, apply, reconcile, and delete.

The browser bundle uses the transport-neutral DTOs in `piqueld-client` and
fetches same-origin `/api/v1` resources. The daemon serves it only on the
loopback TCP listener. The Unix socket is API-only, and the daemon does not add
CORS, authentication, cookies, browser persistence, telemetry, or a public
binding.

## Development

Install the WASM target and the UI build tools (`trunk`, `wasm-bindgen-cli`,
`binaryen`, and `tailwindcss`). Docker must be running as a single-node Swarm
for the daemon to reconcile applications.

```console
rustup target add wasm32-unknown-unknown
cargo run --package piqueld --features embedded-ui -- --config config/piqueld.example.toml
```

The build script runs Tailwind and Trunk and embeds the dashboard. Open
`http://127.0.0.1:7845/dashboard/`. Re-run the command after editing UI sources
to rebuild the bundle, then refresh the browser.

A direct transport compile is available with:

```console
cargo check --package piqueld-ui --target wasm32-unknown-unknown
```

## Production assets

The source files `apps/piqueld-ui/index.html`, `tailwind.css`, and the Rust UI
are committed. Tailwind's generated CSS and Trunk-generated
HTML/WASM/JavaScript loader assets are build outputs and are not committed.

The release dashboard ships inside the daemon binary itself. Building with the
feature embeds the bundle; the daemon's build script runs Tailwind and Trunk,
so `trunk`, `wasm-bindgen-cli`, `binaryen`, and `tailwindcss` must be on the
path:

```console
cargo build --release --package piqueld --features embedded-ui --locked
```

There is no runtime UI configuration: the dashboard exists exactly when the
binary was built with the feature, and binaries built without it are API-only.
Packagers can provide a prebuilt distribution through `PIQUELD_UI_DIST`,
which skips the build script's UI tool invocation. The current default Nix
package builds without the embedded dashboard feature.

The TCP router serves bundle files below `/dashboard/` and uses `index.html`
only for extensionless dashboard paths. API, health, and unknown paths never
receive the SPA shell. Content-hashed asset filenames are served with
immutable caching; the shell is always revalidated.

The dashboard performs one initial refresh, then bounded pagination
(20 items per page, at most 20 pages) with bounded-concurrency application
status reads. Background polls run every 15 seconds after success and back off
to at most 120 seconds after failures. Polls pause while the document is
hidden, never overlap, and a manual refresh remains available. A failed refresh
keeps the last successful view visible and marks it stale.

Healthy replica counts come from Docker's container healthcheck verdicts for
services that declare one; services without a healthcheck count running tasks
as healthy.

Accessibility coverage includes semantic headings and lists, a skip link,
keyboard-operable buttons, visible focus, live status/error regions, responsive
layouts for narrow widths, and light/dark color tokens with contrast-oriented
status colors.

The supported browser baseline is a current evergreen Chromium, Firefox,
Safari, or Edge release with WebAssembly, ES modules, Fetch, and standard CSS
media-query support. Internet Explorer, JavaScript-disabled browsing, and
older browsers without those primitives are outside the support target.

The advanced UI remains deferred: forms, mutation workflows, secrets,
logs and streams, state transfer, authentication, persistence, global state
machinery, and richer navigation are intentionally not part of this dashboard.
