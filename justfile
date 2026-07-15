format:
    @cargo fmt --all

lint:
    @cargo clippy --workspace

check:
    @cargo check --workspace --all-targets
    @./scripts/check-dependency-boundaries.sh

test:
    @cargo nextest run --workspace

doc-test:
    @cargo test --doc --workspace

validate: format lint check test doc-test
