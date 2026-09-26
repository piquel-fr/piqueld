#!/usr/bin/env bash
# Run Chromium in a pinned container; keep the Rust fixture and test runner local.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root/e2e"
version=$(node -p 'require("./package.json").devDependencies["@playwright/test"]')
image="mcr.microsoft.com/playwright:v${version}-noble"
# Docker shares the host network so WebAuthn still uses secure-context localhost.
# Select a free port; only the test browser server listens here, on loopback.
port=$(node --input-type=module -e 'import net from "node:net";const s=net.createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})')
container=""
cleanup() {
    if [[ -n "$container" ]]; then docker rm -f "$container" >/dev/null; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# No implicit browser download: setup-e2e pulls this exact image explicitly.
container=$(docker run --detach --pull=never --init --network=host --ipc=host \
    --mount "type=bind,src=$root/e2e,dst=/tests,readonly" --workdir /tests \
    "$image" node node_modules/@playwright/test/cli.js run-server --host 127.0.0.1 --port "$port")
for ((attempt=0; attempt<100; attempt++)); do
    if docker logs "$container" 2>&1 | grep -q 'Listening on'; then break; fi
    if [[ $(docker inspect --format '{{.State.Running}}' "$container") != true ]]; then
        docker logs "$container"
        exit 1
    fi
    sleep 0.1
done
if ! docker logs "$container" 2>&1 | grep -q 'Listening on'; then
    docker logs "$container"
    echo 'Playwright browser server did not become ready' >&2
    exit 1
fi
export PW_TEST_CONNECT_WS_ENDPOINT="ws://127.0.0.1:$port/"
pnpm exec playwright test "$@"
