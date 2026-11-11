# piqueld

A pure Rust infrastructure control plane for a single-node Docker Swarm: the
`piqueld` daemon, the `piquelctl` CLI, and the Leptos dashboard. See
`docs/README.md` for the documentation index.

## Testing

The `piqueld-dev` skill in `.agents/skills` (linked from `.claude/skills`)
runs this worktree's isolated development instance (`just dev`, see
`docs/dev/development.md`) on its own localhost port, for testing changes through
the manifest, CLI, and dashboard preview.

## Commits & Pull Requests

Titles should follow the `feat(server): add docker swarm reconciliation` convention.

Open pull requests as drafts and mark them ready for review once the work is
done. Drafts run only the quick CI jobs; marking a pull request ready runs the
slow ones (Docker, Playwright, macOS), which it must pass
before merging. Release builds (Nix, packaging, install) run after merging; see
"Release checks" in `docs/dev/validation.md` to run them on a branch that
changes a manifest, build script, the flake or `install.sh`.

`main` merges through a merge queue: `gh pr merge` adds a pull request to it,
and the queue reruns CI on top of the latest main. Never merge with `--admin`.

`docs/dev/releasing.md` describes versions and releases.

## Adding features

**piqueld** has three different surfaces to access its features: the manifest,
the CLI and the dashboard. When adding a feature to one, make sure it is
appropriately added to all of them.
For example, if we add a new setting for applications, that setting should be
present in the manifest and editable through the CLI and the dashboard.

When changing/adding to configuration, make sure to update the NixOS module.

## Documentation

The documentation in `docs/` is an mdBook with a user guide (`docs/user`) and a
developer guide (`docs/dev`). Update it in the same pull request as the change
it describes, following `docs/dev/documentation.md`, and run `just docs` to
build it and check its links. The `piqueld-docs` skill in `.agents/skills`
points agents to that guide.
