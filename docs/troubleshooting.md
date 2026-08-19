# Troubleshooting

Start with the daemon's liveness and readiness endpoints. Liveness only proves
the process is serving; readiness also checks SQLite, Docker, Swarm-manager
state, registry, and managed ingress:

```console
curl --unix-socket /run/piqueld/piqueld.sock http://localhost/api/v1/system/health
curl --unix-socket /run/piqueld/piqueld.sock http://localhost/api/v1/system/readiness
```

If readiness is degraded, inspect `journalctl -u piqueld`, `docker info`,
`docker node ls`, the configured loopback registry, and the managed
`piqueld-traefik` service. Do not delete managed resources to make a symptom
disappear; an ownership conflict is a safety result and needs investigation.

Common cases:

- **401 on loopback HTTP:** configure a protected bearer credential and pass it
  through `piquelctl --token-file`; the Unix socket remains the local admin path.
- **A service is degraded:** use `piquelctl show NAME --json`
  and inspect the operation/status diagnostics, then verify image digest,
  network, volume, secret, and health-check availability.
- **A build is stuck:** inspect the operation and build endpoints/CLI output,
  verify Git credentials and registry readiness, and wait for the bounded
  operation timeout before retrying.
- **A route is unavailable:** check that the host is present in the manifest, the
  target port is declared, the managed ingress service is ready, and the
  configured origin port is reachable. Cloudflare/DNS/Tailscale are outside the
  daemon and must be checked separately.
- **A delete leaves a volume:** this is intentional. Export control-plane state
  and remove the volume only after data has been copied and no task/container
  still uses it.

For a clean qualification, use an isolated disposable Docker daemon and the
commands in [`acceptance-runbook.md`](acceptance-runbook.md). Never run ignored
Docker tests against a production daemon.
