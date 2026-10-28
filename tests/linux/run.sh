#!/usr/bin/env bash
# Run the Linux test suite in Docker containers, one per distro. Needs a running Docker daemon.
#   tests/linux/run.sh                  all distros (ubuntu, fedora, arch)
#   tests/linux/run.sh --distro fedora  one distro
#   tests/linux/run.sh --rebuild        rebuild the images first
# Copies tracked and untracked files (not ignored ones) into the container, so uncommitted
# changes are tested and the host's tui/target is not used. Cargo caches live in named volumes.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# The official archlinux image is amd64 only; lopsided/archlinux is the multi-arch community build.
case "$(uname -m)" in arm64|aarch64) arch_base=lopsided/archlinux:latest ;; *) arch_base=archlinux:latest ;; esac
declare -A bases=([ubuntu]=ubuntu:24.04 [fedora]=fedora:41 [arch]="$arch_base")
distros=(ubuntu fedora arch)
rebuild=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rebuild) rebuild=1 ;;
    --distro) shift; [[ -n "${bases[${1:-}]:-}" ]] || { echo "unknown distro ${1:-}; use ubuntu, fedora or arch" >&2; exit 2; }; distros=("$1") ;;
    *) echo "usage: run.sh [--rebuild] [--distro ubuntu|fedora|arch]" >&2; exit 2 ;;
  esac
  shift
done

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "the Docker daemon is not reachable" >&2; exit 1; }

failed=()
for d in "${distros[@]}"; do
  image="osh-linux-test-$d"
  echo "######## $d (${bases[$d]})"
  if [[ $rebuild -eq 1 ]] || ! docker image inspect "$image" >/dev/null 2>&1; then
    docker build --build-arg "BASE=${bases[$d]}" -t "$image" "$repo/tests/linux"
  fi
  # Stubs and the suite are executable in git; tar keeps modes.
  if ! (cd "$repo" && COPYFILE_DISABLE=1 git ls-files -z --cached --others --exclude-standard | COPYFILE_DISABLE=1 tar --null -T - -cf -) |
    docker run --rm -i \
      -v "osh-linux-cargo-$d:/root/.cargo/registry" \
      -v "osh-linux-target-$d:/work/tui/target" \
      "$image" bash -c 'tar --warning=no-unknown-keyword -x -C /work && exec /work/tests/linux/suite.sh'; then
    failed+=("$d")
  fi
done

# The release workflow is linted here; `just test-release` runs its Linux jobs under act.
echo "######## .github/workflows (actionlint)"
docker run --rm -v "$repo:/repo:ro" -w /repo rhysd/actionlint:1.7.7 -color || failed+=("actionlint")

if [[ ${#failed[@]} -gt 0 ]]; then echo "FAILED distros: ${failed[*]}" >&2; exit 1; fi
echo "all distros passed: ${distros[*]}"
