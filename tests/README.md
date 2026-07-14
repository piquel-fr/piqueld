# Cross-cutting tests

Workspace integration and NixOS VM tests will live here as their implementing
increments add real behavior. Crate-specific integration tests remain beside their
crate so `cargo test --workspace` discovers them naturally.

