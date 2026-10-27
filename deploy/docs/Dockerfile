# The documentation book, served by the `piqueld-docs` application in
# piqueld.toml next to this file. Build it from the repository root:
#
#   docker build --file deploy/docs/Dockerfile .
#
# See docs/dev/publishing.md.

# Builds the book with the mdBook that flake.lock pins, through the same
# `nix develop .#docs` shell as `just docs` and CI.
FROM nixos/nix:2.35.4@sha256:f159a58f390677f41e8e900a74b6e2f9c7f27b364122ae6acf7a652ce6c551c0 AS book
ENV NIX_CONFIG="experimental-features = nix-command flakes"
# The flake alone first, so editing the book keeps the toolchain layer cached.
WORKDIR /flake
COPY flake.nix flake.lock ./
RUN nix develop .#docs --command mdbook --version
WORKDIR /book
COPY book.toml ./
COPY docs docs
RUN nix develop /flake#docs --command mdbook build

# Serves the built book on port 8080, with its 404.html for missing pages. The
# image sets no user, so this runs the server as an unprivileged one: it binds
# an unprivileged port and reads the world-readable book.
FROM joseluisq/static-web-server:2.44.1@sha256:94618e3d17802265a21926217e6f9e7832b311b6fbf5933684001739fafed06a
ENV SERVER_PORT=8080
COPY --from=book /book/target/book /public
USER 65532:65532
