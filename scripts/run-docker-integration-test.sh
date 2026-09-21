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

dind_image="${PIQUELD_DIND_IMAGE:-docker:29.6.2-dind@sha256:bfec1f5159c63a81ca6fdedbd81404d2c0e16378ed0feec3bb3fbf3998847659}"
runtime_dir="$(mktemp -d -t piqueld-dind.XXXXXXXX)"
socket_path="$runtime_dir/docker.sock"
container_id=""

cleanup() {
  if [[ -n "$container_id" ]]; then
    docker rm --force --volumes "$container_id" >/dev/null 2>&1 || true
  fi
  if [[ -n "$runtime_dir" && -d "$runtime_dir" && "$(basename "$runtime_dir")" == piqueld-dind.* ]]; then
    rm -rf -- "$runtime_dir"
  fi
}
trap cleanup EXIT INT TERM

# Pre-mount /tmp so the DinD entrypoint does not hide the nested data bind.
container_id="$(docker run \
  --detach \
  --privileged \
  --tmpfs /tmp:rw,exec,dev \
  --env DOCKER_TLS_CERTDIR= \
  --volume "$runtime_dir:/piqueld-socket" \
  --volume "$runtime_dir:$runtime_dir" \
  --publish 127.0.0.1::80 \
  --publish 127.0.0.1::443 \
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
    # SIGKILL when the test runner ignores the initial SIGTERM.
    if command -v timeout >/dev/null 2>&1; then
      test_wrapper=(timeout --kill-after=30s "${PIQUELD_DOCKER_TEST_TIMEOUT:-15m}")
    else
      echo "docker-test requires GNU timeout to bound the test run" >&2
      exit 1
    fi
    http_port="$(docker inspect --format '{{(index (index .NetworkSettings.Ports "80/tcp") 0).HostPort}}' "$container_id")"
    https_port="$(docker inspect --format '{{(index (index .NetworkSettings.Ports "443/tcp") 0).HostPort}}' "$container_id")"
    PIQUELD_DOCKER_ISOLATED=1 \
      PIQUELD_DOCKER_SOCKET="$socket_path" \
      PIQUELD_DOCKER_DATA_DIR="$runtime_dir" \
      PIQUELD_INGRESS_HTTP_PORT="$http_port" \
      PIQUELD_INGRESS_HTTPS_PORT="$https_port" \
      "${test_wrapper[@]}" \
      cargo nextest run --locked -p piqueld --lib --run-ignored only --no-capture -E 'test(ingress_caddy)' --test-threads=1
    # Tests share one daemon and mutate its Swarm state.
    PIQUELD_DOCKER_ISOLATED=1 \
      PIQUELD_DOCKER_SOCKET="$socket_path" \
      "${test_wrapper[@]}" \
      cargo nextest run --locked -p piqueld --test docker_integration --run-ignored only --test-threads=1
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
