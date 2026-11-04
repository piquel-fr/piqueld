<!-- Links are absolute, so they also work on crates.io, which shows this file. -->
# piqueld

[![CI](https://github.com/piquel-fr/piqueld/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/piquel-fr/piqueld/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/piqueld.svg)](https://crates.io/crates/piqueld)
[![docs.rs](https://img.shields.io/docsrs/piqueld-client?label=piqueld-client)](https://docs.rs/piqueld-client)

piqueld is a control plane for one Docker Engine running a single-node Swarm,
written in Rust. You describe an application in a manifest; piqueld pulls or
builds its images, deploys it, routes HTTPS traffic to it, and keeps it
running. The `piqueld` daemon is managed through its dashboard, the `piquelctl`
CLI, or its HTTP API.

- **Deployments:** prebuilt images or Git builds, environments, releases and
  promotion, branch previews, and deployment on push.
- **Networking:** managed HTTPS routes through Caddy, private routes on your
  tailnet, Cloudflare Tunnel, and DNS records managed through Cloudflare or OVH.
- **Operations:** secrets, volumes, one-shot jobs, logs, notifications,
  telemetry export, and a tamper-evident audit trail.
- **Access:** passkey accounts, scoped API tokens, and per-application
  permissions.

Registry credentials, arbitrary TCP/UDP ports, and multi-node clusters are not
supported yet.

## Installation

piqueld runs on an x86_64 Linux host with Docker Engine. On NixOS, import the
flake's module; elsewhere, install the daemon and the CLI from crates.io:

```console
rustup target add x86_64-unknown-linux-musl
cargo install --locked piqueld piquelctl
```

[Installing piqueld](https://github.com/piquel-fr/piqueld/blob/main/docs/user/getting-started/install.md) covers both, and
setting the daemon up as a service. To script the API from Rust, use the
[`piqueld-client`](https://docs.rs/piqueld-client) crate.

## Documentation

The [documentation](https://github.com/piquel-fr/piqueld/blob/main/docs/README.md) has a [user guide](https://github.com/piquel-fr/piqueld/blob/main/docs/user/introduction.md)
for running piqueld and deploying to it, and a
[developer guide](https://github.com/piquel-fr/piqueld/blob/main/docs/dev/getting-started.md) for changing it.

## Contributing

See [CONTRIBUTING.md](https://github.com/piquel-fr/piqueld/blob/main/CONTRIBUTING.md), and the
[code of conduct](https://github.com/piquel-fr/piqueld/blob/main/CODE_OF_CONDUCT.md). Questions and feedback go to
[contact@piqueld.dev](mailto:contact@piqueld.dev).

## Security

Report vulnerabilities privately to
[security@piqueld.dev](mailto:security@piqueld.dev); see
[SECURITY.md](https://github.com/piquel-fr/piqueld/blob/main/SECURITY.md).

## License

piqueld is licensed under the [Apache License 2.0](https://github.com/piquel-fr/piqueld/blob/main/LICENSE).
