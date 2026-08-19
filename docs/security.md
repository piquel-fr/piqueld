# Security and threat boundary

The prototype assumes a trusted local operator and an untrusted application
manifest, Git source, Docker response, and browser. It reduces accidental and
remote exposure as follows:

- Unix-socket administration is protected by filesystem ownership. TCP is
  loopback-only and fails closed unless a bearer credential is configured.
- Credential files are opened without following links, must be private regular
  files when configured as host files, are rejected from `/nix/store`, and are
  zeroized after use. Secrets never enter labels, logs, API errors, archives,
  browser state, or command arguments.
- Request headers, bodies, time, concurrent work, archive size, build contexts,
  and log buffers are bounded. CORS is an explicit allow-list; duplicate or
  ambiguous authentication headers are rejected.
- Docker mutation requires the current instance/application ownership labels.
  Similar names and unlabelled resources are conflicts, not invitations to
  adopt. Named volumes are retained on application deletion.
- Images are resolved to immutable digests before deployment. Managed Traefik
  images are digest-pinned, run with the Docker socket read-only, disable the
  dashboard/admin API, and expose no host port unless configured explicitly.
- The NixOS module runs as a dedicated user with a private state/runtime
  directory, external systemd credentials, no firewall openings, and restrictive
  systemd sandboxing.

This is not an account system, a multi-tenant isolation boundary, a public
registry, or a replacement for Docker, kernel, registry, Git, Cloudflare, or
Tailscale hardening. Keep the API on the Unix socket or behind an independently
authenticated private proxy.
