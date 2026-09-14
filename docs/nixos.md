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

Validate service startup, private paths, Swarm initialization, CLI access, and
restart with `nix build .#checks.x86_64-linux.nixos-service` (requires KVM or
software virtualization). Configuration and package changes require rebuilding
NixOS; application state remains under `dataDir`.

CI builds and tests all three packages on both native architectures on every
PR and push to main; the NixOS VM test runs on x86_64. Crane splits native and
WebAssembly dependency compilation into cached derivations. Application edits
reuse these artifacts while manifest, lockfile, or toolchain changes can require
fresh dependency builds. The dashboard has its own build, so daemon-only edits
also reuse its distribution. Documentation and CI files are excluded from
package sources.

`nix/ci.nix` also retains the previous successful workspace build artifacts,
allowing Cargo's incremental compiler to reuse unchanged application code.
The artifact index travels with the Nix store cache; missing artifacts fall
back to dependency-only builds. Sources receive newer timestamps than the
archives so Cargo must validate current code. Each snapshot is self-contained,
avoiding an ever-growing chain of previous builds.

CI roots these artifacts and the dashboard distribution in `result-ci` before
collecting unrooted store paths. The CI wrapper uses `--impure` only to read the
local cache index and resolve its immutable store paths; ordinary flake builds
remain independent of that index. Cold caches still require full compilation.

Nix release builds retain release optimization but disable thin LTO, avoiding
whole-program optimization for every package and test executable. The regular
Cargo release profile is unchanged. Native Nix CI uses 32-vCPU runners and
starts the x86_64 VM test as soon as its daemon and CLI packages are ready,
allowing it to overlap with the combined package's build and tests. Each VM
receives four vCPUs to parallelize boot-time service startup.
