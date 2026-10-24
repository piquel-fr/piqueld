# Summary

[piqueld](README.md)

# Getting started

- [Introduction](user/introduction.md)
- [Installing piqueld](user/getting-started/install.md)
- [Creating your account](user/getting-started/first-account.md)
- [Deploying your first application](user/getting-started/first-app.md)

# User guide

- [Configuring services](user/applications/services.md)
- [Secrets and variables](user/applications/secrets-and-variables.md)
- [Volumes and files](user/applications/volumes-and-files.md)
- [Running jobs before a rollout](user/applications/jobs.md)
- [Connecting applications](user/applications/links.md)
- [Running commands in a container](user/applications/commands.md)
- [Accounts and sign-in](user/accounts/authentication.md)
- [Permissions](user/accounts/authorization.md)
- [Dashboard](user/dashboard.md)
- [HTTP ingress](user/guides/ingress.md)
- [Observability](user/guides/observability.md)
- [Backups and upgrades](user/guides/backups-and-upgrades.md)

# Reference

- [Application manifest](user/reference/manifest.md)
- [Daemon configuration](user/reference/configuration.md)
- [NixOS module](user/reference/nixos.md)
- [piquelctl](user/reference/cli.md)
- [HTTP API](user/reference/api.md)

# Developer guide

- [Getting started](dev/getting-started.md)
- [Quickstart](dev/quickstart.md)
- [Development instances](dev/development.md)
- [Automated validation](dev/validation.md)
- [Contract coverage](dev/contract-coverage.md)
- [Changing the documentation](dev/documentation.md)

# Architecture

- [Module boundaries](dev/architecture/dependency-flow.md)
- [Persistence](dev/architecture/persistence.md)
- [Resource planning](dev/architecture/resource-planning.md)
- [Docker reconciliation](dev/architecture/docker-reconciliation.md)
- [Authentication](dev/architecture/authentication.md)
- [Dashboard](dev/architecture/dashboard.md)
- [ADR 0001: SQLx SQLite persistence](dev/architecture/adr/0001-sqlx-sqlite.md)
- [ADR 0002: Cargo builds the dashboard](dev/architecture/adr/0002-cargo-built-dashboard.md)
