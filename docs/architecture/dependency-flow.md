# Workspace dependency flow

`piqueld-core` is the pure center of the workspace. It owns manifest, resource, and
planning contracts and must not depend on Axum, SQLx, Bollard, or UI code.

`piqueld-client` depends on core and will own typed HTTP client behavior. `piquelctl`
and `piqueld-ui` depend on the client and core contracts. The `piqueld` daemon depends
on core directly; persistence, Docker, API, and other
adapters remain internal daemon modules. Applications never become dependencies of
libraries.
