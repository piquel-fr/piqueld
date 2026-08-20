#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
package="${1:-result}"
output="${2:-artifacts}"

if [[ ! -x "$package/bin/piqueld" || ! -x "$package/bin/piquelctl" ]]; then
  echo "usage: $0 NIX_PACKAGE_PATH [OUTPUT_DIRECTORY]" >&2
  echo "the package must contain bin/piqueld and bin/piquelctl" >&2
  exit 2
fi
if [[ ! -f "$package/share/piqueld/ui/index.html" ]]; then
  echo "the package does not contain the production UI bundle" >&2
  exit 2
fi
if [[ -n "$(git status --porcelain)" && "${ALLOW_DIRTY_RELEASE:-0}" != 1 ]]; then
  echo "refusing to package a dirty checkout" >&2
  exit 1
fi

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)"
commit="$(git rev-parse HEAD)"
epoch="${SOURCE_DATE_EPOCH:-$(git show -s --format=%ct HEAD)}"
case "$(uname -m)" in
  x86_64)
    target="x86_64-unknown-linux-gnu"
    nix_system="x86_64-linux"
    ;;
  aarch64)
    target="aarch64-unknown-linux-gnu"
    nix_system="aarch64-linux"
    ;;
  *)
    echo "unsupported release architecture" >&2
    exit 2
    ;;
esac
name="piqueld-${version}-${target}"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging/$name/bin" "$staging/$name/share/piqueld"
cp "$package/bin/piqueld" "$package/bin/piquelctl" "$staging/$name/bin/"
cp -R --no-preserve=mode "$package/share/piqueld/ui" "$staging/$name/share/piqueld/ui"

cat >"$staging/$name/RELEASE-METADATA" <<EOF
format=piqueld-release-v1
version=$version
target=$target
git_commit=$commit
source_date_epoch=$epoch
cargo_lock_sha256=$(sha256sum Cargo.lock | cut -d' ' -f1)
flake_lock_sha256=$(sha256sum flake.lock | cut -d' ' -f1)
nix_flake_attribute=packages.${nix_system}.release
EOF

chmod -R u=rwX,go=rX "$staging/$name"
mkdir -p "$output"
tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
  -C "$staging" -cf - "$name" | gzip -n >"$output/$name.tar.gz"
(
  cd "$output"
  sha256sum "$name.tar.gz" >SHA256SUMS
)
cp "$staging/$name/RELEASE-METADATA" "$output/RELEASE-METADATA"
