# Application dashboard

The Leptos dashboard manages application configuration through forms. Create an
empty application in a modal, add services in a modal and declare named volumes, and edit images, replicas,
environment variables, commands, arguments, mounts, health checks, startup
dependencies, and resource limits. Each settings group has its own **Save changes** button. Saving updates
the database without changing running containers.

**Preview** opens a dialog with the planned changes and actions. **Deploy** captures the saved configuration
in a persisted deployment and applies it. Both buttons require all local edits
to be saved or discarded. Every deployment supersedes pending work and
refreshes image resolution, including when configuration has not changed.
Completed deployments retain their terminal state in history.
Retries use the captured deployment, not subsequent configuration edits.

The piqueld logo links to the home page. The sidebar groups Overview,
Applications, and Builds; the Observe section (Events, Errors, Analytics,
Notifications); and the System section (Daemon status, Host settings, Accounts).
Its footer shows daemon connectivity and the signed-in account with a sign-out
button. The home page and Applications show the five most recent deployments
across applications; each row opens that deployment in its application history.
Applications also has a clickable directory with each application's health and
last deployment time.

Applications have one main tab row: Overview (the default), Services, Source,
Routes, Volumes, Jobs, Secrets, Deployments, Builds, Logs, and Events. The Jobs
tab adds, edits, reorders, and removes the one-shot jobs that run before each
rollout, with one row per command element; saving replaces only the job list. The Routes tab
edits public hostnames, each pointing at a service port or a redirect, and shows each deployed route's HTTPS readiness and
diagnostics. Saving routes updates only the route field; Deploy activates the
change. Services lists saved services with their observed health merged in.
The Overview tab shows the runtime status, the latest operation, observed
services, reconciliation diagnostics, and the delete action. The Events tab has
an **Errors only** filter. Each service row opens a service page with a
breadcrumb back to the application and tabs for source and scaling,
environment, command and arguments, volume mounts, health checks, startup
dependencies, resource limits, and logs. Startup dependencies list the
application's other services as checkboxes. Service form drafts are retained when switching tabs.
Selecting None for a health check hides its remaining fields.
The pencil beside the application name opens its rename form.
The Deployments tab lists expandable deployment rows with Details, Snapshot,
and Attempts sections. Attempts load when first opened; refresh and older-attempt
controls appear below the list. Operation IDs appear only in deployment Details. Current target, last successful deployment, and observed runtime health
are distinct. History remains until the application is deleted. Deploying an
empty application removes its runtime services and network. Removing volumes or
deleting an application retains Docker volume data; deleting an application
also deletes its configuration and all database history.

Forms save typed fields or settings sections through individual endpoints, without
resubmitting the application manifest. Related fields within a form save atomically.
All form saves update configuration only; Deploy remains explicit.
Concurrent edits are rejected using configuration revisions; failed saves retain
local form values. Navigation warns about unsaved edits. Host settings are
read-only. There is no raw manifest editor or deployment restoration workflow.

The browser bundle uses HTTP DTOs and shared typed lifecycle records and
fetches same-origin `/api/v1` resources. The daemon serves it only on the
configured localhost or Tailscale TCP listeners. The Unix socket is API-only,
and the daemon does not add CORS, telemetry, or a public binding. Passkey login
establishes an HTTP-only session cookie; the Accounts page manages users,
invitations, passkeys, and tokens. Log display preferences persist in browser
local storage. See [authentication](authentication.md).

The **Secrets** tab lists names and versions, creates or replaces write-only text
values, and deletes unreferenced secrets. Submitted values are cleared and cannot
be read back; use the CLI for binary secret files. If a write or deletion fails,
further secret changes are disabled until metadata refresh succeeds. Metadata
refresh and secret writes cannot overlap. Pending deletion is shown explicitly;
retry Delete to complete cleanup. After daemon-wide key recovery, affected secrets
show “Value discarded by secret key recovery.” Replace each value and explicitly
deploy to adopt it; old deployments never silently pick up replacements. Recovery
itself is available through `piquelctl secrets recover-key`.
Each service has a separate
secret-file reference editor. Save references, then deploy explicitly to mount
those versions. Repository-backed applications keep references in their Git
manifest. Secret values and unsaved references participate in navigation warnings.

## Development

Building the dashboard needs only Cargo and the `wasm32-unknown-unknown` target,
which `rust-toolchain.toml` installs. `nix develop` provides the same toolchain
plus Nextest and Cargo Watch. Docker must be accessible to your user and support
a single-node Swarm. Start the development workflow:

```console
just dev
```

This watches the daemon and dashboard sources, rebuilds the embedded bundle, and
runs the daemon using `examples/piqueld.toml`, or a gitignored
`piqueld.local.toml` at the repository root when present. Open
`http://localhost:7845/dashboard/` and refresh the browser after a rebuild. To
reach it through a private HTTPS proxy such as Tailscale serve, copy the example
to `piqueld.local.toml` and set `server.allowed_hosts` and `auth.public_url` to
the proxy hostname.
Stopping the command allows the daemon its graceful shutdown period before
terminating any remaining processes.

