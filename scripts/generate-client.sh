#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
case "${1:-}" in
  ''|--check) ;;
  *) echo "usage: $0 [--check]" >&2; exit 1 ;;
esac

# Pin both the executable and its digest; generation is a development task only.
version=7.20.0
digest=871e0155287a87b579ff31096b2d45b1f95a115edfe631411ec6cff4848d0f03
jar="target/client-codegen/openapi-generator-cli-${version}.jar"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT

verify() {
  local actual
  if command -v sha256sum >/dev/null; then
    actual=$(sha256sum "$1")
  else
    actual=$(shasum -a 256 "$1")
  fi
  test "${actual%% *}" = "$digest"
}

if ! test -f "$jar"; then
  curl --fail --location --silent --show-error \
    "https://repo.maven.apache.org/maven2/org/openapitools/openapi-generator-cli/${version}/openapi-generator-cli-${version}.jar" \
    --output "$temporary/generator.jar"
  verify "$temporary/generator.jar"
  mkdir -p "$(dirname "$jar")"
  mv "$temporary/generator.jar" "$jar"
fi
if ! verify "$jar"; then
  echo "OpenAPI Generator checksum mismatch: $jar" >&2
  exit 1
fi

if ! java -jar "$jar" generate -g rust \
  -i docs/openapi-v1.json \
  -c tools/client-codegen/config.json \
  -t tools/client-codegen/templates \
  -o "$temporary/output" \
  --global-property apis,apiDocs=false,apiTests=false >"$temporary/generator.log" 2>&1; then
  cat "$temporary/generator.log" >&2
  exit 1
fi
generated="$temporary/output/src/apis/default_api.rs"
# The template emits one module. Fail visibly if tags split operations into
# multiple files, rather than silently dropping the newly grouped endpoints.
api_files=("$temporary/output/src/apis/"*.rs)
if test "${#api_files[@]}" != 1 || test "${api_files[0]}" != "$generated"; then
  echo "Expected a single default API module; update client generation for the new API groups." >&2
  exit 1
fi
rustfmt --edition 2024 --config-path . "$generated"
destination=crates/piqueld-client/src/generated.rs
if test "${1:-}" = --check; then
  if ! diff -u "$destination" "$generated"; then
    echo "Client bindings are stale; run just generate-client." >&2
    exit 1
  fi
else
  cp "$generated" "$destination"
fi
