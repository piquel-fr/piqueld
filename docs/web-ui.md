# Application dashboard

The Leptos dashboard manages application configuration through forms. Create an
empty application in a modal, add services in a modal and declare named volumes, and edit images, replicas,
environment variables, commands, arguments, mounts, health checks, and resource
limits. Each settings group has its own **Save Changes** button. Saving updates
the database without changing running containers.

**Preview** shows the planned changes. **Deploy** captures the saved configuration
in a persisted deployment and applies it. Both buttons require all local edits
to be saved or discarded. Every deployment supersedes pending work and
refreshes image resolution, including when configuration has not changed.
Completed deployments retain their terminal state in history.
Retries use the captured deployment, not subsequent configuration edits.

The piqueld logo links to the home page. The sidebar links to Applications and
Host settings. The home page and
Applications show the three most recent deployments across applications; each
row opens that deployment in its application history. Applications also has a
compact, clickable directory.

Applications have one main tab row: Overview (the default), Source, Services,
Volumes, Deployments, and Diagnostics. Services contains the compact service list
and observed runtime services. Reconciliation diagnostics appear in Diagnostics.
Each service row opens a service page
headed by application / service, with tabs for source and scaling, environment,
command and arguments, volume mounts, health checks, and resource limits.
Service form drafts are retained when switching tabs. Selecting None for a
health check hides its remaining fields.
The pencil beside the application name opens its rename form. The Overview
tab contains the application ID, state, generation, and runtime data.
The Deployments tab lists expandable deployment rows with Details, Snapshot,
and Attempts sections. Attempts load when first opened; refresh and older-attempt
controls appear below the list. Operation IDs appear only in deployment Details. Current target, last successful deployment, and observed runtime health
are distinct. History remains until the application is deleted. Deploying an
empty application removes its runtime services and network. Removing volumes or
deleting an application retains Docker volume data; deleting an application
also deletes its configuration and all database history.

Concurrent edits are rejected using configuration revisions; failed saves retain
local form values. Navigation warns about unsaved edits. Host settings are
read-only. There is no raw manifest editor or deployment restoration workflow.

The browser bundle uses HTTP DTOs and shared typed lifecycle records and
fetches same-origin `/api/v1` resources. The daemon serves it only on the
loopback TCP listener. The Unix socket is API-only, and the daemon does not add
CORS, authentication, cookies, browser persistence, telemetry, or a public
binding.

## Development

Use `nix develop` for the Rust/WASM toolchain, Nextest, Cargo Watch, Trunk,
wasm-bindgen, Binaryen, and Tailwind. Alternatively install those tools and
add the WASM target with `rustup target add wasm32-unknown-unknown`.
Docker must be accessible to your user and support a single-node Swarm.
Start the development workflow:

```console
just dev
```

This watches the daemon and dashboard sources, builds the embedded bundle
with Tailwind and Trunk, and runs the daemon using
`examples/piqueld.toml`. Open `http://127.0.0.1:7845/dashboard/` and
refresh the browser after a rebuild. Stopping the command allows the daemon
its graceful shutdown period before terminating any remaining processes.

Compile the browser client and dashboard without building assets with:

```console
cargo check --package piqueld-ui --target wasm32-unknown-unknown
```

The browser modules separate dashboard navigation and lists, editor state,
configuration forms, deployment history, navigation guards, and shared controls.
Styles are split into theme, dashboard layout, and editor rules. To format view
macros as well as Rust, run `leptosfmt` from `apps/piqueld-ui` (it reads the local
`leptosfmt.toml`), followed by `cargo fmt --all`.

## Production assets

The source files `apps/piqueld-ui/index.html`, `tailwind.css`, and the Rust UI
are committed. Tailwind's generated CSS and Trunk-generated
HTML/WASM/JavaScript loader assets are build outputs and are not committed.

The release dashboard ships inside the daemon binary itself. Building with the
feature embeds the bundle; the daemon's build script runs Tailwind and Trunk,
so `trunk`, `wasm-bindgen-cli`, `binaryen`, and `tailwindcss` must be on the
path:

```console
cargo build --release --package piqueld --bin piqueld --features embedded-ui --locked
# Build both the embedded daemon and CLI: just build-embedded
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

Secrets, streaming logs, state transfer, and authentication remain outside the
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


The application's Logs tab reads recent output directly from Docker. Refresh is
manual by default; optional five-second polling runs only while the tab and page
are visible. Output is a bounded snapshot, not an accumulated daemon log archive.

The main overview groups daemon connectivity with deployment readiness for
SQLite, Docker reachability, and Swarm suitability. Green and red labels state
each result in text, and one refresh updates the complete dashboard status.
These diagnostics never disable configuration controls.
