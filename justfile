# Runs all validation commands
validate: format lint check test doc-test

# Runs all code generation
gen: generate-openapi

format:
    @cargo fmt --all

lint:
    @cargo clippy --workspace --all-targets

check:
    @cargo check --workspace --all-targets
    @./scripts/check-dependency-boundaries.sh

generate-openapi:
    @cargo run --package piqueld --bin generate_openapi

test:
    @cargo nextest run --workspace

doc-test:
    @cargo test --doc --workspace
