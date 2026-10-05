# shellcheck shell=bash
# Docker-in-Docker helpers shared by `just docker-test` and `just dev`. Source
# this file; each isolated engine listens on /piqueld-socket/docker.sock inside
# its container, a bind mount of a private host directory.

dind_image="${PIQUELD_DIND_IMAGE:-docker:29.6.2-dind@sha256:bfec1f5159c63a81ca6fdedbd81404d2c0e16378ed0feec3bb3fbf3998847659}"

# Waits up to 60 seconds for the engine in container $1, then opens its socket
# to the host user, whose private directory still guards it. Prints the
# engine's logs and fails if it exits or never becomes ready.
dind_wait() {
  local container=$1
  for _attempt in {1..60}; do
    if docker exec \
      --env DOCKER_HOST=unix:///piqueld-socket/docker.sock \
      "$container" \
      docker info \
      >/dev/null 2>&1
    then
      docker exec "$container" chmod 666 /piqueld-socket/docker.sock
      return 0
    fi
    if [[ "$(docker inspect --format '{{.State.Running}}' "$container" 2>/dev/null || true)" != "true" ]]; then
      echo "the isolated Docker daemon exited before becoming ready" >&2
      docker logs "$container" >&2 || true
      return 1
    fi
    sleep 1
  done

  echo "the isolated Docker daemon did not become ready within 60 seconds" >&2
  docker logs "$container" >&2 || true
  return 1
}
