#!/usr/bin/env bash
# Run the Linux test suite in a Docker container. Needs a running Docker daemon.
#   tests/linux/run.sh            build the image if needed, copy the working tree, run the suite
#   tests/linux/run.sh --rebuild  rebuild the image first
# Copies tracked and untracked files (not ignored ones) into the container, so uncommitted
# changes are tested and the host's tui/target is not used. Cargo caches live in named volumes.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image=osh-linux-test

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "the Docker daemon is not reachable" >&2; exit 1; }

if [[ "${1:-}" == --rebuild ]] || ! docker image inspect "$image" >/dev/null 2>&1; then
  docker build -t "$image" "$repo/tests/linux"
fi

# Stubs and the suite are executable in git; tar keeps modes.
(cd "$repo" && COPYFILE_DISABLE=1 git ls-files -z --cached --others --exclude-standard | COPYFILE_DISABLE=1 tar --null -T - -cf -) |
  docker run --rm -i \
    -v osh-linux-cargo:/root/.cargo/registry \
    -v osh-linux-target:/work/tui/target \
    "$image" bash -c 'tar --warning=no-unknown-keyword -x -C /work && exec /work/tests/linux/suite.sh'
