# Application manifest

The supported document is a strict TOML or JSON
`piqueld.dev/v1alpha1` `Application`. Unknown fields and unsupported source
types are errors. Services explicitly select a prebuilt Docker/OCI image or a Git build source.
Decode errors name the rejected field and its line and column, and may quote a
mistyped value, e.g.
``unknown field `routes`, expected ... at line 14 column 1``. For TOML requests,
the API returns them, like validation errors, as `details.errors` entries with a
path and message; JSON request bodies report only `json_malformed`.

The manifest is shared by the application's environments (for a
repository-backed application, environments following another branch than the
one `spec.manifest` names, and previews, read it from their own branch; a
[promoted environment](#promoted-environments) deploys the manifest
of the release it receives), including services,
replica counts, limits, volumes, routes, jobs, repository settings and secret
file references. Manually set secret values live in one store per application,
shared by the environments their access lists allow; generated secret values
and the deployed snapshot belong to each environment. Values that differ between environments, such as hostnames or replica
counts, come from [variables](#variables). Which environments exist, and
where each deploys from, can be [declared](#declared-environments) too.
[Manifest settings](manifest-settings.md) lists every setting, with its type
and default.

```toml
api_version = "piqueld.dev/v1alpha1"
kind = "Application"

[metadata]
name = "notes"

[[spec.services]]
name = "web"
replicas = 1

[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1.4.0"

[spec.services.environment]
RUST_LOG = "info"

[[spec.services.mounts]]
volume = "data"
target = "/var/lib/notes"

[[spec.volumes]]
name = "data"
```

Services support replicas, environment variables, command and argument arrays,
health checks, startup dependencies, rollout settings, CPU/memory limits, mounts of declared
named volumes, read-only [files](#files), file references to application secrets, and
receiving the daemon's [telemetry](#telemetry-receivers). Named volumes are retained when an application
is deleted. Secret values are never manifest fields (manifests may only ask piqueld to
generate them), and there are no manifest
fields for directly published ports. HTTP routes, by exact or wildcard
hostname, are declared in `spec.routes`; see [Routes](#routes) and
[managed ingress](../networking/routes.md).

Rollouts start each replacement task before stopping the old one, except for
services that mount any volume without `read_only = true`. Those stop the old
task first so single-writer stores such as PostgreSQL or SQLite never share
their data directory, and are briefly unavailable during each rollout. A
service can choose its own order; see [Rollout](#rollout).

Services of one application share a private network on which each answers to
its manifest name, so `web` reaches a `postgres` service at `postgres:5432`.
Names resolve only within that application; identically named services in other
applications are unreachable, unless their application exports them through a
[link](#links).

Names are 1–63 lowercase ASCII letters, digits, or hyphens; they start with a
letter and cannot end with a hyphen. Applications may be empty. Deploying an empty application removes its services and network, retaining volume data.
Image references reject URL schemes, credentials, malformed tags, and malformed
digests; registry hostnames are validated case-insensitively and canonicalized
to lowercase (IPv6 literal hosts are not accepted). Mount targets are normalized
absolute paths below `/`; environment names and runtime strings reject invalid
control data. Environment values and mount targets reach the container as
written once [variables](#variables) are rendered. Docker Swarm's own `{{ }}`
templates, such as `{{.Service.Name}}`, are not expanded.
`PIQUELD_INGRESS_PROXIES` is reserved; piqueld injects it into
routed services (see [managed ingress](../networking/routes.md#read-the-clients-address)).
Health-check paths are absolute, health-check timeouts cannot exceed their
interval, and resource limits must specify CPU, memory, or both.

Explicit budgets bound every manifest; exceeding one is a distinct validation
error whose message names the offending environment key where applicable:

| Budget | Limit |
| --- | --- |
| Services per application | 64 |
| Replicas per service | 1–100 |
| Named volumes per application | 64 |
| Routes per application | 64 |
| Generated secrets per application | 64 |
| Secret mounts per service | 64 |
| Variables per `variables` table | 128 |
| `[spec.environments]` blocks | 64 |
| Environment entries per service | 256 |
| Environment key size | 255 bytes |
| Environment value size | 65,536 bytes |
| Command / arguments elements | 128 each |
| Command / arguments element size | 4,096 bytes |
| Mounts per service | 32 |
| Files per service | 32 |
| File size | 512,000 bytes (500 KiB, Swarm's config limit) |
| Files per application | 2 MiB in total |
| Jobs per application | 16 |
| Exports, and imports, per application | 64 each |
| Applications per export | 64 |
| Job timeout | 86,400 seconds |
| Health-check interval | 3,600 seconds |
| Health-check start period | 3,600 seconds |
| Health-check retries | 1–100 |
| Rollout monitor | 3,600 seconds |
| CPU limit | 1,048,576 millicores |

Process, health-check and resource settings look like this:

```toml
[[spec.services]]
name = "api"
command = ["api-server"]        # replaces the image's entrypoint
arguments = ["--port", "8080"]  # replaces its default arguments

[spec.services.healthcheck]
type = "http"                   # or "command", with command = ["pg_isready"]
port = 8080
path = "/health"
interval_seconds = 10
timeout_seconds = 3
start_period_seconds = 30       # failed checks do not count while it starts
retries = 3                     # consecutive failures before unhealthy

[spec.services.resources]
cpu_millis = 500                # millicores
memory_bytes = 536870912
```

Defaults are replicas `1`, empty command and arguments, writable mounts, and
health-check values of path `/health`, interval `10` seconds, timeout `3`
seconds, no start period, and `3` retries. During the start period, failed
checks do not count towards `retries`, so slow-starting services are not
restarted, while a passing check makes the container healthy at once. A start
period does not extend `reconciliation.convergence_timeout_seconds` (120 seconds
by default): a service whose first check passes later still fails its
deployment, so raise that setting for such services. HTTP
health checks need nothing in the image: piqueld
mounts its static `piqueld-probe` at `/run/piqueld/probe` as a Swarm config
and Docker runs it as the health check, for example
`/run/piqueld/probe http --port 8080 --path /health --timeout 3`. It sends one
plain HTTP GET to `127.0.0.1` inside the container, without following
redirects, and passes on a 2xx or 3xx status within the timeout, so Swarm still
gates tasks and rollouts on health, even for scratch and distroless images.
Mounts cannot target `/run/piqueld/probe`, nor `/run/piqueld/telemetry`, where
[telemetry receivers](#telemetry-receivers) get their socket. Command health checks run their
`command` unchanged, without a shell. Services, mounts,
volumes, and environment maps are
canonicalized before hashing. The specification hash is SHA-256 over a versioned
canonical JSON envelope (`piqueld-spec-hash/v2`) covering only the canonical
spec and the contents of the repository files it mounts. The application name selects which application an apply targets; changing
it targets a different application. Use the explicit rename action to retain
identity and resources, then update the manifest name.

Individual field endpoints edit the saved internal manifest atomically, validate
the result, and save without deployment unless requested. Configuration
revision checks apply to field edits as well as manifest imports, and
repository-backed applications are [edited](#editing-repository-backed-applications)
the same way.
`piquelctl app repository disconnect NAME --yes` removes repository backing while
retaining the current saved services and volumes.

The parser is pure. Apply saves the complete normalized configuration without
starting a deployment unless explicitly requested. Deploy captures that saved
revision and prepares every source again. Reconciliation and retries reuse the
captured deployment and its prepared target, so later saved edits cannot enter
an existing deployment. Deploy resolves and builds the saved configuration's sources again.
Resolved runtime state remains separate from portable manifests. Generations
advance on saves, changed names, deletion intent, and environment creation,
renames and deletion, since environment names select configuration.

## Variables

Declare variables with a default in `[spec.variables]`, for one environment in
`[spec.environments.<name>.variables]`, or for every preview in
`[spec.previews.variables]` (see [Previews](#previews)), and reference them as
`${{ vars.<name> }}`:

```toml
[spec.variables]
web_replicas = 1

[spec.environments.staging.variables]
domain = "staging.piquel.fr"

[spec.environments.production.variables]
domain = "piquel.fr"
web_replicas = 3

[[spec.routes]]
hostname = "${{ vars.domain }}"
service = "web"
port = 3000
visibility = "public"

[[spec.services]]
name = "web"
replicas = "${{ vars.web_replicas }}"
```

An environment's own value overrides the default of the same name, as
`web_replicas` does in production. An environment may also declare a variable
that has no default, like `domain`. An environment uses its own value, else the
default. Every environment renders the same manifest (or its own branch's), so each environment that
deploys a reference needs a value for it: planning or deploying an environment
without one fails with `variable_value_missing`, naming the environment, the
field and the variable. Here staging and production each set `domain`, so they
serve different hostnames; two environments that render the same hostname
still conflict.

Syntax:

- `${{ namespace.name }}`, with optional whitespace inside the braces.
  `$${{` writes a literal `${{`. `${VAR}` and `$VAR` are ordinary text, so
  shell and application expansion keep working.
- References are lookups only: no functions, operators or conditionals.
  Substituted text is never evaluated again.

Variables are strings, integers or booleans. Their names start with a letter or
`_` and use letters, digits, `_` or `-`, at most 63 bytes; string values are at
most 65,536 bytes. A string value may reference system variables, but not other
variables. System variables:

| Reference | Value |
| --- | --- |
| `app.name` | Application name. |
| `env.name` | Environment name; a preview's slug. |
| `env.slug` | DNS-safe environment identifier: the environment name, or a preview's slug. |
| `git.branch`, `git.sha` | Branch and commit the manifest was read from: the environment's branch, or a one-off `--branch`; only with `spec.manifest`. |
| `deployment.id` | ID of the deployment being captured. |

`secrets.*` is reserved for secret references.

Variable values are not secret: plans, `app show`, `env show`, deployments and
the dashboard show them, even where they render into `environment` values,
which plans otherwise redact. Mount passwords and keys as secret files
(`[[spec.services.secrets]]`) instead.

References are allowed in values: service `environment` values, `command` and
`arguments`, health check settings, `replicas`, resource limits, rollout `order`
and `monitor_seconds`, route `hostname` and redirect `to`, job `command`, import `environment`, service
secret mount `name`, and the build inputs `image`, `dockerfile`, `context`, build argument values and
`target`. They are rejected elsewhere, including names, table keys, `type`
fields, mount and secret targets, `spec.secrets` declarations, Git repository
settings and `spec.manifest`. [Files](#files) are never rendered: `${{` in a
file's content is mounted as written.

The build inputs are baked into images, so each deployment's
[release](cli.md#commands) fingerprints their rendered values. Rendering a
release for another environment, as [promotion](#promoted-environments) does,
reuses its images and requires every build
input to render the same there; a build argument such as
`VITE_ORIGIN = "https://${{ vars.domain }}"` makes the release incompatible
(`release_incompatible`) with environments whose `domain` differs. Keep
per-environment values in runtime fields, like `environment`, when the images
should be shared.

A field that is exactly one reference, like `replicas = "${{ vars.web_replicas }}"`,
takes the variable's own value, which must have the field's type: strings are
never parsed into numbers. Any other text containing a reference renders to a
string. Rendered values then pass the field's usual validation.

Validation (saving, `app validate`) rejects references to a variable declared
nowhere, unknown namespaces and system variables, and malformed references, with
the field path. Values that contain references are validated once rendered, when
an environment is planned or deployed. A `[spec.environments.<name>]` block
also [declares](#declared-environments) the environment `<name>`, so saving one
for an environment that doesn't exist yet creates it.

A deployment renders the manifest for its environment when it is captured, or,
when repository-backed, once the manifest is fetched and its commit is known. The
deployment records the manifest as captured, the value of every variable in
scope, and the rendered manifest; retries reuse them and never read variables
again. Plans render with `deployment.id` set to `preview` and, for repository
manifests, the environment's branch and pinned commit (or 40 zeros), since
nothing is fetched. `deployment.id` changes with every deployment, so services that use
it are replaced on every deployment.

Renaming an environment would change which `[spec.environments.<name>]` block
applies, so piqueld refuses to rename an environment while the manifest it
deploys (for a repository-backed environment, the one last fetched from its
branch) has a block for its current or its new name (`environment_configured`). Remove
the block, rename the environment, then add the block back under the new name.

A rename never touches running services. When the configuration renders the
changed name through `${{ env.name }}`, `${{ env.slug }}` or `${{ app.name }}`,
the environment no longer reports the saved configuration as resolved: deploy
it to apply the new name.

## Validation and editor support

`piquelctl app validate --file application.toml` applies the same parser and
validation as the daemon, without contacting one, and lists every error with
its field path, including references to undeclared variables. Validation runs against the CLI's version of `piqueld-core`;
`app plan` checks the manifest against the connected daemon's version.

[`application-manifest.schema.json`](../../application-manifest.schema.json) is a
JSON Schema (draft 7) generated from the manifest types by `just generate`, and
it ships with each set of release binaries. It describes structure, field types, and
documentation. Names, budgets, and cross-references are left to
`app validate`. Editors that use [Taplo](https://taplo.tamasfe.dev/), such as
VS Code with Even Better TOML, give completion, hover documentation, and inline
errors when a manifest starts with a schema directive:

```toml
#:schema https://raw.githubusercontent.com/piquel-fr/piqueld/main/docs/application-manifest.schema.json
api_version = "piqueld.dev/v1alpha1"
```

Replace `main` with the commit your daemon runs, or point the directive at a
local copy of the schema. To apply the schema without directives, add a
`.taplo.toml` rule:

```toml
[[rule]]
include = ["deploy/**/*.toml"]
schema.path = "https://raw.githubusercontent.com/piquel-fr/piqueld/main/docs/application-manifest.schema.json"
```

## Git build sources

A service may explicitly build from Git instead of pulling a prebuilt image:

```toml
[[spec.services]]
name = "web"
[spec.services.source]
type = "git"
[spec.services.source.repository]
url = "https://example.com/team/application.git"
branch = "main"
# commit = "0123456789012345678901234567890123456789"
[spec.services.source.build]
type = "docker"
dockerfile = "services/web/Dockerfile"
context = "."
# Optional: values passed as --build-arg, and a multi-stage target.
args = { VITE_AUTH_ORIGIN = "https://auth.example.com" }
target = "runtime"
```

The daemon requires Git and the Docker CLI with the Buildx plugin in its PATH. Git inherits the host's
credentials; piqueld does not store credentials or prompt for them. Only trusted
repositories are supported: Dockerfiles execute build instructions on the host's
Docker Engine. Builds are serialized across applications, and the existing
`reconciliation.prepare_timeout_seconds` bounds preparation (default: 300 seconds).
Increase that budget for longer builds.

Each preparation gets an isolated checkout. A full configured commit hash is
used directly; otherwise the branch head is resolved once. Dockerfile and context
paths are relative to the repository root, must stay within it, and default build
context is `.`. Build argument names follow the environment variable rules, and
because they are passed on the `docker buildx build` command line, build arguments may
total at most 256 KiB. `target` must name a stage in the Dockerfile; without it Docker builds the final
stage. Build arguments are not secret: Docker records them in image metadata and
piqueld shows them in manifests and build history. Use service secrets for
sensitive values. There is no automatic build backend detection, submodule or LFS
setup, or registry publishing. Built images are labelled with the installation's
identity, and [image cleanup](../../dev/architecture/docker-reconciliation.md#image-retention) removes
the ones no environment runs, deploys, or keeps so their releases can be
promoted again. Docker's build cache is
reused and base images are refreshed with `--pull`. Builds record no provenance
attestations, which differ on every build, so a rebuild Docker serves from its
cache keeps the same image ID and shares its release.

The resolved source records the full Git commit and content-addressed local image
ID. Every repository and build field, including build arguments and the target, is
part of that source's identity, so changing any of them triggers a rebuild. All
service sources are prepared before any application rollout. A failed checkout or
build preserves the existing running deployment. Normal reconciliation
reuses prepared images; explicit deploy resolves and builds sources again.
Local images are supported only on the existing single-node Swarm topology.

A repository-backed application can build from its own manifest repository:

```toml
[spec.services.source]
type = "git"
repository = "self"
[spec.services.source.build]
type = "docker"
dockerfile = "infra/auth.Dockerfile"
```

`self` uses the repository the manifest was fetched from, at that exact commit,
so every such service in a deployment builds from one revision. Disconnecting
repository backing replaces `self` with the former manifest repository and
branch.

Git checkout permits file, Git, HTTP(S), and SSH transports. Executable remote
helpers such as `ext::` are disabled, including through host URL rewrites.

## Declared environments

Every `[spec.environments.<name>]` block declares the environment `<name>` and
where it deploys from. A manifest that declares environments is the source of
truth for which environments the application has and how each one is sourced:

```toml
[spec.environments.staging]
sync = true                  # pushes to its branch deploy it

[spec.environments.production]
promote_from = "staging"     # only receives releases promoted from staging
```

| Field | Meaning |
| --- | --- |
| `branch` | Branch of the manifest repository it follows; by default the one `spec.manifest` names. |
| `commit` | Full commit it deploys instead of its branch's head; without `branch`, by default the one `spec.manifest` pins. |
| `sync` | Whether pushes to its branch [deploy it](#deploying-on-push) while the repository syncs; `false` by default. |
| `promote_from` | Environment it receives [promoted releases](#promoted-environments) from, instead of building. |
| `visibility` | The strictest visibility its [routes](#routes) get; `public` by default. |
| `variables` | Its [variable](#variables) values, overriding the defaults. |

`branch`, `commit` and `sync = true` need a [manifest repository](#repository-backed-manifests)
(`manifest_repository_required`); without one, an environment deploys the saved manifest. `promote_from` excludes
the other three (`environment_source_conflict`), `sync` excludes its own
`commit` (`environment_sync_pinned`; one pinned by `spec.manifest` stays opted
in but never follows pushes), and `promote_from` must name another
declared environment without leading back to itself (`promotion_source_invalid`,
`promotion_cycle`). Validation (`app validate`, `app plan`, saving, fetching)
reports each at its field, e.g. `spec.environments.production.promote_from`.

Saving the manifest (`app apply`, the dashboard's edits), or deploying it from
the branch `spec.manifest` names, converges the application's environments to
the ones it declares:

- A declared environment that doesn't exist is created, not deployed. A new
  application gets exactly the environments it declares; one that declares
  none gets a `production` environment, as before.
- An existing one whose block sets its source (`branch`, `commit`,
  `promote_from` or `sync`), or whose block in the previous saved manifest
  did, changes to it when it differs, deploying nothing, as
  `piquelctl env source`, `env branch` and `env sync` would; a block that
  removes them returns it to the default. A block that sets none, such as one
  only setting variables or visibility (all blocks did before environments
  were declared), keeps an existing environment's source and sync as they are. An environment
  that becomes promoted while a deployment of it is in progress keeps its
  source until the next save or fetch, unless that deployment is the fetch
  itself.
- An environment the manifest no longer declares is never deleted, since it
  has volumes and secrets: it keeps running, and `app show`, `app plan` and
  the dashboard report it until you delete it with `piquelctl env delete`.

Deployments on push of that branch count too. Manifests fetched from other
branches, one-off `--branch`/`--commit` deployments and previews never create or
change environments. Disconnecting the repository drops every declaration's
`branch`, `commit` and `sync`, returning declared environments to the saved
manifest.

`app plan` lists what saving a manifest would change, e.g.

```text
Environments:
  + staging: created, deploys from branch main, follows pushes
  ~ production: deploys from releases promoted from staging instead of branch main
  ! qa: no longer declared; kept until deleted with `piquelctl env delete`
```

and `app show` and the application's Environments tab list how the
environments differ from the saved manifest. `app apply --deploy` deploys
every declared environment that builds; promoted ones wait for their first
`env promote`. So for an application such as
[`examples/staging-production.toml`](https://github.com/piquel-fr/piqueld/blob/main/examples/staging-production.toml),
applying it with `--deploy` is enough: staging follows `main`, and production
receives what staging ran once you promote it.

A declared environment's existence belongs to its block, so `piquelctl env
create`, `env rename` (to or from a declared name) and `env delete` refuse it
with `environment_declared`: remove the block to delete the environment, or
rename the block to rename it. When the block sets its source, `env source`,
`env branch` and `env sync` refuse it too, and the dashboard shows them
read-only. Other environments, including every environment of an application
whose manifest declares none, keep these commands. Field edits configure
existing environments only: one that would add a block for a name no
environment has, such as `piquelctl app variable set --env` with a typo, fails
with `environment_unknown`, so new environments are declared with `app apply`.
A field edit that adds the first block, such as `piquelctl env visibility` or setting a variable in the
dashboard, also adds an empty block for every other existing environment, so
editing configuration never leaves environments undeclared.

## Repository-backed manifests

Manifest retrieval is independent from service source selection. Add this to an
application to read its next configuration from Git when Deploy is requested:

```toml
[spec.manifest]
path = "infra/piqueld/application.toml"
[spec.manifest.repository]
url = "https://example.com/team/infrastructure.git"
branch = "main"
# commit = "0123456789012345678901234567890123456789"
```

Create the application manually with `piquelctl app apply --file bootstrap.toml`.
A bootstrap manifest may contain only its header, metadata, and `spec.manifest`;
services can be supplied by the first fetched manifest. Then click **Deploy** in
the dashboard or run `piquelctl app deploy NAME --yes` (`env deploy NAME ENV` when
the application has several environments).

The repository URL and manifest path belong to the application: every
environment reads the same file from the same repository. The branch belongs to
each environment. [Declared environments](#declared-environments) follow their
block's `branch` and `commit`, by default the branch and pinned commit
`spec.manifest` names. Otherwise, the `production` environment created with
the application, and environments created later without `--branch`, follow
that branch; other environments follow their own:

```console
piquelctl env create notes staging --branch main
piquelctl env branch notes staging release
piquelctl env branch notes production main --commit 0123456789012345678901234567890123456789
```

Changing a branch redeploys nothing: the environment's next deployment fetches
it. Connecting a repository to an application points every environment at the
branch `spec.manifest` names; disconnecting returns every environment to the
saved manifest. Both leave [promoted environments](#promoted-environments)
promoted. Environments report their source as `saved`, as the branch they
follow, or as `promoted from <name>`.

Deploying an environment resolves its pinned commit (or branch head), reads only
the exact configured TOML/JSON file, and checks that its name matches the
existing application. It renders the file with that environment's variables and
records the environment, commit, manifest, and rendered manifest in the
deployment. Empty manifests remove services and networks while retaining volume
data. Other files in the repository are ignored. A missing file fails with
`manifest_not_found`; no application is deleted. Invalid files and build
failures preserve both accepted configuration and the existing running target.

Each environment keeps the manifest last fetched from its branch. Its hostname
reservations, the rename check, `app plan --env`, `env show`, and its dashboard
page read that manifest, so a fetch for one environment never changes what
another deploys or reserves. An environment that was never deployed has fetched
nothing and reserves no hostnames yet. The application's saved manifest is the
one last fetched from the branch and commit `spec.manifest` names, by any of
its environments, keeping the application's own connection, unless it was
[edited](#editing-repository-backed-applications) since: the application page
and `app show` display it, environments deploy it after disconnecting, and it
[declares the environments](#declared-environments). Fetches from other
branches, and one-off revisions, change only their environment's own manifest.

A fetched file's own `spec.manifest` is ignored: files on different branches
often differ, and one branch must not redirect every environment. It may be
omitted; when present it must still decode like any manifest's. When it names
another repository URL or manifest path than the application's connection, the
deployment records a `manifest_connection_ignored` warning, shown by
`app deploy`/`env deploy` and on the dashboard's deployment snapshot. Change the
connection with `piquelctl app repository url` or `path`. The fetched manifest
and its commit are persisted for restart/retry; a new Deploy fetches again. A
Git service source with an explicit repository resolves its own revision
independently; `repository = "self"` builds the fetched manifest commit. Image
sources are explicitly refreshed, even when the fetched manifest is unchanged.

Manifest connection settings do not change the runtime spec hash. Source
builds, deployment, and rollback retain the behavior described above. Rename is
rejected with `repository_managed`: the name must match the fetched manifests',
so rename the application while disconnected, then in the repository.

`piquelctl env deploy NAME ENV --branch feature` (or `--commit SHA`, also on
`app deploy`) fetches the manifest from another revision for one deployment,
without changing the environment's branch. `self` sources build that revision
too. Subsequent deploys fetch the environment's own branch again.

### Editing repository-backed applications

A repository-backed application is edited like any other, with field edits
(`piquelctl app service`, `app variable`, …, or the dashboard) or `app apply`,
which change its saved manifest. Pushing the manifest changes it too.

Each application keeps the manifest it last read from the branch and commit
`spec.manifest` names. A deployment of that branch (or of its pinned commit)
fetches the manifest file again; while the file is the one last read, the push
left the manifest unchanged, and it deploys the saved manifest, edits included,
building `repository = "self"` services and reading repository files at the
fetched commit. When the file differs, it deploys the file, which also becomes
the saved manifest once the deployment is prepared, as `app apply` would save
it, unless another deployment read a manifest since: a push that changes the
manifest replaces the edits. Environments following
other branches, one-off revisions and previews always deploy the manifest from
their own branch.

While the saved manifest differs from the one last read, `app show` and the
application's settings tabs list the edited fields, from the repository's
value to the edited one. **Reset to repository** on the application page, or
`piquelctl app repository reset NAME`, discards them, saving the manifest last
read again; deploying stays explicit, or pass `--deploy`. Disconnecting keeps
the saved manifest, edits included.

### Deploying on push

`sync` in the connection deploys pushes:

```toml
[spec.manifest]
path = "infra/piqueld/application.toml"
sync = { mode = "poll", interval_seconds = 300 }  # or { mode = "webhook" }
[spec.manifest.repository]
url = "https://example.com/team/infrastructure.git"
branch = "main"
```

It is part of the application's connection, set with
`piquelctl app repository connect --sync` or `app repository sync`, or in the
dashboard's repository settings. Like the rest of `spec.manifest`, a fetched
file's `sync` is ignored. Sync is `off` by default.

While it is on, every preview, and every environment that opted in, with
`sync = true` in its [declaration](#declared-environments) or, when undeclared,
`piquelctl env sync APP ENV on` (or on its dashboard page), and follows an
unpinned branch, redeploys when its branch moves:

- **`poll`** lists the repository's branch heads every `interval_seconds`
  (60–86400, 300 by default) with one `git ls-remote`, however many
  applications, environments, and previews read it.
- **`webhook`** lists them when GitHub reports a push, and once after the
  daemon starts, since pushes reported while it was down may not have
  deployed. See [webhooks](../networking/webhooks.md) for exposing the
  endpoint and `piquelctl app repository webhook` for its URL and secret. A
  delivery only says that something moved: piqueld lists the branches itself
  and never deploys a commit a payload names. A missed delivery waits for
  the next push, or a deployment by hand.

Sync follows from a deployment: each deployment of an environment or preview
records the head of the branch it tracks at that time (the commit it fetched,
or, for a one-off revision, the head listed then), and sync deploys the head it
lists, exactly, once the branch moves past that. So:

- Nothing follows before its first deployment. Changing an environment's
  branch, or the application's repository URL, also waits for the next
  deployment.
- Each head deploys once, and a failed deployment is not retried until the
  branch moves again.
- A one-off `--branch` or `--commit` deployment stays until the branch moves.
  A rollback is a move, and deploys the earlier commit.
- A push while its first deployment runs waits for it, then deploys if the
  branch moved past what it fetched.
- A gone branch deploys nothing. Sync never creates or deletes previews.

Pushes while a deployment of the same environment or preview runs coalesce:
sync waits for it to finish, then deploys the head of then, once. A
deployment that is failing is superseded right away instead, since the push may
fix it. Sync deployments go through the same build queue as any other.

Pinned environments never sync. Opting an environment in, or turning sync on,
deploys what moved since the last deployments at the next listing, so both
need `apps:deploy` as well as `apps:write`; so does saving a manifest that
declares an environment with `sync = true` while the repository syncs. Nothing else does: changes to an
application's connection, branches, or environments take effect with each
one's next deployment.

A repository is listed once for every application reading it, at the shortest
of their intervals. One that cannot be listed deploys nothing and is retried
with backoff, from one minute up to an hour, as is one whose deployments could
not be submitted. `app show` and the application page show the
last check and its error; `env show`, `preview list`, and their dashboard pages
show whether each one follows pushes and the branch head of its last
deployment. Sync deployments are attributed to `sync:poll` or `sync:webhook`
in events, which also record the commit in a `branch_synced` event.

Services, and [jobs](#jobs), mount secrets as files:

```toml
[[spec.services.secrets]]
name = "database-password"
target = "/run/secrets/database-password"
```

References contain names and paths, never values. Edit a service's mounts
with `piquelctl app service secret` or the dashboard. A service supports up to 64
secret mounts, with unique normalized paths under `/run/secrets`. A name declared
in `spec.secrets` is generated for each environment (see below). Any other name
comes from the application's secret store, whose values are set with
`piquelctl app secret` or the dashboard and must exist when a deployment captures
its inputs: otherwise the deployment fails with `secret_missing`, naming each
secret without a value. A name can't be both declared and stored: setting a stored value for
a declared name, or saving a declaration for a stored name, fails validation
with `secret_name_conflict`.

Each stored secret lists who may mount it: every environment, including ones
created later (the default), or only the listed ones, plus a flag for previews.
Previews may mount it only with that flag, which is off by default: "every
environment" never includes previews. Lists hold environment IDs, so a renamed
environment keeps its access and a deleted one drops out of every list. When a
deployment captures its inputs, or `app plan --env` previews them, an environment
that mounts a stored secret its list excludes fails with `secret_access_denied`,
naming the environment and the secret. Narrowing a list doesn't affect running
deployments, which keep their pinned versions.

Mount names accept variables, so each environment can mount its own stored
secret:

```toml
[[spec.services.secrets]]
name = "${{ vars.stripe_key }}"
target = "/run/secrets/stripe"

[spec.environments.production.variables]
stripe_key = "stripe-live"

[spec.environments.staging.variables]
stripe_key = "stripe-test"
```

Whether the name is generated or stored is decided on the rendered name.

An application can declare secrets whose values piqueld generates, so a new
application deploys in one `apply --deploy` step:

```toml
[[spec.secrets]]
name = "database-password"
generate = { type = "random", bytes = 32, encoding = "hex" }

[[spec.secrets]]
name = "signing-key"
generate = { type = "rsa", bits = 3072 }
```

Random secrets use 16–512 bytes from the operating system, encoded as lowercase
`hex` (the default) or unpadded `base64url`. RSA secrets are 2048-, 3072-, or
4096-bit private keys in PKCS#8 PEM.

A value is generated when a deployment prepares its inputs, a service or job
mounts the secret, and it has no stored value. Declarations that no service or
job mounts are not generated, so they never count against secret quotas. A value is never
changed afterwards: later applies, deploys, and edits to the declaration keep
it, so rotation stays explicit: `piquelctl env secret APP --env ENV regenerate NAME`
generates a new version for the next deployment. A value discarded by key
recovery has nothing left to keep, so the next deployment that mounts it
generates a new one. Removing a declaration retains the generated value.
`app plan` lists a changed declaration as `secrets.<name>`, never a
value.

Besides applying a manifest, declarations can be edited with
`piquelctl app generated-secret`, the **Generated secrets** card of the dashboard's
Secrets tab, or `PUT /api/v1/applications/{id}/generated-secrets`.

## Promoted environments

A promoted environment never builds and never fetches. It only receives
releases promoted from another environment of the same application, so it runs
exactly the images that environment ran: the same commit, base images and build
cache.

```toml
[spec.environments.production]
promote_from = "staging"
```

```console
piquelctl env promote notes production --plan
piquelctl env promote notes production [--deployment ID | --release ID]
```

[Declare](#declared-environments) the source with `promote_from`, or, for an
environment the manifest doesn't declare, use
`piquelctl env create notes production --promote-from staging` or
`piquelctl env source notes production --promote-from staging` to convert an
existing one.

**Sources.** The source is stored by ID, so renaming either environment keeps
it. Any environment of the application can be a source, including a promoted
one, so chains such as staging → canary → production work. Previews,
environments of other applications and environments being deleted cannot
(`promotion_source_invalid`), and an environment cannot promote from itself,
directly or through others (`promotion_cycle`). Deleting an environment that
others promote from is refused with `promotion_source_in_use`, naming them;
deleting the application deletes them all.

Converting is always explicit: existing and migrated environments never become
promoted on their own, only by a declaration or `env source`, and connecting or
disconnecting the manifest repository leaves promoted environments promoted.
Removing `promote_from`, or `env source --tracking`, returns one to the saved
manifest, or to the branch `spec.manifest` names; `branch`, or `env branch`,
makes it follow another branch. Changing the source deploys nothing. Deploying a
promoted environment (`env deploy`, `app deploy`) fails with
`environment_promoted`. An environment can't become promoted while a deployment
of it is requested or running (`application_busy`), since that deployment may be
fetching or building; retrying an earlier one fails with
`environment_promoted`, even when it had already built, since a promoted
environment only runs releases. The deployment it already runs is still
repaired when it drifts.

**What gets promoted.** `env promote` deploys the release of the source
environment's current deployment. `--deployment ID` names the deployment you
reviewed: the promotion is refused with `promotion_source_changed` once it is no
longer the source's current one, so what you reviewed is what is deployed.
`--release ID` deploys an earlier release of the application, rendered with the
environment's current secrets, without checking any source; `env restore`
deploys a previous deployment again exactly, with the secret versions it pinned. The release is pinned when the
promotion is accepted, so later deployments of the source cannot retarget it.
Promotion never happens automatically when the source deploys.

**Preconditions.** All of them are checked before anything is captured:

- the source's current deployment succeeded and every service of it runs as
  deployed (same image, replicas, and configuration) and healthy right now
  (`promotion_source_not_ready`);
- the release instantiates for the environment: its build inputs render as
  they did when it was built (`release_incompatible`, see
  [Variables](#variables));
- its images are present, or are registry images pulled again by digest
  (`image_unavailable` otherwise); [image cleanup](../../dev/architecture/docker-reconciliation.md#image-retention)
  is held off until the promotion is saved;
- every secret the rendered release mounts is usable: stored secrets exist,
  their access list allows the environment, and their value wasn't discarded by
  key recovery, and no mounted secret is being deleted (`secrets_unavailable`,
  listing every one at once). Declared secrets the environment lacks are
  generated for it and stored only if the promotion is accepted.

Acceptance checks the source again, so a source that moved on in the meantime
fails with `promotion_source_changed`.

**Rendering.** The release's own manifest is rendered with the target's
`[spec.environments.<target>]` block and variables from that same manifest, so
every per-environment setting is reviewed in Git and travels with the release.
Only secrets, volumes and the environment's other runtime state come from the
target. The release's images are deployed as recorded, never rebuilt, so
per-environment values must stay out of build inputs. A promoted environment's
manifest, as shown by `env show`, is the one of the release it last received.

**Plans.** `env promote --plan` shows what a promotion would do without
changing anything: the release, its provenance and whether its images are
present, where it comes from, the rendered changes against the environment's
current deployment (including routes), the runtime plan, volumes it would create
empty, and every missing or inaccessible secret. Plans of an earlier release
(`--release`) work for any environment, promoted or not.

**History.** Each deployment records its origin: `build`, `promotion` (with the
source environment and deployment), or `release` (an earlier release deployed by
ID). Each release lists the deployments that received it.

## Previews

A preview is a disposable deployment of one branch of the application's
manifest repository, for testing a branch by hand or from an agent. It is an
environment of its own kind: it has its own deployments, generated secrets,
volumes, routes, logs and Docker network, but it never appears among the
application's environments, and environment commands never act on it.

```console
piquelctl preview create notes --branch feat/login [--slot agent-2]
piquelctl preview deploy notes feat/login      # redeploy the branch head
piquelctl preview deploy notes --local         # deploy the current worktree, unpushed
piquelctl preview list notes
piquelctl preview delete notes feat/login
piquelctl preview prune notes --branch-gone
```

Only repository-backed applications have previews; creating one for another
application fails with `preview_requires_repository`. A preview deploys the
manifest file from its own branch, rendered with `[spec.previews]` and never
with an `[spec.environments.<name>]` block:

```toml
[spec.previews]
# The strictest visibility preview routes get; private by default.
visibility = "private"

[spec.previews.variables]
domain = "${{ env.slug }}.dev.example.com"
web_replicas = 1
```

Each value in `[spec.previews.variables]` overrides the `[spec.variables]`
default of the same name, or declares a variable without one, for every
preview. `git.branch` and `git.sha` are the branch and commit the preview
fetched.

**Identity.** A preview is keyed by its branch and optional slot, so several
agents can work on one branch. Creating a preview again with the same branch
and slot returns the existing preview and its latest deployment without
redeploying it; `preview deploy` redeploys. Creation, deployment and deletion
need no application revision, so agents creating previews never conflict with
each other or with people saving the manifest.

**Slugs.** Each preview gets a DNS-safe slug derived once from
`<application>-<branch>[-<slot>]`: lowercase letters and digits, with every
other run of characters as one hyphen, truncated, then a hyphen and a short
hash of the exact application, branch and slot (`notes-feat-login-3fa2c1`).
Slugs are at most 40 characters, so a prefix still fits in a 63-character DNS
label. The slug is stored when the preview is created and never recomputed, so
renaming the application keeps it. It is `env.slug` (and `env.name`), and also
the preview's name, so no environment of the application can share it.

**Routes.** Previews render the same routes as environments. Use flat
hostnames, one label below a shared domain, so one wildcard certificate
(`*.dev.example.com`) covers every preview. The slug already names the
application, so a route needs a prefix only to tell several routes apart:

```toml
# notes-feat-login-3fa2c1.dev.example.com
[[spec.routes]]
hostname = "${{ env.slug }}.dev.example.com"
service = "web"
port = 3000

# api-notes-feat-login-3fa2c1.dev.example.com
[[spec.routes]]
hostname = "api-${{ env.slug }}.dev.example.com"
service = "api"
port = 8080
```

In practice these hostnames go through a variable, so environments keep their
own: `hostname = "${{ vars.domain }}"` with `domain` set per environment and in
`[spec.previews.variables]`. A [wildcard route](../networking/wildcards.md) such
as `*.${{ env.slug }}.dev.example.com` gives each preview a subdomain per tenant,
with a certificate of its own.

Hostnames are reserved as for environments: a preview reserves the hostnames
its last fetched manifest renders, and two previews or environments rendering
the same hostname conflict with `hostname_conflict`. Preview routes are private
unless `[spec.previews] visibility = "public"`. Private hostnames need DNS
records pointing at the apps tailnet node (see
[public and private routes](../networking/private-routes.md)).

**Secrets.** Each preview generates its own `spec.secrets` values. It mounts a
stored secret only when the secret's access allows previews; otherwise its
deployment fails with `secret_access_denied`.

**Volumes.** A preview's volumes start empty. The preview keeps an inventory of
every volume its deployments created, including volumes a later manifest
dropped. Deleting the preview removes all of them, with their data, and checks
that none is left before the preview is gone; a deletion that cannot remove one
is retried. Environment volumes are retained, as always.

**Resources.** The daemon's [`[previews]` limits](configuration.md#previews)
bound every preview, so the same manifest serves environments and previews.
Each CPU or memory limit a service leaves unset gets the preview default, and
replicas above `max_replicas` are capped; explicit limits are kept. Both are
warnings on the preview's plan and deployment, never errors. Environments are
never bounded. Creating a preview beyond `max_per_application` or `max_total`
fails with `preview_limit_reached`.

**Lifetime.** Previews are never deleted automatically. `preview list` and
`preview show` report each branch's state, read on demand with `git ls-remote`
(reads arriving while one runs share it):
`exists` (at the deployed commit, or before the first fetch), `moved` (the
branch head is past the deployed commit), `gone`, `local` (its latest
deployment is a local snapshot, below), or `unknown` when the repository could
not be read. `preview prune --branch-gone` deletes, after
confirmation, the previews whose branch is gone; the daemon checks again and
keeps any it can no longer confirm gone. Repository or authentication errors
never count as gone: pruning then deletes nothing. Previews cannot be
[promotion](#promoted-environments) sources and do not record releases.

**Local worktrees.** `piquelctl preview deploy notes --local [--slot agent-2]`
deploys the worktree of the current directory, uncommitted and untracked files
included and ignored files excluded, without pushing it. The CLI commits it to
a snapshot on top of `HEAD`, without changing the branch, index or refs, and
uploads it as a Git bundle. The preview of the current branch and slot is
created if needed, as with `preview create`. The daemon reads the manifest from
the snapshot at the application's manifest path, renders `${{ git.sha }}` as
the snapshot commit, and builds `repository = "self"` services from the
snapshot; everything else (variables, bounds, secrets, routes) is as for the
branch. The deployment history records the snapshot commit and its base, the
commit of the repository it was uploaded against. A preview running a local
snapshot reports its branch as `local` and is never pruned, even when the
branch was never pushed. Deploying on push leaves it running until the branch
moves, or is first pushed. Redeploying the preview deploys the branch head
instead. Submodules, Git LFS and other filters, partial clones, and SHA-256
repositories are not supported.

## Files

Services mount read-only files, each from exactly one source:

```toml
[[spec.services.files]]
target = "/etc/otelcol/config.yaml"
path = "otel/collector.yaml"
# mode = "0444"

[[spec.services.files]]
target = "/etc/app/motd"
content = "hello"
```

- `path` reads a file from the manifest's own repository, at the exact commit
  the manifest was fetched from. It is relative to the repository root, like a
  Git build's `dockerfile`, and must stay inside it: `..` and symlinks leaving
  the checkout or pointing into `.git` are rejected. Only repository-backed
  applications can use it; elsewhere validation fails with
  `file_requires_repository`. `preview deploy --local` reads it from the
  uploaded snapshot.
- `content` is the file's text, inline.

Contents are never rendered with variables, so configuration that uses `${…}`
or `{{ }}` itself, such as Prometheus' or Grafana's, is mounted exactly as
written. Values that differ between environments belong in environment
variables, which most such tools expand.

`target` is an absolute, normalized container path, unique among the service's
mount, secret, and file targets. It cannot be a directory holding another of
them, nor `/run/secrets`, `/run/piqueld`, a path inside them, or a directory
holding them; files inside a mounted volume's directory are fine. `mode` is an
octal permission from `"0000"` to `"0777"`, `"0444"` by default; files are owned by root. Contents
can't be empty (`file_empty`), and repository files must be UTF-8 text.
Directories are not supported: list files one by one.

Files are read-only: piqueld publishes each content as an immutable Swarm
config, which Docker always mounts read-only. An image that rewrites its own
configuration at startup must copy the file somewhere writable first.

A deployment reads its repository files when it fetches the manifest, and
records their bytes with the deployment and its release. Retries and restarts
reuse them and never read the repository again, and a promoted environment,
which never fetches, mounts exactly the release's bytes. A changed file is a
manifest change like any other: it takes effect with the next deployment, or
the next push when [deploying on push](#deploying-on-push). New content is a
new config, so only the services mounting it roll out, in their usual
[rollout](#rollout) order. `app plan` lists each file as
`services.<name>.files.<target>`, with inline content summarized by size and
digest; since it reads nothing from the repository, it shows changed paths, but
not changed repository contents. Promotion plans compare the captured bytes
too, as `repository_files.<path>`.

Files are [edited](#editing-repository-backed-applications) like the rest of
the configuration, with `piquelctl app file` or the dashboard. Disconnecting the
repository fails validation while services still mount repository files;
replace them with inline `content` first.

## Startup dependencies

```toml
[[spec.services]]
name = "auth"
depends_on = ["postgres"]
```

A deployment rolls a service out only after every service in its `depends_on`
converges in that deployment: all replicas run and pass their health checks.
A dependency without a health check counts once its replicas are running, so
declare one on databases and other slow starters. Dependencies that are
already converged and unchanged do not delay the dependent. A failed or
timed-out dependency leaves its dependents unchanged. Each dependency gets the
full `reconciliation.convergence_timeout_seconds` to converge, since the
deadline restarts whenever a service converges.

Each entry names another service in the same application, at most once
(`service_dependency_missing`, `service_dependency_duplicate`); lists longer
than the 64-service limit are rejected (`service_dependency_count_excessive`). Cycles,
including a service depending on itself, are rejected with
`service_dependency_cycle` on every service in or behind the cycle.
Renaming a service through field edits updates references to it; removing one
drops it from other services' dependencies.

This is startup ordering only. It gives no runtime guarantee after rollout:
a dependency that later becomes unhealthy does not stop or restart its
dependents, and drift repair does not pass a dependency that is still
converging.

## Rollout

```toml
[[spec.services]]
name = "worker"

[spec.services.rollout]
order = "stop-first"   # or "start-first"
monitor_seconds = 30
```

Docker replaces one task at a time and pauses the rollout when a replacement
fails within `monitor_seconds` of starting. `stop-first` stops the old task
before starting its replacement: tasks never overlap, at the cost of a short
downtime. `start-first` starts the replacement first, so there is no downtime
but the two tasks briefly run side by side. Use `stop-first` for a singleton
worker that must not run twice, and `start-first` for a store that tolerates
two writers.

Both fields are optional, and an empty or missing block keeps the defaults:
the order is derived from the mounts (`stop-first` when any volume is mounted
writable, `start-first` otherwise) and the monitor window is 30 seconds. The
order must be one of the two values above, and `monitor_seconds` must be
between 1 and 3,600 (`rollout_monitor_invalid`). Docker only reports an update
as complete once the last replacement's monitor window has passed, so keep it
below `reconciliation.convergence_timeout_seconds`.

`app plan` lists each service's effective order, marked `derived` or
`explicit`, and its monitor window. It warns with
`rollout_start_first_writable_volume` when `start-first` is set on a service
that mounts a volume writable, since two tasks then write the same data
directory. The warning does not block the deployment. Changing either setting
updates the service once.

## Telemetry receivers

One service per application may receive the daemon's own traces, metrics and
logs, such as an OpenTelemetry Collector:

```toml
[[spec.services]]
name = "collector"
receives_telemetry = true
```

A second one fails with `telemetry_receiver_duplicate`, and more than one
replica with `telemetry_receiver_replicas`, since one collector listens on the
socket. For the same reason its socket counts as a writable mount, so its
rollouts stop the old task first unless `rollout.order` says otherwise (which
warns). The setting is part of
the manifest and its hash, but the socket it receives on is not: in an
environment granted telemetry (`piquelctl env telemetry APP ENV on`, which
requires `telemetry:grant`), piqueld mounts the service's socket directory
at `/run/piqueld/telemetry`, runs the service as the daemon's user, and exports
to `/run/piqueld/telemetry/otlp.sock`. Without the grant, the service runs
without the mount, and `app plan`, promotion plans and the deployment warn with
`telemetry_not_granted`. The mount follows the grant within seconds, without a
deployment, which restarts the service. Previews are never granted. Volume
mounts and files can't use `/run/piqueld/telemetry`. The receiving
application sees every application's telemetry; see
[receiving telemetry](../guides/observability.md#receiving-telemetry-in-an-application)
for the collector configuration and what that implies.

## Jobs

Jobs run a container to completion at a defined point of every deployment,
for example database migrations:

```toml
[[spec.jobs]]
name = "migrate"
service = "auth"             # reuse the new image, env, secrets and mounts
command = ["auth-service", "migrate"]
run = "before-rollout"
timeout_seconds = 300        # default
volume_access = "read-only"  # default, or "exclusive"
stop_service = false         # default

[spec.jobs.environment]      # added to auth's, for the job only
AUTH_DATABASE_OWNER_PASSWORD = "file:///run/secrets/database-password"

[[spec.jobs.secrets]]        # added to auth's, for the job only
name = "database-password"
target = "/run/secrets/database-password"
```

A job reuses the prepared image, environment, secret files, volume mounts,
resource limits, and private network of the referenced service in the same
deployment. Its `command` replaces the service's command and arguments.
Its own `environment` and `secrets`, declared like a service's, are added to the service's for the job's container
only: the service's replicas never get them, so a secret only a migration
needs, such as a database owner's password, stays out of the running service.
A job can't override its service: setting a variable the service sets, or
mounting a secret at a target the service already uses for a volume, secret,
or file, fails validation with `job_setting_conflict`. Health
checks do not apply, including an image's `HEALTHCHECK`, and a job does not
answer for the service's network name. `before-rollout` is currently the only
run point: jobs run in declared order after every source is prepared and before
the deployment is promoted. Missing networks and volumes are created first, and
a deployment whose plan is blocked runs no jobs.

A job mounts every volume read-only by default, because the previous version
of its service keeps running while it does. A migration that writes a volume,
such as one changing a SQLite database, declares `volume_access =
"exclusive"`. Its service's writable volumes then stay writable, and once the
job's dependencies are ready, every service that mounts one of them stops.
The job starts only after their containers have exited, and the services
start again with the rollout. Exclusive access requires the service to mount
a writable volume, and the job's startup dependencies cannot mount it, or
validation fails with `job_volume_access_invalid`. Plans warn with
`job_stops_services`, naming the services that stop.

A migration that drops or renames something the previous version of its
service still uses would break that version while the job and rollout run.
Set `stop_service = true` to stop the job's service first:

```toml
[[spec.jobs]]
name = "migrate"
service = "auth"
command = ["auth-service", "migrate"]
run = "before-rollout"
stop_service = true
```

Once the job's dependencies are ready, `auth` stops. The job starts only
after its containers have exited, and `auth` starts again with the rollout.
This trades a short outage for never running the old code against a schema it
doesn't understand. Plans warn with `job_stops_services`. To avoid the outage,
split the change into migrations that each version can run beside.

Jobs inherit their referenced service's `depends_on`. Before each job, those
services and their transitive dependencies start or update to the prepared
configuration and converge, including their health checks. For example, set
`depends_on = ["postgres"]` on `auth` and give `postgres` a readiness health
check: even on the first deployment, the sequence is start Postgres, wait until
it is healthy, run the migration, then roll out auth. Without a health check,
a dependency counts as ready once its replicas are running. Each dependency
gets the normal service convergence timeout; the job's own timeout starts
after dependencies are ready.

Other services wait until all jobs succeed. If a dependency has its own jobs,
put all of them before the job that needs that dependency, or validation rejects
the order with `job_dependency_order_invalid`. This also applies to transitive
dependencies. Jobs cannot reach services that have not started and are not in
their inherited dependency list.

A non-zero exit, a rejected task, or exceeding `timeout_seconds` (1–86,400) fails
the deployment with `job_failed` or `job_timeout`. Services outside the job's
startup dependencies keep their previous configuration. While the deployment
can still continue, active-target repair keeps the dependencies it started or
updated and the services stopped for a job. Once a job has failed,
the deployment was cancelled, or an ownership or immutable configuration
conflict blocks the deployment's plan, and the job's container has stopped,
repair returns them to the running deployment: dependencies roll back and
stopped services start again. Data the job changed is not reverted. A
deployment blocked by a conflict rolls out again once the conflict is
resolved; if one of its jobs that stops services already succeeded, the
services it stopped stay stopped until then, since they would run against migrated data. The application
stays degraded, and failed jobs are not retried automatically; deploy again
after fixing the cause. Each job succeeds at most once per deployment: a
retried deployment skips jobs that already succeeded, and after a daemon
restart, or when Docker could not report a job's status, a still-running job is
resumed rather than started again. Its timeout counts from when it started, so
resuming never extends it.
Retrying a promoted deployment or repairing drift never runs jobs, and
neither does [restoring](api.md#restoring-a-deployment) an earlier deployment with `env
restore`: running its old migrations against newer data is exactly the risk a
restore avoids. A restore does not revert data either, so write migrations in
expand/contract steps that the previous version still runs against. A deployment
that is superseded or cancelled stops its running job, and the next deployment
stops any job an earlier one left behind. Each run, its bounded output, and
Docker's explanation of a failed task are kept in the application's build
history; output past 1 MiB per run is dropped and the run is marked truncated.

Plans list each job as a `run_job` action after the actions that prepare it,
and change lists show job settings as `jobs.<name>.<field>`, with commands
redacted like service commands, and the order jobs run in as `jobs.order`.

Besides applying a manifest, jobs can be edited with `piquelctl app job`, the
dashboard's Jobs tab, or `PUT /api/v1/applications/{id}/jobs`. Renaming a
service repoints its jobs; removing a service removes them.

## Links

A link lets the services of one application reach a service of another
directly, without a route through the gateway. Both applications declare it:
the exporting application lists who may reach the service, and the importing
application asks for it.

```toml
# auth
[[spec.exports]]
service = "api"
applications = ["notes", "billing"]
# previews = true    # let previews of those applications import it too

# notes
[[spec.imports]]
application = "auth"
service = "api"
# environment = "staging"
```

Every service of `notes` then reaches the `api` service of `auth` as
`api.auth` (`<service>.<application>`), on any port it listens on, e.g.
`http://api.auth:8080`. Nothing is reachable by default: an import that no
export grants connects to nothing.

**Isolation.** Each link gets its own overlay network, which only the
importing environment's services and the services it imports join. Linking
does not join the two applications' private networks: `notes` cannot reach
`auth`'s other services, by name or by address, and two applications importing
from `auth` cannot reach each other. Docker networks connect services, not
ports, so every port the exported service listens on is reachable, and the
exported service can in turn reach the importing services on the link's
network. Export services that only listen where importers should connect.

**Environments.** An import reaches the environment of the exporting application
named like the importing environment: `notes` staging reaches `auth` staging.
A preview's name is its slug, so previews usually name one.
`environment` names another one, and accepts variables, so each environment
or the previews can choose:

```toml
[[spec.imports]]
application = "auth"
service = "api"
environment = "${{ vars.auth_env }}"

[spec.variables]
auth_env = "${{ env.name }}"

[spec.previews.variables]
auth_env = "staging"
```

An export grants every environment of the listed applications, and their
previews only with `previews = true`, like a stored secret's access list. Each
environment of the exporting application decides for itself: what it exports is
what it deploys. Previews never export.

**Lifecycle.** A link follows what both environments deploy, never a manifest
that is only saved. It connects once both sides have deployed it, and goes away
once either side deploys without it, is deleted, or starts being deleted: the
exported service then leaves the link's network, and the network is removed.
Deploying the other side is not needed; it adjusts within seconds, even while
one of its services is failing or a deployment of it is running. Deployments
never wait for a link: until the exporter deploys its export, `api.auth` does
not resolve in `notes`, so its services should retry, as they would while
`api` restarts. Joining or leaving a link rolls the exported service and the
importing services out again, as any network change does. Renaming the
exporting application changes the names its services answer to.

Jobs reach the links of the deployment they run in, but the exporter joins a
link only once the importing deployment is promoted, after its jobs. A job
that needs a new import therefore can't reach it on the deployment that adds
it: deploy the import first.

An exporting environment links to at most 64 importing environments. The
oldest keep their links, and imports of newer ones wait until a slot frees up.

`env show` and the Overview of an environment or preview in the dashboard list
each import and whether it connects, or why not: no deployed environment of
that name exports the service to this environment, or it already links to 64.
Whether that environment exists is not shown, since its application may be
hidden from you. They also list the environments importing an environment's
services, from the applications you can read. `app plan` for an existing
environment and `env promote --plan` warn about imports that would not connect
(`import_unconnected`).

**Names.** Exports and imports name applications, so manifests stay portable
across installations. Deploying an environment binds each name its exports and
imports use to the application holding it then, and its links follow that
application, like a stored secret's access list follows environments:
renaming the application keeps them, and an application later taking a freed
name connects nothing. Later deployments keep a binding while its application
exists, and bind a name anew only once it is gone. A name no application held
at deployment matches whichever application takes it, until the next
deployment binds it, so an export may name an application created later.
Whoever first creates an application of that name then gains the grant, so
prefer naming applications that already exist. An application may name
itself, linking its environments to each other, such as a preview to
`staging`.

**Validation.** An export names a service of its application, at most once
(`export_service_missing`, `export_duplicate`), and at least one application,
each once (`export_applications_empty`, `export_duplicate`); an import names
each service of an application once (`import_duplicate`). Exceeding a link
budget fails with `link_count_excessive`.

Besides applying a manifest, links can be edited with `piquelctl app export`
and `app import`, the dashboard's Links tab, or
`PUT /api/v1/applications/{id}/exports` and `/imports`. Renaming a service
renames its export, so importers must import the new name; removing a service
removes its export.

## Routes

```toml
[[spec.routes]]
name = "web"
hostname = "notes.example.com"
service = "web"
port = 3000
visibility = "public"
```

`name` is optional. It gives the route a stable name, unique within the
application, with the same rules as service names (1–63 lowercase letters,
digits or hyphens, starting with a letter and ending with a letter or digit).
It cannot reference variables, so it is the same in every environment and
preview, whatever hostname the route renders there:
`piquelctl preview url notes feat/login --route web` always prints the `web`
route's URL. Duplicate names fail with `route_name_duplicate`.

`visibility` is `public` (the internet) or `private` (only the tailnet, on the
same hostname), and defaults to `private`. `[spec.environments.<name>]
visibility` and `[spec.previews] visibility` cap the routes of an environment and
of previews: a route's effective visibility is the stricter of its own and the
ceiling. Environments default to `public`, which restricts nothing; previews
default to `private`.

```toml
[spec.environments.staging]
visibility = "private"
```

See [private routes](../networking/private-routes.md) for the apps tailnet
node and DNS.

Each route either references a service in the same application and its internal
HTTP port (1–65535), or sets `redirect` instead:

```toml
[[spec.routes]]
hostname = "www.notes.example.com"
redirect = { to = "https://notes.example.com", status = 308, preserve_path = true }
visibility = "public"
```

Redirects are answered by the gateway, so an application may consist of redirect
routes alone. `to` is an `http`/`https` URL with a lowercase public hostname, an
optional port and path, and no query, fragment, or `{}`; it must not target a
hostname the route itself serves. `status` is 301, 302, 303, 307, or 308 (default 308).
`preserve_path` (default `true`) appends the request path and query to `to`.
Up to 64 routes are allowed. Hostnames are public ASCII
DNS names (use Punycode for internationalized domains), normalized to lowercase
without a trailing dot. Paths, schemes, and hostname ports are rejected. A hostname
starting with `*.`, such as `*.tenants.example.com`, is a
[wildcard route](../networking/wildcards.md): it serves every name one label below
`tenants.example.com` that no exact route claims. `*` is allowed only as the
first label.
A hostname belongs to one application across saved, captured, and deployed state.
Other applications cannot add exact routes below a wildcard, unless they routed
that name first.
Conflicts fail atomically with `hostname_conflict`, including when ingress is off.

Save does not change live routes. Deploy activates additions/destination changes
once backends converge; explicit removals are withdrawn before backend cleanup.
Routes are still accepted and deployed while ingress is disabled, but are not
exposed until the daemon's global `ingress.enabled` setting is enabled.
