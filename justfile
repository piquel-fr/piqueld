# The default command regenerates checked-in output and then validates it.
default: validate generate-openapi

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

# Explicitly mutating generation command.
generate-openapi:
    @cargo run --package piqueld --bin generate_openapi

docker-test:
    @bash ./scripts/run-docker-integration-test.sh

# Explicit Nix evaluation; it may need network access for a missing flake input.
nix-check:
    @nix flake check --no-update-lock-file
