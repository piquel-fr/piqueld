# Validation checks committed artifacts without silently repairing stale output.
default: validate

validate: fmt-check lint check test doc-test deny openapi-check boundary check-wasm

build:
    @cargo build --workspace --locked

# Native CLI validation on Linux and macOS, including its shared contracts.
validate-cli:
    @cargo clippy --locked --package piquelctl --package piqueld-client --package piqueld-core --all-targets --all-features -- -D warnings
    @cargo nextest run --locked --package piquelctl --package piqueld-client --package piqueld-core
    @cargo test --locked --doc --package piqueld-client --package piqueld-core

build-cli:
    @cargo build --release --locked --package piquelctl

run *ARGS:
    @cargo run --package piquelctl -- {{ARGS}}

daemon *ARGS:
    @cargo run --package piqueld --bin piqueld -- {{ARGS}}

fmt:
    @cargo fmt --all

fmt-check:
    @cargo fmt --all -- --check

lint:
    @cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

check:
    @cargo check --locked --workspace --all-targets

# Compiles the browser transport, which no host-target recipe reaches.
check-wasm:
    @rustup target add wasm32-unknown-unknown
    @cargo check --locked --package piqueld-client --all-targets --target wasm32-unknown-unknown

# Runs focused transport tests in a headless browser. The development shell
# must provide wasm-bindgen-test-runner and a supported WebDriver.
test-wasm:
    @rustup target add wasm32-unknown-unknown
    @WASM_BINDGEN_TEST_ONLY_WEB=1 CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner cargo test --locked --package piqueld-client --lib --target wasm32-unknown-unknown

test:
    @cargo nextest run --locked --workspace

# Verify the real embedded bundle and run daemon tests with its generated CSP.
test-embedded:
    @cargo nextest run --locked --package piqueld --features embedded-ui

doc-test:
    @cargo test --locked --doc --workspace

deny:
    @cargo deny check

# Check both generated artifacts against freshly generated endpoint metadata.
openapi-check:
    @cargo run --package piqueld --features openapi-codegen --bin generate_openapi -- --check

boundary:
    @./scripts/check-dependency-boundaries.sh

# Browser UI development checks and the embedded-dashboard build are explicit
# because they require the wasm target, Trunk, wasm-bindgen-cli, binaryen, and
# Tailwind. They do not change the default validation.
ui-check:
    @cargo check --target wasm32-unknown-unknown -p piqueld-client -p piqueld-ui

# Release daemon and CLI; the daemon build script invokes Tailwind and Trunk.
# Select only the shipped binaries to avoid optimizing the OpenAPI generator,
# and build them together so Cargo can share dependencies and schedule both.
build-embedded:
    @cargo build --release --package piqueld --package piquelctl --bin piqueld --bin piquelctl --features piqueld/embedded-ui --locked

daemon-embedded *ARGS:
    @cargo run --package piqueld --bin piqueld --features embedded-ui -- {{ARGS}}

# Full local development: daemon, Tailwind, and Trunk are cleaned up together.
dev:
    @bash ./scripts/dev.sh

# Generate the OpenAPI document and client together.
generate:
    @cargo run --package piqueld --features openapi-codegen --bin generate_openapi

docker-test:
    @bash ./scripts/run-docker-integration-test.sh
