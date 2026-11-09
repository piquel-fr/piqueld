#!/bin/sh
# Installs piqueld on an x86_64 Linux host with systemd and Docker Engine. Run
# it as yourself:
#
#   curl -fsSL https://piqueld.dev/install.sh | sh
#
# It says what it will do and asks before changing anything. It installs the
# daemon and piquelctl from a GitHub release to /usr/local/bin, creates the
# piqueld user, writes /etc/piqueld/config.toml unless it exists, installs and
# (re)starts the piqueld systemd service, and offers to add you to the piqueld
# group, so that you run piquelctl without sudo. The steps that need root run
# with sudo, and print their command first. Running it again updates piqueld,
# keeping its configuration and data. See
# https://docs.piqueld.dev/user/getting-started/install.html.
#
# Options:
#   --yes                Don't ask: install, and add the user running the
#                        script to the piqueld group. Without a terminal to
#                        ask on, the script needs it.
#
# Environment:
#   PIQUELD_VERSION      The version to install, such as 0.1.0; the latest
#                        release by default.
#   PIQUELD_RELEASE_URL  Where the version's release files are; its GitHub
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
    printf '%s\n' "$*"
}

fail() {
    printf 'piqueld: error: %s\n' "$*" >&2
    exit 1
}

# Runs a command as root, printing it first: through sudo, unless the script
# already runs as root.
as_root() {
    say "  \$ ${sudo:+sudo }$*"
    $sudo "$@"
}

# Asks a yes-or-no question on the terminal, yes by default. With --yes, the
# answer is yes.
confirm() {
    [ "$yes" = 1 ] && return
    printf '%s [Y/n] ' "$1" >/dev/tty
    answer=
    read -r answer </dev/tty || true
    case "$answer" in
    "" | [Yy]*) return 0 ;;
    *) return 1 ;;
    esac
}

