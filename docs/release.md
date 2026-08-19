# Release procedure

Release builds are produced by the Nix `release` package. It contains the
`piqueld` daemon, `piquelctl`, and the production UI under
`share/piqueld/ui`:

```console
nix build .#release -L --no-update-lock-file --out-link /tmp/piqueld-release
/tmp/piqueld-release/bin/piqueld --version
/tmp/piqueld-release/bin/piquelctl --version
```

Create the deterministic operator archive from a clean checkout:

```console
./scripts/package-release.sh /tmp/piqueld-release artifacts
(cd artifacts && sha256sum --check SHA256SUMS)
```

The archive records its format, version, target, commit, source-date epoch,
`Cargo.lock` hash, `flake.lock` hash, and Nix attribute. Tar ordering, ownership,
timestamps, gzip headers, and permissions are normalized. The release workflow
builds twice under different umasks and compares the results.

The split Nix attributes are `piqueld`, `piquelctl`, and `piqueld-ui`; use them
when a host packages the daemon, CLI, or immutable assets separately. Do not put
master keys, bearer tokens, Git tokens, or generated state archives in the Nix
store or source repository.
