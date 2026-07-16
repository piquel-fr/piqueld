format:
    @cargo fmt --all

lint:
    @cargo clippy --workspace

check:
    @cargo check --workspace --all-targets
    @./scripts/check-dependency-boundaries.sh

generate-openapi:
    @cargo run --package piqueld --example generate_openapi

test:
    @cargo nextest run --workspace

doc-test:
    @cargo test --doc --workspace

validate: format lint check test doc-test
