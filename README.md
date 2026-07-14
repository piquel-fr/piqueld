# piqueld

A pure Rust infrastructure control plane. The repository currently contains the
workspace foundation for the first single-node Docker Swarm prototype.

## Development

Enter the reproducible shell with `nix develop`, or use a Rust 1.96-or-newer
toolchain directly. The standard verification commands are:

```console
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test --doc --workspace
cargo deny check
./scripts/check-dependency-boundaries.sh
nix flake check
```

The daemon reads `/etc/piqueld/config.toml` by default; set `PIQUELD_CONFIG` to an
alternative read-only host configuration. Application intent does not belong in
this file. See `docs/architecture/dependency-flow.md` for crate boundaries and
`docs/architecture/0001-sqlx-sqlite.md` for database ownership.
