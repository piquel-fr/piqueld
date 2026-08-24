# Unified web UI and API plan

## Goal

Run the HTTP API and operator dashboard from the same `piqueld` process while
keeping the dashboard an optional deployment artifact.

The production URL layout will be:

| Path | Behavior |
| --- | --- |
| `/` | Permanently redirect to `/dashboard/` when the UI is enabled. |
| `/dashboard` | Permanently redirect to `/dashboard/` when the UI is enabled. |
| `/dashboard/` | Render the dashboard overview. |
| `/dashboard/applications` | Render the application list. |
| `/dashboard/applications/{id}` | Render one application's detail page. |
| `/api/v1/...` | Serve the versioned HTTP API. |
| `/api/v1/openapi.json` | Serve the OpenAPI document. |
| `/health` | Serve TCP listener liveness outside the OpenAPI contract. |

The paths above are HTTP paths. Host names, TLS, certificates, and reverse
proxy configuration are explicitly outside this plan.

## Product boundaries

- `piqueld` remains the only production server process.
- The UI remains a client-side-rendered Leptos application compiled to static
  HTML, CSS, JavaScript loader, and WebAssembly assets.
- The UI uses same-origin `/api/v1` requests through `piqueld-client`; no CORS
  configuration is introduced.
- `/api/v1` remains the API version prefix, avoiding an unrelated API version
  migration.
- The Unix socket remains API-only and serves no health or dashboard routes.
- The dashboard remains read-only during this work.
- SSR, hydration, Leptos server functions, and `cargo-leptos` are not introduced.
- TLS, SSL, HTTPS termination, certificates, public listener changes,
  authentication, and authorization are not part of this work.

## Dependency choices

Only two additions are needed:

- Add `leptos_router` at the same version as the existing `leptos` dependency.
  It provides typed dashboard routes without changing the rendering model.
- Add the standalone Tailwind CSS CLI as a pinned development and build tool.
  It generates static CSS and does not add a Node.js runtime or browser
  dependency.

Do not upgrade Leptos or replace Trunk as part of this redesign. Record the
new dependencies in the Nix development/build environments and run
`cargo deny check` after the dependency changes.

## Router design

### Separate API and web composition

Refactor the daemon routing into three explicit pieces:

1. `api_router` owns only `/api/...` routes and API-specific JSON fallbacks.
2. `health_router` owns `/health` and is not passed to Utoipa.
3. `web_router` composes the API, health, redirects, and optional dashboard
   assets for the TCP listener.

The Unix listener uses `api_router` directly. UI asset configuration must move
out of `ApiState`; API handlers should not carry static-file state.

All API handlers continue to be registered through Utoipa's Axum integration
so the runtime routes and generated contract cannot drift. OpenAPI generation
must use the API router rather than the complete TCP router.

### OpenAPI scope

Update `piqueld-client`, checked-in documentation, and contract tests together.

The generated document must satisfy all of these invariants:

- Every key under `paths` starts with `/api/`.
- It contains no `/`, `/dashboard`, `/health`, or static asset route.
- It does not contain a hard-coded host name or listener address.
- Its documented operations match the Axum API router.

The existing OpenAPI snapshot check should assert the path-prefix invariant in
addition to comparing generated output.

### Dashboard routing and fallback

Scope static serving to `/dashboard` instead of using a global fallback:

- Exact `/` and `/dashboard` requests return a permanent redirect to the
  canonical `/dashboard/` path when the UI is enabled.
- Existing files below `/dashboard/` are served as static assets.
- Extensionless dashboard paths fall back to the dashboard `index.html`,
  allowing browser refreshes on Leptos routes.
- Missing extensionful assets return 404 and never receive `index.html`.
- Unknown `/api/...` paths return structured JSON API errors.
- Unknown paths outside `/api` and `/dashboard` return an ordinary 404.

Build the Trunk bundle with `/dashboard/` as its public base so generated CSS,
JavaScript, and WebAssembly URLs remain correct on direct and nested page loads.

## Optional UI deployment

Keep UI assets external to the daemon binary. Resolve UI availability once at
startup into a type such as:

```rust
enum UiAssets {
    Disabled,
    Directory(PathBuf),
}
```

Use the existing `server.ui_dir` and packaged `PIQUELD_UI_DIR` mechanism with
these clarified semantics:

1. An explicit `server.ui_dir` enables the UI and has highest precedence.
2. Otherwise, a package-provided `PIQUELD_UI_DIR` enables the UI.
3. If neither exists, the UI is disabled.

Do not fall back to a hard-coded `/usr/share/piqueld/ui` directory. This makes
a plain `piqueld` binary naturally API-only without adding a second boolean.
If a configured asset directory is missing or incomplete, dashboard requests
return the existing clear 503 response. If UI assets are disabled, dashboard
and root routes are not registered.

Expose two deployment outputs:

- A daemon-only package containing `piqueld`, `piquelctl`, configuration, and
  no Trunk, Tailwind, WASM build, or UI assets.
- A combined package that adds the built UI assets and supplies their path to
  `piqueld`. The combined package may remain the default package.

`cargo build --package piqueld` must continue to build the daemon without
building or linking the UI.

## Leptos application structure