# Fails unless this is an x86_64 Linux host running systemd and Docker Engine,
# with the commands this script and the daemon use, and a way to ask.
check_host() {
    [ "$(uname -s)" = Linux ] || fail "piqueld runs on Linux, not $(uname -s)"
    [ "$(uname -m)" = x86_64 ] || fail "piqueld has release binaries for x86_64 only, not $(uname -m)"
    [ -d /run/systemd/system ] || fail "systemd isn't running; piqueld runs as a systemd service"
    missing=
    for command in curl tar sha256sum useradd systemctl docker git ssh; do
        command -v "$command" >/dev/null 2>&1 || missing="$missing $command"
    done
    [ -z "$missing" ] || fail "install these commands first:$missing (git and ssh fetch your repositories)"
    # Some hosts start Docker on its socket's first use.
    systemctl is-active --quiet docker.service || systemctl is-active --quiet docker.socket ||
        fail "Docker Engine isn't running; install it from https://docs.docker.com/engine/install/ and start it"
    getent group docker >/dev/null || fail "the docker group is missing; Docker Engine's packages create it"
    if [ "$(id -u)" = 0 ]; then
        sudo=
    else
        command -v sudo >/dev/null 2>&1 || fail "run this as root, or install sudo"
        sudo=sudo
    fi
    if [ "$yes" = 0 ] && ! (: </dev/tty) 2>/dev/null; then
        fail "there's no terminal to ask on; to install without asking, run: curl -fsSL https://piqueld.dev/install.sh | sh -s -- --yes"
    fi
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

# Says what the script will do, and asks to go ahead. Sets `join` when the user
# running it should join the piqueld group.
plan() {
    installed=$("$prefix/piqueld" --version 2>/dev/null || true)
    if [ -n "$installed" ]; then
        say "This updates ${installed} to piqueld $version, on this host:"
    else
        say "This installs piqueld $version on this host:"
    fi
    say "  - Download $archive from"
    say "    $release,"
    say "    and check it against the release's SHA256SUMS."
    say "  - Install piqueld and piquelctl to $prefix."
    getent passwd piqueld >/dev/null ||
        say "  - Create the piqueld system user, in the docker group."
    if [ -e "$config" ]; then
        say "  - Keep the configuration in $config."
    else
        say "  - Write the configuration to $config, which serves"
        say "    the dashboard at http://localhost:7845 of this host."
    fi
    say "  - Install the piqueld service as $unit,"
    say "    then enable and (re)start it."
    [ -z "$sudo" ] || say "Each step that needs root runs with sudo, and prints its command first."
    say
    if ! confirm "Continue?"; then
        say "Nothing was changed."
        exit 0
    fi

    join=0
    user=$(id -un)
    if [ -n "$sudo" ] && ! id -nG "$user" | tr ' ' '\n' | grep -qx piqueld; then
        say
        say "Members of the piqueld group run piquelctl on this host without sudo."
        if confirm "Add $user to the piqueld group?"; then
            join=1
        fi
    fi
    say
}

# Downloads the version's archive to the directory $1, checks it against the
# release's checksums, and unpacks it there.
fetch() {
    say "Downloading $archive"
    curl --proto '=https,file' --fail --silent --show-error --location \
        --output "$1/$archive" "$release/$archive" ||
        fail "couldn't download $release/$archive"
    curl --proto '=https,file' --fail --silent --show-error --location \
        --output "$1/SHA256SUMS" "$release/SHA256SUMS" ||
        fail "couldn't download $release/SHA256SUMS"
    (cd "$1" && grep "  $archive\$" SHA256SUMS | sha256sum --check --status) ||
        fail "$archive doesn't match the release's checksum"
    tar --extract --gzip --file "$1/$archive" --directory "$1"
}

# Writes to $1 the configuration the daemon starts with: the dashboard at
# http://localhost:7845 of this host.
default_config() {
    cat >"$1" <<'EOF'
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
}

# Waits up to a minute for the daemon to answer on its socket.
wait_for_daemon() {
    say "Waiting for piqueld to start"
    attempts=0
    until $sudo "$prefix/piquelctl" status >/dev/null 2>&1; do
        attempts=$((attempts + 1))
        [ "$attempts" -lt 60 ] ||
            fail "piqueld didn't start; see its logs with: ${sudo:+sudo }journalctl -u piqueld"
        sleep 1
    done
    as_root "$prefix/piquelctl" status
}

# Says what to do next, with piquelctl run as the user can run it.
next_steps() {
    piquelctl=piquelctl
    [ -n "$sudo" ] && [ "$join" = 0 ] && piquelctl="sudo piquelctl"
    say
    say "piqueld $version is running. Next:"
    [ "$join" = 0 ] ||
        say "  - Log out and back in, so that your membership of the piqueld group applies."
    say "  - Create your account: run \`$piquelctl setup-link\`, open the link it prints,"
    say "    and sign up with a passkey. Then sign piquelctl in with \`$piquelctl login\`."
    say "    https://docs.piqueld.dev/user/getting-started/first-account.html"
    say "  - Check the installation: $piquelctl doctor"
}

main() {
    yes=0
    for argument in "$@"; do
        case "$argument" in
        --yes | -y) yes=1 ;;
        *) fail "unknown option $argument; the only option is --yes" ;;
        esac
    done
    check_host
    version=$(version)
    archive="piqueld-$version-x86_64-linux.tar.gz"
    release="${PIQUELD_RELEASE_URL:-$repository/releases/download/v$version}"
    plan

    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    fetch "$work"
    files="$work/piqueld-$version-x86_64-linux"
    [ -z "$sudo" ] || say "Running the steps that need root with sudo"
    as_root install -m 0755 "$files/piqueld" "$files/piquelctl" "$prefix/"
    getent passwd piqueld >/dev/null ||
        as_root useradd --system --user-group --groups docker --home-dir /var/lib/piqueld \
            --no-create-home --shell "$(command -v nologin || echo /bin/false)" piqueld
    if [ ! -e "$config" ]; then
        default_config "$work/config.toml"
        as_root install -D -m 0640 -g piqueld "$work/config.toml" "$config"
    fi
    as_root install -m 0644 "$files/piqueld.service" "$unit"
    as_root systemctl daemon-reload
    as_root systemctl enable --quiet piqueld
    as_root systemctl restart piqueld
    [ "$join" = 0 ] || as_root usermod --append --groups piqueld "$user"
    wait_for_daemon
    next_steps
}

main "$@"
