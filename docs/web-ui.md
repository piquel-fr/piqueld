# Web control-plane dashboard

The Leptos dashboard is the browser companion to the versioned API. It keeps
the Plan 06C bounded polling model for the application list and adds the
smallest complete operator workflows needed by the product stack:

- structured application editing for images, Git sources, services,
  environment, ports, commands, mounts, secrets, health checks, resources,
  volumes, and routes;
- server-generated plan previews, generation-checked apply/delete operations,
  reviewed deletion, and conflict recovery that preserves the local form;
- operation progress, build-log polling, runtime status, bounded log buffers,
  same-origin event streams, automatic browser reconnection, and polling
  fallback;
- write-only secret creation and rotation, with no reveal or browser storage;
- portable/encrypted state export and digest-bound, phrase-confirmed,
  transactional state replacement; and
- accessible responsive navigation, live status regions, keyboard focus, and
  bounded output throughout.

The browser uses the transport-neutral DTOs and mutation methods in
`piqueld-client`. EventSource is limited to the server's resumable operation
and runtime-log streams; build output uses the typed build and log endpoints
because the current API does not expose a separate build SSE route.

The TCP listener serves the dashboard; the Unix socket remains API-only. The
browser client uses same-origin requests with no cookies, browser persistence,
or telemetry. It relies on the daemon's configured bearer or trusted-proxy
transport policy; it never stores credentials or secret values.

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
`http://127.0.0.1:7845`. A direct transport compile is available with:

```console
just ui-check
```

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

The fallback serves only GET/HEAD. Extensionless, non-reserved routes receive
the shell; API, health, OpenAPI, percent-encoded, and unknown extensionful
paths never receive it. Assets are opened descriptor-relatively beneath the
configured bundle directory with symlink and magic-link resolution disabled,
bounded to 16 MiB, checked for metadata changes, and served with content types,
HEAD support, single-range support, and explicit cache policy. Fingerprinted
assets are immutable; the shell is revalidated. Responses include CSP,
`nosniff`, frame, referrer, and permissions policies. If the bundle is absent,
browser routes return a clear uncached 503 and extensionful files remain 404s.

The dashboard still performs bounded sequential pagination (20 items per page,
at most 20 pages). Background list polls run every 15 seconds after success and
back off to at most 120 seconds after failures; they pause while hidden, never
overlap, and retain the last successful view after an error. Live buffers retain
at most 1,000 lines per view.

## Supported browser baseline

The supported baseline is a current evergreen Chromium, Firefox, Safari, or
Edge release with WebAssembly, ES modules, Fetch, EventSource, and standard CSS
media-query support. Internet Explorer, JavaScript-disabled browsing, and
older browsers without those primitives are outside the support target.
