# NixOS service

Import the flake module and enable the service:

```nix
{
  imports = [ inputs.piqueld.nixosModules.default ];
  services.piqueld.enable = true;
  users.users.alice.extraGroups = [ "piqueld" ];
}
```

The default package embeds the dashboard. `services.piqueld.package` can select
`inputs.piqueld.packages.${pkgs.system}.daemon` for API-only operation.
The default module installs the CLI when the daemon is enabled; set
`programs.piquelctl.enable = false` to omit it. For a remote-management machine
with only the CLI, import `inputs.piqueld.nixosModules.piquelctl` and enable
`programs.piquelctl`. This module does not enable Docker or create the daemon
service or user.

Connection profiles use the same typed-settings style as the daemon:

```nix
{
  imports = [ inputs.piqueld.nixosModules.piquelctl ];
  programs.piquelctl = {
    enable = true;
    settings.profiles.production = {
      url = "http://127.0.0.1:7845";
      timeout = "2m";
    };
  };
}
```

Each profile must set exactly one of `socket` or `url`; `timeout` is optional.
The generated profiles file is selected by default while preserving the CLI's
`--profiles-file` and `PIQUELD_PROFILES_FILE` overrides.

`dataDir` defaults to `/var/lib/piqueld`, with mode `0700`, holding `piqueld.db`.
`runtimeDir` defaults to `/run/piqueld`, prepared by systemd with mode `0750`.
The socket is `/run/piqueld/piqueld.sock`, owned by `piqueld:piqueld` with mode
`0660`. After logging in again to refresh group membership, operators can run
`piquelctl status` without sudo. Membership grants full deployment/operator
access, but no direct access to private state or permission to replace the socket.
Only the existing `piqueld.service` is needed; no proxy or socket unit is required.

Custom `runtimeDir` values must be dedicated directories below `/run`. Configure
CLI profiles or `--socket` explicitly when overriding this default.
The module enables Docker and grants the service user Docker access, which is
host-administrative authority. No firewall ports are opened. TCP is disabled by
default. Use `settings.server.listen_mode = "localhost";` for local HTTP,
or configure Tailscale explicitly:

```nix
services.tailscale.enable = true;
services.piqueld.settings.server = {
  listen_mode = "tailscale"; # or "both" to also serve localhost
  port = 7845;
};
```

Join the host to your tailnet separately. The module supplies the Tailscale CLI
and orders piqueld after `tailscaled` for these modes; it does not enable or
configure Tailscale itself. Ordering does not guarantee connectivity: if
Tailscale is unavailable at startup, piqueld warns and requires a restart to
activate remote listening. Ensure your firewall and tailnet policy permit the
selected port on the Tailscale interface. Every reachable caller is trusted:
the HTTP API has no authentication.

`settings` declares typed options for `server.listen_mode`, `server.port`, `docker.socket`,
`docker.auto_initialize_swarm`, all three `reconciliation` intervals/timeouts,
and both `retention` periods, with the daemon's defaults. Reconciliation values must be 1–86400 seconds; retention values are
nonnegative days, with zero disabling pruning. Unknown settings are rejected.
The module always supplies
`server.data_dir` from `dataDir` and `server.runtime_dir` from `runtimeDir`. It does not configure a registry, Traefik,
authentication, or an external UI directory. Git, SSH, and Docker executables
are present on the service PATH. Configure SSH credentials and known hosts for
the service user, not the interactive operator; host home directories are protected.
Never put credential values into Nix settings, which are stored in the Nix store.

Nix packages and development shells support x86_64 Linux. Configuration and
package changes require rebuilding NixOS; application state remains under
`dataDir`.

The focused permissions integration test is available with:

```console
nix build .#checks.x86_64-linux.unix-socket
```

It starts the real service in a NixOS VM and checks member/nonmember access,
private-state isolation, socket recovery, and the absence of an extra socket unit.
