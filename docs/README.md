# piqueld

piqueld is a control plane for one Docker Engine running a single-node Swarm.
You describe an application in a manifest; piqueld pulls or builds its images,
deploys it, routes HTTPS traffic to it, and keeps it running. You manage it
through its dashboard, the `piquelctl` CLI, or its HTTP API.

This documentation has two parts:

- The [user guide](user/introduction.md) is for people who run piqueld and
  deploy applications to it: installation, accounts, guides for each feature,
  and the reference for the manifest, the daemon configuration, `piquelctl`,
  and the API.
- The [developer guide](dev/getting-started.md) is for people who change
  piqueld itself: development setup, testing, architecture, and how to extend
  it.

## Contact

- Questions and feedback: [contact@piqueld.dev](mailto:contact@piqueld.dev)
- Security vulnerabilities: [security@piqueld.dev](mailto:security@piqueld.dev).
  Report them privately, never in a public issue; see the
  [security policy](https://github.com/piquel-fr/piqueld/blob/main/SECURITY.md).