Compile the browser client and dashboard without building the bundle with:

```console
cargo check --package piqueld-ui --target wasm32-unknown-unknown
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

No Trunk, wasm-bindgen CLI, binaryen, Tailwind, or Node is involved. The shell
has no inline scripts, so the Content-Security-Policy is a constant. See
[ADR 0002](architecture/0002-cargo-built-dashboard.md) for the reasoning and
the planned move to Cargo artifact dependencies, which would remove the nested
Cargo invocation.

```console
cargo build --release --package piqueld --bin piqueld --features embedded-ui --locked
# Build both the embedded daemon and CLI: just build-embedded
```

There is no runtime UI configuration: the dashboard exists exactly when the
binary was built with the feature, and binaries built without it are API-only.
The combined Nix package (`.#`) includes `piquelctl` and a daemon with the
release dashboard embedded, built by the same build script. The `.#daemon`
output contains only the daemon without the feature, and the `.#cli` output
contains only `piquelctl`.

The TCP router serves bundle files below `/dashboard/` and uses `index.html`
only for extensionless dashboard paths. API, health, and unknown paths never
receive the SPA shell. Content-hashed asset filenames are served with
immutable caching; the shell is always revalidated.

The dashboard performs one initial refresh, then bounded pagination and
application-status and recent-deployment reads. Background polls run every 15 seconds after success and back off
to at most 120 seconds after failures. Polls pause while the document is
hidden, never overlap, and a manual refresh remains available. A failed refresh
keeps the last successful view visible and marks it stale.

Healthy replica counts come from Docker's container healthcheck verdicts for
services that declare one; services without a healthcheck count running tasks
as healthy.

Accessibility coverage includes semantic headings and lists, a skip link,
keyboard-operable buttons, visible focus, live status/error regions, responsive
layouts for narrow widths, and a permanent dark palette with contrast-oriented status colors.

The supported browser baseline is a current evergreen Chromium, Firefox,
Safari, or Edge release with WebAssembly, ES modules, Fetch, and standard CSS
media-query support. Internet Explorer, JavaScript-disabled browsing, and
older browsers without those primitives are outside the support target.

Secrets, streaming logs, and state transfer remain outside the
current dashboard scope. Event history is available through the API and CLI.
Deployment history polls every two seconds while its tab and the page are visible.

Service source settings explicitly select a container image or Git with a
Dockerfile build. Git settings include repository, branch, optional commit,
Dockerfile path, and build context relative to the repository root. Saving
scaling or other service settings preserves the selected source.

Repository manifest settings select the repository, branch, optional commit, and
exact manifest file. Deploy fetches that file before preparing service sources.
While backing is enabled, edit runtime configuration in Git; connection settings
remain editable here. Configuration saved during preparation is preserved.

**Download saved manifest** exports the current server-saved configuration as TOML. Unsaved form edits and runtime/deployment state are excluded. Repository connection settings are preserved; the download does not fetch Git or require Docker. Original comments and formatting are not retained.


Application and service Logs tabs read recent output directly from Docker.
Service and stdout/stderr dropdowns filter in the daemon. Snapshots refresh every
30 seconds while the view and browser tab are visible; Refresh is also available
on the right. Merged terminal output appears only with Both streams selected.

Runtime and build output share a monospace terminal with grey messages, separate
metadata, local HH:mm:ss timestamps (full precision on hover), red backgrounds
for ERROR/FATAL messages, and subtle warning backgrounds for other stderr output.
Task IDs and stream labels are omitted. Timestamp, service visibility, and wrap
preferences persist in this browser. Application logs default to showing timestamps
and services. Build timestamps default off and have a separate saved preference;
build output never shows service labels or a service toggle. Wrapping defaults off,
and service labels default hidden in a service’s own Logs tab. Terminal escape formatting is stripped. New output follows
the bottom only when the reader has not scrolled up.

The main overview groups daemon connectivity with deployment readiness for
SQLite, Docker reachability, and Swarm suitability. Green and red labels state
each result in text, and one refresh updates the complete dashboard status.
These diagnostics never disable configuration controls.

Build history is available from the main navigation and each application's Builds
tab. Compact attempt rows expand to show source, revision, Docker build settings,
operation timing, commit, image, and retained output size. Opening a row loads its
output into the same accessible log viewer used by the Logs tab. Running attempts
refresh output every 30 seconds while expanded and visible, stopping after the
final completed output loads. Opening shows the newest output; Load older output
prepends preceding pages. Refresh replaces the output with the latest page.
New captures retain timestamps and stdout/stderr identity, allowing daemon-side
stream filtering. Expiration and truncation remain explicit.

See [observability](observability.md) for diagnostic history, daemon statistics, analytics, metrics
and webhook delivery configuration.

## Browser tests

The [Playwright suite](../tests/playwright/README.md) exercises the embedded dashboard against
an isolated API fixture. Run `just setup-playwright` once, then `just test-playwright`. Chromium
is provided by a pinned Docker image; the tests need no system browser, Python,
or ChromeDriver. The `browser-playwright` CI job retains failure traces and screenshots.
