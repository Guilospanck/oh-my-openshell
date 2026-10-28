#!/usr/bin/env bash
# Runs the release workflow (.github/workflows/release.yml) for real with nektos/act, in Docker:
# a tag push event for v0.0.0-act, the build job for the ubuntu-24.04 matrix entry (checkout,
# mlugg/setup-zig, scripts/release, upload-artifact to act's local artifact server), then the
# publish job (download-artifact with merge-multiple, then `gh release create` against a recording
# gh, so nothing is published). Needs Docker and network access; slow (it compiles osh).
#
#   tests/release/run.sh        (or: just test-release)
#
# Not covered, and why:
#   - the macos-14 and macos-13 entries: act runs jobs in Linux containers only;
#   - ubuntu-24.04-arm: act would run it in the same container as ubuntu-24.04 (this host's arch),
#     so it adds nothing; on an arm64 host the archive is linux-aarch64, on x86_64 linux-x86_64;
#   - GitHub itself: the hosted runner images, the real artifact service and `gh release create`.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
docker info >/dev/null 2>&1 || { echo "the Docker daemon is not reachable" >&2; exit 1; }
image=osh-act-runner
tag=v0.0.0-act

echo "== building the act image ($image)"
docker build -q -t "$image" "$repo/tests/release" >/dev/null

# Under the repo: act runs beside the Docker daemon (through its socket), and a container VM
# (Colima, Docker Desktop) shares the home folder, not /var/folders. Git-ignored (.act-run.*).
work="$(mktemp -d "$repo/.act-run.XXXXXX")"
trap 'rm -rf "$work"' EXIT
printf '{"ref": "refs/tags/%s", "ref_type": "tag"}\n' "$tag" > "$work/event.json"

# act fetches the workflow's actions with an empty token, which GitHub refuses; clone them
# anonymously at the refs the workflow names and hand them to act as local repositories.
actions=(mlugg/setup-zig@v2 actions/upload-artifact@v4 actions/download-artifact@v4)
grep -q 'actions/checkout@v4' "$repo/.github/workflows/release.yml" || { echo "release.yml changed its actions; update tests/release/run.sh" >&2; exit 1; }
for a in "${actions[@]}"; do
  grep -q "uses: $a\$" "$repo/.github/workflows/release.yml" || { echo "release.yml no longer uses $a; update tests/release/run.sh" >&2; exit 1; }
done

echo "== act push (release.yml, ubuntu-24.04 entry, then publish)"
set +e
docker run --rm -v /var/run/docker.sock:/var/run/docker.sock -v "$repo:$repo" -w "$repo" \
  -e WORK="$work" -e IMAGE="$image" "$image" bash -c '
    set -e
    local_repos=()
    for a in '"${actions[*]}"'; do
      dir="$WORK/actions/${a%@*}"
      git -c advice.detachedHead=false clone -q --depth 1 --branch "${a#*@}" "https://github.com/${a%@*}" "$dir"
      local_repos+=(--local-repository "$a=$dir")
    done
    exec act push -W .github/workflows/release.yml -e "$WORK/event.json" \
      --matrix os:ubuntu-24.04 -P "ubuntu-24.04=$IMAGE" --pull=false \
      --artifact-server-path "$WORK/artifacts" "${local_repos[@]}"' 2>&1 | tee "$work/act.log"
rc=${PIPESTATUS[0]}
set -e

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1"; fails=$((fails + 1)); }
arch="$(docker info --format '{{.Architecture}}')"; case "$arch" in arm64|aarch64) arch=aarch64 ;; *) arch=x86_64 ;; esac
archive="osh-$tag-linux-$arch.tar.gz"
[[ $rc -eq 0 ]] && pass "act runs the workflow to the end" || fail "act exited $rc"
grep -q "built dist/$archive\|built .*/dist/$archive" "$work/act.log" && pass "scripts/release built $archive in the job" || fail "scripts/release built $archive"
grep -q "gh-stub: gh release create $tag" "$work/act.log" && pass "publish runs gh release create $tag" || fail "publish runs gh release create"
grep -q "gh-stub: asset dist/$archive " "$work/act.log" && pass "the downloaded artifact reaches the publish job" || fail "the artifact reaches the publish job"
grep -q "gh-stub: asset dist/$archive.sha256 " "$work/act.log" && pass "its checksum too" || fail "the checksum reaches the publish job"
echo
if [[ $fails -eq 0 ]]; then echo "the release workflow ran under act (Linux entry only; macOS entries need GitHub)"; else echo "$fails release workflow check(s) failed"; exit 1; fi
