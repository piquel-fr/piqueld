# Linux release archives

The CI embedded-release job builds the daemon (including the dashboard) and CLI
on Ubuntu 24.04. It packages them with example configuration, build metadata and
a SHA-256 checksum. These archives target the runner's architecture and require
glibc 2.39 or newer. They are not Nix packages.

Extract the archive and add its `bin` directory to PATH. Install Docker Engine,
the Docker CLI, Git and OpenSSH when deploying SSH repositories. Copy the example
configuration, set a private writable data directory, and start
`piqueld --config /path/to/piqueld.toml`. The dashboard is embedded in the daemon.

Build with `just build-embedded` and
`cargo build --release --locked --package piquelctl` on the documented baseline,
then run `scripts/package-linux-release.sh VERSION`. The script rejects Nix ELF
interpreters and search paths. CI verifies the archive in a fresh Ubuntu
container without the build environment or Nix store before uploading it.

Archives are reviewable CI artifacts. This change does not publish a GitHub
release automatically.
