# Plan 06A product boundary

The first usable piqueld product is intentionally small: one daemon, one local
SQLite database, one Docker Engine in single-node Swarm mode, and one polling API.

An application is a normalized manifest containing metadata, prebuilt image
services, replicas, environment, command/args, health checks, resource limits,
named volumes, and volume mounts. The daemon resolves images to immutable digests,
persists desired and resolved state, and reconciles private networks, volumes, and
services. Deleting an application removes its services and private network but
retains its named volumes.

The API is available over loopback TCP and a Unix socket. It exposes application,
plan, status, and durable operation resources. The typed client is transport
neutral at its DTO boundary and polls operation state.

The product deliberately does not contain source checkout or image builds,
registry management, credentials or secrets, routing or published ports,
Traefik, authentication, packaging, CLI/UI functionality, or an event stream.
Those concerns are not represented in the manifest, persistence schema, runtime
planner, Docker adapter, OpenAPI document, or dependency graph.

The pure core and Docker/operation handler seams remain because they are exercised
by focused tests and make the supported runtime deterministic without introducing
replacement abstractions for removed systems.
