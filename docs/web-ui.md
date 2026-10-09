# Application dashboard

The Leptos dashboard manages application configuration through forms. Create an
empty application in a modal, add services in a modal and declare named volumes, and edit images, Git builds
(including Docker build arguments and targets), replicas, environment variables, commands, arguments, mounts, health checks, startup
dependencies, rollout order and monitor window, and resource limits. Each settings group has its own **Save changes** button. Saving updates
the database without changing running containers.

**Preview** opens a dialog with the planned changes and actions, each service's
effective rollout order (derived or explicit) and monitor window, and plan warnings. **Deploy** captures the saved configuration
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
across applications; each row opens that deployment in its environment's history.
Applications also has a clickable directory with each application's health (its
least healthy environment) and last deployment time.

An application's page holds its shared configuration and history across all
of its environments. Saving changes future deployments of every environment;
running deployments keep their captured configuration. The **Environments** tab
lists the environments with their health and latest deployment, deploys any one
of them, and creates new ones, including for an application with no
environments. Clicking anywhere on an environment's row opens its own page,
`/dashboard/applications/<app>/environments/<environment>`, with a breadcrumb
back to the application. Its Overview shows the runtime status, the environment's
source, and renames or deletes the environment; its other tabs are Deployments,
Secrets, Logs, and Events. The environment's Secrets tab lists each secret the
manifest it deploys mounts and where the value comes from: generated for the
environment, the application's secret store (with its version), missing from the
store, which would fail the next deploy with `secret_missing`, not allowed by the
secret's access list, which would fail it with `secret_access_denied`, or discarded by key recovery, which fails it with
`secret_unavailable` until the value is replaced. Below it, the environment's generated values
can be regenerated for the next deployment, or deleted so a later deployment
generates new ones. Deletion confirms the environment
name, retains its Docker volumes, and leaves the application and other
environments intact; **Retry deletion** resumes cleanup when needed. An
environment page whose environment no longer exists says so rather than showing
another environment.

