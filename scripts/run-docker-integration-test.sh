#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "docker-test requires a Linux host with a local Docker-compatible daemon" >&2
  exit 1
fi
if ! command -v docker >/dev/null 2>&1; then
  echo "docker-test requires the Docker CLI" >&2
  exit 1
fi

if [[ -n "${DOCKER_CONTEXT:-}" ]]; then
  docker_endpoint="$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}')"
elif [[ -n "${DOCKER_HOST:-}" ]]; then
  docker_endpoint="$DOCKER_HOST"
else
  # An empty context name is not inspectable; fall back to the default socket.
  # A lookup failure must abort rather than silently select the fallback,
  # because later Docker commands would still target the CLI-resolved context.
  if ! docker_context="$(docker context show)"; then
    echo "docker-test could not resolve the Docker context" >&2
    exit 1
  fi
  if [[ -n "$docker_context" ]]; then
    docker_endpoint="$(docker context inspect "$docker_context" --format '{{.Endpoints.docker.Host}}')"
  else
    docker_endpoint="unix:///var/run/docker.sock"
  fi
fi
if [[ "$docker_endpoint" != unix:///* ]]; then
  echo "docker-test requires a local Unix-socket Docker endpoint, found: $docker_endpoint" >&2
  exit 1
fi
if ! docker info >/dev/null 2>&1; then
  echo "docker-test requires access to a running Docker-compatible daemon" >&2
  exit 1
fi

dind_image="${PIQUELD_DIND_IMAGE:-docker:29.6.2-dind}"
runtime_dir="$(mktemp -d -t piqueld-dind.XXXXXXXX)"
socket_path="$runtime_dir/docker.sock"
container_id=""

cleanup() {
  if [[ -n "$container_id" ]]; then
    docker rm --force "$container_id" >/dev/null 2>&1 || true
  fi
  if [[ -n "$runtime_dir" && -d "$runtime_dir" && "$(basename "$runtime_dir")" == piqueld-dind.* ]]; then
    rm -rf -- "$runtime_dir"
  fi
}
trap cleanup EXIT INT TERM

container_id="$(docker run \
  --detach \
  --privileged \
  --env DOCKER_TLS_CERTDIR= \
  --volume "$runtime_dir:/piqueld-socket" \
  "$dind_image" \
  dockerd \
  --host=unix:///piqueld-socket/docker.sock \
  --storage-driver=vfs \
)"

for _attempt in {1..60}; do
  if docker exec \
    --env DOCKER_HOST=unix:///piqueld-socket/docker.sock \
    "$container_id" \
    docker info \
    >/dev/null 2>&1
  then
    docker exec "$container_id" chmod 666 /piqueld-socket/docker.sock
    # A hung test must not hang the harness forever; --kill-after forces a
    # SIGKILL when cargo test ignores the initial SIGTERM.
    if command -v timeout >/dev/null 2>&1; then
      test_wrapper=(timeout --kill-after=30s "${PIQUELD_DOCKER_TEST_TIMEOUT:-15m}")
    else
      echo "docker-test requires GNU timeout to bound the test run" >&2
      exit 1
    fi
    PIQUELD_DOCKER_ISOLATED=1 \
      PIQUELD_DOCKER_SOCKET="$socket_path" \
      "${test_wrapper[@]}" \
      cargo test -p piqueld --test docker_integration -- --ignored
    exit 0
  fi
  if [[ "$(docker inspect --format '{{.State.Running}}' "$container_id" 2>/dev/null || true)" != "true" ]]; then
    echo "the isolated Docker daemon exited before becoming ready" >&2
    docker logs "$container_id" >&2 || true
    exit 1
  fi
  sleep 1
done

echo "the isolated Docker daemon did not become ready within 60 seconds" >&2
docker logs "$container_id" >&2 || true
exit 1
