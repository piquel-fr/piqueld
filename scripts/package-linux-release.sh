#!/usr/bin/env bash
# Package native Linux builds. Build these on Ubuntu 24.04, outside Nix.
set -euo pipefail
release_version="${1:?usage: package-linux-release.sh VERSION [BINARY_DIRECTORY] [OUTPUT_DIRECTORY]}"
binary_dir="${2:-target/release}"
output_dir="${3:-dist}"
[[ "$release_version" =~ ^[a-zA-Z0-9._-]+$ ]] || { echo 'invalid version' >&2; exit 1; }
[[ "$(uname -s)" == Linux ]] || { echo 'Linux required' >&2; exit 1; }
release_arch="$(uname -m)"
case "$release_arch" in x86_64|aarch64) ;; *) echo 'unsupported architecture' >&2; exit 1 ;; esac
release_name="piqueld-${release_version}-linux-${release_arch}"
release_stage="$(mktemp -d)"
trap 'rm -rf -- "$release_stage"' EXIT
mkdir -p "$release_stage/$release_name/bin" "$output_dir"
for binary in piqueld piquelctl; do
  binary_path="$binary_dir/$binary"
  [[ -x "$binary_path" ]] || { echo "missing executable: $binary_path" >&2; exit 1; }
  # Nix interpreters and runtime paths cannot be shipped in a portable archive.
  elf_headers="$(readelf -l -d "$binary_path")"
  if [[ "$elf_headers" == *"/nix/store"* ]]; then
    echo "$binary uses a Nix interpreter or runtime search path" >&2; exit 1
  fi
  install -m755 "$binary_path" "$release_stage/$release_name/bin/$binary"
done
install -m644 examples/piqueld.toml "$release_stage/$release_name/piqueld.example.toml"
cat > "$release_stage/$release_name/BUILD.txt" <<META
Version: $release_version
Commit: $(git rev-parse HEAD)
Architecture: $release_arch
Runtime baseline: Ubuntu 24.04 (glibc 2.39), or compatible newer Linux
Daemon: embedded dashboard
External runtime tools: Docker Engine/CLI, Git, OpenSSH for SSH repositories
META
cp docs/releases.md "$release_stage/$release_name/README.md"
tar -C "$release_stage" -czf "$output_dir/$release_name.tar.gz" "$release_name"
(cd "$output_dir" && sha256sum "$release_name.tar.gz" > "$release_name.tar.gz.sha256")
printf '%s\n' "$output_dir/$release_name.tar.gz"
