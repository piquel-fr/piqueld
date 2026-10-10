# HTTP API

The daemon's transport-independent API is `piqueld::api::ApplicationService`.
It is a cheap, clonable handle shared by HTTP handlers and future MCP or scheduled
callers. Its private store and runtime back every domain operation: mutation
acceptance, planning, application views, history, logs, manifest export, and host
status. Its implementation lives in the private `api::service` module; the Axum
adapter lives in `api::http`. Future MCP and cron adapters can be added alongside
HTTP. Adapters decode transport inputs and map service results and errors, while
runtime implementation and boundary types remain in `application`.
`accept` enforces revision/identity preconditions, explicit force overrides, and
idempotency for every caller; `Mutation::save` saves configuration and optionally
deploys it. Inputs use validated manifests and typed application and environment IDs.

`ApplicationService::start` opens the store, connects Docker, and starts the
reconciliation worker. The process binds listeners and holds directory locks
before calling it, then cancels and joins the returned worker before releasing
those locks. `ApplicationService::new` accepts supplied storage and runtime
adapters for tests or embedded use; it does not start background work.

The API is rooted at `/api/v1` over a Unix socket and optional localhost or Tailscale TCP.
Responses use a `data` envelope; lists contain `items` and an opaque
`next_cursor`. Errors expose a safe message, code, details, and request ID.
Clients poll for progress.

