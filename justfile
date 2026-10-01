# Validation formats Rust sources and checks generated artifacts for freshness.
default: validate

validate: fmt lint check test doc-test deny openapi-check boundary check-wasm test-playwright

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

# CI checks formatting without modifying sources; local validation uses fmt.
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

# Includes the Go-backed tailscale feature, so tests need a Go toolchain.
test: test-tailnet-bridge
    @cargo nextest run --locked --workspace --features piqueld/tailscale

# Verify the patched descriptor/peer handoff without a tailnet login.
test-tailnet-bridge:
    @cd vendor/libtailscale-sys/libtailscale && go test ./...

# Verify the real embedded bundle and run daemon tests against it.
test-embedded:
    @cargo nextest run --locked --package piqueld --features embedded-ui

doc-test:
    @cargo test --locked --doc --workspace

deny:
    @cargo deny check

# Check generated artifacts against freshly generated endpoint and manifest metadata.
openapi-check:
    @cargo run --package piqueld --features openapi-codegen --bin generate_openapi -- --check

boundary:
    @./scripts/check-dependency-boundaries.sh

# UI checks and embedded builds need only the wasm32 target from
# rust-toolchain.toml: the daemon build script compiles and binds the dashboard
# itself. Default all-feature Clippy builds the embedded dashboard, and
# Playwright validation builds it through its Rust fixture too.
ui-check:
    @cargo check --target wasm32-unknown-unknown -p piqueld-client -p piqueld-ui

# Release daemon and CLI; the daemon build script compiles the dashboard,
# and the tailscale feature needs Go.
# Select only the shipped binaries to avoid optimizing the OpenAPI generator,
# and build them together so Cargo can share dependencies and schedule both.
build-embedded:
    @cargo build --release --package piqueld --package piquelctl --bin piqueld --bin piquelctl --features piqueld/embedded-ui,piqueld/tailscale --locked

daemon-embedded *ARGS:
    @cargo run --package piqueld --bin piqueld --features embedded-ui -- {{ARGS}}

# Full local development: the watcher and daemon are cleaned up together.
dev:
    @bash ./scripts/dev.sh

# Generate the OpenAPI document, client, and manifest JSON Schema together.
generate:
    @cargo run --package piqueld --features openapi-codegen --bin generate_openapi

docker-test:
    @bash ./scripts/run-docker-integration-test.sh
# Explicit development-only setup: pinned JS packages and a containerized browser.
setup-playwright:
    @pnpm --dir tests/playwright install --frozen-lockfile --ignore-scripts
    @docker pull "mcr.microsoft.com/playwright:v$(node -p 'require("./tests/playwright/package.json").devDependencies["@playwright/test"]')-noble"

# The Rust fixture serves the embedded UI/API with isolated state and no Docker backend.
test-playwright *ARGS:
    @cargo build --locked --package piquelctl
    @cargo build --locked --package piqueld --features embedded-ui --example browser_fixture
    @pnpm --dir tests/playwright check
    @bash scripts/test-playwright.sh {{ARGS}}
