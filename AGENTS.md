# piqueld

A pure Rust infrastructure control plane. The repository currently contains the
workspace foundation for the first single-node Docker Swarm prototype.

## Validation

When you are done making changes, make sure these commands run without errors:
* `just`

## Testing

Skills in `.agents/skills` (linked from `.claude/skills`) describe the testing
workflow: `piqueld-dev` runs this worktree's isolated development instance
(`just dev`, see `docs/development.md`) on its own localhost port,
`piqueld-e2e` exercises a change through the manifest, CLI, and dashboard
preview, and `piqueld-verify` selects focused automated checks.

## Commits & Pull Requests

Titles should follow the `feat(server): add docker swarm reconciliation` convention.

## Adding features

**piqueld** has three different surfaces to access its features: the manifest,
the CLI and the dashboard. When adding a feature to one, make sure it is
appropriately added to all of them.
For example, if we add a new setting for applications, that setting should be
present in the manifest and editable through the CLI and the dashboard.

When changing/adding to configuration, make sure to update the NixOS module.
