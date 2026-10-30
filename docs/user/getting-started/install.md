# Installing piqueld

This page installs the piqueld daemon and the `piquelctl` CLI on a Linux host,
and sets up the address you open the dashboard at.

## Before you start

You need:

- **An x86_64 Linux host.** NixOS is the supported way to install piqueld. On
  other distributions, you [build it yourself](#other-linux-distributions).
- **Docker Engine on that host, dedicated to piqueld.** piqueld runs your
  applications as Docker Swarm services and manages the engine's services,
  networks and volumes. If the engine isn't part of a Swarm yet, piqueld makes
  it a single-node Swarm when it starts.

Anyone who controls the host also controls piqueld: they can read its database
and drive its Docker engine. piqueld calls them the
[host operator](../accounts/recovery.md#the-host-operator) and lets
them use `piquelctl` on the host without signing in.

## Install on NixOS

1. Add piqueld to your flake's inputs:

   ```nix
   inputs.piqueld.url = "github:piquel-fr/piqueld";
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

At first, piqueld only listens on a Unix socket on the host, so the dashboard
isn't reachable from a browser. Pick one of the addresses below before you
create your account: the passkey you sign in with only works at the address it
was created on.

### piqueld's own tailnet node

If you use [Tailscale](https://tailscale.com), this is the simplest option.
piqueld joins your tailnet as its own device and serves the dashboard over
HTTPS at `https://piqueld.<tailnet>.ts.net`, with no proxy or certificate to
manage.

1. In the Tailscale admin console, enable MagicDNS and HTTPS certificates for
   your tailnet, then create an auth key.
2. Save the auth key in a file only root can read, such as an agenix or
   sops-nix secret.
3. Add to your configuration:

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

piqueld serves the dashboard on `http://localhost:7845` of the host:

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

```nix
services.piqueld.settings = {
  server.listen_mode = "localhost";
  # The proxy forwards requests for this hostname.
  server.allowed_hosts = [ "piqueld.example.com" ];
  auth.public_url = "https://piqueld.example.com";
};
```

Keep the proxy reachable only from networks you trust, such as your LAN or
tailnet. Never expose piqueld to the internet, even through a proxy.

## Other Linux distributions

piqueld has no packages for other distributions yet, and the NixOS module is
the only tested setup. To run it elsewhere, build it from source with
`just build-embedded`, as described in the
[developer guide](../../dev/getting-started.md), and copy
`target/release/piqueld` and `target/release/piquelctl` to `/usr/local/bin`.
Then set it up the way the NixOS module does:

1. Create a `piqueld` system user and group, and add the user to the `docker`
   group:

   ```console
   sudo useradd --system --user-group --groups docker \
     --home-dir /var/lib/piqueld --no-create-home piqueld
   ```

2. Write the daemon's configuration to `/etc/piqueld/config.toml`, with the
   settings from [the previous section](#choose-how-to-reach-the-dashboard).
   Each `services.piqueld.settings` option is the same setting in the file:
   `settings.server.listen_mode = "localhost"` becomes

   ```toml
   [server]
   listen_mode = "localhost"
   ```

   The [configuration reference](../reference/configuration.md) lists every
   setting.

3. Save this unit as `/etc/systemd/system/piqueld.service`:

   ```ini
   [Unit]
   Description=piqueld application control plane
   After=docker.service
   Wants=docker.service

   [Service]
   ExecStart=/usr/local/bin/piqueld --config /etc/piqueld/config.toml
   User=piqueld
   Group=piqueld
   SupplementaryGroups=docker
   StateDirectory=piqueld
   StateDirectoryMode=0700
   RuntimeDirectory=piqueld
   RuntimeDirectoryMode=0750
   UMask=0077
   Restart=on-failure

   [Install]
   WantedBy=multi-user.target
   ```

   systemd creates the data directory, `/var/lib/piqueld`, and the runtime
   directory holding the API socket, `/run/piqueld`. The daemon also needs
   `git`, `ssh` and `docker` on its `PATH`, which includes `/usr/bin`.

4. If piqueld joins your tailnet as [its own node](#piquelds-own-tailnet-node),
   it also needs `tailscale` and `tailscaled` on its `PATH`. Store the auth key
   in a file only root can read, such as `/etc/piqueld/ts-auth-key`, and pass
   it to the service by adding to the unit's `[Service]` section:

   ```ini
   LoadCredential=ts-auth-key:/etc/piqueld/ts-auth-key
   ```

   Then refer to it by that name in `config.toml`:

   ```toml
   [tailscale]
   enabled = true
   auth_key_file = "ts-auth-key"
   ```

5. Start the daemon, and have it start at boot:

   ```console
   sudo systemctl daemon-reload
   sudo systemctl enable --now piqueld
   ```

6. Add the users who should run `piquelctl` on the host to the `piqueld`
   group.

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

- **`piquelctl` can't connect to the socket.** Check that the service runs with
  `systemctl status piqueld`, and read its logs with `journalctl -u piqueld`.
  Without `sudo`, you need to be in the `piqueld` group, and logged in again
  since you were added.
- **Docker isn't ready.** piqueld keeps serving while Docker is down, and
  starts deploying once it's back. **System status**, on the dashboard's
  **Overview**, shows whether it reaches Docker and whether the engine is a
  Swarm manager.
