# Releasing piqueld

A release publishes the daemon, `piquelctl` and `piqueld-client` to
[crates.io](https://crates.io/crates/piqueld), with the two crates they depend
on, then tags the commit. CI then creates the GitHub release, with the static
binaries the [install script](../user/getting-started/install.md#install-with-the-script)
installs. A maintainer runs it with `just release`.

## Versions

Every crate shares the workspace version, `version` under
`[workspace.package]` in the root `Cargo.toml`, and they are released
together. A daemon and a client of the same version speak the same API.

Versions follow [semantic versioning](https://semver.org). Before 1.0:

- a minor release, such as 0.2.0, may change the manifest, the daemon
  configuration, `piquelctl`, the HTTP API or the Rust crates in ways that
  break existing users. Its release notes say how to upgrade.
- a patch release, such as 0.1.1, only fixes bugs: upgrading to it needs no
  changes.

The crates build with Rust 1.99 or newer: `rust-version` under
`[workspace.package]`, which the `msrv` CI job checks. Raising it is a change
users notice, so the release notes say so.

Each release is tagged `v<version>`, such as `v0.1.0`, on a commit of `main`.
A released [migration](architecture/persistence.md#adding-a-migration) is
never edited, since databases that applied it won't run it again.

## What gets published

| Crate | What it is |
| --- | --- |
| `piqueld` | The daemon, with its dashboard and health probe embedded |
| `piquelctl` | The CLI |
| `piqueld-client` | The typed Rust client, with its documentation on [docs.rs](https://docs.rs/piqueld-client) |
| `piqueld-core` | The contracts the others share |
| `piqueld-probe` | The static health probe the daemon embeds |

`piqueld-ui`, `piqueld-generated` and `xtask` set `publish = false`. Cargo
leaves `piqueld-generated` out of the published `piqueld` and `piquelctl`,
which use it only as a development dependency.

Each crate builds from its package alone, as `cargo install` builds it:

- The daemon's migrations are in `apps/piqueld/migrations`, inside its crate.
- The daemon's build script builds the health probe from the `piqueld-probe`
  crate, whose build script exports its directory as `DEP_PIQUELD_PROBE_DIR`,
  with the `probe` profile of `apps/piqueld/build/probe.toml`. This works the
  same from the workspace and from crates.io.
- The published daemon can't build the dashboard, so it ships it prebuilt; see
  the [dashboard architecture](architecture/dashboard.md#production-assets).
- `piqueld` and `piquelctl` leave out their tests and examples, which read
  files from across the repository, through `include` in their manifests.

`just publish`, which runs `cargo xtask publish`, builds the dashboard into
`apps/piqueld/dashboard/`, runs `cargo publish --workspace`, which publishes
the crates in dependency order, then removes the dashboard. The `package` CI
job runs it with `--dry-run` on every pull request that changes the crates,
once it is ready for review: it packages each crate and builds it from its
package, without uploading anything. Run it yourself after changing a
manifest, a build script, or a file a crate reads at compile time:

```console
just publish --dry-run --allow-dirty
```

## Release binaries

Each GitHub release has the archive the install script,
[`apps/site/install.sh`](https://github.com/sourceorigin-dev/piqueld/blob/main/apps/site/install.sh),
downloads, `piqueld-<version>-x86_64-linux.tar.gz`, and its `SHA256SUMS`. The
archive holds the daemon, with its dashboard, `piquelctl`, the systemd unit
[`apps/piqueld/piqueld.service`](https://github.com/sourceorigin-dev/piqueld/blob/main/apps/piqueld/piqueld.service),
and the license. The binaries link musl and OpenSSL statically, so they run on
any x86_64 Linux: the daemon builds OpenSSL from source for musl targets.

`just dist`, which runs `cargo xtask dist`, builds them into `target/dist`. It
needs `musl-gcc`, from the `musl-tools` package on Debian and Ubuntu, for the
C code of OpenSSL, SQLite and ring. The `install` CI job runs it on every pull
request that changes what the archive is built from, once it is ready for
review, then installs the archive with the script on the runner, which has
systemd and Docker Engine, and installs it again as an update. The script
reads the archive from `PIQUELD_RELEASE_URL` there, instead of the release.

The release workflow,
[`.github/workflows/release.yml`](https://github.com/sourceorigin-dev/piqueld/blob/main/.github/workflows/release.yml),
runs on each `v<version>` tag. It builds the archive and creates the GitHub
release with it, with notes listing the pull requests merged since the
previous release. The release is published only once its files are uploaded,
so the script always finds the latest release's archive.

## Set up publishing

Do this once, on the machine you release from:

1. On crates.io, open **Account Settings**, then **API Tokens**, and create a
   token with the `publish-new` and `publish-update` scopes, limited to the
   crates `piqueld*` and `piquelctl`.
2. Save it for Cargo:

   ```console
   cargo login
   ```

You also need the Rust toolchain of `rust-toolchain.toml` with its targets,
and the [GitHub CLI](https://cli.github.com) signed in to the repository.

## Release a version

1. Open a pull request that sets the new version in the root `Cargo.toml`:
   `version` under `[workspace.package]`, and the `version` of each internal
   dependency under `[workspace.dependencies]`. Then run
   `cargo update --workspace` to update `Cargo.lock`. The Nix packages read the
   version from `Cargo.toml`.
2. Merge it, and check out the merge commit:

   ```console
   git switch main && git pull
   ```

3. Check the release without changing anything:

   ```console
   just release --dry-run
   ```

   It runs every check below, and packages and builds each crate without
   uploading it, then prints the commands the other steps would run.
4. Release it:

   ```console
   just release
   ```

   It checks that you are on the latest `main` with a clean working tree, and
   that the version isn't released yet. It then publishes the five crates in
   dependency order, which takes several minutes, tags the commit
   `v<version>`, and pushes the tag. Last, it closes the GitHub milestone of
   the minor version, such as `v0.2` for 0.2.0. A patch release finds it
   already closed.

5. Wait for the tag's
   [release workflow](https://github.com/sourceorigin-dev/piqueld/actions/workflows/release.yml)
   to build the release binaries and create the GitHub release, which takes
   about as long as the `install` CI job. For a release that breaks existing
   setups, then edit the release on GitHub to say how to upgrade.

## Check that it works

The crates' pages on crates.io list the new version. On a machine with Rust,
install it:

```console
cargo install --locked piquelctl
piquelctl --version
```

docs.rs builds the new `piqueld-client` documentation within a few minutes.

The GitHub release lists the archive and `SHA256SUMS`. On a disposable Linux
host with systemd and Docker Engine, the install script installs the new
version. On a host that already runs piqueld, it replaces the service's unit
and restarts the daemon, so keep your changes to the unit in a drop-in:

```console
curl -fsSL https://piqueld.dev/install.sh | sh
piquelctl --version
```

## Troubleshooting

- **Publishing fails partway.** The crates published before the failure stay
  published, and crates.io refuses to publish a version twice, so
  `just release` can't run again. Fix the cause, publish the remaining crates
  with `--exclude` for each one already published, then tag the commit and
  push the tag, which starts the release workflow. Then close the milestone
  on GitHub:

  ```console
  just publish --exclude piqueld-probe --exclude piqueld-core
  git tag --annotate v0.2.0 --message "piqueld 0.2.0"
  git push origin v0.2.0
  ```

- **Tagging fails.** The crates are published. Run the `git` command that
  failed again, then the ones above that come after it, and close the
  milestone on GitHub.
- **The release workflow fails.** The crates are published and the tag is
  pushed, but the GitHub release doesn't exist yet, so the install script
  still installs the previous one. If it failed for a passing reason, such as
  a network error, re-run it from its page on GitHub. A re-run uses the
  workflow and code of the tag, so any other failure needs a fix, then a new
  patch release.

- **A published version is broken.** A version can't be replaced. Yank it
  with `cargo yank --version 0.2.0 <crate>` for each crate, so new installs
  skip it, and release a fixed patch version.
