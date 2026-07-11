#!/usr/bin/env bash
set -euo pipefail

forbidden='^(axum|bollard|leptos|libsql|sqlx)( |$)'
dependencies="$(cargo tree --package piqueld-core --edges normal --prefix none)"

if printf '%s\n' "$dependencies" | grep -E "$forbidden"; then
  echo "piqueld-core contains a forbidden runtime dependency" >&2
  exit 1
fi

echo "piqueld-core dependency boundary is intact"

