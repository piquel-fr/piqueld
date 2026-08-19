# Architecture

piqueld is a single process controlling one Docker Engine configured as a
single-node Swarm. The process has four deliberately small boundaries:

1. `piqueld-core` parses strict manifests, validates them, resolves canonical
   names, and produces desired resources. It has no HTTP, Docker, SQL, or UI
   dependency.
2. The SQLite store persists normalized intent, resolved sources, operation
   state, observations, and status. Mutations that change desired state and
   their operation journal are transactional.
3. The daemon API and transport-neutral client expose the same versioned DTOs to
   the Unix socket, loopback HTTP, CLI, and Leptos dashboard.
4. Docker, registry, BuildKit, secrets, and Traefik adapters perform bounded
   side effects. Ownership labels are checked before repair or deletion, and
   reconciliation is the only owner of runtime convergence.

The normal flow is:

```text
manifest -> strict validation -> normalized desired state -> durable operation
         -> source resolution/build -> desired Docker resources
         -> bounded observe/repair -> status and logs
```

Logical secrets are encrypted in SQLite and become immutable, generation-named
Swarm secrets. State archives contain control-plane state and explicit dependency
metadata; they do not contain volume contents, registry blobs, Git checkouts, or
the encryption key. See [`state-archive-v1.md`](state-archive-v1.md).

The managed Traefik service is private by default. An optional explicit host
origin port publishes application routes; piqueld does not configure Cloudflare,
Tailscale, DNS, firewalls, or public account access.
