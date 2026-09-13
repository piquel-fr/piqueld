# NixOS service

Import the flake module and enable the service:

```nix
{
  imports = [ inputs.piqueld.nixosModules.default ];
  services.piqueld.enable = true;
}
```

The default package embeds the dashboard. `services.piqueld.package` can select
`inputs.piqueld.packages.${pkgs.system}.daemon` for API-only operation.
The CLI is installed by default when the daemon is enabled; set
`installCli = false` to omit it. For a remote-management machine with only the
CLI, import the module and set `services.piqueld.installCli = true;` while
leaving `services.piqueld.enable = false;`. This does not enable Docker or
create the daemon service or user.

`dataDir` defaults to `/var/lib/piqueld`, with mode 0700. Its derived socket is
`piqueld.sock` (0600) and database is `piqueld.db`. Run the local CLI as root or
as the service user; membership in a socket group does not grant access.
The module enables Docker and grants the service user Docker access, which is
host-administrative authority. No firewall ports are opened. TCP is disabled by
default. Opt in with `settings.server.http_listen = "127.0.0.1:7845";` only
when all local users are trusted: the HTTP API has no authentication.

`settings` declares typed options for `server.http_listen`, `docker.socket`,
`docker.auto_initialize_swarm`, all three `reconciliation` intervals/timeouts,
and both `retention` periods, with the daemon's defaults except for disabled
TCP. Reconciliation values must be 1–86400 seconds; retention values are
nonnegative days, with zero disabling pruning. Unknown settings are rejected.
The module always supplies
`server.data_dir` from `dataDir`. It does not configure a registry, Traefik,
authentication, or an external UI directory. Git, SSH, and Docker executables
are present on the service PATH. Configure SSH credentials and known hosts for
the service user, not the interactive operator; host home directories are protected.
Never put credential values into Nix settings, which are stored in the Nix store.

Validate service startup, private paths, Swarm initialization, CLI access, and
restart with `nix build .#checks.x86_64-linux.nixos-service` (requires KVM or
software virtualization). Configuration and package changes require rebuilding
NixOS; application state remains under `dataDir`.
