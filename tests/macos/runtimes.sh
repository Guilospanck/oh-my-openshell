#!/usr/bin/env bash
# macOS container runtimes for real: runs the macOS osh build's own runtime steps against the real
# Colima (`colima start`, a real Linux VM with Docker) and the real Podman machine (`podman machine
# init` and `start`, a real VM), then checks that Docker and Podman answer.
#
#   tests/macos/runtimes.sh [--keep]        (or: just test-macos-runtimes)
#
# Isolation: everything lives in one throwaway folder used as HOME (short, under /private/tmp,
# because Lima and Podman put unix sockets there), with TMPDIR and CONTAINERS_CONF in it too and
# no other variable of the user's passed on, so the Colima "default" profile and the
# "podman-machine-default" machine osh creates are new instances that share nothing with the
# user's. osh runs under sandbox-exec: no file write outside that folder (and /dev and the per-user
# temp dir), no real brew, launchctl, sudo or pkill (recording stand-ins from tests/macos/shims
# take their place, so no OpenShell is installed and no launchd agent is loaded; the macOS VM test
# does those for real). Podman comes from the official installer package, unpacked into the
# folder, not installed. The user's Colima profiles and containers are recorded before and after
# and must be byte-identical. The instances are deleted at the end, pass or fail, unless --keep.
#
# Needs: macOS on Apple silicon, colima, limactl and docker on PATH (Homebrew), network access.
set -uo pipefail

[[ "$(uname)" == Darwin ]] || { echo "tests/macos/runtimes.sh runs on macOS only" >&2; exit 2; }
command -v sandbox-exec >/dev/null 2>&1 || { echo "sandbox-exec is missing" >&2; exit 2; }
for t in colima limactl docker; do
  command -v "$t" >/dev/null 2>&1 || { echo "$t is required (brew install colima docker)" >&2; exit 2; }
done
keep=0
[[ "${1:-}" == --keep ]] && keep=1
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
podman_version="${OSH_PODMAN_VERSION:-v6.1.4}"
work="$(cd "$(mktemp -d /private/tmp/osh-rt.XXXXXX)" && pwd -P)"
HOME_T="$work/h"
mkdir -p "$HOME_T/.installed" "$HOME_T/tmp" "$work/shims" "$work/bin"
user_tmp="$(cd "$(getconf DARWIN_USER_TEMP_DIR)" && pwd -P)"
fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1${2:+: $2}"; fails=$((fails + 1)); }
check() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi; }

# The environment every tool of the test runs in: nothing of the user's is reachable through it.
iso() {
  # No XDG_* or COLIMA_HOME: as on a stock Mac, every tool derives its folders from HOME. (With
  # XDG_CONFIG_HOME set and no ~/.colima, Colima puts its socket under $XDG_CONFIG_HOME/colima.)
  env -i HOME="$HOME_T" USER="$USER" LOGNAME="$USER" LANG=en_US.UTF-8 TMPDIR="$HOME_T/tmp" \
    CONTAINERS_CONF="$work/containers.conf" STUB_LOG="$work/stub.log" \
    PATH="$work/shims:$HOME_T/.installed:$work/bin:/usr/bin:/bin:/usr/sbin:/sbin" "$@"
}
user_state() {
  # As the user's own shell sees it: their HOME, their DOCKER_HOST.
  colima list 2>&1
  docker ps --format '{{.ID}} {{.Names}} {{.Image}} {{.CreatedAt}}' 2>&1
  limactl list 2>&1 | grep -v '^time='
}
teardown() {
  if [[ $keep -eq 1 ]]; then echo "keeping $work (delete the instances with HOME=$HOME_T and the same env)"; return; fi
  echo "== deleting the test's Colima instance and Podman machine"
  iso colima delete --data --force >/dev/null 2>&1 || true
  iso podman machine rm -f >/dev/null 2>&1 || true
  # A hung VM process must not outlive the folder it runs from.
  pkill -f "$HOME_T/" 2>/dev/null || true
  rm -rf "$work"
}
trap teardown EXIT
trap 'exit 130' INT TERM

user_state > "$work/user-before.txt"

echo "== Podman $podman_version from the official installer package (unpacked, not installed)"
pkg="$work/podman.pkg"
curl -fsSL -o "$pkg" "https://github.com/containers/podman/releases/download/$podman_version/podman-installer-macos-arm64.pkg" \
  || { echo "cannot download Podman" >&2; exit 1; }
pkgutil --expand-full "$pkg" "$work/pkg" >/dev/null || { echo "cannot unpack the Podman package" >&2; exit 1; }
pbin="$(dirname "$(find "$work/pkg" -type f -path '*/podman/bin/podman' | head -1)")"
[[ -x "$pbin/podman" && -x "$pbin/vfkit" && -x "$pbin/gvproxy" ]] || { echo "the package has no podman, vfkit and gvproxy" >&2; exit 1; }
# Homebrew's podman (what `brew install podman` gives osh's users) keeps vfkit, Apple's
# Virtualization framework, as the machine provider (it patches out Podman 6's krunkit default), so
# the test uses it too. (The package's krunkit only finds libkrun when installed in /opt/podman.)
printf '[engine]\nhelper_binaries_dir = ["%s"]\n[machine]\nprovider = "applehv"\n' "$pbin" > "$work/containers.conf"
# Only the runtimes are real; the rest of Homebrew stays out of reach.
for t in colima lima limactl docker; do ln -s "$(command -v "$t")" "$work/bin/$t"; done
ln -s "$pbin/podman" "$work/bin/podman"
# brew, launchctl, sudo and pkill are the recording stand-ins. curl is not: Colima downloads its
# image with it. OpenShell is the stand-in too, already "installed", so no installer runs.
for t in brew launchctl sudo pkill; do cp "$repo/tests/macos/shims/$t" "$work/shims/$t"; done
cp -R "$repo/tests/macos/installable" "$work/installable"
# What the OpenShell installer leaves behind: the CLI, its client certificates and its own
# `openshell` registration (see tests/macos/shims/curl).
cp "$work/installable/openshell" "$HOME_T/.installed/openshell"
mkdir -p "$HOME_T/.config/openshell/gateways/openshell/mtls"
echo 'openshell https://127.0.0.1:17670' > "$HOME_T/.openshell-gateways"
: > "$work/stub.log"

