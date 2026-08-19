# Read-only web dashboard

Plan 06C adds a small client-side-rendered Leptos dashboard. It answers four
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

Install the target and Trunk once, or use the corresponding tools from
`nix develop`:

```console
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
```

Start the daemon on its default loopback address, then run the UI development
server:

```console
just ui-dev
```

`trunk` serves the UI locally and proxies its `/api` requests to
`http://127.0.0.1:7845`, so the browser still exercises same-origin-style API
paths. A direct transport compile is available with:

```console
just ui-check
```

The production bundle has a dependency-free Chromium smoke covering desktop
and narrow layouts, empty and populated states, detail selection ordering,
refresh failure and recovery, keyboard focus, and the absence of mutations:

```console
just ui-browser-smoke
```

Set `PIQUELD_BROWSER` when Chromium is not available under its usual command
name.

## Production assets

The source files `apps/piqueld-ui/index.html`, `style.css`, and the Rust UI are
committed. Trunk-generated HTML/CSS/WASM/JavaScript loader assets are build
outputs and are not committed. Build them with:

```console
just ui-build
sudo install -d /usr/share/piqueld/ui
sudo cp -R target/piqueld-ui-dist/. /usr/share/piqueld/ui/
```

The default configuration leaves `server.ui_dir` unset. Deployments can select
another absolute directory in `[server]`; that explicit override always wins.
The Nix package builds the same release bundle, installs it under its own
`$out/share/piqueld/ui`, and wraps `piqueld` with that path. Users do not need
to discover or copy the store path into configuration.

The normal package contains only the operator-facing `piqueld` and `piquelctl`
binaries, the example configuration, and the dashboard assets. The
`generate_openapi` helper and the native `piqueld-ui` placeholder are not
installed.

The TCP router serves existing assets and uses `index.html` only for
extensionless, non-reserved browser paths. API, health, OpenAPI, and unknown API
paths never receive the SPA shell. If the bundle is absent, the browser route
returns a clear 503 message and missing extensionful files remain 404s.

The dashboard performs one initial refresh, then bounded sequential pagination
(20 items per page, at most 20 pages) and application status reads. Background
polls run every 15 seconds after success and back off to at most 120 seconds
after failures. Polls pause while the document is hidden, never overlap, and a
manual refresh remains available. A failed refresh keeps the last successful
view visible and marks it stale.

Accessibility coverage includes semantic headings and lists, a skip link,
keyboard-operable buttons, visible focus, live status/error regions, responsive
layouts for narrow widths, and light/dark color tokens with contrast-oriented
status colors.

The supported browser baseline is a current evergreen Chromium, Firefox,
Safari, or Edge release with WebAssembly, ES modules, Fetch, and standard CSS
media-query support. Internet Explorer, JavaScript-disabled browsing, and
older browsers without those primitives are outside the support target.

The advanced Plan 12 UI remains deferred: forms, mutation workflows, secrets,
logs and streams, state transfer, authentication, persistence, global state
machinery, and richer navigation are intentionally not part of this dashboard.
