# Database migrations

SQLx's SQLite driver owns persistence. The database at
`<server.data_dir>/piqueld.db` contains application manifests, environments
with their resolved targets and status, operation history, and the control-plane
instance identity.
The store reads and writes these records; it does not resolve images or plan
runtime changes.

`0001_control_plane.sql` creates the consolidated prototype schema with six
tables: instance metadata, applications, application status, operations,
informational events, and request receipts. Accepted manifests may have no resolved
target yet; operation preparation publishes the target after planning checks.
Operation promotion is durable, so restart recovery knows whether to maintain the
old target during preparation or continue the new rollout. All runtime SQLite
writers, including build output, commit metadata, completion, and recovery,
share an asynchronous queue. Read-then-write transactions acquire SQLite's write
lock before reading, preventing stale WAL snapshots during concurrent builds.
Request receipts are committed with acceptance and expire after 24 hours,
independently of operation and event retention. Earlier prototype schemas, including those without the distinct `superseded`
operation state, require a fresh database.

`0003_deployment_inputs.sql` adds durable candidate manifests for manual Deploy.
Candidates and fetched commit provenance remain separate from accepted intent
until source preparation succeeds, and are removed with their operation history.
Existing version-1 databases are upgraded while retaining instance identity.

Startup reads `PRAGMA user_version`, rejects an unsupported newer schema, and
applies missing embedded migrations transactionally. Each migration commits its
schema changes, `user_version`, and matching instance metadata together. A
restart or later migration failure can resume from any committed version without
changing the instance identity.

`0002_deployments.sql` separates editable configuration from deployment inputs.
Each deployment references its execution operation and captures a manifest and
configuration revision. Attempt outcomes survive ordinary event pruning, and
successful convergence is retained independently of later runtime repair.
The migration captures the latest recoverable legacy intent; older operations
lack source manifests and are not presented as reconstructed deployments.

Saving configuration does not create or supersede an operation. Deployment
preparation reads only its snapshot, including after restart. Prepared image
resolutions remain attached to that operation during retries. A new deployment
resolves image tags again. Empty applications are valid and deploy without
services or networks.

Deployment snapshots, execution records and attempt outcomes are retained until
environment deletion. Deletion removes the environment's database records,
environment-owned history and request receipts after runtime verification; Docker
volumes remain. The application is removed with its last environment when its
deletion was requested. Daemon-scoped failures retain optional environment
context after deletion.
Application events use `retention.event_days`; shared daemon events use
`retention.daemon_event_days` (both default to 90 days; zero disables pruning). Non-deployment operation retention remains configurable.

The daemon prepares its private data directory before opening SQLite. The store
checks the database file path. During builds, the daemon build script provisions
a disposable migrated database for SQLx query checks; it does not open an
operator database.

Migration 0004 adds executor-independent build attempts and bounded output chunks.
Build metadata is owned by the application rather than an operation, so pruning
operation history cannot erase build history. Interrupted running records are
recovered at coordinator startup; output retention leaves metadata intact.

`0011_job_runs.sql` adds job names and exit codes to build records, so one-shot
job runs share build output storage, paging, and retention.

`0005_structured_build_logs.sql` adds capture timestamps and stdout/stderr
identity to output chunks. Old unstructured output is marked expired because
its missing provenance cannot be reconstructed; build metadata remains intact.
The stream index supports filtering before backward pagination.

`0006_observability.sql` adds self-contained diagnostic and action context to
events, active action recovery, retention coverage, and a durable webhook outbox
with activation watermarks and observed health conditions. Existing events keep
application scope; old diagnostic detail is not reconstructed. The migration
starts notification processing after existing events, avoiding historical alerts.
See [observability](observability.md) for the API and deletion contract.

Recovery deliveries are paired with their original failures at each destination
in the same observability migration.

`retention.event_days` defaulted to 30 before this release, and both event
retention settings briefly defaulted to no pruning afterwards. They now default to
90 days, so upgrading an installation that relied on unbounded history prunes
older events on the next reconciliation. Set both to `0` before upgrading to keep it.