Convert the current single dashboard component into a multi-route CSR
application with one shared layout and one WASM bundle:

```text
App
└── Router
    └── DashboardLayout
        ├── OverviewPage             /dashboard/
        ├── ApplicationsPage         /dashboard/applications
        ├── ApplicationDetailPage    /dashboard/applications/:id
        └── NotFoundPage             /dashboard/*
```

Keep transport and polling behavior in shared state/services rather than
duplicating requests in each page. Preserve the current bounded pagination,
polling backoff, stale-data behavior, visibility handling, and request
sanitization.

The first route split should not require new API endpoints:

- Overview shows daemon status and a compact application summary.
- Applications shows the existing application collection.
- Application detail shows the existing detail response for the selected ID.

Navigation uses Leptos router links so page changes do not reload the WASM
application. Direct requests and browser refreshes still work through the
server-side dashboard fallback.

## Tailwind integration

Replace the hand-written component stylesheet incrementally rather than
redesigning behavior at the same time:

1. Add a Tailwind input stylesheet with the Tailwind import, theme tokens, and
   explicit source scanning for `apps/piqueld-ui/src/**/*.rs` and `index.html`.
2. Generate one ignored CSS build output consumed by Trunk.
3. Port the shared layout and reusable visual patterns first, then each page.
4. Keep complete utility class names in Rust source so Tailwind can detect
   them; map dynamic states to fixed class strings.
5. Preserve semantic HTML, keyboard operation, visible focus, responsive
   layouts, dark mode, and status contrast.

Generated Tailwind CSS is a build artifact and must not be committed.

## Development workflow

Add `just dev` as the single entry point for full UI development. It starts
and cleans up these child processes together:

1. `piqueld` using `config/piqueld.example.toml` on `127.0.0.1:7845` in watch mode using `cargo-watch`.
2. Tailwind in watch mode.
3. Trunk in serve/watch mode with `/dashboard/` as its public base and `/api/`
   proxied unchanged to `http://127.0.0.1:7845/api/`.

Trunk reloads the browser after Leptos or generated CSS changes.

Keep lower-level recipes for focused work:

- `just ui-check` checks the WASM client and UI crates.
- `just ui-build` builds production assets rooted at `/dashboard/`.
- `just ui-browser-smoke` validates the production bundle.
- A daemon-only run recipe remains available when UI tooling is unnecessary.

The development documentation must state that Docker is still required to run
the real daemon and identify the Trunk dashboard URL.

## Implementation sequence

### Phase 1: HTTP and OpenAPI boundaries

- Refactor API, health, TCP web, and Unix router composition.
- Move the OpenAPI endpoint and exclude health from the document.
- Scope UI fallback to `/dashboard` and add canonical redirects.
- Update API contract tests, the typed client, CLI tests, and the OpenAPI
  snapshot.

This phase should leave the current dashboard content intact.

### Phase 2: Optional asset resolution and packaging

- Replace the default asset-directory fallback with `UiAssets` resolution.
- Remove UI asset state from `ApiState`.
- Add daemon-only and combined Nix package outputs.
- Update configuration, deployment, and quick-start documentation.
- Verify the daemon-only package does not build or contain UI artifacts.

### Phase 3: Multi-route Leptos UI

- Add `leptos_router` without upgrading Leptos.
- Introduce the shared dashboard layout and route-specific page components.
- Move existing dashboard behavior into overview, application list, and detail
  pages.
- Preserve polling and stale-state behavior through shared state.
- Add focused route and state tests.

### Phase 4: Tailwind and development command

- Add and pin the standalone Tailwind CLI.
- Port the existing visual design to Tailwind while preserving accessibility.
- Update Trunk production and development base paths.
- Add `just dev` process orchestration and cleanup.
- Update the browser smoke test for redirects, nested routes, refreshes, and
  UI-disabled behavior.

## Validation

Add focused coverage for:

- `/` and `/dashboard` redirecting to `/dashboard/` only when UI assets are
  enabled.
- Direct loads of every Leptos route returning the dashboard shell.
- API and missing static asset paths never returning the dashboard shell.
- API-only mode serving `/api/v1/...` while omitting dashboard routes.
- The Unix router exposing only `/api/...` routes.
- Every generated OpenAPI path beginning with `/api/`.
- `piqueld-client` and `piquelctl` using the unchanged `/api/v1` resource paths.
- Trunk output using `/dashboard/` asset URLs.
- Keyboard navigation, responsive layout, polling failure/recovery, and direct
  application-detail navigation in the browser smoke test.

Run focused checks while implementing each phase. Before completion, run the
repository-required full validation:

```console
just
```

## Definition of done

- One `piqueld` process serves both API and UI in the combined deployment.
- The canonical URL behavior matches the route table in this plan.
- OpenAPI describes only `/api/...` paths.
- The dashboard has distinct overview, application list, and application
  detail routes using Leptos Router and Tailwind.
- `just dev` starts the daemon and hot-reloading UI toolchain with one command.
- A daemon-only deployment contains no UI assets or UI build-time dependencies.
- No TLS/SSL behavior, dependencies, or configuration are introduced.
- Documentation and focused tests reflect both combined and API-only modes.
- `just` completes without errors.
