#!/bin/sh
# Installs piqueld on an x86_64 Linux host with systemd and Docker Engine:
#
#   curl -fsSL https://piqueld.dev/install.sh | sudo sh
#
# It installs the daemon and piquelctl from a GitHub release to /usr/local/bin,
# creates the piqueld user, writes /etc/piqueld/config.toml unless it exists,
# and installs, enables and (re)starts the piqueld systemd unit. Running it
# again installs another version over the current one, keeping the
# configuration and data. See
# https://docs.piqueld.dev/user/getting-started/install.html.
#
# Environment:
#   PIQUELD_VERSION      the version to install, such as 0.1.0; the latest
#                        release by default.
#   PIQUELD_RELEASE_URL  where the version's release files are; its GitHub
#                        release by default. CI points it at a local build.
#
# Everything is in functions that run on the last line, so a download cut off
# midway runs nothing.

set -eu

repository=https://github.com/sourceorigin-dev/piqueld
prefix=/usr/local/bin
config=/etc/piqueld/config.toml
unit=/etc/systemd/system/piqueld.service

say() {
    printf 'piqueld: %s\n' "$*"
}

fail() {
    printf 'piqueld: error: %s\n' "$*" >&2
    exit 1
}

# Fails unless this is an x86_64 Linux host running systemd and Docker Engine,
# with the commands this script and the daemon use.
check_host() {
    [ "$(id -u)" = 0 ] || fail "run this as root: curl -fsSL https://piqueld.dev/install.sh | sudo sh"
    [ "$(uname -s)" = Linux ] || fail "piqueld runs on Linux, not $(uname -s)"
    [ "$(uname -m)" = x86_64 ] || fail "piqueld has release binaries for x86_64 only, not $(uname -m)"
    [ -d /run/systemd/system ] || fail "systemd isn't running; piqueld runs as a systemd service"
    missing=
    for command in curl tar sha256sum useradd systemctl docker git ssh; do
        command -v "$command" >/dev/null 2>&1 || missing="$missing $command"
    done
    [ -z "$missing" ] || fail "install these commands first:$missing (git and ssh fetch your repositories)"
    docker info >/dev/null 2>&1 || fail "Docker Engine isn't running; install it from https://docs.docker.com/engine/install/ and start it"
    getent group docker >/dev/null || fail "the docker group is missing; Docker Engine's packages create it"
}

download() {
    curl --proto '=https,file' --fail --silent --show-error --location --output "$2" "$1"
}

# Prints the version to install: PIQUELD_VERSION, without a leading v, or the
# latest release's, from where GitHub redirects its latest release.
version() {
    if [ -n "${PIQUELD_VERSION:-}" ]; then
        echo "${PIQUELD_VERSION#v}"
        return
    fi
    latest=$(curl --proto '=https' --fail --silent --show-error --location --head \
        --output /dev/null --write-out '%{url_effective}' "$repository/releases/latest") ||
        fail "couldn't find the latest release of piqueld"
    case "$latest" in
    */releases/tag/v*) echo "${latest##*/tag/v}" ;;
    *) fail "piqueld has no release yet" ;;
    esac
}

# Downloads the version's archive to the directory $1, checks it against the
# release's checksums, and unpacks it there.
fetch() {
    archive="piqueld-$version-x86_64-linux.tar.gz"
    release="${PIQUELD_RELEASE_URL:-$repository/releases/download/v$version}"
    say "downloading piqueld $version"
    download "$release/$archive" "$1/$archive" || fail "couldn't download $release/$archive"
    download "$release/SHA256SUMS" "$1/SHA256SUMS" || fail "couldn't download $release/SHA256SUMS"
    (cd "$1" && grep "  $archive\$" SHA256SUMS | sha256sum --check --status) ||
        fail "$archive doesn't match the release's checksum"
    tar --extract --gzip --file "$1/$archive" --directory "$1"
}

# Writes the configuration the daemon starts with: the dashboard at
# http://localhost:7845 of this host.
write_config() {
    install -d -m 0755 "$(dirname "$config")"
    (
        umask 027
        cat >"$config" <<'EOF'
#:schema https://raw.githubusercontent.com/sourceorigin-dev/piqueld/main/docs/piqueld-config.schema.json
# piqueld's configuration. Every setting is described at
# https://docs.piqueld.dev/user/reference/configuration-settings.html.
# Apply a change with: sudo systemctl restart piqueld

[server]
# Serves the dashboard at http://localhost:7845 of this host. To open it from
# another machine, forward the port: ssh -L 7845:localhost:7845 <host>
# For a tailnet address or your own reverse proxy instead, see
# https://docs.piqueld.dev/user/getting-started/install.html#choose-how-to-reach-the-dashboard
listen_mode = "localhost"
EOF
    )
    chgrp piqueld "$config"
    say "wrote $config"
}

# Waits up to a minute for the daemon to answer on its socket.
wait_for_daemon() {
    attempts=0
    until "$prefix/piquelctl" status >/dev/null 2>&1; do
        attempts=$((attempts + 1))
        [ "$attempts" -lt 60 ] ||
            fail "piqueld didn't start; see its logs with: journalctl -u piqueld"
        sleep 1
    done
}

main() {
    check_host
    version=$(version)
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    fetch "$work"
    files="$work/piqueld-$version-x86_64-linux"

    install -m 0755 "$files/piqueld" "$files/piquelctl" "$prefix/"
    say "installed piqueld and piquelctl to $prefix"
    if ! getent passwd piqueld >/dev/null; then
        useradd --system --user-group --groups docker --home-dir /var/lib/piqueld \
            --no-create-home --shell "$(command -v nologin || echo /bin/false)" piqueld
        say "created the piqueld user"
    fi
    [ -e "$config" ] || write_config
    install -m 0644 "$files/piqueld.service" "$unit"
    systemctl daemon-reload
    systemctl enable --quiet piqueld
    systemctl restart piqueld
    wait_for_daemon

    "$prefix/piquelctl" status
    cat <<EOF

piqueld $version is running. Next:
  - Check the installation:           sudo piquelctl doctor
  - Let a user run piquelctl:         sudo usermod -aG piqueld <user>
  - Create your account:              https://docs.piqueld.dev/user/getting-started/first-account.html
EOF
}

main "$@"
