# The default command regenerates checked-in output and then validates it.
default: generate-openapi validate

validate: fmt-check lint check test doc-test deny openapi-check boundary

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

# Browser UI development and release asset commands are explicit because they
# require the wasm target and Trunk. They do not change the default validation.
ui-check:
    @cargo check --target wasm32-unknown-unknown -p piqueld-client -p piqueld-ui

ui-dev:
    @cd apps/piqueld-ui && env -u NO_COLOR trunk serve --proxy-backend=http://127.0.0.1:7845

ui-build:
    @cd apps/piqueld-ui && env -u NO_COLOR trunk build --release --public-url / --dist ../../target/piqueld-ui-dist

# Explicitly mutating generation command.
generate-openapi:
    @cargo run --package piqueld --bin generate_openapi

docker-test:
    @bash ./scripts/run-docker-integration-test.sh

# Explicit Nix evaluation; it may need network access for a missing flake input.
nix-check:
    @nix flake check --no-update-lock-file
