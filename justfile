# The default command regenerates checked-in output and then validates it.
default: generate-openapi validate

validate: fmt-check lint check test doc-test deny openapi-check boundary check-wasm

build:
    @cargo build --workspace --locked

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
    @cargo check --package piqueld-client --target wasm32-unknown-unknown

test:
    @cargo nextest run --locked --workspace

doc-test:
    @cargo test --locked --doc --workspace

deny:
    @cargo deny check

openapi-check:
    @cargo run --package piqueld --bin generate_openapi -- --check

boundary:
    @./scripts/check-dependency-boundaries.sh

# Browser UI development checks and the embedded-dashboard build are explicit
# because they require the wasm target, Trunk, wasm-bindgen-cli, binaryen, and
# Tailwind. They do not change the default validation.
ui-check:
    @cargo check --target wasm32-unknown-unknown -p piqueld-client -p piqueld-ui

# Release daemon with the dashboard bundle compiled in; the build script
# invokes Tailwind and Trunk itself.
build-embedded:
    @cargo build --release --package piqueld --features embedded-ui --locked

daemon-embedded *ARGS:
    @cargo run --package piqueld --bin piqueld --features embedded-ui -- {{ARGS}}

# Full local development: daemon, Tailwind, and Trunk are cleaned up together.
dev:
    @bash ./scripts/dev.sh

# Explicitly mutating generation command.
generate-openapi:
    @cargo run --package piqueld --bin generate_openapi

docker-test:
    @bash ./scripts/run-docker-integration-test.sh

# Explicit Nix evaluation; it may need network access for a missing flake input.
nix-check:
    @nix flake check --no-update-lock-file
