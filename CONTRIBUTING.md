# Contributing

Thanks for helping with piqueld. The [developer guide](docs/dev/getting-started.md)
covers setting up the toolchain, running a development instance, and the
validation every change must pass.

- Open pull requests as drafts, and mark them ready for review once the work is
  done. Drafts run only the quick CI jobs; marking a pull request ready runs the
  slow ones (Docker, Playwright, macOS, Nix), which it must pass before merging.
- Titles follow conventional commits, such as
  `feat(server): add docker swarm reconciliation`.
- A feature is reachable through the manifest, `piquelctl`, and the dashboard;
  a change to one updates the others. Configuration changes also update the
  NixOS module.
- Update the documentation in the same pull request; see
  [changing the documentation](docs/dev/documentation.md).

Report security vulnerabilities privately; see [SECURITY.md](SECURITY.md).
Other questions go to [contact@piqueld.dev](mailto:contact@piqueld.dev).
