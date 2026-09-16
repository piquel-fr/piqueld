# Client generation

The daemon's Utoipa metadata produces `docs/openapi-v1.json` (OpenAPI 3.1).
OpenAPI Generator 7.20.0 then produces `crates/piqueld-client/src/generated.rs`.
The generator version and SHA-256 are pinned in `scripts/generate-client.sh`.
Java 11 or newer, curl, and rustfmt are needed for generation; normal Cargo
builds use the checked-in Rust and need none of the generator tooling.
The development shell includes Java and curl. The first generation downloads
the verified JAR into `target/client-codegen`; subsequent runs reuse it.

Run `just generate` after changing API metadata or shared wire contracts.
Run `just client-check` to compare regenerated bindings without modifying them.
CI separately checks that both the specification and client are current.

`config.json` maps the endpoint request/response schemas to existing Rust
contracts. Add a mapping when an endpoint introduces a new top-level wire type;
unmapped types fail Rust compilation. The generator handles OpenAPI parsing,
parameter types, operation names, and media types. The single request template
connects those operations to the existing bounded TCP/Unix/browser transport,
without generating duplicate data models or adding runtime dependencies.
JSON envelopes remain explicit in the generated API. The public convenience
methods unwrap them, perform local validation, and handle readiness's documented
503 payload. JSON and TOML request representations are generated as enums;
manifest downloads decode as UTF-8 rather than JSON.

Edit endpoint annotations, configuration, or the template, never generated Rust.
When upgrading the generator, update its version and digest together, regenerate,
and run the client and daemon contract tests plus native/WASM validation.
