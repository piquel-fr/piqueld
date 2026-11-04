# Changelog

Every release of piqueld, newest first. Versions follow
[semantic versioning](https://semver.org): before 1.0, a minor release may
break existing setups and says how to upgrade. See
[releasing piqueld](docs/dev/releasing.md).

Add an entry under **Unreleased** in the pull request that makes a change
users will notice.

## Unreleased

## 0.1.0 - 2026-11-04

The first release of piqueld: the `piqueld` daemon, the `piquelctl` CLI, and
the `piqueld-client` Rust crate, on crates.io and as a NixOS module. Building
them needs Rust 1.95 or newer.

- **Deployments:** applications described in a manifest, from prebuilt images
  or Git builds. Environments, releases and promotion, branch previews, jobs
  before a rollout, restoring an earlier deployment, and deploying on push.
- **Networking:** HTTPS routes through Caddy, wildcard routes with DNS-01
  certificates, private routes on your tailnet, Cloudflare Tunnel, DNS records
  managed through Cloudflare or OVH, and webhooks.
- **Operations:** secrets and variables, volumes and files, commands in
  running containers, logs, notifications, telemetry export over OTLP, and a
  tamper-evident audit trail.
- **Access:** passkey accounts, scoped API tokens, per-application
  permissions, and CLI sign-in from another machine.
- **Surfaces:** every feature through the dashboard, `piquelctl`, and the
  manifest, and the versioned HTTP API under `/api/v1`.
