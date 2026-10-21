#!/usr/bin/env bash
set -euo pipefail

image="${1:?Usage: test-container.sh IMAGE PLATFORM}"
platform="${2:?Usage: test-container.sh IMAGE PLATFORM}"
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
test_dir=$(mktemp -d)
container=""

cleanup() {
    if [[ -n "$container" ]]; then
        docker logs "$container" || true
        docker rm -f "$container" >/dev/null || true
    fi
    rm -rf "$test_dir"
}
trap cleanup EXIT

docker run --rm --platform "$platform" "$image" --help >/dev/null
docker run --rm --platform "$platform" "$image" --version
docker run --rm --platform "$platform" \
    --mount "type=bind,src=$root/examples/docker,dst=/etc/stk,readonly" \
    "$image" check --config /etc/stk

# Exercise the default command and read-only config mount without an SSH server.
cat > "$test_dir/config.yaml" <<'YAML'
hosts:
  dormant:
    auto: false
YAML
container=$(docker run -d --platform "$platform" --network none \
    --mount "type=bind,src=$test_dir,dst=/etc/stk,readonly" "$image")

ready=false
for _ in {1..30}; do
    if docker exec "$container" stk status --system > "$test_dir/status" 2>/dev/null; then
        ready=true
        break
    fi
    sleep 1
done
if [[ "$ready" != true ]]; then
    echo "Container control endpoint did not become ready" >&2
    exit 1
fi
cat "$test_dir/status"
docker exec "$container" stk reload --system

docker stop --time 10 "$container" >/dev/null
exit_code=$(docker inspect --format '{{.State.ExitCode}}' "$container")
if [[ "$exit_code" != 0 ]]; then
    echo "Expected graceful container shutdown, got exit code $exit_code" >&2
    exit 1
fi
