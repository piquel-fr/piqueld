# Building images from Git

A service can build its image from a Git repository with a Dockerfile, instead
of pulling one from a registry. Each time you deploy, piqueld checks the
repository out and builds the image on the host.

## Before you start

- You have [an account](../getting-started/first-account.md), and `piquelctl`
  is signed in. The commands on this page run on your machine.
- The host can reach the repository. Public repositories work as they are;
  private ones need [an SSH key](#build-from-a-private-repository).
- You trust the repository. Its Dockerfile's instructions run on the host's
  Docker engine.

## Build a service from a repository

This example builds [traefik/whoami](https://github.com/traefik/whoami), a
small web server whose repository has a Dockerfile at its root.

1. Save this manifest as `whoami.toml`:

   ```toml
   api_version = "piqueld.dev/v1alpha1"
   kind = "Application"

   [metadata]
   name = "whoami"

   [[spec.services]]
   name = "web"

   [spec.services.source]
   type = "git"

   [spec.services.source.repository]
   url = "https://github.com/traefik/whoami.git"
   branch = "master"

   [spec.services.source.build]
   type = "docker"
   dockerfile = "Dockerfile"

   [spec.services.healthcheck]
   type = "http"
   port = 80
   path = "/health"
   ```

   - `repository` names the repository and the branch to build.
   - `build` names the Dockerfile, relative to the root of the repository.
     The build context is the root too, unless you set `context`.

2. Save the manifest and deploy it:

   ```console
   piquelctl --timeout 10m app apply --file whoami.toml --deploy
   ```

   `piquelctl` waits 30 seconds by default, which a build can take longer
   than, so `--timeout 10m` gives it longer.

Each deployment builds the latest commit of the branch. Docker's build cache
keeps the steps that didn't change fast, and base images are pulled again, so
they stay up to date. If the checkout or the build fails, the deployment
fails and the application keeps running what it ran before.

To build one exact commit instead, add its full hash to `repository`:

```toml
[spec.services.source.repository]
url = "https://github.com/traefik/whoami.git"
branch = "master"
commit = "1ce75d01b6978863647da42557a707a479da3a51"
```

Remove `commit` to follow the branch again. The
[manifest reference](../reference/manifest.md#git-build-sources) describes
every setting.

### With piquelctl

Add a service that builds from Git, or switch an existing one to Git:

```console
piquelctl app service add whoami web --git https://github.com/traefik/whoami.git --branch master
piquelctl app service source git whoami web https://github.com/traefik/whoami.git --branch master
```

`source git` replaces the whole source. Any option you leave out goes back to
its default: branch `main`, Dockerfile `Dockerfile`, context `.`, and no
build arguments or target. To change one setting, use its own command:

```console
piquelctl app service source branch whoami web master
piquelctl app service source commit whoami web 1ce75d01b6978863647da42557a707a479da3a51
piquelctl app service source commit whoami web --clear
piquelctl app service source dockerfile whoami web build/Dockerfile
piquelctl app service source context whoami web build
```

These commands only save the change. Deploy it with
`piquelctl --timeout 10m app deploy whoami`, or add `--deploy` to the command.

### In the dashboard

1. Open **Applications**, then the application, then the service on its
   **Services** tab.
2. On the **Source & scaling** tab, set **Source** to **Git repository with
   Dockerfile**.
3. Fill in **Repository**, **Branch** and **Dockerfile path**.
   **Commit (optional)** pins a commit.
4. Select **Save changes**, then **Deploy to production** on the application's
   page.

![The Source & scaling tab of the web service: Source is "Git repository with Dockerfile", Repository is https://github.com/traefik/whoami.git, Branch is master, a commit is pinned, and Dockerfile path is Dockerfile](../../images/applications/service-git-source.png)

## Pass build arguments and choose a stage

Add `args` and `target` to `build`:

```toml
[spec.services.source.build]
type = "docker"
dockerfile = "Dockerfile"
args = { VERSION = "1.10" }
target = "runtime"
```

- Each entry of `args` is a Docker build argument, which the Dockerfile reads
  with `ARG`.
- `target` builds that stage of a multi-stage Dockerfile, instead of the last
  one.

Build arguments aren't secret. Docker stores them in the image, and piqueld
shows them in the manifest and the build history. Give secrets to the running
service instead.

With `piquelctl`, pass them to `source git`, which replaces the whole source:

```console
piquelctl app service source git whoami web https://github.com/traefik/whoami.git \
  --branch master --build-arg VERSION=1.10 --target runtime
```

In the dashboard, they are **Build target (optional)** and, under **Build
arguments (not secret)**, **Add build argument**.

Build settings accept [variables](../reference/manifest.md#variables), but an
image built from a value that differs between environments can't be promoted
from one to the other. See
[keeping builds the same everywhere](environments.md#keep-builds-the-same-in-every-environment).

## Build from a private repository

piqueld reaches private repositories over SSH, with a key belonging to the
daemon's user, `piqueld`. Run these commands on the host. They use
`/var/lib/piqueld`, the user's home directory when piqueld is
[installed](../getting-started/install.md) as described.

1. Create a key without a passphrase:

   ```console
   sudo -u piqueld mkdir -p -m 700 /var/lib/piqueld/.ssh
   sudo -u piqueld ssh-keygen -t ed25519 -N '' -C piqueld -f /var/lib/piqueld/.ssh/id_ed25519
   ```

   Git runs without a terminal, so it can't ask for a passphrase.

2. Give the key read access to the repository. On GitHub, open the
   repository's **Settings**, then **Deploy keys**, and add the contents of
   `/var/lib/piqueld/.ssh/id_ed25519.pub`, without write access.

3. Connect once, so that SSH records the server's host key:

   ```console
   sudo -u piqueld ssh -T git@github.com
   ```

   Check that the fingerprint SSH shows matches the server's published one,
   such as [GitHub's](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints),
   and answer `yes`. Git can't answer that question later.

4. Use the repository's SSH URL in the manifest:

   ```toml
   [spec.services.source.repository]
   url = "git@github.com:example/notes.git"
   branch = "main"
   ```

Git uses the daemon user's `~/.ssh` and `~/.gitconfig`, but not the host's
`/etc/gitconfig`. piqueld refuses credentials written in a URL, such as
`https://token@github.com/…`.

## Build from the manifest's own repository

When piqueld reads an application's manifest from Git, as described in
[deploying on push](deploy-on-push.md), its services can build from that same
repository:

```toml
[spec.services.source]
type = "git"
repository = "self"

[spec.services.source.build]
type = "docker"
dockerfile = "Dockerfile"
```

The service builds the commit the manifest was read from, so a deployment's
manifest and images always come from one commit. It has no branch or commit
of its own: the branch each environment follows decides.

In the dashboard, set **Source** to **Manifest repository with Dockerfile**.
`piquelctl` can't switch a service to it; set it in the manifest.

## Check that it works

`piquelctl app show` lists each service's source:

```console
$ piquelctl app show whoami
whoami (app-01a16b4a50a57cb18bf09c184230367a)

Configuration revision: 1
Environment: production (app-01a16b4a50a57cb18bf09c184230367a): ready, deploys from saved

Service: web
  Replicas: 1
  Source: git https://github.com/traefik/whoami.git (master)
```

piqueld records every build, whether it succeeds or fails. In the dashboard,
**Builds** in the sidebar lists the builds of every application, and an
application's **Builds** tab lists its own. Select a build to see the commit
it built, the image it produced, and its output.

![A build of whoami's web service, expanded: it succeeded in 31.5 seconds, built commit 1ce75d01 of https://github.com/traefik/whoami.git (master), and its output ends with the Dockerfile's last steps](../../images/applications/build-output.png)

With `piquelctl`, list an application's builds by its ID, which
`piquelctl app show` prints:

```console
$ piquelctl builds list --application app-01a16b4a50a57cb18bf09c184230367a
1  app-01a16b4a50a57cb18bf09c184230367a  web  Succeeded  1792801398229
```

Each line is a build: its number, the environment's ID, the service, its
state, and when it started, in Unix milliseconds. Read a build's output by its
number:

```console
piquelctl builds logs 1
```

It prints the latest output. When there is more, it ends with the command's
`--before` option that prints the output before it.

## Troubleshooting

- **The deployment fails with `git_build_failed`.** The checkout or the build
  failed. Its output ends with the error: open the build on the **Builds**
  page, or run `piquelctl builds logs`. Fix the repository or the source's
  settings, then deploy again. `piquelctl` suggests `env reconcile`, which
  retries the same commit and settings: it only helps when the cause was
  temporary, such as the network.
- **The deployment fails with `preparation_timeout`.** A deployment has
  `reconciliation.prepare_timeout_seconds`, 300 seconds by default, to check
  out and build its services. The host builds one image at a time, and the
  time spent waiting for other builds counts too. Raise the setting in the
  [daemon's configuration](../reference/configuration.md).
- **`piquelctl` times out, but the build goes on.** The command stopped
  waiting, not the deployment. Follow it on the **Builds** page, or give
  `piquelctl` a longer `--timeout`.
- **The output says `Permission denied (publickey)` or `Host key verification
  failed`.** The daemon's user has no key the server accepts, or doesn't know
  the server's host key yet. Follow
  [building from a private repository](#build-from-a-private-repository).
- **Validation fails with `git_credentials_inline`.** The repository URL
  contains a user name, password or token. Use an SSH URL and key instead.
- **A Git setting of the host doesn't apply,** such as a rule rewriting
  GitHub URLs to SSH. Git ignores `/etc/gitconfig`: add the setting to the
  daemon user's `~/.gitconfig`.
- **A large repository fails to check out.** Checkouts must fit in
  [`checkouts.max_bytes`](../reference/configuration.md#git-checkouts), 4 GiB
  by default, and hold at most 500,000 files and directories. piqueld doesn't fetch Git
  submodules or Git LFS files.
- **The build output stops before the end.** piqueld keeps the first 4 MiB of
  each build's output for 30 days. `build_history.log_max_bytes` and
  `build_history.log_retention_days` in the
  [daemon's configuration](../reference/configuration.md) change that.
