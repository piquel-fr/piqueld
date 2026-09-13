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
The CLI is installed by default; set `installCli = false` to omit it.

`dataDir` defaults to `/var/lib/piqueld`, with mode 0700. Its derived socket is
`piqueld.sock` (0600) and database is `piqueld.db`. Run the local CLI as root or
as the service user; membership in a socket group does not grant access.
The module enables Docker and grants the service user Docker access, which is
host-administrative authority. No firewall ports are opened. TCP defaults to
loopback and still has no authentication; do not expose it publicly.

`settings` contains current daemon TOML settings; the module always supplies
`server.data_dir` from `dataDir`. It does not configure a registry, Traefik,
authentication, or an external UI directory. Git, SSH, and Docker executables
are present on the service PATH. Configure SSH credentials and known hosts for
the service user, not the interactive operator; host home directories are protected.
Never put credential values into Nix settings, which are stored in the Nix store.

Validate service startup, private paths, Swarm initialization, CLI access, and
restart with `nix build .#checks.x86_64-linux.nixos-service` (requires KVM or
software virtualization). Configuration and package changes require rebuilding
NixOS; application state remains under `dataDir`.