An application owns the saved manifest and its configuration revision
(`generation`). Its environments deploy that manifest; each owns its deployment
history, operations, status, volumes, generated secrets, routes, and Docker
network; manually set secrets live in the application's secret store.
Environment IDs are the IDs Docker names, ownership labels, and history derive
from. Applications that existed before environments have one environment named
`production` that kept their ID; new applications get a `production`
environment sharing the application ID, and further environments get their own IDs.
An environment's `source` is `{ "type": "saved" }` for applications without a
manifest repository, or `{ "type": "branch", "branch": "main" }` for
repository-backed ones: each such environment follows its own branch of the
application's repository, optionally pinned to a commit (`"commit"`). The repository URL and
manifest path stay on the application (`spec.manifest`). Connecting a repository
points every environment at the branch `spec.manifest` names; disconnecting
returns them to the saved manifest. A
[promoted environment](application-manifest.md#promoted-environments) has
`{ "type": "promoted", "environment": "env-..." }`, naming by ID the environment
it receives releases from; repository changes leave it promoted.

A repository-backed environment deploys the manifest last fetched from its
branch, which `EnvironmentDetailView.manifest` returns (null before its first
fetch). Hostname reservations, the `environment_configured` rename check, and
`plan` with `environment=ID` read that manifest, so a fetch for one environment
never changes another. The application's saved manifest is the one last
fetched by any environment, with the application's own connection; it is what
application views show and what environments deploy after disconnecting.
A sibling hostname conflict returns `hostname_conflict` with the hostname and
reserving environment name in `details.hostname` and `details.environment`.

A [preview](application-manifest.md#previews) is an environment whose `kind`
is `{ "type": "preview", "branch": "feat/login", "slot": "agent-2", "slug":
"notes-feat-login-agent-2-503aa8" }`; environments have `{ "type": "environment" }`.
A preview's `name` is its slug and its `source` is its branch. Application
views list previews separately in `previews`, never in `environments`, and the
environment mutation endpoints (deploy, rename, branch, source, promote, delete,
reconcile) return 404 for a preview's ID, as the preview endpoints do for an
environment's. The environment read endpoints (`detail`, `status`,
`deployments`, `logs`, `secrets`) accept a preview's ID. Preview creation,
deployment and deletion take no `expected_generation` and never advance the
application revision: creation is idempotent on (branch, slot) instead.

Creating a preview that would exceed a [`[previews]` limit](configuration.md#previews)
fails with 409 `preview_limit_reached`. Its `details` are a `PreviewLimitReached`:
the `limit` (`per_application` or `total`), its `max`, and the `previews` it
counts that the caller may read, oldest deployment first, each with its `id`,
`application_id`, `preview` (branch, slot, slug), `deleting` (previews count
until their deletion finishes) and `last_deployment` (`id`, `created_at_ms`). Returning an existing preview never counts against a limit.
`PreviewView.bounds` lists how the limits bounded the preview's deployed target:
`preview_limits_defaulted` and `preview_replicas_capped` warnings, also recorded
on its deployments and plans. `SystemStatus.previews`, absent when they cannot be counted, counts previews against
the limits, installation-wide and for each readable application, sums the
CPU and memory limits of their deployed replicas, and counts the replicas
deployed before the limits applied, which run without them until redeployed.

[Deploying on push](application-manifest.md#deploying-on-push) is configured by
`spec.manifest.sync`. Environment views carry `sync`, true once an environment
opted in (previews follow their application), and `synced`, the branch head as
of its last deployment (`commit`, `at_ms`), which sync follows from.
`PreviewView.sync` is `following` while the application syncs and once the
preview was deployed, and `ApplicationView.sync_check` is the last listing of the repository's
branches (`checked_at_ms`, and `error` when it failed). Sync deployments are
ordinary operations whose events carry a `system` actor, `sync:poll` or
`sync:webhook`, with a `branch_synced` event naming the commit. GitHub
deliveries never reach this API; see [push webhooks](ingress.md#push-webhooks).

Application list items contain `id`, `name`, generation metadata, deletion
intent, timestamps, and their environments. Read `/api/v1/applications/{id}`
when the complete normalized manifest is needed.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/v1/system/status` | Daemon status, with `images`: the built images cleanup kept, the total size of those it removed since the daemon started, and when it last ran; DNS providers and certificates only with `system:read`; preview counts per readable application |
| GET | `/api/v1/system/configuration` | Effective read-only host settings |
| POST | `/api/v1/system/dns/refresh` | Check DNS provider credentials and zones now (`system:operate`); returns `DnsStatus`, empty without `system:read` |
| GET | `/api/v1/openapi.json` | Generated API schema |
| GET | `/api/v1/applications` | Paginated application summaries (up to 100 per page) |
| GET | `/api/v1/applications/{id}` | Full latest accepted application intent and its environments |
| POST | `/api/v1/applications/plan` | Preview a manifest without pulling images; `environment=ID` selects the environment to compare with |
| POST | `/api/v1/applications/apply` | Save configuration by name; `?deploy=true` also deploys its only environment |
| DELETE | `/api/v1/applications/{id}` | Request deletion of every environment and preview; no body. `environments=a,b` must name every environment when there are several |
| POST | `/api/v1/applications/{id}/rename` | Rename an idle application without redeployment |
| POST | `/api/v1/applications/{id}/environments` | Add an environment: `{ "name": "staging", "branch": "main", "commit": null, "expected_generation": 3 }`; `branch` defaults to the one `spec.manifest` names and requires a repository-backed application. `"promote_from": "env-..."` instead creates a promoted environment; it excludes `branch` and `commit` (400 `source_invalid`) |
| PUT | `/api/v1/environments/{id}/branch` | Follow another branch, or pin or unpin a commit, without redeploying: `{ "branch": "release", "commit": null, "expected_generation": 3 }` |
| PUT | `/api/v1/environments/{id}/sync` | Opt an environment into or out of its application's sync: `{ "enabled": true }`; needs no `expected_generation`, and opting in while it syncs needs `apps:deploy` too. 404 for previews, which follow their application |
| GET | `/api/v1/applications/{id}/webhook` | `WebhookView`: the payload URL to configure in GitHub (absent until the daemon sets `ingress.webhook_hostname`) and when the secret was generated |
| POST | `/api/v1/applications/{id}/webhook/secret` | Generate a new webhook secret, replacing the previous one (`apps:write`); the `WebhookSecret` response is the only time it is shown |
| PUT | `/api/v1/environments/{id}/source` | Make the environment promoted, `{ "promote_from": "env-...", "expected_generation": 3 }`, or, with `promote_from` null, track the saved manifest or the branch `spec.manifest` names again; deploys nothing |
| POST | `/api/v1/environments/{id}/promote` | Promote a release into a promoted environment: `{ "deployment": null, "release": null, "expected_generation": 3 }`; 202 with `AcceptedPromotion` |
| POST | `/api/v1/environments/{id}/promote/plan` | Plan that promotion, or an earlier `release` for any environment, without changing anything; `PlanView` with `release` |
| GET | `/api/v1/environments/{id}` | Environment metadata |
| GET | `/api/v1/environments/{id}/detail` | Environment, application intent, resolved generation, current release, observed runtime, operation, diagnostics, and the URL of each rendered route with its state (see [URL readiness](#url-readiness)) |
| GET | `/api/v1/environments/{id}/status` | Intent progress and separate runtime health |
| POST | `/api/v1/environments/{id}/deploy` | Deploy the environment from its source with fresh source resolution; supersede pending work. `branch=NAME` or `commit=SHA` fetches a repository-backed manifest from that revision instead of the environment's branch, for this deployment only |
| GET | `/api/v1/environments/{id}/deployments` | Deployment snapshots, newest first, three per page |
| GET | `/api/v1/environments/{id}/deployments/{deployment}/attempts` | Retained outcomes, newest first, 100 per page |
| GET | `/api/v1/applications/{id}/releases` | The application's releases, newest first, twenty per page |
| POST | `/api/v1/environments/{id}/rename` | Rename an environment without redeployment: `{ "name": "...", "expected_generation": 3 }` |
| DELETE | `/api/v1/environments/{id}` | Request deletion of one environment; no body |
| POST | `/api/v1/environments/{id}/reconcile` | Retry the latest operation with its saved inputs once it has ended or failed; an operation still in progress is returned unchanged |
| GET | `/api/v1/applications/{id}/previews` | Previews with status, latest operation, hostnames and branch state (`exists`, `moved`, `gone` or `unknown`), from one `git ls-remote` |
| POST | `/api/v1/applications/{id}/previews` | Create and deploy a preview: `{ "branch": "feat/login", "slot": "agent-2" }`; 202 with `CreatedPreview` (`preview`, `operation`, `created: true`), or 200 with the existing preview of that branch and slot and its latest operation (`created: false`), never redeploying it. 409 `preview_requires_repository` without a manifest repository, `preview_limit_reached` over a `[previews]` limit |
| POST | `/api/v1/applications/{id}/previews/prune` | Delete each listed preview whose branch is confirmed gone: `{ "previews": [preview IDs] }`; returns the `DeletedPreview`s and keeps the rest. 502 `repository_unavailable`, deleting nothing, when the repository cannot be read |
| GET | `/api/v1/previews/{id}` | One `PreviewView` |
| POST | `/api/v1/previews/{id}/deploy` | Deploy the head of the preview's branch; 202 with `AcceptedOperation` |
| DELETE | `/api/v1/previews/{id}` | Delete the preview and every volume it created; 202 with `AcceptedOperation` |
| GET | `/api/v1/operations/{id}` | Inspect progress, attempt count, and safe diagnostics |
| GET | `/api/v1/events` | Paginated informational history, oldest first |
| GET | `/api/v1/environments/{id}/exec` | WebSocket streaming a command in a running service task |

### Field endpoints

All paths below are relative to `/api/v1/applications/{id}`. Edits accept
`expected_generation=N` (required unless `force=true`), `deploy=true|false`
(default false), and `Idempotency-Key`. They return `Envelope<SavedApplication>`
with HTTP 200 for saving or HTTP 202 when a deployment is accepted.
`deploy=true` deploys the application's only environment and fails with 409
`environment_required` (listing them in `details.environments`) when it has
several or none. The server
loads, edits, validates, and saves its internal manifest inside one transaction;
clients do not need to read and replace it. The request receipt and optional
immutable deployment snapshot commit in the same transaction.

| Method | Path | JSON body |
| --- | --- | --- |
| PUT | `/name` | `{ "value": "new-name" }` |
| POST | `/services` | `Service` (new name required) |
| DELETE | `/services/{service}` | None |
| POST | `/volumes` | `{ "name": "data" }` |
| DELETE | `/volumes/{volume}` | None; mounted volumes are rejected |
| PUT | `/routes` | `{ "value": [{ "hostname": "notes.example.com", "service": "web", "port": 3000 }] }`; replaces this application's routes |
| PUT | `/jobs` | `{ "value": [{ "name": "migrate", "service": "web", "command": ["notes", "migrate"], "run": "before-rollout", "timeout_seconds": 300 }] }`; replaces the jobs, in execution order |
| PUT | `/variables` | `{ "value": { "defaults": { "domain": "piquel.fr" }, "environments": { "staging": { "domain": "staging.piquel.fr" } } } }`; replaces every variable value |
| PUT | `/services/{service}/name` | `{ "value": "worker" }` |
| PUT | `/services/{service}/replicas` | `{ "value": 3 }`, or a reference such as `"${{ vars.web_replicas }}"`; typed fields accept references |
| PUT | `/services/{service}/source` | `{ "value": Source }` |
| PUT | `/services/{service}/source/image` | `{ "value": "nginx:stable" }` |
| PUT | `/services/{service}/source/git/{url,branch,commit,dockerfile,context}` | `{ "value": "..." }`; commit may be null |
| PUT / DELETE | `/services/{service}/environment/{key}` | PUT: `{ "value": "..." }`; DELETE: none |
| PUT | `/services/{service}/{command,arguments}` | `{ "value": ["element", "..."] }` |
| PUT | `/services/{service}/mount` | `Mount`; adds/replaces by container target |
| DELETE | `/services/{service}/mount` | `{ "value": "/container/target" }` |
| PUT | `/services/{service}/depends-on` | `{ "value": ["db"] }`; replaces the services that must be healthy before this one rolls out |
| PUT | `/services/{service}/rollout` | `{ "value": { "order": "stop-first", "monitor_seconds": 30 } }`; replaces the rollout block, and omitted fields use their defaults |
| PUT | `/services/{service}/secrets` | `{ "value": [{ "name": "token", "target": "/run/secrets/token" }] }`; replaces file references without exposing values |
| PUT | `/services/{service}/healthcheck` | `{ "value": HealthCheck }`; null clears |
| PUT | `/services/{service}/healthcheck/{port,path,command,interval,timeout}` | Typed `{ "value": ... }` |
| PUT | `/services/{service}/resources/{cpu,memory}` | `{ "value": 500 }`; null clears the selected limit |
| PUT / DELETE | `/repository` | PUT: `{ "value": RepositoryManifest }`; DELETE: disconnect |
| PUT | `/repository/{url,path}` | `{ "value": "..." }`; every environment fetches from them. Branches belong to environments: see `/api/v1/environments/{id}/branch` |
| PUT | `/repository/sync` | `{ "value": { "mode": "poll", "interval_seconds": 300 } }`, `{ "mode": "webhook" }`, or `{ "mode": "off" }`. Turning sync on needs `apps:deploy` too, as does saving or connecting with it on |

The braces listing multiple names denote separate documented endpoints. Nested
source/check settings require the appropriate variant; switch variants through
`source` or `healthcheck`. Unknown fields, missing values, invalid manifests, and
missing resources are rejected. Optional values must explicitly use null to clear.
The same validation and Git ownership rules apply even with `force=true`.
Disconnecting a repository preserves saved services and volumes for local editing.
Renaming a service updates its routes, its jobs, and dependents' `depends_on`;
removing a service removes its routes and jobs and drops it from other services'
`depends_on`.
Jobs inherit the referenced service's `depends_on`, including transitive
dependencies. Those services converge before the job starts; other services
wait for all jobs to succeed. If a dependency has its own jobs, they must appear
earlier in the job list (`job_dependency_order_invalid`).
Routes remain saved while ingress is disabled and become active only after deployment
with ingress enabled in the daemon's read-only TOML configuration.

For dashboard forms, typed section endpoints also allow atomically replacing
`/volumes`, service `/environment`, `/mounts`, `/resources`, `/general`
(source and replicas), and `/process` (command and arguments). These never replace
the complete application manifest. See the generated OpenAPI document for exact
request schemas and error responses.

`POST /api/v1/applications` with `{ "value": "name" }` creates an empty application,
requiring the name to be absent. It supports `deploy` and `Idempotency-Key`.
Full-manifest apply remains available for import.

Saving and deployment acceptance work while Docker is unavailable. Execution
errors are recorded asynchronously. Preview requires runtime observation and
returns `503 docker_unavailable` during an outage. Environment detail remains
readable and reports unavailable runtime observation as a diagnostic.

Apply and plan accept JSON `{ "manifest": ..., "expected_generation": 3,
"expected_application_id": "app-..." }`, or complete TOML with
`Content-Type: application/toml` or `text/toml`. TOML preconditions use
`X-Expected-Generation` and `X-Expected-Application-Id`.

Every mutation of an application or one of its environments is conditioned on the
inspected application revision. Apply, deploy, delete, rename, and environment
creation require preconditions unless the endpoint is explicitly
called with the query parameter `force=true`. Missing preconditions return 400
`precondition_required`. Apply requires generation zero to create an absent name,
or both the inspected application ID and generation to update an existing name.
Deploy and delete require `expected_generation` in its query; renames,
environment creation, source changes and promotions take it in JSON.
Revision mismatches return 409 `generation_conflict`; identity mismatches return
409 `identity_conflict`. Checks and acceptance are atomic.

Force overrides both revision and name-based identity preconditions, even if stale
values were supplied. Forced apply overwrites whichever application currently has
the manifest name, or creates one if absent. ID-based mutations still target their
URL's ID. Force never bypasses manifest validation, resource ownership, or rename
busy/name-collision checks, and needs the same [permissions](authorization.md)
as the change without it.

Reconcile acts on the current intent without requiring preconditions;
it accepts an optional `expected_generation` query for callers that want one.
Reconcile can continue an already-requested deletion. Preview preconditions are
also optional. The CLI supplies apply/delete/rename preconditions automatically;
`--yes` skips confirmation and `--force` requests the override independently.

Configuration generation starts at 1 and advances on saves, changed names,
application deletion intent, and environment creation, renames and deletions,
since environment names select `[spec.environments.<name>]` configuration.
Deployments leave it unchanged; each environment records the revision it last
resolved as `resolved_generation`, which environment changes and renames keep
current unless the environment's configuration renders the changed name
(`${{ env.name }}`, `${{ env.slug }}`, `${{ app.name }}`). Apply replaces the full configuration without merging. It returns
200 with `SavedApplication` (`application_id`, `generation`, and null `operation_id`).
With `?deploy=true`, apply atomically saves and deploys, returning 202 with a populated
`operation_id`. Saving during deletion is rejected.

Deploy captures exactly the inspected saved revision, returning 202 with
`AcceptedOperation`. The saved manifest is rendered for the environment when
captured (repository manifests once fetched): a reference without a value
fails the request with 422 and `variable_value_missing` in `details.errors`.
Deployment snapshots record the captured manifest as `template`, the rendered
values as `variables`, and the rendered manifest as `application` (null until a
repository manifest is fetched). A fetched file's own `spec.manifest` is ignored:
the snapshot's `template.spec.manifest` is the repository, path, and revision it
was actually fetched from. When the file names another repository URL or path,
the snapshot's `warnings` lists `manifest_connection_ignored`; branches are not
compared, since they differ between environments. Every explicit Deploy creates a new snapshot and supersedes
pending work, even for unchanged configuration. It resolves image tags again;
matching healthy containers need no restart. Empty applications are valid: an
empty deployment removes services and networks while retaining volume data.

Reconciliation and retries use deployment snapshots and their prepared digests,
never newer saved edits. Configuration saves do not supersede operations. Deployments and
attempt outcomes remain indefinitely until environment deletion. The deployments
response distinguishes `current_target`, `last_successful`, and mutable operation
progress; last successful does not imply automatic rollback after a failed rollout.
History endpoints accept `cursor` for subsequent pages.

Each successful preparation in an environment that builds its own source (one
deploying the saved manifest or following a branch) records an immutable
release, and the deployment's `release` names it; a promotion's deployment
names the release it received. A `ReleaseView` holds the
captured manifest with references unresolved (`release.template`), the commit it
was read from when repository-backed (`release.commit`), each service's
provenance and image (`release.sources`: the registry digest of an image pulled
by reference, or the commit and local image ID of a Git build), and
`fingerprint`, each service's rendered build inputs by field: `source.image`,
or `source.repository`, `source.commit`, `source.build.dockerfile`,
`source.build.context`, `source.build.args.<NAME>`, and `source.build.target`.
`content_hash` covers the whole captured manifest (including its name and the
branch it was read from, which `${{ app.name }}` and `${{ git.branch }}`
render), its commit, and every source; preparations of one application with the
same hash share one release,
so environments running the same content name the same release. Releases
belong to the application: deleting an environment keeps them, and they are
removed with the application. `EnvironmentDetailView.release` names the release
the environment's current target runs. A recorded digest does not guarantee
the image still exists on the host: `availability` says whether it does, as
`{"state": "present"}`, `{"state": "pullable", "missing": [...]}` when only
registry images are missing, which deploying the release pulls again by
digest, or `{"state": "unavailable", "missing": [...]}` when a built image is
gone, each missing entry naming its `service` and `image`. It is absent when
Docker can't be asked. Deployments that use a retained image again fail with
`image_unavailable`, naming the service and image, when it is gone and can't
be pulled again; see [image retention](docker-reconciliation.md#image-retention). Deployments prepared before releases
existed record theirs when the daemon first starts after upgrading.

A release can be rendered for another environment of its application without
rebuilding: its own manifest renders with that environment's variables, and
every build input must render exactly as in its fingerprint. Otherwise the
release is incompatible there (`release_incompatible`, naming each field, e.g.
`web.source.build.args.VITE_ORIGIN`), typically because a build argument bakes
in one environment's domain. Promotion uses this.

A [promoted environment](application-manifest.md#promoted-environments) never
builds or fetches: deploying it returns 409 `environment_promoted`, making an
environment promoted returns 409 `application_busy` while a deployment of it
is requested or running, and retrying an earlier deployment that never
prepared fails with that diagnostic. Its source must be another live environment of the application,
never a preview (422 `promotion_source_invalid`, `details.environment`), and
must not promote from it, directly or through others (422 `promotion_cycle`,
`details.environments` lists the chain). Deleting an environment others promote
from returns 409 `promotion_source_in_use`, naming them in
`details.environments`.

`POST /api/v1/environments/{id}/promote` takes the source's current deployment
by default; `deployment` requires it to still be that one, and `release`
deploys an earlier release of the application instead, with the environment's
current secrets and no source check. Both together return 400
`promotion_invalid`. Like Deploy, it requires `expected_generation` unless
forced, needs `apps:deploy`, and supersedes pending work; it never advances the
application revision. Before anything is captured, it checks that the source's
current deployment succeeded and its services run as deployed and healthy now (409
`promotion_source_not_ready`, `details.environment`), that the release
instantiates for the environment (422 `release_incompatible`), that its images
are present or can be pulled again by digest (409 `image_unavailable`), and
that every stored secret it mounts exists and allows the environment (409
`secrets_unavailable`, with every problem in `details.secrets` as
`{ "problem": "missing" | "access_denied" | "unavailable" | "deleting", "secret": name }`;
`unavailable` is a stored value key recovery discarded, and `deleting` covers
generated secrets too). A request repeating an accepted one's
`Idempotency-Key` returns its response before any check. Acceptance checks
the source again in its transaction: a source whose current deployment is no
longer the requested or checked one returns 409 `promotion_source_changed` with
`details.environment` and `details.deployment`. Promoting into an environment
that builds its own source returns 409 `environment_not_promoted`.

The release's own manifest is rendered with the environment's
`[spec.environments.<name>]` block and variables from that manifest and
compiled with the release's images, and the prepared target is saved with the
accepted deployment, so the controller only rolls it out. `AcceptedPromotion`
is an `AcceptedOperation` with the pinned `release_id` and its `origin`.
`DeploymentView.origin` records where every deployment came from:
`{ "type": "build" }`, `{ "type": "promotion", "environment": "env-...",
"deployment": "operation-..." }`, or `{ "type": "release" }` for a release
deployed by ID. `ReleaseView.promotions` lists, newest first, the deployments
that received the release (`environment_id`, `deployment_id`, `origin`,
`created_at_ms`).

`POST /api/v1/environments/{id}/promote/plan` takes the same body, ignores
`expected_generation`, and needs `apps:read`. It applies the source checks and
`release_incompatible`, then returns a `PlanView` whose `changes` compare the
rendered release with the environment's current deployment and whose `plan`
compares it with the observed runtime. Its `release` is a `ReleasePlan`: the
`ReleaseView` with `availability`, the `origin`, `new_volumes` that would be
created empty, and every unusable secret in `secrets`, which do not fail the
plan. Plans of an earlier `release` work for any environment.

Mutation endpoints accept an optional `Idempotency-Key` (1–128 ASCII letters,
digits, `-`, `_`, `.`, or `:`). The CLI generates one UUID per command and reuses
it for transport retries. The daemon atomically stores the accepted response and a
fingerprint of the normalized request in SQLite for 24 hours. A matching retry
returns the original response before checking the current generation, even after
restart, failure, or supersession. It never restarts the operation. Different input,
or another account, under the same key returns 409 `request_id_conflict`. Expired keys are treated as
new requests subject to current preconditions. Receipts contain no manifests or
raw request bodies, and do not guarantee exactly-once Docker effects. Force is
part of request identity: retrying a forced request replays its receipt instead of
overwriting intervening changes again. A new forced command needs a new key.

Rename accepts JSON `{ "name": "new-name", "expected_generation": 3 }` and returns
200 with `RenamedApplication`. It rejects pending/running operations in any
environment and deletion intent with 409 `application_busy`, and occupied names
with 409 `application_name_collision`. It preserves the stable ID, runtime
resources, and operation identity; a changed name advances generation and records
an event in every environment's history. Update the manifest's name before
subsequent name-based apply.

Environment creation and rename return 200 with `EnvironmentView`. Names follow
the logical-name rules and are unique within the application (409
`application_name_collision`). A new environment starts `not_deployed`; renaming
never touches the runtime. Every environment reserves the hostnames the manifest
it deploys renders for it, so environments of one application may use different
hostnames through variables; two that render the same hostname fail with 409
`hostname_conflict`. Renaming an environment returns 409 `environment_configured`
while that manifest has a `[spec.environments.<name>]` block for the old or new
name. Creating an environment with a `branch`, or changing the branch of one,
for an application without a manifest repository fails with 422
`manifest_repository_required`; an invalid branch or commit fails with
`git_branch_invalid` or `git_commit_invalid`. Changing a branch or source
advances the application revision and leaves that environment unresolved until
it deploys or is promoted into.

Deleting an application requests deletion of each environment and preview and
returns 202 with `DeletedApplication` (`application_id`, `generation`, and one
`AcceptedOperation` per environment and preview). Previews need no confirmation. With several environments, `environments`
must list every environment name (409 `environment_confirmation_required`
otherwise, naming them in `details.environments`); force never skips this. The
application disappears with its last environment; an application without
environments is removed immediately. Deleting one environment keeps the
application and its other environments.

Operations have kind `apply`, `refresh` (the stored kind used by Deploy), or `delete`
and state `requested`,
`running`, `succeeded`, `failed`, `cancelled`, or `superseded`. New intent marks
pending/running older operations `superseded`, separately from cancellation.
The CLI treats supersession as success with an explicit outcome and stops waiting
immediately, without following the replacement. Each execution increments
`attempt`. Deployment attempt outcomes remain available even after event pruning.
Deletion retains volumes and completes only after runtime absence is verified.
It then removes the environment, its operations, deployments, attempts, builds,
and receipts. Its events stay in the application's history until the application
is deleted. Clients waiting for deletion poll environment (or application)
absence; its operation endpoint also returns 404 after cleanup.

Preview returns 200 with a `PlanView`, no durable changes, and no image pulls.
The response includes the inspected generation (zero for an absent name), an
`identical` flag, latest operation, redacted manifest field changes, a runtime
plan, each service's effective rollout order (`derived` or `explicit`) and
monitor window, and `variables`: the value of every reference in scope for the
selected environment. The manifest is rendered for that environment; a missing
value fails with 422. A `[spec.environments.<name>]` block naming no
environment adds a non-blocking `environment_block_unknown` warning. A `start-first` order set on a service with a writable volume adds
a non-blocking `rollout_start_first_writable_volume` warning to the plan. Manifest
differences and the runtime plan compare with the environment selected by
`environment=ID` (by default the application's only environment): its last
deployment snapshot, not saved configuration, and its observed runtime. Without a
selection and with several environments, they compare the saved manifests as
written and the runtime plan is empty. A new application renders for
`production`. An environment of another
application returns 404. Image tags report resolution requirements even when unchanged,
matching Deploy's refresh behavior. Environment,
command, argument, and health-check values are redacted in previews, including
runtime actions. Execution computes its own unredacted plan after preparation.
Previews cannot freeze mutable tags or runtime state.

Events accept optional `environment_id`, `cursor`, and `limit` (1–100, default 50).
They survive ordinary operation pruning but are removed with their environment.
Application-level facts (edits, renames) are recorded in every environment's history.
Event retention is independently configured by `retention.event_days` (default
90; zero disables pruning). Events contain safe diagnostics and identifiers,
never manifests, environment values, or raw Docker errors. Failure events preserve
`error_code`, `phase`, and `resource`; operation reads expose the current phase and
resource. Significant resource mutations and active-target repairs are recorded,
while unchanged observations and timer ticks are omitted.

The API requires a session cookie or bearer credential on every transport.
Only setup/login endpoints and TCP `/health` are public; see
[authentication](authentication.md). What each caller may do is decided by its
[grants](authorization.md); every operation names its requirement in the
contract's `x-piqueld-access` extension. TCP requests also validate Host and browser
Fetch Metadata headers; DNS names need `server.allowed_hosts` (see
[configuration](configuration.md)). Rejected requests return
`403 browser_access_denied`. Authentication checks API mutation origins against
`auth.public_url`, returning `403 origin_mismatch` for rejected origins.
Invalid URL path parameters return `400 path_invalid`, with the usual JSON error
and request ID. The dashboard
is served at `/dashboard/`; `/health` is an unversioned TCP liveness endpoint.
The Unix socket serves the API alone. See [the CLI guide](piquelctl.md) and
[the generated contract](openapi-v1.json).

`POST /api/v1/environments/{id}/deploy` prepares all sources again, including
Git builds. It requires the inspected application generation unless forced and
supersedes pending work in that environment. An identical idempotency-key replay
returns the original acceptance. Use `piquelctl env deploy NAME [ENV] --yes` (or
`app deploy NAME --yes` for an application with one environment) to request and
wait for deployment.

When `spec.manifest` is configured, each environment has `source: branch` and
Deploy first fetches the manifest from that environment's branch (or the
one-off `branch`/`commit`). Once the deployment is prepared, it becomes that
environment's last fetched manifest and the application's saved configuration;
other environments keep their own. Its `refresh` operation records
`fetching_manifest` progress and a `manifest_fetched` event with the commit
hash. Fetches never change the generation.
Failures use `manifest_not_found`, `manifest_fetch_failed`, or `manifest_invalid`.
Direct apply may repair manifest connection settings but rejects changes to
repository-managed runtime fields with `409 repository_managed`.
Reconcile retries the latest operation with its saved inputs; deploy refreshes
sources and fetches repository-backed configuration when configured.

`GET /api/v1/applications/{id}/manifest` downloads saved configuration as `application/toml`, with an attachment filename and `Cache-Control: no-store`. It does not observe Docker or resolve sources.

`GET /api/v1/environments/{id}/logs` reads Docker container output for services
owned by this environment and daemon instance. Optional `service` filters by
logical service name; optional `stream=stdout|stderr` filters before limiting
results (omit it for both, including merged terminal output). `tail` defaults to 200 (1–1000) and `since_seconds` to 3600
(1–86400). Records include timestamp, service, task ID, stream and message.
Snapshots are capped at 1 MiB of collected text and 256 tasks, with `truncated`
indicating a partial result. Docker retains the source logs; removed containers
have no available history. No output is stored by piqueld.

`GET /api/v1/environments/{id}/exec` opens a WebSocket that runs a one-off
command in a running task of a service (preferring healthy tasks over those
still starting) owned by this environment and daemon instance. Requests that
are not WebSocket handshakes return `426 upgrade_required`. The caller needs
[`apps:exec`](authorization.md) on the environment's application; cookie-authenticated
handshakes must send the configured `Origin`, like mutations.

The client's first message is a JSON text message such as `{ "service": "auth",
"command": ["auth-service", "invite", "create"], "stdin": false, "tty": null }`;
`tty` takes an initial `{ "width", "height" }` size and reports terminal output
as stdout. Every later message is binary: a one-byte tag, then the payload, at
most 1 MiB per message. Clients send stdin bytes (1), stdin end (2; ignored with
a terminal, where disconnecting detaches) and terminal resizes (3, `u16` width
then height). Closing the connection, rather than sending stdin end, stops the
session. The daemon only sees the close after reading the input sent before it,
so a client that disconnects with input still queued behind a command that
stopped reading is noticed when the command reads or exits.

The daemon sends stdout (1), stderr (2), and finally either the exit code (3,
big-endian `i64`) or a failure (4): the `u16` HTTP status an equivalent request
would have returned, then the `ErrorBody` JSON. Failures to start the command,
such as an invalid request, `404 not_found` or `409 service_not_running`, arrive
this way too, so browsers can read them. `piqueld_core::exec` implements the
messages. Progenitor's generated WebSocket methods do not build for WASM, so
`piqueld-client` implements this method by hand for native targets. History
records `command_started` and `command_finished` events, never the command.

`GET /api/v1/system/readiness` reports top-level `ready` plus separate `database`,
`docker`, and `swarm` verdicts. Each verdict has `status: "ready"` or
`status: "failed"`; failed verdicts include a required safe `message`, while ready
verdicts omit it. HTTP 200 means deployment dependencies are available; HTTP 503
carries the same structured envelope when they are not. Probes have bounded
deadlines and do not initialize Swarm or repair resources. Docker being unavailable
does not block configuration saves or history reads. `/health` remains a
process-liveness endpoint. A separate `ingress` object reports gateway health and
per-route HTTPS readiness, for applications the caller can read, without
affecting `ready`; see
[ingress](ingress.md#status-and-recovery). Each route status carries the
route's `name`, when it has one. No registry checks are introduced.

### URL readiness

`EnvironmentDetailView.urls` lists a `RouteUrl` for every route the current
runtime target of an environment or preview renders: its `name`, `url`
(`https://` and the hostname), `visibility`, destination, `state` and
`pending`. The daemon derives the state from what it already observes and
never probes the URL for a client. A URL is `ready` exactly when nothing in
`pending` holds:

1. `ingress`: the gateway acknowledged this exact route (hostname,
   visibility and destination);
2. `https` (with a `message`): the listener serving the route's visibility is
   healthy now (the gateway and, in tunnel mode, the tunnel for public routes;
   the apps node for private ones), and the daemon's latest check of this route
   found it `ready`, i.e. DNS answers that listener, which serves the hostname
   with a trusted certificate (a DNS-01 certificate for private routes). A
   check made before the route changed does not count;
3. `dns` (with its `state`): the hostname's managed records are not `pending` or
   `dns_conflict` (`manual` and `managed` records do not keep a URL pending);
4. `service` (with the `service`): the route's service is observed
   `converged` with every desired replica healthy. Redirects have no service.

Otherwise the URL is `pending`, with every condition that holds, in that
order. Daemons older than URL readiness omit `urls`; clients must not read
that as "no routes". `piquelctl env url`, `preview url` and `wait --ready routes`, and the
dashboard, read this field.

`GET /api/v1/builds` lists attempts newest first, with optional `application_id`,
`environment_id`,
`cursor`, and `limit` (1–100, default 50). Each executed Git-service preparation
creates an independent record before checkout. Image pulls do not create records.
Outcomes are running, succeeded, failed, or interrupted. Resolved commits and
image IDs are recorded when available; retries create new attempts. Job runs
appear in the same history with `job` set, the service whose container they
reuse, the image that ran, and `exit_code` once the container exited. Notes
from piqueld, such as Docker's explanation of a failed task or unavailable
output, are appended to a job's output as stderr lines prefixed `piqueld:`.

`GET /api/v1/builds/{id}/logs` returns the newest output in chronological order,
with `previous_offset` as an exclusive `before` cursor to load older chunks.
Optional `stream=stdout|stderr` filters in the daemon before paging. Each of the
up to 16 chunks includes its byte offset, capture timestamp in milliseconds,
stream, and text. Stream and capture time are required for every chunk. Pages
contain at most 64 KiB of output, decoded as lossy UTF-8; offsets count original
bytes. The old forward `offset` query and combined `text` response are removed.
Migration expires previously captured unstructured output while retaining build
metadata. Truncation and expiration are explicit. Output retains the configured
prefix, defaults to 4 MiB per attempt and expires 30 days after completion.
Metadata survives operation pruning and is deleted with its environment.

See [observability](observability.md) for diagnostic history, daemon statistics, analytics, metrics
and webhook delivery configuration.

Manually set secrets live in one store per application; generated secrets
belong to their environment. Secret endpoints expose metadata only:

- `GET /api/v1/applications/{id}/secrets` lists the store's secrets: names,
  current generations, update times, `deleting` and `unavailable` status, and
  `access`. Access is `{ "environments": "all" | { "only": [environment IDs] },
  "previews": bool }`. `all` covers environments created later; IDs keep a renamed
  environment's access, and deleting an environment removes it from every list.
  Previews may mount the secret only when `previews` is true; `all` never covers
  them, and lists accept only environment IDs.
- `PUT /api/v1/applications/{id}/secrets/{name}` accepts an `application/octet-stream`
  value of 1–512000 bytes. `X-Expected-Generation: 0` creates; a current generation
  replaces. The `environments` query parameter (`all`, or comma-separated
  environment IDs; empty for none) replaces the access list, together with
  `previews` (default `false`). `previews` alone returns 400 `query_invalid`, so
  it never resets the environments. Without them a new secret allows every
  environment and no previews, and an existing one keeps its list. Names the saved
  manifest or an environment's last fetched one declares in `spec.secrets` return
  422 `manifest_validation_failed` with `secret_name_conflict` in `details.errors`. Each store
  supports at most 100 logical secrets, 1,000 retained values and 100 MiB of
  ciphertext. Discarded unavailable versions do not consume this value quota.
  Exceeding the retained-version or byte quota returns 409 `secret_quota_exceeded`;
  delete unused secrets to free space.
- `PUT /api/v1/applications/{id}/secrets/{name}/access` replaces the access list
  with a JSON `SecretAccess` body. Environments must belong to the application.
  Narrowing it affects later deployments only.
- `GET /api/v1/environments/{id}/secrets` lists an environment's generated secrets
  with the same metadata, without `access`.
  `POST /api/v1/environments/{id}/secrets/{name}/regenerate` with the current
  `X-Expected-Generation` generates a new version from the saved manifest's
  declaration (404 when it declares none), to rotate a value or replace one
  discarded by key recovery; running deployments keep their pinned versions.
  `DELETE` at `/api/v1/environments/{id}/secrets/{name}` discards a generated
  value, so a later deployment generates a new one.
- `DELETE` on either secret path requires `X-Expected-Generation` and refuses
  references in saved configuration, the current runnable deployment, or the
  active target of any environment that uses the secret.
  The captured deployment manifest protects references even before version pinning.
  Deletion reserves the secret, releases the database writer lock, then removes
  Docker versions before encrypted records. Partial failure or restart leaves
  `deleting: true`; retry DELETE to finish. Replacement and new configuration
  references return 409 `secret_deleting` until cleanup completes. A version conflict
  returns 409; missing or invalid key material returns 503 `secret_storage_unavailable`
  with a `details.diagnostic_id` for value-dependent work.

Both lists are unpaginated arrays returned directly in `data`, without `items` or
`next_cursor`.

A service secret mount names a generated secret when `spec.secrets` declares the
name, and a stored secret otherwise; mount names may reference variables and are
resolved after rendering. A deployment checks access when it captures its inputs,
before rollout: an environment that mounts a stored secret its access list excludes
fails with 409 `secret_access_denied` and `details` `{ "environment": name,
"secret": name }`. `POST /api/v1/applications/plan?environment=ID` returns the same error, so
it surfaces before a deploy. A name both declared and stored fails with
422 `manifest_validation_failed` with `secret_name_conflict` in `details.errors`.

Values never appear in responses, manifests or deployment snapshots. Deployments
pin immutable versions during effective-input preparation; retries preserve those
pins. Earlier ciphertext versions remain until logical-secret, environment or
application deletion. Each environment that mounts a stored version gets its own
Docker secret for it.

Quota enforcement never evicts pinned versions. To retire a secret, save and deploy
configuration without its references, then delete it. An existing database above
the quota remains readable and deployable; new writes require freeing space.

`POST /api/v1/system/secrets/recover-key` recovers from a lost or unusable master
key by discarding stored and generated values for **all applications and
environments**. It returns 409
`secret_key_usable` while the current key still works. Discarded versions are
marked unavailable; metadata, running Docker services and their secrets are
preserved. Supplying replacement stored values creates new versions, and an
explicit new deployment is required to adopt them; that deployment also generates
new values for discarded generated secrets it mounts. Deployments that need
discarded stored values fail with `secret_unavailable` and the logical names. The response contains
`affected_environments`, `affected_applications`, `affected_secrets` and
`discarded_versions`; no key or
secret value is returned. Repeating the request after a lost response returns
`secret_key_usable`, because no stored value then needs the old key.
