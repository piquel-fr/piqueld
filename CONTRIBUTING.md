# Contributing

Thanks for helping with piqueld. The [developer guide](docs/dev/getting-started.md)
covers setting up the toolchain, running a development instance, and the
validation every change must pass. Everyone taking part follows the
[code of conduct](CODE_OF_CONDUCT.md).

Look for issues labelled
[`good first issue`](https://github.com/piquel-fr/piqueld/labels/good%20first%20issue)
to start with. Before working on a larger change, open an issue to discuss it.

- Open pull requests as drafts, and mark them ready for review once the work is
  done. Drafts run only the quick CI jobs; marking a pull request ready runs the
  slow ones (Docker, Playwright, macOS, Nix, packaging), which it must pass
  before merging.
- Titles follow conventional commits, such as
  `feat(server): add docker swarm reconciliation`.
- A feature is reachable through the manifest, `piquelctl`, and the dashboard;
  a change to one updates the others. Configuration changes also update the
  NixOS module.
- Update the documentation in the same pull request; see
  [changing the documentation](docs/dev/documentation.md).
- Add an entry to [CHANGELOG.md](CHANGELOG.md) under **Unreleased** for a
  change users will notice. [Releasing piqueld](docs/dev/releasing.md)
  describes versions and releases.

Report security vulnerabilities privately; see [SECURITY.md](SECURITY.md).
Other questions go to [contact@piqueld.dev](mailto:contact@piqueld.dev).
