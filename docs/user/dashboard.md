# Application dashboard

The Leptos dashboard manages application configuration through forms. Create an
empty application in a modal, add services in a modal and declare named volumes, and edit images, Git builds
(including Docker build arguments and targets), replicas, environment variables, commands, arguments, mounts, read-only files, health checks, startup
dependencies, rollout order and monitor window, and resource limits. Each settings group has its own **Save changes** button. Saving updates
the database without changing running containers.

**Preview** opens a dialog with the planned changes, the variable values it
renders, the planned actions (saying so when network, volume and service changes
wait for image resolution and builds, which the deployment plans), each service's
effective rollout order (derived or explicit) and monitor window, and plan warnings. **Deploy** captures the saved configuration
in a persisted deployment and applies it. Both buttons require all local edits
to be saved or discarded. Every deployment supersedes pending work and
refreshes image resolution, including when configuration has not changed.
Completed deployments retain their terminal state in history.
Retries use the captured deployment, not subsequent configuration edits.

The piqueld logo links to the home page. The sidebar groups Overview,
Applications, and Builds; the Observe section (Events, Errors, Analytics,
Notifications); and the System section (Daemon status, Host settings, Accounts,
Audit). Notifications, Daemon status and Host settings need `system:read`;
retrying a failed notification delivery needs `system:operate`.
Notifications lists [destinations](guides/observability.md#notifications) and
deliveries. With `notifications:manage` it adds, edits, tests and deletes
destinations: their kind, URL (write-only), categories with an optional message
each, applications and previews.
Its footer shows daemon connectivity and the signed-in account with a sign-out
button. The home page and Applications show the five most recent deployments
across applications; each row opens that deployment in its environment's history.
Daemon status also runs the [installation checks](reference/cli.md#installation-checks)
when it opens and on demand, showing each with its next step.
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
Secrets, Logs, Terminal, and Events. The environment's Secrets tab lists each secret the
manifest it deploys mounts and where the value comes from: generated for the
environment, the application's secret store (with its version), missing from the
store, which would fail the next deploy with `secret_missing`, not allowed by the
secret's access list, which would fail it with `secret_access_denied`, or discarded by key recovery, which fails it with
`secret_unavailable` until the value is replaced. A promoted environment not
promoted into yet lists what the release its source runs mounts. Below it, the
application's secret store works as in the application's Secrets tab, limited to the
secrets this environment may mount or mounts; a new secret saved there may be mounted by
this environment only, unless its access is changed. Then the environment's generated values
can be regenerated for the next deployment, or deleted so a later deployment
generates new ones. Deletion confirms the environment
name, retains its Docker volumes, and leaves the application and other
environments intact; **Retry deletion** resumes cleanup when needed. An
environment page whose environment no longer exists says so rather than showing
another environment.

The **Terminal** tab of an environment or preview runs a one-off command in a
running task of one of its services, like
[`piquelctl app exec`](reference/cli.md#running-commands). It needs
[`apps:exec`](accounts/permissions.md#application-permissions) on the
application. Choose a **Service**, enter a **Command** (`/bin/sh` by default)
and select **Run**. The command is split into arguments with shell quoting, but
no shell runs it unless you name one.

**Interactive terminal**, on by default, gives the command a terminal the size
of the tab: what you type, paste and resize reaches the command. Without it, the
command gets no input and its output is shown as is. The terminal shows 16
colors from the dashboard's palette, with other colors shown as the nearest of
them, and keeps 5000 lines of history for the mouse wheel. To copy, select text
and copy it as you would anywhere else. Ctrl+Shift+V or Command+V pastes. On
macOS, Option types characters, as in Terminal.app, rather than acting as
Meta; with arrows and other named keys it still acts as Meta.

When a command can't start, the tab shows the daemon's reason, such as
"Service has no running task". Once the command ends, it shows its exit code.
Switching tabs keeps the session. **Disconnect**, running another command, or leaving the
page stops the output, but the command may keep running in the task, as when
`piquelctl` ends early. History records the command's start and exit, never the
command.

The **Previews** tab lists the application's
[previews](reference/manifest.md#previews), never shown among its
environments: each preview's branch, slug, slot, branch state (exists, moved
or gone, with shortened commits; local while it runs a local snapshot, with
the branch head or "never pushed"; or unknown with the repository error), and
status (or **Deleting**) with its status message on hover. A **Bounded**
badge marks a preview whose deployment runs a service with the
[`[previews]`](reference/configuration.md#previews) default limits or fewer replicas than
its manifest asks for, listed on hover. It also shows whether pushes
redeploy it, with the commit sync last deployed or found.
The list loads when the tab opens and on **Refresh**, not on a timer, since
each load runs `git ls-remote` on the manifest repository. **New preview** asks
for a branch and an optional slot, which tells apart several previews of one
branch (one per agent, say), then opens the preview's deployments; for a
preview of that branch and slot that already exists, it opens it without
redeploying. It is disabled until a manifest repository is connected in Source.
**Delete** confirms, then deletes the preview with every volume it created and
their data. **Prune gone** confirms, then deletes every preview whose branch
was gone when the list loaded; the daemon checks each branch again, keeps any
it can no longer confirm gone, and deletes nothing while the repository cannot
be read. None of them needs a saved revision, so unsaved edits elsewhere on the
page don't block them.

Each row opens the preview's page, which has an environment page's tabs.
Its Overview shows the preview's branch, slot, slug, and ID, with its branch
state, what it deploys and its bounds read on load and on **Refresh**, then its
runtime and URLs, its variables rendered for previews, and a card to delete it.
**Deploys** is the head of its branch, or, after `piquelctl preview deploy
--local`, the local snapshot's commit and the base it was uploaded against,
with what **Redeploy branch head** does then. **Redeploy branch head** always
deploys the head of its branch from the repository, replacing a local snapshot;
for a branch never pushed it fails until the branch is pushed. Snapshots are
only uploaded from a worktree with the CLI. Deployment history shows a local
snapshot's origin as "Local snapshot \<commit> on \<base>".

**Preview** and **Deploy to \<environment>** on an environment page target that
environment. On the application page they target its only environment; with
several, **Deploy** opens a list of every environment, each with its own **Deploy** or
**Promote from \<source>** button. On an environment's own page the button is just **Deploy**. Deployment actions
are disabled for deleting environments. Preview plans the manifest the
environment deploys: the saved one for an environment following the branch
`spec.manifest` names, once the application has read a manifest from it (it is
deployed until a push changes the manifest there); for one that follows another
branch, the one last fetched from it, so Preview is disabled until its first
deployment. An environment following a branch also offers **Deploy revision**.
It deploys the manifest on the head of another branch of the application's
repository, or on a commit given by its full hash, once, like
`piquelctl env deploy --branch` or `--commit`. Nothing is saved: the
environment's next deployment, or a push to its branch when it deploys on push,
returns to its own branch. A service's Logs tab reads the only
environment's logs, or links to each environment's logs.

A deployed environment's page also offers **Stop**, which confirms, then
[stops](reference/api.md#stopped-environments) it: the gateway withdraws its routes and
its services scale to zero, keeping their configuration and volumes. Its
health then shows **Stopped** on its page, in the Environments list, and on the
application, and its Source card says pushes don't deploy it. **Start** runs it
again, as does deploying or promoting into it. Previews have no Stop; delete
them instead.

A promoted environment never builds, so its page and its Environments row offer
**Promote from \<source>** instead of Preview and Deploy, and its source's page
offers **Promote**, before Deploy, listing the environments promoted from it, each
with its own **Promote from \<source>** button. Either first plans the promotion of
its source environment's current release and shows it: where the release comes
from, the release itself (commit, each service's image, availability), the
changes against the environment's current deployment, new (empty) volumes, the
runtime actions, and every secret that is missing or that the environment may
not use. **Promote release** is disabled while a secret is listed or the
release's images were removed, and promotes
exactly the source deployment that was planned: if the source deployed again
meanwhile, the daemon refuses it (`promotion_source_changed`) and the dialog
shows why.

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
Previews, Services, Source, Variables, Routes, Volumes, Jobs, Links, Secrets, Releases, Builds,
and Events. The Releases tab lists the application's immutable releases, newest
first: each successful preparation of an environment's deployment (never a preview's) records one,
environments that prepared the same content share it, and deleting an environment keeps
them. Expanding a release shows the commit its manifest was read from, its
content hash, and for each service its image (a registry digest or local image
ID), where it came from, and the build inputs it was prepared from. A badge
says whether its images are still present, will be pulled again by digest
when deployed, or were removed, so it can't be deployed again. **Deployed to**
lists every environment it was promoted into, or deployed again by release ID,
with that deployment. **Deploy to \<environment>**, offered for each promoted
environment, plans deploying the release there with that environment's current
secrets, like `piquelctl env promote --release`, and deploys it once confirmed. The Jobs
tab adds, edits, reorders, and removes the one-shot jobs that run before each
rollout, with one row per command element; saving replaces only the job list.
Jobs inherit the referenced service's startup dependencies, which start or
update and become healthy first. Other services wait for all jobs to succeed;
if a job fails, its dependencies return to the running deployment. A job's
volume access is read-only by default; exclusive access stops the services
using its writable volumes until the rollout. Each job also lists the
environment variables and secret files only its container gets, added to its
service's. The Links tab edits the
application's [links](reference/manifest.md#links): the services it exports
and the applications that may import them (and whether their previews may),
and the services of other applications it imports, with an optional
environment; exports and imports each save their own list. The Routes tab
edits hostnames, each with an optional [name](reference/manifest.md#routes)
and pointing at a service port or a redirect with a public or
private visibility, and shows each deployed route's name, effective visibility, the DNS
records its hostname needs, whether piqueld
[manages them](guides/ingress.md#managed-dns-records) (`manual`, `managed`, `pending` or
`dns_conflict`), HTTPS readiness and diagnostics in every environment and preview.
Each environment's Overview sets its visibility ceiling. System status shows a
Previews card with the previews against their limits, installation-wide and for
each readable application, flagged at or over a limit, and the CPU and memory
limits of their deployed replicas. It also shows which DNS providers manage
route records and the state of their certificates, the Cloudflare Tunnel's ID and connection state in tunnel mode, and the private listener
with the apps tailnet node's name and addresses. With `system:operate`,
**Check DNS providers** checks their credentials now. Saving routes updates only the route field;
Deploy activates the change. Services lists saved services, with their observed
health merged in when the application has one environment. The Secrets tab
holds the application's secret store, the secrets piqueld generates (each a
random value or an RSA key; changing or removing one keeps values already
generated), and each service's secret file references. The Overview tab shows the application's identity, configuration
revision, and environments, and the delete action. Builds and Events cover every
environment, each linking to its environment, and include application-wide
events such as edits and renames; a deleted environment's events remain until
the application is deleted. Events has an **Errors only** filter. An environment's Overview shows the runtime status, the latest operation,
a URLs card, a Links card when it imports services or others import its
services, a Telemetry card when it is granted the daemon's
[telemetry](guides/observability.md#receiving-telemetry-in-an-application) or
deploys a service receiving it (whether it is granted, which service receives
it, and whether the daemon is connected), observed services, and
reconciliation diagnostics. The Links card
lists each import as connected or waiting, with why, and the environments of
readable applications importing its services; a preview's Overview shows it
too. The URLs card
lists the URL of each route the current deployment renders, with its name,
visibility, destination and state: **ready**, or **pending** with what it waits
for (ingress, HTTPS, DNS records, or a healthy service), as the daemon reports
it in [URL readiness](reference/api.md#url-readiness). Each service row opens a service page with a
breadcrumb back to the application and tabs for source and scaling,
environment, command and arguments, volume mounts, files, health checks,
startup dependencies, rollout, telemetry, resource limits, and logs. Files lists each
read-only file's container path, mode, and inline content; files a
repository-backed manifest reads show their repository path. Startup dependencies list the
application's other services as checkboxes. Rollout selects an order (derived
from mounts, stop first, or start first) and an optional monitor window, blank
for the 30-second default. Telemetry marks the service as the application's
telemetry receiver. Service form drafts are retained when switching tabs.
Selecting None for a health check hides its remaining fields.
The pencil beside the application name opens its rename form.
The pencil beside a service's name, on its page, renames the service. Routes,
jobs, exports and startup dependencies that name the service follow the new
name, and the dashboard opens the renamed service's page. Save or discard edits
to the service's other settings first.
An environment's Deployments tab lists expandable deployment rows with Details, Snapshot,
and Attempts sections. Attempts load when first opened; refresh and older-attempt
controls appear below the list. Operation IDs appear only in deployment Details,
next to the deployment's release, which links to it in the application's
Releases tab, and its origin: built from the environment's source, promoted
from another environment's deployment, a release deployed again, or, for a
preview, a local snapshot with its commit and base. The
environment's Overview links to the release its current target runs. Current target, last successful deployment, and observed runtime health
are distinct. History remains until the environment is deleted. Deploying an
empty application removes its runtime services and network. Removing volumes or
deleting an application retains environments' Docker volume data; deleting an
application deletes every environment and preview, with the previews' volumes,
its configuration, and all database history. The
confirmation names the environments being deleted.

Forms save typed fields or settings sections through individual endpoints, without
resubmitting the application manifest. Related fields within a form save atomically.
All form saves update configuration only; Deploy remains explicit. A
repository-backed application's forms edit it the same way; its settings tabs
then list its [edits](reference/manifest.md#editing-repository-backed-applications)
from the manifest last read from the repository, which deployments of the
connection's branch deploy until a push changes the manifest there, and
**Reset to repository** discards them, after confirmation. Its Overview says whether its configuration is read from Git or
also edited in piqueld.
Concurrent edits are rejected using configuration revisions; failed saves retain
local form values. **Reload saved configuration**, offered after a failure or a
change made elsewhere, reloads it in place: forms without edits show the latest
version, and edited ones keep their edits until saved or discarded, discarding
back to the latest version. Navigation warns about unsaved edits. Host settings are
read-only. There is no raw manifest editor or deployment restoration workflow.

The browser bundle uses HTTP DTOs and shared typed lifecycle records and
fetches same-origin `/api/v1` resources. The daemon serves it only on the
configured localhost or Tailscale TCP listeners and, with a
[dashboard hostname](guides/ingress.md#dashboard-hostname), through private ingress.
The API Unix socket is API-only, and the daemon does not add CORS, browser telemetry,
or a public binding. Passkey login
establishes an HTTP-only session cookie; the Accounts page manages accounts and
their grants, invitations, passkey enrollment links, and tokens. See
[managing accounts](accounts/accounts.md). Pages, tabs, and actions the
signed-in account cannot use are hidden. Without `apps:write` on an
application, its configuration forms are read-only. The daemon still checks
every request, and refuses what the account's grants don't allow with
`permission_denied`. See
[permissions](accounts/permissions.md#what-people-without-access-see).
A link from `piquelctl sign-in-link` opens a page that signs the browser in as
the [host operator](accounts/recovery.md#the-host-operator) for 12 hours, without
an account. The dashboard then shows "Host operator" as the signed-in user and
hides actions that need an account: adding passkeys, creating tokens,
invitation and enrollment links, and CLI login approval. Accounts with
`accounts:manage` see live host operator sessions on the Accounts page; since
those sessions act with `admin` on everything, only accounts holding that can
revoke them. When the session expires or is revoked, the dashboard asks to sign
in again over the current page, keeping unsaved edits; signing in as another
account or the host operator reloads the dashboard without them.
The Audit page shows the [audit trail](guides/observability.md#audit-trail), naming the
application or environment each request addressed, and each
session or token links to its own activity. New tokens can be bound to a
tailnet user or tag; see [tailnet-bound tokens](accounts/cli-and-tokens.md#bind-a-token-to-tailnet-devices). With `audit:read`, **Verify
integrity** checks the trail's [hash chain](guides/observability.md#tamper-evidence). Log display preferences persist in browser
local storage.

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

Streaming logs, running commands in containers (`piquelctl app exec`), and
state transfer remain outside the current dashboard scope.
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
The application page shows the manifest last fetched from the branch
`spec.manifest` names, and reloads it after a fetch unless repository settings
are being edited.

Each environment of a repository-backed application follows its own branch.
**New environment** asks for the branch (by default the one `spec.manifest`
names) and an optional commit, the Environments list shows each environment's
branch, and the **Source** card on an environment's Overview changes it; nothing
is redeployed until its next deployment. The environment's Variables card and
Logs service filter read the manifest last fetched from its branch; the
Variables card says when it has fetched nothing yet.

The **Source** card makes this one choice: **Deploys** either *Builds its own source* (the
saved manifest, or for a repository-backed application the branch and optional commit
below) or *Releases promoted from \<environment>*, which never builds and only receives
releases promoted from it. The card says what the environment does now and what the
selected choice would do; **Save source** applies it, redeploying nothing. A promoted
environment's card links to its source.
**New environment** can create a promoted environment directly. Its Variables
card reads the manifest of the release last promoted into it. Its Visibility card
edits the saved manifest, like every environment's; since a promoted environment
uses the visibility and variables recorded in the release it receives, a saved
change applies once its source records a new release and it is promoted. Unsaved
changes in the Source, Visibility, and rename forms block deployment and warn
before navigation, like other forms.

When the manifest [declares environments](reference/manifest.md#declared-environments),
it owns their existence, so **Rename environment** and **Delete environment** are disabled for them, and,
when an environment's `[spec.environments.<name>]` block sets its source, its
source and deploy on push: its **Source** card then names that block and is
read-only, and so is **Deploy on push**. A
**Differs from the manifest** card above the Environments list shows changes
waiting for the manifest's next save or fetch, and environments it no longer
declares (*Not declared*), which keep running until deleted. **New environment**
then says that environments it creates aren't declared.

A deployment's Snapshot shows the manifest path and revision it was fetched
from, and a warning when the fetched file's own `spec.manifest` names another
repository or path (`manifest_connection_ignored`); that section is ignored.

**Deploy on push** in the repository settings selects how the application
[follows pushes](reference/manifest.md#deploying-on-push): off, polling every
given number of seconds, or GitHub webhooks. While it is on, the card shows the
last check of the repository's branches and why it failed. With webhooks, a
**GitHub webhook** card shows the payload URL to configure (or that the daemon
must set `ingress.webhook_hostname`), the content type and event, and whether a
secret exists. **Generate secret** creates one and shows it once, with a
**Copy** button, as the payload URL has (browsers offer no clipboard on plain
HTTP origins, so there the text is only selectable), until **Done** or leaving
the Source tab;
**Rotate secret** confirms first, since the previous secret stops verifying at
once. Neither is retried automatically when its outcome is unknown, as a retry
would rotate the secret again. On an environment's Overview, the **Source** card says
whether pushes deploy it (following, off, pinned, not opted in, stopped, or
waiting for a deployment, with the reason) and the branch head of its last
deployment ("Not pushed" when a local snapshot was deployed before its branch
existed); **Deploy on push** opts it in without a saved revision, and
**Stop deploying on push** opts it out. A
preview's page shows the same, without the opt-out. An environment's Overview
has a **Telemetry** settings card: **Grant telemetry** confirms, then grants the
environment telemetry, and **Revoke telemetry** revokes it, for holders of
`telemetry:grant`. Events caused by sync name
`sync:poll` or `sync:webhook` as their requester.

**Manifest**, beside the deployment actions, downloads the current server-saved configuration as TOML. Unsaved form edits and runtime/deployment state are excluded. Repository connection settings are preserved; the download does not fetch Git or require Docker. Original comments and formatting are not retained.


Environment and service Logs tabs read recent output directly from Docker.
Service and stdout/stderr dropdowns filter in the daemon. Snapshots refresh every
30 seconds while the view and browser tab are visible; Refresh is also available
on the right. Merged terminal output appears only with Both streams selected.

Runtime and build output share a monospace terminal with grey messages, separate
metadata, local HH:mm:ss timestamps (full precision on hover), red backgrounds
for ERROR/FATAL messages, and subtle warning backgrounds for other stderr output.
Task IDs and stream labels are omitted. Timestamp, service visibility, and wrap
preferences persist in this browser. Environment logs default to showing timestamps
and services. Build timestamps default off and have a separate saved preference;
build output never shows service labels or a service toggle. Wrapping defaults off,
and service labels default hidden in a service’s own Logs tab. Terminal escape formatting is stripped. New output follows
the bottom only when the reader has not scrolled up.

The main overview and Daemon status group daemon connectivity with deployment
readiness for SQLite, Docker reachability, Swarm suitability, and HTTPS ingress,
plus the tailnet node when enabled. A Telemetry export card per destination
shows the `[telemetry]` endpoint and the receiving service of each granted
environment of a readable application, with whether it is connected. A Built images card shows
how many images this installation built that image cleanup kept, and the size
of those it removed since the daemon started. Green and red labels state
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

See [observability](guides/observability.md) for diagnostic history, daemon statistics, analytics, telemetry
export and webhook delivery configuration.

