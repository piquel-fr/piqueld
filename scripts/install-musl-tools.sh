#!/usr/bin/env bash
# Install musl-tools for the static release build. GitHub's runners fetch
# packages through an Azure mirror, which is sometimes unreachable for hours;
# on failure, retry once through Ubuntu's own archive.
set -euo pipefail

install() {
  sudo apt-get update -qq && sudo apt-get install --yes --no-install-recommends musl-tools
}

install || {
  echo "::warning::the runner's package mirror failed; retrying through archive.ubuntu.com"
  echo 'http://archive.ubuntu.com/ubuntu/' | sudo tee /etc/apt/apt-mirrors.txt >/dev/null
  install
}
