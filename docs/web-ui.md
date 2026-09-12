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

Use `nix develop` for the Rust/WASM toolchain, Nextest, Cargo Watch, Trunk,
wasm-bindgen, Binaryen, and Tailwind. Alternatively install those tools and
add the WASM target with `rustup target add wasm32-unknown-unknown`.
Docker must be accessible to your user and support a single-node Swarm.
Replace the example configuration's UID with your own before starting:

```console
just dev
```

This watches the daemon and dashboard sources, builds the embedded bundle
with Tailwind and Trunk, and runs the daemon using
`config/piqueld.example.toml`. Open `http://127.0.0.1:7845/dashboard/` and
refresh the browser after a rebuild. Stopping the command allows the daemon
its graceful shutdown period before terminating any remaining processes.

Compile the browser client and dashboard without building assets with:

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
# or: just build-embedded
```

There is no runtime UI configuration: the dashboard exists exactly when the
binary was built with the feature, and binaries built without it are API-only.
The combined Nix package (`.#`) includes `piquelctl` and a daemon with the
release dashboard embedded. It builds the bundle hermetically in `preBuild` and
hands it to the build script through `PIQUELD_UI_DIST`, which skips tool
invocation for packagers that supply their own distribution directory. The
`.#daemon` output contains only the daemon without the feature, and the
`.#cli` output contains only `piquelctl`.

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
