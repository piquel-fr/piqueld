# Previews

A preview is a short-lived copy of an application that runs one Git branch,
or the uncommitted changes in your worktree. Use one to try a change before it
reaches staging, then delete it.

## Before you start

- The application reads its manifest from Git, as described in
  [deploying on push](deploy-on-push.md). Previews deploy branches of that
  repository.
- Creating and deploying previews needs `previews:deploy` on the application,
  and deleting them `previews:delete`. `apps:deploy` and `apps:delete`
  include them; see [preview permissions](../accounts/authorization.md#preview-permissions).
- `piquelctl` is signed in. The commands on this page run on your machine.

## Preview a branch

1. Push the branch, such as `feat/login`, to the application's repository.

2. Create its preview:

   ```console
   $ piquelctl --timeout 10m preview create notes --branch feat/login
   Preview notes-feat-login-2ef7d4 of branch feat/login (preview-01a16b51e4b070c08f54293a1e968fe6) created
   Operation: operation-01a16b51e4b9781299d65ba140e022ad succeeded
   URL: https://notes-feat-login-2ef7d4.preview.notes.example.com (web) pending: HTTPS (Ingress is disabled in daemon configuration)
   ```

The preview deploys the manifest from `feat/login`, and builds the branch's
`repository = "self"` services. It has its own deployments, secrets, volumes
and routes, and never shows up among the application's environments.

Its name, `notes-feat-login-2ef7d4`, is its slug: the application and the
branch, with a short hash. Its URL is `pending` until its route serves
requests. Here, the host doesn't serve HTTP ingress.

Running `preview create` again returns the existing preview without
redeploying it. To deploy the branch's latest commit:

```console
piquelctl preview deploy notes feat/login
```

While the application [deploys on push](deploy-on-push.md#deploy-every-push),
every preview also deploys each push to its branch.

In the dashboard, open the application's **Previews** tab and select **New
preview**. Fill in **Branch**, and **Slot (optional)** to tell several
previews of one branch apart, then select **Create preview**.

![The Previews tab of notes: three previews, two of branch feat/login, one of them running a local snapshot, and one of feat/search, whose branch is gone. Each has a Delete button, and the tab has Refresh, Prune gone and New preview buttons](../../images/applications/previews.png)

## Preview your local changes

On your machine, in a worktree of the application's repository, deploy what
the worktree holds, without committing or pushing it:

```console
$ cd ~/src/notes
$ piquelctl --timeout 10m preview deploy notes --local
snapshot: succeeded · 517 bytes against 512110719156: 0 unpushed commits and the snapshot [0s]
Info: sending uncommitted files: Dockerfile, draft.html
upload: succeeded · uploaded [0s]
…
Preview notes-feat-login-2ef7d4 of branch feat/login (preview-01a16b51e4b070c08f54293a1e968fe6) exists
Deploys: local snapshot 7a72cf118ee4 on 512110719156
Operation: operation-01a16b5265c47e108d917b5610ba595d succeeded
```

It deploys to the preview of the current branch, and creates it if needed.
It sends the worktree's committed, uncommitted and untracked files, but not
the files Git ignores. Your branch, index and files don't change.

Only what the repository lacks is uploaded: the commits you haven't pushed,
and your changes. piqueld fetches the rest from the application's
repository. Run the command again after each change, to deploy a new
snapshot to the same preview.

The preview's page shows the snapshot it runs, and the commit it was taken
on:

![The page of preview notes-feat-login-2ef7d4: branch feat/login, branch state Local, and it deploys local snapshot d08b6bd5432f on 512110719156](../../images/applications/preview-local.png)

To deploy the pushed branch again, select **Redeploy branch head**, or run
`piquelctl preview deploy notes feat/login`. A push to the branch also
replaces the snapshot, while the application deploys on push.

## Give previews their own settings

Previews ignore the `[spec.environments.<name>]` blocks of the manifest, and
use its `[spec.previews]` block instead:

```toml
[spec.previews]
visibility = "private"

[spec.previews.variables]
domain = "${{ env.slug }}.preview.notes.example.com"
```

- `visibility` is the strictest visibility of their routes. It is `private`
  by default, which keeps every preview on your tailnet.
- `variables` override the defaults of `[spec.variables]` for every preview.
- `${{ env.slug }}` is the preview's slug, so each preview gets its own
  hostname.

Previews read the manifest from their own branch, so set this block in the
repository. The [manifest reference](../reference/manifest.md#previews)
describes how to name preview hostnames, so that one wildcard certificate
covers them all.

Each preview generates its own values for the secrets the manifest
generates. A secret you stored with `piquelctl app secret` is only given to
previews when its access allows them:

```console
piquelctl app secret notes access stripe-test --previews
```

## Run several previews of one branch

A slot tells several previews of the same branch apart, such as one per
person or agent:

```console
$ piquelctl --timeout 10m preview create notes --branch feat/login --slot agent-2
Preview notes-feat-login-agent-2-503aa8 of branch feat/login (slot agent-2) (preview-01a16b53633f78c3a27ae33924595565) created
```

Address it with its slug, or with its branch and `--slot`, such as
`piquelctl preview deploy notes feat/login --slot agent-2`. `preview deploy
--local` takes `--slot` too.

## Find a preview's address and logs

```console
piquelctl preview url notes feat/login
piquelctl preview logs notes feat/login
piquelctl preview show notes feat/login
```

`preview show` also says what the preview deploys and how its branch moved.
In the dashboard, select the preview on the **Previews** tab: its
**Overview** lists its URLs, and its **Logs** tab its services' output.

## Limits

Previews share the host with production, so the daemon bounds them. Its
[`[previews]` settings](../reference/configuration.md#previews) are:

| Setting | Default | Effect |
| --- | --- | --- |
| `max_per_application` | `10` | Previews of one application |
| `max_total` | `30` | Previews on the host |
| `default_cpu_millis` | `500` | CPU limit of a service that sets none, in millicores |
| `default_memory_bytes` | `536870912` | Memory limit of a service that sets none, 512 MiB |
| `max_replicas` | `1` | Most replicas a service runs |

A service bounded by these settings runs anyway. `piquelctl preview show`
reports it, such as `Bounded: web runs with the [previews] default of 500
millicores of CPU and 536870912 bytes of memory, since its manifest sets no
limit`, and the dashboard marks the preview **Bounded**.

## Delete a preview

Deleting a preview removes its services, secrets, history and volumes. Its
volumes' data is deleted with them.

```console
$ piquelctl preview delete notes feat/login --slot agent-2
Preview preview-01a16b53633f78c3a27ae33924595565 deleted (its volumes removed)
```

The command asks you to confirm first. To delete every preview whose branch
was deleted from the repository:

```console
$ piquelctl preview prune notes --branch-gone
Deleting preview notes-feat-search-47d42e of branch feat/search (preview-01a16b53ea647651b798f1520e0e6ddc) with its volumes: operation operation-01a16b5d2b5e7391938bf4de96c7aea8
```

In the dashboard, each preview on the **Previews** tab has a **Delete**
button, and **Prune gone** deletes the previews whose branch is gone.

piqueld never deletes a preview on its own, even once its branch is gone.
Deleting the application deletes its previews, with their volumes.

## Check that it works

```console
$ piquelctl preview list notes
BRANCH  SLOT  SLUG  STATE  BRANCH STATE  SYNC  SYNCED  LAST OPERATION  ID
feat/login  -  notes-feat-login-2ef7d4  ready  local  following  512110719156  succeeded operation-01a16b5265c47e108d917b5610ba595d  preview-01a16b51e4b070c08f54293a1e968fe6
feat/login  agent-2  notes-feat-login-agent-2-503aa8  ready  exists  following  512110719156  succeeded operation-01a16b53634b78c1b3cf84de38ea3a26  preview-01a16b53633f78c3a27ae33924595565
feat/search  -  notes-feat-search-47d42e  ready  gone  following  6716f479bb30  succeeded operation-01a16b53ea6c7bd2b62464b14337fe86  preview-01a16b53ea647651b798f1520e0e6ddc
```

The branch state compares the preview with its branch in the repository:

| State | Meaning |
| --- | --- |
| `exists` | The branch is at the commit the preview deployed. |
| `moved` | The branch moved past the commit the preview deployed. |
| `gone` | The branch was deleted. |
| `local` | The preview runs a snapshot of a local worktree. |
| `unknown` | piqueld couldn't read the repository. |

## Troubleshooting

- **Creating fails with `preview_requires_repository`.** The application
  doesn't read its manifest from Git. Connect a repository first; see
  [deploying on push](deploy-on-push.md#connect-an-existing-application).
- **Creating fails with `preview_limit_reached`.** The application, or the
  host, has as many previews as `[previews]` allows. The error lists them:
  delete one you no longer need.
- **`--local` says no remote-tracking branch has an ancestor of `HEAD`.**
  None of your worktree's commits was pushed, so piqueld would need the whole
  history. Push the branch, or pass `--full-history` to upload all of it. The
  daemon still needs to read the application's repository.
- **`--local` fails with `snapshot_base_unavailable`.** The daemon can't
  fetch, from the application's repository, the commit your snapshot builds
  on. Push it, or pass `--full-history`.
- **`--local` fails with `snapshot_too_large`.** The upload or the worktree
  is larger than the daemon accepts:
  [`snapshots.max_upload_bytes`](../reference/configuration.md#local-snapshots)
  and [`checkouts.max_bytes`](../reference/configuration.md#git-checkouts).
  When the files are too large, the command names the largest ones.
- **`--local` refuses the worktree.** Git submodules, nested repositories,
  Git LFS and other filters, partial clones and SHA-256 repositories aren't
  supported.
- **The deployment fails with `secret_access_denied`.** The preview mounts a
  stored secret whose access doesn't include previews. Allow them, as in
  [giving previews their own settings](#give-previews-their-own-settings), or
  mount another secret in previews.
- **The deployment fails with `hostname_conflict`.** The preview renders the
  same hostname as an environment or another preview. Build preview
  hostnames from `${{ env.slug }}`.