`0007_authentication.sql` adds accounts, passkeys, hashed session/API
credentials, invitations, and a durable initial-setup marker. Existing installations require first-account
setup after upgrading; application data and running reconciliation are preserved.
There is no anonymous API compatibility mode. See [authentication](authentication.md).
Follow the [upgrade and rollback checklist](authentication.md#upgrading-an-existing-installation)
before deployment. An older daemon rejects the migrated schema; rollback requires
restoring the pre-upgrade data directory together with the old binary/configuration.

`0008_application_secrets.sql` adds application-secret metadata, encrypted versions,
per-deployment pins and a persistent master-key verifier. Empty pin sets are
recorded too, so retries cannot silently pick up subsequently added values. Secret
value changes do not update application configuration or request deployments.
Deletion reservations survive restarts; retry deletion to finish cleanup. Lost-key
recovery marks discarded versions unavailable without changing pins, and
unavailable values are excluded from retained-value quotas.

`0009_ingress.sql` adds transactional hostname reservations, hostnames reserved
for the installation's own website, and a per-application routing projection. Desired routes are journaled before gateway I/O; the last
accepted projection retains ownership until a removal is acknowledged. Saved
configuration, captured deployment inputs, resolved targets, and both routing
projections participate in reservation checks within the same SQLite transaction.
An interrupted gateway update is reapplied from durable intent. Application deletion
cascades routing records only after gateway withdrawal and runtime cleanup succeed.

`0010_history_pagination.sql` aligns the deployment application index with its
ID-based cursor and ordering. Filtered event/build queries use direct application
equality, and filtered output uses direct stream equality, so their existing
compound indexes bound each page without scanning unrelated history.

`0012_environments.sql` splits applications into applications and their
environments. Every existing application becomes an application with one
environment named `production`; both keep the existing ID, so Docker names,
ownership labels, secret encryption context and history are unchanged and
nothing is redeployed. The former `applications` table is renamed
`environments` and keeps each environment's resolved target and deletion intent.
A new `applications` table holds the name, saved manifest, configuration
revision and deletion intent. Runtime tables (`environment_status`,
`operations`, `deployments`, `builds`, `events`, `environment_secrets`,
`secret_versions`, deployment pins, `environment_routes`, hostname reservations,
active actions and notification conditions) name their owner `environment_id`.
Stored operation records and request receipts that still say `application_id`
keep decoding. An older daemon rejects the migrated schema; restore the
pre-upgrade backup to roll back.

`0013_manifest_variables.sql` makes saved manifests templates in which
`${{ namespace.name }}` references a variable. It escapes any `${{` already in
saved manifests and pending deployment candidates as `$${{`, so their text keeps
its meaning. Deployments gain `template_json`, the manifest as captured with
references unresolved, and `variables_json`, the values they rendered to;
existing deployments record their literal manifest and no variables.
`manifest_json` keeps the rendered manifest, which is JSON `null` until a
repository-backed manifest is fetched.

`0014_environment_branches.sql` stores where each environment deploys from.
Environments gain `branch` and `pinned_commit` (NULL for environments that
deploy the saved manifest) and `manifest_json`, the manifest last fetched from
their branch. Every environment of a repository-backed application takes the
branch and pinned commit its `spec.manifest` names, and the saved manifest,
which was the last fetched one, as its own, so nothing deploys or reserves
differently after upgrading. Stored environment responses that replay retried
requests take the migrated source. Deployments gain `warnings_json`, problems found
while fetching that did not stop the deployment; existing ones have none.

`0015_authorization.sql` adds grants for accounts, credentials, and invitations,
and lets an invitation target an existing account for passkey enrollment. Every
existing account receives `admin` on every application, so access is unchanged
until reduced; see [authorization](authorization.md#upgrading). Grants on an
application cover each of its environments and are removed with it. Request
receipts record the account that made the request. Receipts from before the
upgrade have no account, so retrying an account's request across the upgrade
answers 409 `request_id_conflict`; the host operator still replays them.

`0016_scoped_tokens.sql` marks credentials limited to their own grants. Existing
API tokens become scoped with `admin` on every application, so they keep their
account's full access, but they can no longer create credentials.

`0017_audit.sql` adds the audit trail, kept independently of application
history, and records the account and credential behind operations and their
events. A trigger copies an operation's actor onto every event about it.

`0018_security.sql` links audit records into a hash chain, records which
network addresses each credential was used from, and stores the single
outstanding admin recovery link. Releases apply it together with
`0017_audit.sql`, so the trail is still empty; records written by pre-release
builds cannot be linked and are dropped.

`0019_tailnet.sql` stores the tailnet user or tag a token is bound to and the
tailnet identity behind each audited request.

`0020_host_operator.sql` stores host operator sign-in links and the browser
sessions they open, and records the host operator's Unix user on audited
requests, operations, events, and running actions. The trigger copying an
operation's actor onto its events now copies it too.

`0021_application_secret_store.sql` moves manually set secrets into one store
per application, where each lists the environments that may mount it. A name is
either generated or stored per application, so when environments disagree about a
name (one set it manually, another's manifest declares it), the first environment
that uses it decides: `production`, else the oldest. An environment uses a name
when it holds a value for it or the manifest it deploys declares it in
`spec.secrets`: the saved one, or for an environment following a branch only the
last one fetched from it (none before its first fetch). The deciding environment's secret is generated,
and stays with it, when its manifest declares the name or a retained deployment
both declares and pins it; otherwise it is manual and moves, with access limited
to that environment, so no other environment gains access by upgrading. Deleting
a secret drops its pins, so a name set manually after its generated value was
deleted counts as manual. An environment that disagrees with the deciding one
needs a change before its next deployment: if it declares a name that moved, it
fails with `secret_name_conflict` until its manifest renames the declaration; if
it set a name the deciding environment generates, that value is missing until it
mounts another stored name through a variable. Same-named secrets of other environments
stay with them, keeping their deployments' pins, until deleted. Moved versions
keep their Docker secret names, and their deployment pins move with them, so
running services and retries resolve the same values and nothing is redeployed.
The encryption context binds a value to its owner, so on its first start with a
usable master key the daemon re-encrypts moved values for their application in
one transaction. Until then they remain readable in their former context.

`0022_releases.sql` adds immutable releases. Each successful preparation in an
environment that builds its own source records what it deployed: the captured
manifest, the commit it was read from, and each service's source and image.
Releases belong to the application, so deleting an environment keeps them;
preparations with the same content hash share one. Deployments gain
`release_id`. A release's content hash needs the daemon, so on its first start
after upgrading it records releases for deployments already prepared, oldest
first, sharing them the same way; deployments that never finished preparing
have none.

`0023_previews.sql` makes previews environments of another `kind`, so they
reuse every environment table. Existing rows become `environment`. A preview
always has a branch and never a pinned commit, records its optional slot in
`preview_slot`, and is named by its slug, so the existing unique name index
keeps previews and environments of one application apart. The partial unique
index `preview_key` on (application, branch, slot) makes creating a preview
idempotent without the application revision. `preview_volumes` lists every
Docker volume each preview's deployments created; it is removed with its
preview, after deletion has removed and verified those volumes. Nothing is
redeployed.

`0025_sync.sql` records what [deploying on push](application-manifest.md#deploying-on-push)
observes; how an application syncs is part of its saved manifest. Environments
gain `sync`, set once one opts in (previews follow their application), and
`synced_commit`/`synced_at_ms`, the branch head as of its last deployment,
cleared when its branch or the application's repository URL changes.
Applications gain the last check of their repository, `sync_checked_at_ms` and
`sync_error`. `webhook_secrets` holds each application's webhook secret,
encrypted with the secret master key; lost-key recovery deletes them.
Operations, events, and active actions gain `actor_system`, e.g. `sync:poll`,
and the trigger copying an operation's actor onto its events copies it too.
Existing applications don't sync, so nothing is redeployed. It follows
`0024_dns_records.sql`; a database that ran this change's former
`0024_sync.sql` (only development instances) needs resetting.

## Upgrade and rollback

Migrations are forward-only. An older daemon rejects a database with a newer
schema; reverting the binary alone is insufficient. Before upgrading, create a
SQLite backup in a private directory, for example:

```sh
sqlite3 /var/lib/piqueld/piqueld.db ".backup '/safe/location/piqueld-before-upgrade.db'"
sqlite3 /safe/location/piqueld-before-upgrade.db 'PRAGMA integrity_check; PRAGMA user_version;'
```

The SQLite backup command includes committed WAL data. Preserve the matching
daemon configuration and previous binary too. To roll back, stop the upgraded
daemon, restore the backup as `piqueld.db` in a fresh data directory, and configure
the previous daemon to use that directory. Do not reuse WAL/SHM files from the
upgraded database. Restore also reverts application edits and deployment history
accepted after the backup; reconciliation observes the current Docker state
against that restored intent.

The migration tests cover a populated schema-5 upgrade, retained event identity
and notification activation, and restoration of the pre-upgrade snapshot.
