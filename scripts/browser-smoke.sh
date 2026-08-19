#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
dist="${PIQUELD_UI_DIST:-$PWD/apps/piqueld-ui/dist}"
browser="${PIQUELD_BROWSER:-}"

if [[ -z "$browser" ]]; then
  for candidate in chromium chromium-browser google-chrome; do
    if command -v "$candidate" >/dev/null 2>&1; then
      browser="$candidate"
      break
    fi
  done
fi
if [[ -z "$browser" ]]; then
  echo "no Chromium-compatible browser found; set PIQUELD_BROWSER" >&2
  exit 2
fi
if [[ ! -f "$dist/index.html" ]]; then
  echo "UI bundle missing at $dist; run trunk build first" >&2
  exit 2
fi

work="$(mktemp -d)"
server_pid=""
cleanup() {
  status=$?
  if (( status != 0 )); then
    echo "browser smoke failed; diagnostic output follows" >&2
    for log in server.log browser.log; do
      if [[ -s "$work/$log" ]]; then
        echo "--- $log" >&2
        tail -n 200 "$work/$log" >&2
      fi
    done
    if [[ -s "$work/dom.html" ]]; then
      echo "--- rendered DOM (last 20000 bytes)" >&2
      tail -c 20000 "$work/dom.html" >&2
      echo >&2
    fi
  fi
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
  return "$status"
}
trap cleanup EXIT

python3 -m http.server 4173 --bind 127.0.0.1 --directory "$dist" \
  >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 50); do
  if curl --fail --silent http://127.0.0.1:4173/ >/dev/null; then
    break
  fi
  sleep 0.1
done
curl --fail --silent http://127.0.0.1:4173/ >/dev/null

"$browser" --headless --no-sandbox --disable-gpu --disable-dev-shm-usage \
  --virtual-time-budget=5000 --dump-dom http://127.0.0.1:4173/ \
  >"$work/dom.html" 2>"$work/browser.log"

grep -q 'id="dashboard-main"' "$work/dom.html"
grep -q '>Applications<' "$work/dom.html"
grep -q '>New application<' "$work/dom.html"
grep -q 'aria-label="Primary"' "$work/dom.html"
if grep -q 'canary-do-not-retain' "$work/dom.html"; then
  echo "secret canary unexpectedly appeared in rendered DOM" >&2
  exit 1
fi
