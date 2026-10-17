# piqueld

A pure Rust infrastructure control plane for a single-node Docker Swarm: the
`piqueld` daemon, the `piquelctl` CLI, and the Leptos dashboard. See
`README.md` for the documentation index.

## Testing

The `piqueld-dev` skill in `.agents/skills` (linked from `.claude/skills`)
runs this worktree's isolated development instance (`just dev`, see
`docs/development.md`) on its own localhost port, for testing changes through
the manifest, CLI, and dashboard preview.

## Commits & Pull Requests

Titles should follow the `feat(server): add docker swarm reconciliation` convention.

Open pull requests as drafts and mark them ready for review once the work is
done. Drafts run only the quick CI jobs; marking a pull request ready runs the
slow ones (Docker, Playwright, macOS, Nix), which it must pass before merging.

## Adding features

**piqueld** has three different surfaces to access its features: the manifest,
the CLI and the dashboard. When adding a feature to one, make sure it is
appropriately added to all of them.
For example, if we add a new setting for applications, that setting should be
present in the manifest and editable through the CLI and the dashboard.

When changing/adding to configuration, make sure to update the NixOS module.
