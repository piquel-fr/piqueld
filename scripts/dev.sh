#!/usr/bin/env bash
set -Eeuo pipefail

declare -a child_pids=()

cleanup() {
    local child

    trap - EXIT INT TERM

    # Each process is started in its own session so cargo-watch's cargo run
    # child and its build-script children are stopped with their supervisors.
    for child in "${child_pids[@]}"; do
        pkill -TERM --session "$child" 2>/dev/null || true
    done

    wait 2>/dev/null || true

    # A failed cargo-watch command can exit before its cargo child does.
    # Ensure those detached children cannot keep the API port occupied.
    for child in "${child_pids[@]}"; do
        pkill -KILL --session "$child" 2>/dev/null || true
    done
}

handle_signal() {
    cleanup
    exit 0
}

trap cleanup EXIT INT TERM
trap handle_signal INT TERM

mkdir -p apps/piqueld-ui/generated

# The daemon embeds the dashboard at compile time, so the UI crate is watched
# too: every dashboard edit re-runs the build script (Tailwind + Trunk) and
# restarts the daemon. The build script's own outputs are ignored so a rebuild
# cannot trigger itself.
setsid cargo watch \
    --watch apps/piqueld --watch apps/piqueld-ui --watch crates \
    --watch Cargo.toml --watch Cargo.lock \
    --ignore 'apps/piqueld-ui/generated' \
    --exec 'run --package piqueld --bin piqueld --features embedded-ui -- --config config/piqueld.example.toml' &
child_pids+=("$!")

set +e
wait -n "${child_pids[@]}"
status=$?
set -e

printf 'development process exited with status %d; stopping the remaining processes\n' "$status" >&2
exit "$status"