The **Previews** tab lists the application's
[previews](application-manifest.md#previews), never shown among its
environments: each preview's branch, slot, slug, status (or **Deleting**) with
its status message, its URLs as `https://` links, and its branch state (exists,
moved or gone, with shortened commits, or unknown with the repository error).
The list loads when the tab opens and on **Refresh**, not on a timer, since
each load runs `git ls-remote` on the manifest repository. **Delete** confirms,
then deletes the preview with every volume it created and their data; it needs
no saved revision, so unsaved edits elsewhere on the page don't block it.
Without previews, the tab explains how to create one with `piquelctl preview
create`, and first to connect a manifest repository when there is none.

**Preview** and **Deploy to <environment>** on an environment page target that
environment. On the application page they target its only environment; with
several, **Deploy…** opens the Environments tab to choose one. Deployment actions
are disabled for deleting environments. Preview plans the manifest the
environment deploys: for an environment that follows a branch, the one last
fetched from it, so Preview is disabled until its first deployment. A service's Logs tab reads the only
environment's logs, or links to each environment's logs.

Values that differ between environments come from manifest variables. The
Variables tab edits them: one row per variable, with its default, one column
per environment, including environments the manifest configures before they
exist, and a Previews column for `[spec.previews.variables]`. An environment's
or the previews' value overrides the default, and an empty cell has no
value, so a variable may have only per-environment values. Fields that accept variables, such as replicas
or health check settings, take `${{ vars.<name> }}` in place of a literal. An
environment's Overview lists the value of each variable there, marking variables
without one. Environments that render the same hostname conflict; the error
names the environment already reserving it.

Applications have one main tab row: Overview (the default), Environments,
Previews, Services, Source, Variables, Routes, Volumes, Jobs, Secrets, Releases, Builds,
and Events. The Releases tab lists the application's immutable releases, newest
first: each successful preparation of an environment's deployment (never a preview's) records one,
environments that prepared the same content share it, and deleting an environment keeps
them. Expanding a release shows the commit its manifest was read from, its
content hash, and for each service its image (a registry digest or local image
ID), where it came from, and the build inputs it was prepared from. The Jobs
tab adds, edits, reorders, and removes the one-shot jobs that run before each
rollout, with one row per command element; saving replaces only the job list.
Jobs inherit the referenced service's startup dependencies, which start or
update and become healthy first. Other services wait for all jobs to succeed;
dependency changes remain if a job fails. The Routes tab
edits hostnames, each pointing at a service port or a redirect with a public or
private visibility, and shows each deployed route's effective visibility, the DNS
records its hostname needs, HTTPS readiness and diagnostics in every environment.
Each environment's Overview sets its visibility ceiling, and system status shows
the Cloudflare Tunnel's ID and connection state in tunnel mode, and the private listener
with the apps tailnet node's name and addresses. Saving routes updates only the route field;
Deploy activates the change. Services lists saved services, with their observed
health merged in when the application has one environment. The Secrets tab
holds the application's secret store and each service's secret file references. The Overview tab shows the application's identity, configuration
revision, and environments, and the delete action. Builds and Events cover every
environment, each linking to its environment, and include application-wide
events such as edits and renames; a deleted environment's events remain until
the application is deleted. Events has an **Errors only** filter. An environment's Overview shows the runtime status, the latest operation,
observed services, and reconciliation diagnostics. Each service row opens a service page with a
breadcrumb back to the application and tabs for source and scaling,
environment, command and arguments, volume mounts, health checks, startup
dependencies, rollout, resource limits, and logs. Startup dependencies list the
application's other services as checkboxes. Rollout selects an order (derived
from mounts, stop first, or start first) and an optional monitor window, blank
for the 30-second default. Service form drafts are retained when switching tabs.
Selecting None for a health check hides its remaining fields.
The pencil beside the application name opens its rename form.
An environment's Deployments tab lists expandable deployment rows with Details, Snapshot,
and Attempts sections. Attempts load when first opened; refresh and older-attempt
controls appear below the list. Operation IDs appear only in deployment Details,
next to the deployment's release, which links to it in the application's
Releases tab; the environment's Overview links to the release its current target
runs. Current target, last successful deployment, and observed runtime health
are distinct. History remains until the environment is deleted. Deploying an
empty application removes its runtime services and network. Removing volumes or
deleting an application retains Docker volume data; deleting an application
deletes every environment, its configuration, and all database history. The
confirmation names the environments being deleted.

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
establishes an HTTP-only session cookie; the Accounts page manages accounts and
their grants, invitations, passkey enrollment links, and tokens. Pages and actions
the signed-in account cannot use are hidden; see [authorization](authorization.md).
A link from `piquelctl sign-in-link` opens a page that signs the browser in as
the [host operator](authentication.md#the-host-operator) for 12 hours, without
an account. The dashboard then shows "Host operator" as the signed-in user and
hides actions that need an account: adding passkeys, creating tokens,
invitation and enrollment links, and CLI login approval. Accounts with
`accounts:manage` see live host operator sessions on the Accounts page; since
those sessions act with `admin` on everything, only accounts holding that can
revoke them.
The Audit page shows the [audit trail](observability.md#audit-trail), naming the
application or environment each request addressed, and each
session or token links to its own activity. New tokens can be bound to a
tailnet user or tag; see [tailnet-bound tokens](authorization.md#tailnet-bound-tokens). With `audit:read`, **Verify
integrity** checks the trail's [hash chain](observability.md#tamper-evidence). Log display preferences persist in browser
local storage. See [authentication](authentication.md).

The application's **Secrets** tab lists the secret store's names, versions and
who may mount each one, by environment name. It creates or replaces write-only
text values, edits access (every environment, including ones created later, or
the checked environments, plus whether previews may mount it, which "every
environment" never includes), and
deletes secrets no environment uses. Submitted values are cleared and cannot
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
restarts this worktree's isolated development instance, served on its own
localhost port. Refresh the browser after a rebuild. See
[`development.md`](development.md).

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
environment-status and recent-deployment reads. Background polls run every 15 seconds after success and back off
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

Repository manifest settings select the repository and exact manifest file,
shared by every environment, and, when connecting, the branch and optional commit
every environment starts following. Deploy fetches that file from the
environment's branch before preparing service sources. While backing is enabled,
edit runtime configuration in Git; the repository and path remain editable here.
The application page shows the manifest last fetched by any environment, and
reloads it after a fetch unless repository settings are being edited.

Each environment of a repository-backed application follows its own branch.
**New environment** asks for the branch (by default the one `spec.manifest`
names) and an optional commit, the Environments list shows each environment's
branch, and the **Source** card on an environment's Overview changes it; nothing
is redeployed until its next deployment. The environment's Variables card, Preview, and
Logs service filter read the manifest last fetched from its branch; the
Variables card says when it has fetched nothing yet. A deployment's Snapshot shows the manifest path and revision it was fetched
from, and a warning when the fetched file's own `spec.manifest` names another
repository or path (`manifest_connection_ignored`); that section is ignored.

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
