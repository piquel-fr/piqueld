# Installing piqueld

This page installs the piqueld daemon and the `piquelctl` CLI on a Linux host,
and sets up the address you open the dashboard at.

## Before you start

You need:

- **An x86_64 Linux host with systemd**, such as Debian, Ubuntu, Fedora or
  NixOS.
- **Docker Engine on that host, dedicated to piqueld.** piqueld runs your
  applications as Docker Swarm services and manages the engine's services,
  networks and volumes. If the engine isn't part of a Swarm yet, piqueld makes
  it a single-node Swarm when it starts. Install it with
  [Docker's guide](https://docs.docker.com/engine/install/); on NixOS, the
  module enables it.
- **`git` and `ssh` on the host**, which piqueld fetches your repositories
  with, and `curl` for the install script. The NixOS module provides them.

Anyone who controls the host also controls piqueld: they can read its database
and drive its Docker engine. piqueld calls them the
[host operator](../accounts/recovery.md#the-host-operator) and lets
them use `piquelctl` on the host without signing in.

Install piqueld [with the install script](#install-with-the-script),
[on NixOS](#install-on-nixos), or [by hand](#install-by-hand).

## Install with the script

On the host, run as yourself:

```console
curl -fsSL https://piqueld.dev/install.sh | sh
```

The script lists what it will do, and asks before it changes anything. Then it
installs the latest
[release](https://github.com/sourceorigin-dev/piqueld/releases) of piqueld and
starts it:

- It checks that the host runs systemd and Docker Engine, and has `git` and
  `ssh`.
- It downloads the release's binaries, checks them against the release's
  checksums, and installs `piqueld` and `piquelctl` to `/usr/local/bin`.
- It creates a `piqueld` system user in the `docker` group.
- It writes the daemon's configuration to `/etc/piqueld/config.toml`, unless
  that file exists. The configuration serves the dashboard on the host's
  [localhost](#localhost).
- It installs the `piqueld` systemd service, starts it, and waits until it
  answers.

The steps that need root run with `sudo`, which asks for your password, and
the script prints each of their commands before it runs it.

The script also asks whether to add you to the `piqueld` group. Its members
run `piquelctl` on the host without `sudo`. Log out and back in for the new
group to apply. To let other users in later:

```console
sudo usermod -aG piqueld alice
```

To install a specific version instead of the latest one, set
`PIQUELD_VERSION`:

```console
curl -fsSL https://piqueld.dev/install.sh | PIQUELD_VERSION=0.1.0 sh
```

To install without questions, such as from a provisioning script, pass
`--yes`. It also adds the user running the script to the `piqueld` group:

```console
curl -fsSL https://piqueld.dev/install.sh | sh -s -- --yes
```

Then keep the dashboard on localhost, or
[choose another address](#choose-how-to-reach-the-dashboard), and
[create your account](first-account.md).

### Update piqueld

Run the script again. It shows the version you have and the one it installs,
then installs the latest release, or `PIQUELD_VERSION`, over the current one,
and restarts the daemon. It keeps your configuration and
the data in `/var/lib/piqueld`.

The script replaces the service's unit, so keep your changes to the service in
a drop-in, as [passing files to the daemon](#pass-files-to-the-daemon) does.
Before 1.0, a minor release such as 0.2.0 may need changes to your
configuration: read its release notes before you update.

## Install on NixOS

1. Add piqueld to your flake's inputs:

   ```nix
   inputs.piqueld.url = "github:sourceorigin-dev/piqueld";
   ```

2. Import its module and enable the service:

   ```nix
   { inputs, ... }:
   {
     imports = [ inputs.piqueld.nixosModules.default ];
     services.piqueld.enable = true;
     # Lets alice use piquelctl on this host.
     users.users.alice.extraGroups = [ "piqueld" ];
   }
   ```

   The module enables Docker, creates a `piqueld` system user, runs the daemon
   as the `piqueld` systemd service, and installs `piquelctl`. It doesn't open
   any firewall port.

3. [Choose how to reach the dashboard](#choose-how-to-reach-the-dashboard), and
   add the matching settings.

4. Rebuild the system:

   ```console
   sudo nixos-rebuild switch
   ```

   Log out and back in, so that your new group membership applies.

The [NixOS module reference](../reference/nixos.md) describes every option.

## Choose how to reach the dashboard

`piquelctl` reaches piqueld through a Unix socket on the host. The dashboard
needs an address a browser can open. Pick one of the addresses below before
you create your account: the passkey you sign in with only works at the
address it was created on.

Each option is a few settings:

- With the install script, or [by hand](#install-by-hand), they go in
  `/etc/piqueld/config.toml`. Restart the daemon to apply them:

  ```console
  sudo systemctl restart piqueld
  ```

- On NixOS, they go in `services.piqueld.settings`, and
  `sudo nixos-rebuild switch` applies them.

### piqueld's own tailnet node

If you use [Tailscale](https://tailscale.com), this is the simplest option.
piqueld joins your tailnet as its own device and serves the dashboard over
HTTPS at `https://piqueld.<tailnet>.ts.net`, with no proxy or certificate to
manage.

1. In the Tailscale admin console, enable MagicDNS and HTTPS certificates for
   your tailnet, then create an auth key.
2. Save the auth key in a file only root can read:
   - Without NixOS, save it as `/etc/piqueld/ts-auth-key`, and
     [pass it to the daemon](#pass-files-to-the-daemon) as `ts-auth-key`.
     Install [Tailscale](https://tailscale.com/download/linux) too: piqueld
     runs its own `tailscaled`, and needs `tailscale` and `tailscaled` on its
     `PATH`.
   - On NixOS, use an agenix or sops-nix secret.
3. Add to `/etc/piqueld/config.toml`:

   ```toml
   [tailscale]
   enabled = true
   hostname = "piqueld"
   auth_key_file = "ts-auth-key"
   ```

   Or, on NixOS:

   ```nix
   services.piqueld.settings.tailscale = {
     enabled = true;
     hostname = "piqueld";
     auth_key_file = "/run/secrets/piqueld-ts-auth-key";
   };
   ```

The key is only used the first time the node logs in. See
[the tailnet node](../reference/configuration.md#tailnet-node) for the other
settings.

### Localhost

piqueld serves the dashboard on `http://localhost:7845` of the host. The
install script sets this up:

```toml
[server]
listen_mode = "localhost"
```

Or, on NixOS:

```nix
services.piqueld.settings.server.listen_mode = "localhost";
```

Browsers allow passkeys on `localhost` without HTTPS. To open the dashboard
from another machine, forward the port over SSH, then open
`http://localhost:7845` there:

```console
ssh -L 7845:localhost:7845 <host>
```

Always use `localhost`, not `127.0.0.1`: it is a different address to the
browser, and your passkey won't work there.

### Your own reverse proxy

Run an HTTPS reverse proxy on the host, forwarding to piqueld's localhost
listener, and tell piqueld its address:

```toml
[server]
listen_mode = "localhost"
# The proxy forwards requests for this hostname.
allowed_hosts = ["piqueld.example.com"]

[auth]
public_url = "https://piqueld.example.com"
```

Or, on NixOS:

```nix
services.piqueld.settings = {
  server.listen_mode = "localhost";
  server.allowed_hosts = [ "piqueld.example.com" ];
  auth.public_url = "https://piqueld.example.com";
};
```

Keep the proxy reachable only from networks you trust, such as your LAN or
tailnet. Never expose piqueld to the internet, even through a proxy.

## Install by hand

To install piqueld without the script, do what it does yourself, on the host:

1. Download a release's archive and its checksums from
   [GitHub](https://github.com/sourceorigin-dev/piqueld/releases), check
   them, and install the binaries to `/usr/local/bin`:

   ```console
   version=0.1.0
   release=https://github.com/sourceorigin-dev/piqueld/releases/download/v$version
   curl -fsSLO "$release/piqueld-$version-x86_64-linux.tar.gz" -O "$release/SHA256SUMS"
   sha256sum --check --ignore-missing SHA256SUMS
   tar -xzf "piqueld-$version-x86_64-linux.tar.gz"
   cd "piqueld-$version-x86_64-linux"
   sudo install piqueld piquelctl /usr/local/bin/
   ```

   To build them from source instead, see
   [building from source](#build-from-source).

2. Create a `piqueld` system user and group, and add the user to the `docker`
   group:

   ```console
   sudo useradd --system --user-group --groups docker \
     --home-dir /var/lib/piqueld --no-create-home \
     --shell /usr/sbin/nologin piqueld
   ```

3. Write the daemon's configuration to `/etc/piqueld/config.toml`, readable by
   the `piqueld` group, with the settings from
   [the previous section](#choose-how-to-reach-the-dashboard). The
   [configuration reference](../reference/configuration.md) lists every
   setting.

4. Install the systemd unit, `piqueld.service` in the archive, as
   `/etc/systemd/system/piqueld.service`:

   ```console
   sudo install -m 644 piqueld.service /etc/systemd/system/
   ```

   It runs the daemon as the `piqueld` user:

   ```ini
   {{#include ../../../apps/piqueld/piqueld.service:7:}}
   ```

   systemd creates the data directory, `/var/lib/piqueld`, and the runtime
   directory holding the API socket, `/run/piqueld`. The daemon also needs
   `git`, `ssh` and `docker` on its `PATH`, which includes `/usr/bin`.

5. Start the daemon, and have it start at boot:

   ```console
   sudo systemctl daemon-reload
   sudo systemctl enable --now piqueld
   ```

6. Add the users who should run `piquelctl` on the host to the `piqueld`
   group.

To update piqueld, install the new release's binaries and unit the same way,
then restart it with `sudo systemctl restart piqueld`.

### Build from source

To build piqueld instead of downloading it, such as to run a version that
isn't released yet, install it from
[crates.io](https://crates.io/crates/piqueld) with Cargo. The build takes
several minutes.

1. Install Rust with [rustup](https://rustup.rs), then the musl target, a C
   compiler, `pkg-config` and OpenSSL's development files. On Debian and
   Ubuntu:

   ```console
   rustup target add x86_64-unknown-linux-musl
   sudo apt install build-essential pkg-config libssl-dev
   ```

   The daemon embeds a small static program built for the musl target, which
   runs the HTTP health checks inside your containers.

2. Build the daemon and `piquelctl`, and copy them to `/usr/local/bin`:

   ```console
   cargo install --locked piqueld piquelctl
   sudo install ~/.cargo/bin/piqueld ~/.cargo/bin/piquelctl /usr/local/bin/
   ```

   `--locked` builds with the dependency versions the release was tested
   with. The daemon includes the dashboard. To build a version that isn't
   released yet, run `just build-embedded` in a checkout instead, as the
   [developer guide](../../dev/getting-started.md) describes, and copy
   `target/release/piqueld` and `target/release/piquelctl`.

Then follow the steps above from step 2. Save the unit from
[`apps/piqueld/piqueld.service`](https://github.com/sourceorigin-dev/piqueld/blob/main/apps/piqueld/piqueld.service),
which step 4 shows.

## Pass files to the daemon

Settings that end in `_file`, such as the tailnet auth key or a DNS
provider's API token, name a file only root can read. Without NixOS, pass each
file to the daemon as a systemd credential, and refer to it by the
credential's name:

1. Add the credential to the service in a drop-in, which reinstalling piqueld
   keeps:

   ```console
   sudo systemctl edit piqueld
   ```

   ```ini
   [Service]
   LoadCredential=ts-auth-key:/etc/piqueld/ts-auth-key
   ```

2. Refer to it by its name, `ts-auth-key`, in `/etc/piqueld/config.toml`:

   ```toml
   [tailscale]
   enabled = true
   auth_key_file = "ts-auth-key"
   ```

3. Restart the daemon:

   ```console
   sudo systemctl restart piqueld
   ```

On NixOS, the module passes the files itself: set the setting to the file's
path.

## Check that it works

```console
sudo piquelctl status
```

It prints the daemon's version and how `piquelctl` reached it:

```text
daemon running (version 0.1.0, API v1, instance instance-01a16b10b8307c61b5ac3d27d4bf7937)
Transport: Unix socket /run/piqueld/piqueld.sock
```

Run as root, `piquelctl` acts as the host operator, so it works before any
account exists. Then check the installation:

```console
sudo piquelctl doctor
```

It checks what the daemon can verify about its own setup, such as Docker, its
public URL and its secret key, and gives a next step for each check that needs
action. Once you've signed in, **System status** on the dashboard's
**Overview** also shows whether Docker and its Swarm are ready, and the
**Daemon status** page runs the same checks.

Next, [create your account](first-account.md).

## Troubleshooting

- **The install script stops with an error.** It names what the host is
  missing, such as Docker Engine or `git`: install it, then run the script
  again. If the daemon doesn't start, read its logs with
  `journalctl -u piqueld`.
- **`piquelctl` can't connect to the socket.** Check that the service runs with
  `systemctl status piqueld`, and read its logs with `journalctl -u piqueld`.
  Without `sudo`, you need to be in the `piqueld` group, and logged in again
  since you were added.
- **Docker isn't ready.** piqueld keeps serving while Docker is down, and
  starts deploying once it's back. **System status**, on the dashboard's
  **Overview**, shows whether it reaches Docker and whether the engine is a
  Swarm manager.
