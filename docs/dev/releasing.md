# Releasing piqueld

A release publishes the daemon, `piquelctl` and `piqueld-client` to
[crates.io](https://crates.io/crates/piqueld), with the two crates they depend
on, and creates a GitHub release from its changelog entry. Pushing a version
tag starts it; the
[release workflow](https://github.com/piquel-fr/piqueld/blob/main/.github/workflows/release.yml)
does the rest.

## Versions

Every crate shares the workspace version, `version` under
`[workspace.package]` in the root `Cargo.toml`, and they are released
together. A daemon and a client of the same version speak the same API.

Versions follow [semantic versioning](https://semver.org). Before 1.0:

- a minor release, such as 0.2.0, may change the manifest, the daemon
  configuration, `piquelctl`, the HTTP API or the Rust crates in ways that
  break existing users. Its changelog entry says how to upgrade.
- a patch release, such as 0.1.1, only fixes bugs: upgrading to it needs no
  changes.

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

`cargo xtask publish` builds the dashboard into `apps/piqueld/dashboard/`, runs
`cargo publish --workspace`, which publishes the crates in dependency order,
then removes the dashboard. The `package` CI job runs
`cargo xtask publish --dry-run` on every pull request that changes the crates,
once it is ready for review. It packages each crate and builds it from its
package, without uploading anything. Run it yourself after changing a
manifest, a build script, or a file a crate reads at compile time:

```console
cargo xtask publish --dry-run --allow-dirty
```

## Set up publishing

Do this once, before the first release.

1. On crates.io, open **Account Settings**, then **API Tokens**, and create a
   token with the `publish-new` and `publish-update` scopes, limited to the
   crates `piqueld*` and `piquelctl`.
2. In the repository's **Settings** on GitHub, open **Environments** and create
   an environment named `crates-io`. Limit its deployments to tags matching
   `v*`, and add the token as the secret `CARGO_REGISTRY_TOKEN`.

Once the crates exist, crates.io can trust the release workflow directly
instead, through
[trusted publishing](https://crates.io/docs/trusted-publishing), so no
long-lived token is stored.

## Release a version

1. Open a pull request that prepares the release:
   - Set the new version in the root `Cargo.toml`: `version` under
     `[workspace.package]`, and the `version` of each internal dependency
     under `[workspace.dependencies]`. Then run `cargo update --workspace` to
     update `Cargo.lock`. The Nix packages read the version from `Cargo.toml`.
   - In `CHANGELOG.md`, move the entries under **Unreleased** into a new
     section, `## <version> - <date>`, such as `## 0.2.0 - 2027-01-15`.
2. Merge it.
3. Tag the merge commit, and push the tag:

   ```console
   git switch main && git pull
   git tag --annotate v0.2.0 --message "piqueld 0.2.0"
   git push origin v0.2.0
   ```

The release workflow checks that the tag matches the workspace version and
that the changelog has an entry for it. It then runs `cargo xtask publish`,
and creates the GitHub release with the changelog entry as its notes.

## Check that it works

The release's **Actions** run succeeded, and its GitHub release lists the
changelog entry. On a machine with Rust, install the new version:

```console
cargo install --locked piquelctl
piquelctl --version
```

docs.rs builds the new `piqueld-client` documentation within a few minutes.

## Troubleshooting

- **The workflow fails before publishing.** The tag doesn't match the
  workspace version, or the changelog has no entry for it. Nothing was
  published: delete the tag with `git push origin :refs/tags/v0.2.0` and
  `git tag --delete v0.2.0`, fix the version or changelog in a pull request,
  and tag again.
- **Publishing fails partway.** The crates published before the failure stay
  published, and crates.io refuses to publish a version twice, so the workflow
  can't be run again. Fix the cause, then publish the remaining crates from the
  tag, with `--exclude` for each one already published, and create the GitHub
  release by hand:

  ```console
  git switch --detach v0.2.0
  CARGO_REGISTRY_TOKEN=<token> cargo xtask publish --exclude piqueld-probe --exclude piqueld-core
  gh release create v0.2.0 --verify-tag --title "piqueld 0.2.0" --notes "<the changelog entry>"
  ```

- **A published version is broken.** A version can't be replaced. Yank it
  with `cargo yank --version 0.2.0 <crate>` for each crate, so new installs
  skip it, and release a fixed patch version.
