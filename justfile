# Validation formats Rust sources and checks generated artifacts for freshness.
default: validate

validate: fmt lint check test doc-test deny openapi-check boundary check-wasm test-playwright docs

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

# The native build compiles the browser code out, so the dashboard and client
# are linted as wasm32 too. Without --no-deps this also lints their workspace
# dependencies (piqueld-core) as wasm32.
lint:
    @cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
    @cargo clippy --locked --target wasm32-unknown-unknown --package piqueld-ui --package piqueld-client -- -D warnings

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

# All features, like lint: the daemon tests run against the real embedded
# bundle, and lint, test, and doc-test share one build.
test:
    @cargo nextest run --locked --workspace --all-features

doc-test:
    @cargo test --locked --doc --workspace --all-features

deny:
    @cargo deny check

# Check generated artifacts against freshly generated endpoint and manifest metadata.
openapi-check:
    @cargo run --package piqueld --features openapi-codegen --bin generate_openapi -- --check

boundary:
    @cargo xtask boundary

# UI checks and embedded builds need only the wasm32 target from
# rust-toolchain.toml: the daemon build script compiles and binds the dashboard
# itself. Default all-feature Clippy builds the embedded dashboard, and
# Playwright validation builds it through its Rust fixture too.
ui-check:
    @cargo check --locked --target wasm32-unknown-unknown --package piqueld-client --package piqueld-ui

# Release daemon and CLI; the daemon build script compiles the dashboard.
# Select only the shipped binaries to avoid optimizing the OpenAPI generator,
# and build them together so Cargo can share dependencies and schedule both.
build-embedded:
    @cargo build --release --package piqueld --package piquelctl --bin piqueld --bin piquelctl --features piqueld/embedded-ui --locked

daemon-embedded *ARGS:
    @cargo run --package piqueld --bin piqueld --features embedded-ui -- {{ARGS}}

# Isolated development instance for this worktree (docs/dev/development.md).
[positional-arguments]
dev *ARGS:
    @cargo xtask dev "$@"

# piquelctl from this worktree, connected to its development instance.
[positional-arguments]
ctl *ARGS:
    @cargo xtask dev ctl "$@"

# Generate the OpenAPI document, client, and manifest JSON Schema together.
generate:
    @cargo run --package piqueld --features openapi-codegen --bin generate_openapi

# Builds the documentation book into target/book, failing on any mdBook
# warning, then checks every link and heading anchor in it and in the
# repository's Markdown files, offline (see lychee.toml). Links to this
# repository on GitHub are checked against the checkout; the second remap
# fails those that don't point at main, which would otherwise go unchecked.
# CI runs this on every pull request; see docs/dev/documentation.md.
docs:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! log=$(mdbook build 2>&1) || grep -q WARN <<<"$log"; then
        echo "$log" >&2
        exit 1
    fi
    repository='https://(github\.com/piquel-fr/piqueld/(blob|tree)|raw\.githubusercontent\.com/piquel-fr/piqueld)'
    git ls-files -z --cached --others --exclude-standard '*.md' \
        | xargs -0 lychee --root-dir "{{justfile_directory()}}/target/book" \
            --remap "$repository/main/ file://{{justfile_directory()}}/" \
            --remap "$repository/ file:///links-to-this-repository-must-point-at-main/" \
            target/book

# Serves the documentation book on localhost, rebuilding it on every change.
docs-serve *ARGS:
    @mdbook serve {{ARGS}}

docker-test:
    @cargo xtask docker-test

# Explicit development-only setup: pinned JS packages and a containerized browser.
setup-playwright:
    @pnpm --dir tests/playwright install --frozen-lockfile --ignore-scripts
    @docker pull "mcr.microsoft.com/playwright:v$(node -p 'require("./tests/playwright/package.json").devDependencies["@playwright/test"]')-noble"

# The Rust fixture serves the embedded UI/API with isolated state and no Docker backend.
test-playwright *ARGS: build-playwright
    @cargo xtask playwright {{ARGS}}

# Regenerates the dashboard screenshots in docs/images, through the browser
# suite's fixture with deployments to its fake Docker engine. Not part of
# validation: see docs/dev/documentation.md.
docs-screenshots *ARGS: build-playwright
    @cargo xtask playwright --config screenshots.config.ts {{ARGS}}

# Builds what the browser runs need: the CLI and the Rust fixture, and
# type-checks the Playwright specs.
[private]
build-playwright:
    @cargo build --locked --package piquelctl
    @cargo build --locked --package piqueld --features embedded-ui --example browser_fixture
    @pnpm --dir tests/playwright check
