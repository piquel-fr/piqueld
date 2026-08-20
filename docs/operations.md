# Single-host operations

`piqueld` is a privileged, single-host control plane. Membership in its Unix
socket group, possession of its bearer token, and Docker-socket access are all
host-administrative capabilities. Service hardening narrows accidental access;
Docker access is not a sandbox boundary.

## NixOS installation

Import `nixosModules.default`, enable `services.piqueld`, and keep both
listeners private:

```nix
services.piqueld = {
  enable = true;
  package = inputs.piqueld.packages.${pkgs.system}.piqueld;
  installCli = true;
  credentials.masterKeyFile = "/run/keys/piqueld-master-key";
  credentials.bearerTokenFile = "/run/keys/piqueld-bearer-token";
};
```

Credential files are root-owned private regular files outside `/nix/store`.
The master key is exactly 32 random bytes. The module supplies credentials as
systemd credentials, so generated TOML contains only credential names. Rotating
the bearer token requires a restart; rotating the encryption key is not a
supported migration, so retain it with every database backup.

The module creates the service user, state/runtime directories, embedded
database, loopback registry, UI asset path, and hardened unit. It opens no
firewall ports. Application volumes and registry data require separate backups.

## Private access

Prefer `/run/piqueld/piqueld.sock`; its `0660` mode is the local authentication
boundary. Loopback HTTP denies every request unless a bearer credential is
configured or trusted Tailscale mode is explicitly enabled. For Tailscale
Serve, proxy only to `127.0.0.1:7845`, strip incoming identity headers, and add
identity only from authenticated Tailscale metadata. Set both
`trustedLoopbackProxy` and `trustTailscaleHeaders`. Never expose the listener
through a generic forward proxy.

Cloudflare Tunnel should route public application hostnames only to the private
Traefik origin. It must never route to the control API, registry, Docker socket,
or a Traefik dashboard. The module does not manage Cloudflare or Tailscale
accounts.

## Readiness, metrics, and recovery

`/api/v1/system/health` proves only that the event loop answers. Readiness
checks SQLite, Docker, single-manager Swarm state, the registry, and managed
ingress without creating resources. A readiness failure returns `503` but does
not stop the process. Optional metrics expose only process/dependency gauges;
they persist no time series and use no application names or secrets as labels.

On restart, interrupted durable operations become recoverable and the
reconciler retries idempotent work while transient dependency failures are
logged and retried. Inspect readiness and `journalctl -u piqueld` before
repairing state. Do not delete the database, registry, or named volumes as a
repair shortcut.

Before upgrades, export state and stop writes. Back up the stopped SQLite
database (including WAL/SHM), matching master key, registry blob directory,
and separately managed application volumes. Migrations are forward-only and
transactional at startup; do not downgrade a migrated database.

The prototype provides no database replication, revision rollback, registry or
volume backup, or multi-node disaster recovery. Operators must provide offline
backups and test restores.
