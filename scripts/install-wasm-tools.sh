#!/usr/bin/env bash
# Install the CLI and browser-test runner matching Cargo.lock into the CI cache.
set -euo pipefail

version="$(awk '$0 == "name = \"wasm-bindgen\"" { getline; gsub(/"/, "", $3); print $3; exit }' Cargo.lock)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'invalid wasm-bindgen lockfile version' >&2; exit 1; }
bindir="${RUNNER_TEMP:?}/piqueld-tools/wasm-bindgen-$version"
if [[ ! -x "$bindir/wasm-bindgen" || ! -x "$bindir/wasm-bindgen-test-runner" ]]; then
  download="$(mktemp -d "${RUNNER_TEMP}/wasm-bindgen.XXXXXXXX")"
  trap 'rm -rf "$download"' EXIT
  archive="wasm-bindgen-$version-x86_64-unknown-linux-musl.tar.gz"
  release="https://github.com/wasm-bindgen/wasm-bindgen/releases/download/$version"
  curl --fail --location --silent --show-error "$release/$archive" -o "$download/$archive"
  curl --fail --location --silent --show-error "$release/$archive.sha256sum" -o "$download/checksum"
  awk -v archive="$archive" '{print $1 "  " archive}' "$download/checksum" \
    | (cd "$download" && sha256sum --check --strict -)
  tar -xzf "$download/$archive" -C "$download"
  mkdir -p "$bindir"
  install -m755 "$download/wasm-bindgen-$version-x86_64-unknown-linux-musl/wasm-bindgen" "$bindir/"
  install -m755 "$download/wasm-bindgen-$version-x86_64-unknown-linux-musl/wasm-bindgen-test-runner" "$bindir/"
fi
printf '%s\n' "$bindir" >> "${GITHUB_PATH:?}"
