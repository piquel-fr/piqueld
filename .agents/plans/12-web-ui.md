# Plan 12 — Minimal structured Leptos web UI

## Goal

Provide a small Rust/WASM administrative UI using only the public API. It must make
plans, progress, conflicts, and destructive effects clear without embedding a raw
TOML editor or storing secrets.

## Deliverables

- Leptos CSR application, normal HTML/CSS assets, accessible navigation/loading/
  error states, and daemon-hosted static assets with SPA fallback that never shadows
  `/api` or OpenAPI routes.
- Application list/detail/status; structured create/edit forms for all prototype
  schema fields; plan preview; apply/delete with volume-retention notice.
- Live deployment/build progress and runtime logs using SSE with reconnect behavior.
- Secret metadata list/create/replace/delete controls with write-only value fields
  and no reveal operation.
- Application/state export download and confirmed state import with dependency
  report.
- Visible generation-conflict recovery: preserve local form data and let the user
  reload/review; never silently overwrite.

## Work

1. Reuse core DTO/schema and `piqueld-client` where practical for WASM, extracting a
   transport-neutral client surface if Unix-specific dependencies prevent WASM.
2. Implement typed structured fields for sources, environment, mounts, secrets,
   health checks, volumes, and host routes. Client validation is convenience only;
   render server field-path errors next to controls.
3. Keep secret values only in component memory long enough to submit over the
   authenticated connection. Disable browser persistence/autofill as appropriate,
   clear on success/unmount, and avoid telemetry/console logging.
4. Display plan action type, target, reason, build/resolve phase, and destructive or
   retain semantics before apply.
5. Bound log rendering and virtualize/truncate as needed; provide pause/follow and
   accessible status announcements.
6. Embed or package versioned production assets with the daemon; preserve a dev
   workflow without adding JavaScript/TypeScript application source.

## Verification

- Rust component/state tests for conditional fields, server validation mapping,
  plan/apply flow, conflicts, SSE reconnect, and secret clearing.
- Browser smoke tests cover create/edit/plan/apply/watch/log/delete, secret rotation,
  and state export/import at desktop and narrow viewport widths.
- Accessibility checks for labels, keyboard flow, focus after errors/dialogs, and
  contrast.
- Browser storage, DOM after completion, console, and network URL tests contain no
  secret canary.

## Done when

The full prototype can be administered through structured forms with live feedback,
safe secret handling, and explicit conflicts/destructive semantics, with no TOML
editor and no non-Rust application source.

