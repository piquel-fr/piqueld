# Makes sure everything works & is up to date in the repository
# Generates all required code & runs validation
all: gen validate

# Runs all validation commands
validate: fmt lint check test doc-test

# Runs all code generation
gen: generate-openapi

fmt:
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

docker-test:
    @bash ./scripts/run-docker-integration-test.sh
