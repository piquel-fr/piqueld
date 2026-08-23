# Plan 06C — Add a basic read-only Leptos dashboard

## Goal

Provide the smallest useful browser view of the Plan 06 product. The dashboard is
read-only and answers three questions: is the daemon reachable, what applications
exist, and are they converged and healthy?

Use Leptos in client-side-rendered WASM mode with ordinary HTML and CSS. Keep the
dependency set small and do not introduce a JavaScript/TypeScript application or an
external web service.

## Product surface

The dashboard provides:

- daemon status and version/capability summary;
- a paginated application list;
- application detail with desired manifest/generation;
- observed reconciliation/convergence state and sanitized diagnostics;
- manual refresh plus modest bounded polling; and
- a visible note directing users to `piquelctl` for mutations.

It does not create, edit, plan, apply, or delete anything.

## Deliverables

- A Leptos CSR application integrated into the Rust workspace.
- A transport-neutral client/DTO surface that compiles for WASM while preserving the
  existing native Unix/TCP client.
- Production web assets served by the daemon on loopback TCP under same-origin API
  access, with routing that cannot shadow `/api/v1` or OpenAPI endpoints.
- A responsive, accessible dashboard with explicit loading, empty, degraded, failed,
  and unreachable states.
- Focused Rust/WASM tests and a small browser smoke suite.

## Work

### 1. Keep the client boundary reusable

1. Reuse the public API DTOs and request/response types from `piqueld-client` rather
   than recreating the complete contract in the UI.
2. Gate native Hyper/Tokio/Unix-socket transport behind non-WASM targets. Add only
   the minimal browser fetch and timer support needed for same-origin HTTP.
3. Keep transport selection out of components. Components consume a small typed
   client interface and render server-provided state.
4. Do not create a new shared-contract crate unless target constraints make reuse in
   the existing client impossible and the added crate clearly removes more
   complexity than it adds.

### 2. Build one small dashboard

1. Prefer a single responsive dashboard/detail flow. Do not add a client router
   unless navigation requirements demonstrate that it is simpler than component
   state.
2. Show a compact daemon health/status header, application list, and selected
   application detail.
3. Present desired and observed state distinctly. Include application ID/name,
   generation, image/digest where public, replica/convergence information, latest
   operation state, and actionable sanitized diagnostics.
4. Poll at a modest interval, pause or back off when the page is hidden/unreachable,
   and prevent overlapping requests. Provide an immediate manual refresh.
5. Bound list pagination and diagnostics rendered in the DOM. Do not implement a
   generalized caching/state-management layer.

### 3. Serve assets simply and safely

1. Serve production assets from the existing Axum daemon using the simplest
   established static-file/embed facility that fits packaging. Prefer a small
   maintained dependency over a bespoke hardened static server.
2. Expose the UI only on loopback TCP in this plan. The Unix socket remains available
   to the CLI/API but is not a browser transport.
3. Use same-origin `/api/v1` requests. Do not add CORS, authentication, public
   binding, cookies, browser persistence, or telemetry.
4. Ensure static fallback never captures API, OpenAPI, health, or other daemon
   endpoints. Unknown API routes must remain API errors rather than returning HTML.
5. Keep a straightforward development command for building/serving the WASM assets,
   and document which generated assets are committed, embedded, or built by Nix.

### 4. Accessibility and failure behavior

1. Use semantic HTML, associated labels, keyboard-operable controls, visible focus,
   sufficient contrast, and useful document headings.
2. Announce refresh failures without erasing the last successful view. Clearly mark
   stale data and provide retry.
3. Distinguish daemon unreachable, request failed, no applications, application
   degraded, and application converged states.
4. Use normal CSS with a small design token set. Do not add a component framework or
   CSS build system for this dashboard.

### 5. Dependency discipline

1. Add Leptos CSR and only minimal WASM fetch/timer/browser bindings. Audit feature
   flags so server-only runtimes do not leak into the WASM target.
2. Reuse existing Axum/tower-http facilities where possible for assets.
3. Run dependency and license policy checks. Explain every new direct dependency in
   the handoff.
4. Treat old Plan 12 code as a source of useful implementation ideas, not as code to
   transplant wholesale: its forms, secrets, streams, and state-management needs are
   intentionally absent here.

## Explicitly out of scope

- Create/edit forms, raw TOML editing, plan/apply/delete, or any other mutation.
- Secrets, builds, routes, runtime logs, state import/export, or authentication.
- SSE/WebSockets, service workers, offline mode, browser storage, telemetry, or
  cross-origin deployments.
- A JavaScript/TypeScript application, UI component suite, elaborate router, or
  general global state framework.
- Public or remote network exposure.

## Verification

- Native client tests remain green and the transport-neutral client/DTO layer builds
  for `wasm32-unknown-unknown`.
- Rust component/state tests cover loading, empty, selected application, degraded,
  stale-after-error, pagination, and polling/backoff behavior.
- Daemon routing tests prove assets and fallback work while `/api/v1`, OpenAPI, and
  unknown API paths retain their correct responses.
- Browser smoke tests at desktop and narrow widths cover load, selection, refresh,
  unreachable/recovery behavior, keyboard navigation, and focus visibility.
- Inspect browser storage, cookies, console output, and network requests to confirm
  no persistence, telemetry, cross-origin access, or mutation occurs.
- Run the canonical `just` validation and dependency policy checks.

## Done when

A user can open the daemon's loopback URL and understand daemon availability and the
desired/observed state of every application without performing a mutation. The UI
is an intentionally thin view over the public API, adds no second backend or
duplicated domain model, and leaves the advanced Plan 12 feature set for later.

## Handoff

Document development and production asset commands, new dependencies/features,
daemon route precedence, polling behavior, supported browsers, test coverage, and
the exact advanced Plan 12 capabilities still deferred.
