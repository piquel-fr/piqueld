#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

run_portable() {
  # Keep the repository's justfile authoritative for the ordinary validation
  # contract. The focused contract layer below adds cross-crate evidence.
  just
}

run_contracts() {
  cargo test -p piqueld-core --test manifests --test resource_planning
  cargo test -p piqueld --test sqlx_stack --test persistence
  cargo test -p piqueld --test api_contract
  cargo test -p piqueld-client --test transports
  cargo test -p piquelctl --test cli_transports
  cargo test -p piqueld-ui
}

run_nix() {
  nix flake check --no-update-lock-file
}

case "${1:-all}" in
  portable)
    run_portable
    ;;
  contracts)
    run_contracts
    ;;
  nix)
    run_nix
    ;;
  all)
    run_portable
    run_contracts
    run_nix
    ;;
  *)
    echo "usage: $0 [portable|contracts|nix|all]" >&2
    exit 2
    ;;
esac
