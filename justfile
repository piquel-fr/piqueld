# The default command regenerates checked-in output and then validates it.
default: generate-openapi validate

validate: fmt-check lint check test doc-test deny openapi-check boundary check-wasm

build:
    @cargo build --workspace --locked

run:
    @cargo run --package piqueld

fmt:
    @cargo fmt --all

fmt-check:
    @cargo fmt --all -- --check

lint:
    @cargo clippy --workspace --all-targets --all-features -- -D warnings

check:
    @cargo check --workspace --all-targets

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
    @cargo test --workspace

doc-test:
    @cargo test --doc --workspace

deny:
    @cargo deny check

openapi-check:
    @cargo run --package piqueld --bin generate_openapi -- --check

boundary:
    @./scripts/check-dependency-boundaries.sh

# Explicitly mutating generation command.
generate-openapi:
    @cargo run --package piqueld --bin generate_openapi

docker-test:
    @bash ./scripts/run-docker-integration-test.sh

# Explicit Nix evaluation; it may need network access for a missing flake input.
nix-check:
    @nix flake check --no-update-lock-file
