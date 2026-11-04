# Releasing piqueld

A release publishes the daemon, `piquelctl` and `piqueld-client` to
[crates.io](https://crates.io/crates/piqueld), with the two crates they depend
on, then tags the commit and creates a GitHub release from its changelog
entry. A maintainer runs each step by hand.

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

The crates build with Rust 1.99 or newer: `rust-version` under
`[workspace.package]`, which the `msrv` CI job checks. Raising it is a change
users notice, so it goes in the changelog.

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

1. Open a pull request that prepares the release:
   - Set the new version in the root `Cargo.toml`: `version` under
     `[workspace.package]`, and the `version` of each internal dependency
     under `[workspace.dependencies]`. Then run `cargo update --workspace` to
     update `Cargo.lock`. The Nix packages read the version from `Cargo.toml`.
   - In `CHANGELOG.md`, move the entries under **Unreleased** into a new
     section, `## <version> - <date>`, such as `## 0.2.0 - 2027-01-15`.
2. Merge it, and check out the merge commit:

   ```console
   git switch main && git pull
   ```

3. Check that every crate packages and builds, without uploading anything:

   ```console
   just publish --dry-run
   ```

4. Publish the crates:

   ```console
   just publish
   ```

   It builds the dashboard, publishes the five crates in dependency order, and
   takes several minutes.
5. Tag the commit, and push the tag:

   ```console
   git tag --annotate v0.2.0 --message "piqueld 0.2.0"
   git push origin v0.2.0
   ```

6. Create the GitHub release, with the version's changelog entry as its notes:

   ```console
   gh release create v0.2.0 --verify-tag --title "piqueld 0.2.0" --notes-file <notes>
   ```

## Check that it works

The crates' pages on crates.io list the new version. On a machine with Rust,
install it:

```console
cargo install --locked piquelctl
piquelctl --version
```

docs.rs builds the new `piqueld-client` documentation within a few minutes.

## Troubleshooting

- **Publishing fails partway.** The crates published before the failure stay
  published, and crates.io refuses to publish a version twice. Fix the cause,
  then publish the remaining crates, with `--exclude` for each one already
  published:

  ```console
  just publish --exclude piqueld-probe --exclude piqueld-core
  ```

- **A published version is broken.** A version can't be replaced. Yank it
  with `cargo yank --version 0.2.0 <crate>` for each crate, so new installs
  skip it, and release a fixed patch version.
