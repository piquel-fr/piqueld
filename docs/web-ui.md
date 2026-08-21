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

Install the WASM target, Docker, and the UI tools once, or use the corresponding
tools from `nix develop`:

```console
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
```

Docker is required because the development daemon connects to the real Docker
Engine and reconciles a single-node Swarm. Start the complete development
toolchain with one command:

```console
just dev
```

This starts `piqueld` from `config/piqueld.example.toml`, Tailwind in watch
mode, and Trunk with `/api/` proxied unchanged to
`http://127.0.0.1:7845/api/`. `just dev` serves the watched dashboard at
`http://127.0.0.1:8080/` and accepts the canonical `/dashboard/` path as well.
It reloads the UI after Rust or CSS changes. Set `TRUNK_SERVE_ADDRESS` when
the dev server must be reachable from another interface, for example:

```bash
TRUNK_SERVE_ADDRESS="$(tailscale ip -4)" just dev
```

A direct transport compile is available with:

```console
just ui-check
```

## Production assets

The source files `apps/piqueld-ui/index.html`, `tailwind.css`, and the Rust UI
are committed. Tailwind's generated CSS and Trunk-generated
HTML/WASM/JavaScript loader assets are build outputs and are not committed.
Build them with:

```console
just ui-build
sudo install -d /usr/share/piqueld/ui
sudo cp -R target/piqueld-ui-dist/. /usr/share/piqueld/ui/
```

After copying the bundle, set `server.ui_dir = "/usr/share/piqueld/ui"` (or
pass `--ui-dir`) so the daemon serves it; there is no implicit filesystem
fallback.

The default configuration leaves `server.ui_dir` unset. Deployments can select
another absolute directory in `[server]` or through `--ui-dir`; the command
line always wins. The combined Nix package builds the same release bundle and
installs it under `$out/share/piqueld/ui`; point `server.ui_dir` or `--ui-dir`
at that directory to serve it. The `.#daemon` output contains only the daemon,
and the `.#cli` output contains only `piquelctl`; neither includes UI assets or
UI build tooling.

The combined package contains only the operator-facing `piqueld` and
`piquelctl` binaries, the example configuration, and the dashboard assets. The
`generate_openapi` helper and the native `piqueld-ui` placeholder are not
installed.

The TCP router serves existing assets below `/dashboard/` and uses `index.html`
only for extensionless dashboard paths. API, health, and unknown paths never
receive the SPA shell. If UI assets are configured but missing or incomplete,
dashboard requests return a clear 503 message. With UI assets disabled, root
and dashboard routes are not registered and the process is API-only.

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
