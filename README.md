# piqueld

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

## Documentation

The [documentation](docs/README.md) has a [user guide](docs/user/introduction.md)
for running piqueld and deploying to it, and a
[developer guide](docs/dev/getting-started.md) for changing it.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Questions and feedback go to
[contact@piqueld.dev](mailto:contact@piqueld.dev).

## Security

Report vulnerabilities privately to
[security@piqueld.dev](mailto:security@piqueld.dev); see
[SECURITY.md](SECURITY.md).

## License

piqueld is licensed under the [Apache License 2.0](LICENSE).
