#!/usr/bin/env bash
set -Eeuo pipefail

declare -a child_pids=()

cleanup() {
    local child deadline

    trap - EXIT
    # Repeated signals must not interrupt cleanup and orphan the daemon.
    trap '' INT TERM HUP

    # Each process is started in its own session so cargo-watch's cargo run
    # child and its build-script children are stopped with their supervisors.
    for child in "${child_pids[@]}"; do
        pkill -TERM --session "$child" 2>/dev/null || true
    done

    # The daemon allows ten seconds to finish in-flight requests. Watch may
    # exit first, so check the entire session and bound the grace period.
    deadline=$((SECONDS + 11))
    while ((SECONDS < deadline)); do
        local running=false
        for child in "${child_pids[@]}"; do
            if pgrep --session "$child" >/dev/null; then
                running=true
                break
            fi
        done
        "$running" || break
        sleep 0.1
    done

    # Reap supervisors only after terminating any unresponsive descendants.
    for child in "${child_pids[@]}"; do
        pkill -KILL --session "$child" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}

handle_signal() {
    cleanup
    exit 0
}

trap cleanup EXIT
trap handle_signal INT TERM HUP

mkdir -p apps/piqueld-ui/generated
# Existing directories are validated by the daemon, never silently chmodded.
mkdir -p -m 0700 /tmp/piqueld-dev-run

# The daemon embeds the dashboard at compile time, so the UI crate is watched
# too: every dashboard edit re-runs the build script (Tailwind + Trunk) and
# restarts the daemon. The build script's own outputs are ignored so a rebuild
# cannot trigger itself.
# Keep the command in the session managed by cleanup: cargo-watch otherwise
# creates another session that survives if the watcher exits first. Exec makes
# cargo (and then the daemon) the watcher's direct child, so reloads still stop
# and reap the daemon before starting its replacement.
setsid cargo watch \
    --no-process-group \
    --watch apps/piqueld --watch apps/piqueld-ui --watch crates \
    --watch Cargo.toml --watch Cargo.lock \
    --ignore 'apps/piqueld-ui/generated' \
    --shell 'exec cargo run --package piqueld --bin piqueld --features embedded-ui -- --config examples/piqueld.toml' &
child_pids+=("$!")

set +e
wait -n "${child_pids[@]}"
status=$?
set -e

printf 'development process exited with status %d; stopping the remaining processes\n' "$status" >&2
exit "$status"
