# The marketing site, apps/site, served by the `site` service of the
# `piqueld-dev` application in deploy/piqueld-dev.toml. It is static files with
# no build step. Build it from the repository root:
#
#   docker build --file deploy/site/Dockerfile .
#
# See docs/dev/publishing.md.

# Serves the site on port 8080. The image sets no user, so this runs the server
# as an unprivileged one: it binds an unprivileged port and reads the
# world-readable files.
FROM joseluisq/static-web-server:2.44.1@sha256:94618e3d17802265a21926217e6f9e7832b311b6fbf5933684001739fafed06a
ENV SERVER_PORT=8080
COPY apps/site /public
USER 65532:65532
