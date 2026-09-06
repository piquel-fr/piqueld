#!/usr/bin/env bash
set -euo pipefail

forbidden='^(axum|axum-core|bollard|bollard-stubs|leptos|sqlx|sqlx-core)( |$)'
# Include normal, build, and development edges: the core boundary applies to all
# targets, not only to production library dependencies.
dependencies="$(cargo tree --package piqueld-core --edges all --prefix none)"

if printf '%s\n' "$dependencies" | grep -E "$forbidden"; then
  echo "piqueld-core contains a forbidden runtime dependency" >&2
  exit 1
fi

echo "piqueld-core dependency boundary is intact"
