# piqueld

A pure Rust infrastructure control plane. The repository currently contains the
workspace foundation for the first single-node Docker Swarm prototype.

## Validation

When you are done making changes, make sure these commands run without errors:
* `just`

## Commits & Pull Requests

Titles should follow the `feat(server): add docker swarm reconciliation` convention.

## Adding features

**piqueld** has three different surfaces to access its features: the manifest,
the CLI and the dashboard. When adding a feature to one, make sure it is
appropriately added to all of them.
For example, if we add a new setting for applications, that setting should be
present in the manifest and editable through the CLI and the dashboard.
