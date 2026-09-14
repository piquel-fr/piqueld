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

Nix packages and development shells support x86_64 Linux. Configuration and
package changes require rebuilding NixOS; application state remains under
`dataDir`.
