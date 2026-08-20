# Traefik ingress and Cloudflare Tunnel

`piqueld` owns one overlay network (`piqueld-ingress`) and one Swarm service
(`piqueld-traefik`). The service image is digest-pinned in host configuration.
Its API and dashboard are disabled, Swarm discovery defaults to off, and only
labels generated from an application's `[[spec.routes]]` are considered.

Cloudflare is intentionally not managed by `piqueld`: tunnel accounts, DNS,
credentials, ingress rules, and Access policies remain external state.

## Recommended topology

Keep the Traefik origin unpublished by default. Run the externally managed
`cloudflared` container as a Swarm service attached to `piqueld-ingress`, and
point its supplied tunnel configuration at:

```yaml
ingress:
  - hostname: notes.example.com
    service: http://piqueld-traefik:8080
  - service: http_status:404
```

The catch-all rule is important. Do not add origins for the piqueld API, Docker
socket, local registry, or a Traefik API/dashboard. `piqueld` neither creates
nor validates this external file, and tunnel credentials must not be placed in
the Nix store or application manifests.

`cloudflared` needs only the ingress network and its tunnel credential. It does
not need the Docker socket, the application-private networks, or access to the
control-plane API.

## Optional host publication

Docker Swarm cannot bind a published service port to a particular host address.
Consequently, the Traefik origin is not published unless the operator
explicitly configures `traefik.published_port`. That host-mode port listens on
the Swarm node's interfaces, not loopback alone. Use it only with a host
firewall that restricts the port to the intended private origin path; prefer
the network-only topology and never expose this port to the public Internet.

## Readiness and ownership

The daemon refuses an existing `piqueld-ingress` network or `piqueld-traefik`
service unless its exact current-instance ownership labels match. Routed
deployment work is held in a degraded state until the owned Traefik service has
one desired and one running task. Application status reports infrastructure
state separately from service/task diagnostics.

Startup and periodic scans re-run the idempotent infrastructure ensure
operation, so a missing owned network or Traefik service is recreated; a
same-named unowned resource remains a hard ownership conflict.

Route hosts are canonicalized to lowercase ASCII before collision detection and
label generation. Wildcards, IP literals, trailing dots, rule-control
characters, and local/internal DNS suffixes are rejected.

Runtime log queries are bounded to one day, 1,000 records, and 1 MiB. Records
are multiplexed with logical service, Swarm task, container, stream, and Docker
time. Following uses a bounded, backpressured SSE stream that periodically
discovers replacement tasks without retaining an unbounded per-client buffer in
the daemon. Followers may resume with `Last-Event-ID`; a cursor outside the
bounded replay window receives a `replay_reset` event before the current window.
