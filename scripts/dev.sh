#!/usr/bin/env bash
set -Eeuo pipefail

declare -a child_pids=()

cleanup() {
    local child

    trap - EXIT INT TERM

    # Each process is started in its own session so cargo-watch's cargo run
    # child and Trunk's build children are stopped with their supervisors.
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

tailwindcss --input apps/piqueld-ui/tailwind.css \
    --output apps/piqueld-ui/generated/style.css --minify

setsid cargo watch \
    --watch apps/piqueld --watch crates --watch Cargo.toml --watch Cargo.lock \
    --exec 'run --package piqueld --bin piqueld -- --config config/piqueld.example.toml' &
child_pids+=("$!")

setsid tailwindcss --input apps/piqueld-ui/tailwind.css \
    --output apps/piqueld-ui/generated/style.css --watch=always &
child_pids+=("$!")

(
    cd apps/piqueld-ui
    exec setsid env -u NO_COLOR trunk watch \
        --dist /tmp/piqueld-dev/ui \
        --public-url /dashboard/
) &
child_pids+=("$!")

set +e
wait -n "${child_pids[@]}"
status=$?
set -e

printf 'development process exited with status %d; stopping the remaining processes\n' "$status" >&2
exit "$status"