cat > "$work/profile.sb" <<EOF
(version 1)
(allow default)
(deny file-write*
  (require-all (require-not (subpath "$work")) (require-not (subpath "/dev"))
               (require-not (subpath "$user_tmp"))))
(deny process-exec (literal "/bin/launchctl") (literal "/usr/bin/sudo") (literal "/usr/bin/pkill")
  (literal "/opt/homebrew/bin/brew") (subpath "/opt/homebrew/Library") (subpath "/Applications"))
EOF

echo "== build (macOS binary)"
just --justfile "$repo/.justfile" build >"$work/build.log" 2>&1 || { tail -20 "$work/build.log"; exit 1; }
OSH="$repo/tui/target/release/osh-tui"
# Every osh run has a deadline (VM downloads and boots can hang): OSH_T seconds, default 300.
TIMEOUT="$(command -v timeout || command -v gtimeout)" || { echo "timeout (coreutils) is required" >&2; exit 2; }
osh_rt() { iso "$TIMEOUT" "${OSH_T:-300}" sandbox-exec -f "$work/profile.sb" "$OSH" "$@"; }

echo "== before: neither runtime answers in the test's environment"
iso docker info >/dev/null 2>&1 && fail "docker does not answer yet" || pass "docker does not answer yet"
plan="$(osh_rt setup --gateways docker,podman --dry-run </dev/null 2>&1)"
grep -q 'colima start' <<<"$plan" && pass "the plan starts Colima" || fail "the plan starts Colima" "$plan"
grep -q 'podman machine init' <<<"$plan" && pass "the plan creates a Podman machine" || fail "the plan creates a Podman machine" "$plan"

echo "== osh setup --gateways docker,podman: real colima start, podman machine init and start"
OSH_T=1800 osh_rt setup --gateways docker,podman --yes </dev/null >"$work/setup.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup exits 0" || { fail "setup exits 0" "rc=$rc"; grep -E '^(\[[0-9]+/[0-9]+\]|failed|skipped|declined)|rror' "$work/setup.log" | head -40; }
grep -q '^colima start' "$work/stub.log" && fail "colima ran as a stand-in" || pass "colima was the real one"
check "the test's Colima runs" iso colima status
check "Docker answers on the test's Colima" iso docker --host "unix://$HOME_T/.colima/default/docker.sock" info
check "a container runs on it" iso docker --host "unix://$HOME_T/.colima/default/docker.sock" run --rm alpine:3.20 true
out="$(iso podman machine list --format '{{.Name}} {{.Running}}' 2>&1)"
grep -qE 'podman-machine-default\*? true' <<<"$out" && pass "the test's Podman machine runs" || fail "the test's Podman machine runs" "$out"
check "Podman answers" iso podman info
check "a container runs on the Podman machine" iso podman run --rm docker.io/library/alpine:3.20 true
check "gateway.env names Colima's socket" grep -q '\.colima/docker\.sock' "$HOME_T/.config/openshell/gateway.env"
check "Docker answers on that socket, as the gateway will use it" iso docker --host "unix://$HOME_T/.colima/docker.sock" info
check "osh loaded both gateway agents (stand-in launchd)" bash -c "test -e '$HOME_T/.launchd/dev.guilospanck.openshell-docker' && test -e '$HOME_T/.launchd/dev.guilospanck.openshell-podman'"

echo "== setup again: both runtimes are ready, nothing to do"
out="$(osh_rt setup --gateways docker,podman --yes </dev/null 2>&1)"; rc=$?
[[ $rc -eq 0 && "$out" == *"Nothing to do"* ]] && pass "a second setup is a no-op" || fail "a second setup is a no-op" "rc=$rc $out"

echo "== a stopped Podman machine is started again"
iso podman machine stop >/dev/null 2>&1
for _ in $(seq 60); do [[ "$(iso podman machine list --format '{{.Running}}' 2>/dev/null)" == false ]] && break; sleep 1; done
plan="$(osh_rt setup --gateways docker,podman --dry-run </dev/null 2>&1)"
grep -q 'podman machine start' <<<"$plan" && ! grep -q 'podman machine init' <<<"$plan" \
  && pass "the plan starts the existing machine without creating one" || fail "restart plan" "$plan"
OSH_T=900 osh_rt setup --gateways docker,podman --yes </dev/null >"$work/setup2.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup starts it again" || { fail "setup starts it again" "rc=$rc"; tail -20 "$work/setup2.log"; }
check "Podman answers again" iso podman info

echo "== the user's own Colima and containers are untouched"
user_state > "$work/user-after.txt"
if cmp -s "$work/user-before.txt" "$work/user-after.txt"; then pass "colima list, docker ps and limactl list are byte-identical"
else fail "the user's state changed" "$(diff "$work/user-before.txt" "$work/user-after.txt")"; fi
cp "$work/user-before.txt" "$work/user-after.txt" "${OSH_RT_EVIDENCE:-$work}/" 2>/dev/null || true

echo
if [[ $fails -eq 0 ]]; then echo "all macOS runtime tests passed"; else echo "$fails macOS runtime test(s) failed"; exit 1; fi
