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
The module writes `/etc/piqueld/profiles.toml` and installs the unwrapped CLI.
Every user and every CLI build, including `cargo run -p piquelctl`, discovers
these profiles without environment setup. User profiles can add or replace named
system profiles; `--profiles-file` and `PIQUELD_PROFILES_FILE` bypass discovery.
Run `piquelctl profiles` to see the effective names and endpoints.

For nix-darwin, import `inputs.piqueld.darwinModules.piquelctl` and use the same
`programs.piquelctl` options. The shared CLI module does not require the daemon.

When migrating from the wrapper-based module, rebuild the machine configuration
and remove old `PIQUELD_PROFILES_FILE` exports or custom wrappers unless you
intentionally want an exclusive file override. Remove duplicate TOML generation
and per-user profile symlinks/activation scripts used to expose system profiles;
the generated system file now serves all users. Keep intentional user overrides
in the user configuration file. Profile settings remain in the public Nix store
and must not contain credentials.

`dataDir` defaults to `/var/lib/piqueld`, with mode `0700`, holding `piqueld.db`.
`runtimeDir` defaults to `/run/piqueld`, prepared by systemd with mode `0750`.
The socket is `/run/piqueld/piqueld.sock`, owned by `piqueld:piqueld` with mode
`0660`. After logging in again to refresh group membership, users can connect
to the socket without sudo. Membership permits only a socket connection; run
`piquelctl login` to authenticate an account before `piquelctl status` or other
API operations. It grants no direct access to private state or permission to
replace the socket.
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
selected port on the Tailscale interface. All callers must authenticate. Configure
`settings.auth.public_url` with a stable HTTPS hostname and terminate TLS externally;
see [authentication](authentication.md).

For HTTPS without a proxy, let piqueld join the tailnet as its own node instead.
It does not use the host's `tailscaled`, and its state lives in `dataDir`:

```nix
services.piqueld.settings.tailscale = {
  enabled = true;
  hostname = "piqueld"; # https://piqueld.<tailnet>.ts.net
  auth_key_file = "/run/secrets/piqueld-ts-auth-key"; # first login only
};
```

The key file is passed to the service as the systemd credential `ts-auth-key`;
it never enters the Nix store. `settings.auth.public_url` defaults to the node's
HTTPS URL. See [the tailnet node](configuration.md#tailnet-node) for details.

`settings` declares typed options for `server.listen_mode`, `server.port`,
`server.allowed_hosts`, `auth.public_url`, `tailscale.enabled`,
`tailscale.hostname`, `tailscale.auth_key_file`, `docker.socket`,
`docker.auto_initialize_swarm`, `ingress.enabled`, all three `reconciliation`
intervals/timeouts, the `retention` periods, both `build_history` limits,
`metrics.listen`, and every `notifications` switch and timing, with the daemon's
defaults. Reconciliation values must be 1–86400 seconds; retention values are
nonnegative days, with zero disabling pruning. Unknown settings are rejected.

Webhook destination URLs usually embed credentials, so they are not Nix
settings. Put them in a private file and point `notificationDestinationsFile`
at it:

```nix
services.piqueld = {
  settings.notifications.enabled = true;
  notificationDestinationsFile = "/run/secrets/piqueld-destinations.toml";
};
```

```toml
[[notifications.destinations]]
name = "operations"
kind = "discord" # or "json"
url = "https://discord.com/api/webhooks/..."
```

The file must contain only `[[notifications.destinations]]` entries. systemd
loads it as a service credential, so it can stay root-owned with mode `0600`;
it is appended to the generated configuration inside the service's private
`/tmp` at each start. Restart piqueld after changing it. See
[observability](observability.md) for delivery semantics.
The module always supplies
`server.data_dir` from `dataDir` and `server.runtime_dir` from `runtimeDir`. It does not configure a registry, Traefik,
TLS termination or an external UI directory. Git, SSH, and Docker executables
are present on the service PATH. Configure SSH credentials and known hosts for
the service user, not the interactive operator; host home directories are protected.
Never put credential values into Nix settings, which are stored in the Nix store.

Nix packages and development shells support x86_64 Linux. Configuration and
package changes require rebuilding NixOS; application state remains under
`dataDir`.
