#!/usr/bin/env bash
# Linux test suite. Runs inside the container built by tests/linux/Dockerfile (one per distro), in a
# copy of the repo at /work (see run.sh). Real: just doctor, build, check, and the osh binary with the
# distro's own package manager. Stubbed (tests/linux/stubs on PATH, every call logged to $STUB_LOG):
# the OpenShell installer (curl), systemctl, docker, openshell, whose real versions (OpenShell
# packages, systemd, a container daemon) are not in the container.
set -uo pipefail

REPO="${REPO:-/work}"
STUBS="$REPO/tests/linux/stubs"
STUBS_DOCKER="$REPO/tests/linux/stubs-docker"
STUBS_OPENSHELL="$REPO/tests/linux/stubs-openshell"
J=(just --justfile "$REPO/.justfile")
fails=0

pass() { echo "ok   $1"; }
fail() { echo "FAIL $1${2:+: $2}"; fails=$((fails + 1)); }
check() { # check NAME CONDITION-COMMAND...
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi
}
logged() { grep -qx -- "$1" "$STUB_LOG"; }
line_of() { grep -n -x -m1 -- "$1" "$STUB_LOG" | cut -d: -f1; }

# A fresh HOME and log per scenario; stubs shadow the real tools.
fresh_home() {
  export HOME; HOME="$(mktemp -d)"
  export XDG_CONFIG_HOME="$HOME/.config" STUB_LOG="$HOME/stub.log"
  unset XDG_STATE_HOME
  : > "$STUB_LOG"
}
# The stubs; openshell itself appears in $HOME/.installed only once the stubbed installer has run.
with_stubs() { PATH="$STUBS:$HOME/.installed:$PATH" "$@"; }
# A machine that already has OpenShell.
with_openshell_stub() { PATH="$STUBS:$STUBS_OPENSHELL:$PATH" "$@"; }
# Adds a docker that answers `info` and lists one container and volume: a "ready" Docker runtime.
with_docker_stub() { PATH="$STUBS_DOCKER:$STUBS:$STUBS_OPENSHELL:$PATH" "$@"; }
skips=0
skip() { echo "skip $1${2:+: $2}"; skips=$((skips + 1)); }

[[ "$(uname)" == Linux ]] || { echo "suite.sh must run on Linux" >&2; exit 2; }

echo "== real: build prerequisites"
fresh_home
out="$("${J[@]}" doctor 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "doctor exits 0 with the prerequisites installed" || { fail "doctor exits 0" "$out"; }
grep -q 'ok       zig 0.15' <<<"$out" && pass "doctor finds Zig 0.15" || fail "doctor finds Zig 0.15"
grep -q 'wl-paste\|xclip' <<<"$out" && pass "doctor suggests a Linux clipboard tool" || fail "doctor suggests a Linux clipboard tool"
grep -q pngpaste <<<"$out" && fail "doctor must not mention pngpaste on Linux" || pass "doctor skips pngpaste on Linux"
check "bare just only lists recipes" bash -c "cd '$REPO' && just 2>&1 | grep -q 'doctor'"

echo "== real: dev-env without Zig 0.15"
if env PATH=/usr/bin:/bin bash -c "source '$REPO/dev-env'; osh_use_zig" 2>/dev/null; then
  fail "osh_use_zig fails when no Zig 0.15 exists"
else pass "osh_use_zig fails when no Zig 0.15 exists"; fi
mkdir -p /tmp/fakezig && printf '#!/bin/sh\necho 0.14.1\n' > /tmp/fakezig/zig && chmod +x /tmp/fakezig/zig
if env PATH=/tmp/fakezig:/usr/bin:/bin bash -c "source '$REPO/dev-env'; osh_use_zig" 2>/dev/null; then
  fail "osh_use_zig rejects Zig 0.14"
else pass "osh_use_zig rejects Zig 0.14"; fi

echo "== real: just check and build"
"${J[@]}" check >/tmp/check.log 2>&1 && pass "just check (fmt, clippy, tests)" || { fail "just check"; tail -20 /tmp/check.log; }
"${J[@]}" build >/tmp/build.log 2>&1 && [[ -x "$REPO/tui/target/release/osh-tui" ]] \
  && pass "just build produces osh-tui" || { fail "just build"; tail -20 /tmp/build.log; }

OSH="$REPO/tui/target/release/osh-tui"
if [[ -f /etc/os-release ]]; then . /etc/os-release; else . /usr/lib/os-release; fi
case "$ID" in
  ubuntu|debian) docker_pkg_line='apt-get install -y docker.io' ;;
  fedora) docker_pkg_line='dnf install -y moby-engine' ;;
  arch|archarm) docker_pkg_line='pacman -S --noconfirm --needed docker' ;;
esac

echo "== real: osh doctor and setup --dry-run (nothing installed)"
fresh_home
out="$(env PATH=/usr/bin:/bin "$OSH" doctor 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && pass "doctor exits 1 when OpenShell is missing" || fail "doctor exits 1 when OpenShell is missing" "rc=$rc"
grep -q 'MISSING  openshell' <<<"$out" && pass "doctor reports openshell missing" || fail "doctor reports openshell missing" "$out"
grep -q 'MISSING  container runtime' <<<"$out" && pass "doctor reports the runtime missing" || fail "doctor reports the runtime missing"
out="$(with_stubs "$OSH" setup --dry-run --gateways docker,podman </dev/null 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "setup --dry-run exits 0" || fail "setup --dry-run exits 0" "rc=$rc $out"
grep -q "$docker_pkg_line" <<<"$out" && pass "dry-run plans this distro's docker package" || fail "dry-run plans this distro's docker package" "$out"
grep -q 'OpenShell/main/install.sh' <<<"$out" && pass "dry-run plans the OpenShell installer" || fail "dry-run plans the OpenShell installer"
command -v docker >/dev/null 2>&1 && fail "dry-run must not install docker" || pass "dry-run installs nothing"
out="$(with_stubs "$OSH" setup --dry-run </dev/null 2>&1)"
grep -q -- '--gateways docker or --gateways podman' <<<"$out" && pass "without a pick, setup asks for --gateways" || fail "without a pick, setup asks for --gateways" "$out"
out="$(with_stubs "$OSH" setup --gateways vm --dry-run </dev/null 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && grep -q '/dev/kvm' <<<"$out" && pass "the VM type is refused where there is no KVM" || fail "the VM type is refused where there is no KVM" "rc=$rc $out"

echo "== real packages: osh setup --yes --gateways podman"
fresh_home
case "$ID" in
  ubuntu|debian) refresh=(apt-get update) ;;
  fedora) refresh=(dnf makecache -q) ;;
  arch|archarm) refresh=(pacman -Sy --noconfirm) ;;
esac
if ! "${refresh[@]}" >/dev/null 2>&1; then
  skip "real package installs" "the package index cannot be refreshed in this container (mirror or network), so installs would fail for reasons outside osh"
else
with_stubs "$OSH" setup --yes --gateways podman >"$HOME/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup exits 0" || { fail "setup exits 0" "rc=$rc"; tail -25 "$HOME/out.log"; }
for tool in podman tmux gh xclip; do
  check "setup installed $tool with the distro's package manager" command -v "$tool"
done
check "setup ran the OpenShell installer" logged installer-ran
check "setup wrote a Podman gateway config named after its type" grep -q 'name = "openshell-podman"' "$XDG_CONFIG_HOME/openshell/gateway.toml"
check "the Podman config drives podman" grep -q 'compute_driver = "podman"' "$XDG_CONFIG_HOME/openshell/gateway.toml"
check "setup remembered the gateway" grep -q openshell-podman "$HOME/.local/share/osh/gateways.json"
check "setup enabled the Podman socket" logged "systemctl --user enable --now podman.socket"
check "setup enabled the gateway service" logged "systemctl --user enable openshell-gateway"
check "setup restarted the gateway service" logged "systemctl --user restart openshell-gateway"
a="$(line_of installer-ran)"; b="$(line_of 'systemctl --user restart openshell-gateway')"
[[ -n "$a" && -n "$b" && "$a" -lt "$b" ]] && pass "installer runs before the service restart" || fail "installer runs before the service restart"
check "setup wrote the embedded providers to the data dir" test -f "$HOME/.local/share/osh/providers/npm.yaml"
check "setup does not touch Homebrew or launchd" bash -c "! grep -E '^(brew|launchctl) ' '$STUB_LOG'"
echo "my edit" > "$HOME/.local/share/osh/providers/npm.yaml"
with_stubs "$OSH" setup --yes --gateways podman >/dev/null 2>&1
[[ "$(cat "$HOME/.local/share/osh/providers/npm.yaml")" == "my edit" ]] && pass "a user-edited provider survives a second run" || fail "a user-edited provider survives a second run"
fi

echo "== stubbed: osh uninstall"
with_stubs "$OSH" setup --dry-run </dev/null >/dev/null 2>&1 # writes the data dir
printf 'uninstall\n' | with_stubs "$OSH" uninstall >"$HOME/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "uninstall exits 0" || { fail "uninstall exits 0" "rc=$rc"; tail -15 "$HOME/out.log"; }
check "uninstall disables the user service" logged "systemctl --user disable --now openshell-gateway"
check "uninstall removes the config dir" test ! -e "$XDG_CONFIG_HOME/openshell"
check "uninstall prints the package removal commands" grep -q 'apt remove' "$HOME/out.log"
check "uninstall removes the osh data dir" test ! -e "$HOME/.local/share/osh"
fresh_home; mkdir -p "$XDG_CONFIG_HOME/openshell"
printf 'no\n' | with_stubs "$OSH" uninstall >/dev/null 2>&1 && fail "uninstall aborts on a wrong answer" || pass "uninstall aborts on a wrong answer"
check "an aborted uninstall keeps the config dir" test -d "$XDG_CONFIG_HOME/openshell"

echo "== stubbed: osh gateway-reset"
fresh_home; mkdir -p "$HOME/.local/state/openshell/gateway" "$HOME/.local/state/openshell/docker-sandbox-tokens"
touch "$HOME/.local/state/openshell/gateway/openshell.db" "$HOME/.local/state/openshell/docker-sandbox-tokens/t"
printf 'no\n' | with_stubs "$OSH" gateway-reset openshell >/dev/null 2>&1 && fail "reset aborts on a wrong answer" || pass "reset aborts on a wrong answer"
check "an aborted reset keeps the database" test -e "$HOME/.local/state/openshell/gateway/openshell.db"
printf 'reset\n' | with_docker_stub "$OSH" gateway-reset openshell >"$HOME/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "gateway-reset openshell exits 0" || { fail "gateway-reset openshell exits 0" "rc=$rc"; tail -15 "$HOME/out.log"; }
check "reset deletes sandboxes through the gateway" logged "openshell -g openshell sandbox delete --all"
check "reset stops the user service" logged "systemctl --user stop openshell-gateway"
check "reset starts the user service" logged "systemctl --user start openshell-gateway"
a="$(line_of 'systemctl --user stop openshell-gateway')"; b="$(line_of 'systemctl --user start openshell-gateway')"
[[ -n "$a" && -n "$b" && "$a" -lt "$b" ]] && pass "stop happens before start" || fail "stop happens before start"
check "reset removes the labelled containers" grep -q '^docker rm -f abc123' "$STUB_LOG"
check "reset removes the labelled volumes" grep -q '^docker volume rm vol1' "$STUB_LOG"
check "reset deletes the database" test ! -e "$HOME/.local/state/openshell/gateway/openshell.db"
check "reset clears the sandbox tokens" test ! -e "$HOME/.local/state/openshell/docker-sandbox-tokens/t"
check "reset does not use Colima or launchd" bash -c "! grep -E 'colima|launchctl|brew' '$STUB_LOG' '$HOME/out.log'"
with_stubs "$OSH" gateway-reset openshell-docker --confirmed >/dev/null 2>&1; rc=$?
[[ $rc -eq 2 ]] && pass "gateway-reset openshell-docker is refused on Linux (exit 2)" || fail "gateway-reset openshell-docker is refused" "rc=$rc"
with_stubs "$OSH" gateway-reset >/dev/null 2>&1; rc=$?
[[ $rc -eq 2 ]] && pass "gateway-reset without a name prints usage (exit 2)" || fail "gateway-reset without a name" "rc=$rc"

echo "== other subcommands"
out="$(with_stubs "$OSH" gateway-docker </dev/null 2>&1)"; rc=$?
[[ $rc -ne 0 && "$out" == *"Install the Docker gateway config"* && "$out" == *"--yes"* ]] && pass "gateway-docker is gateway add docker: it plans the Docker gateway and wants --yes without a terminal" || fail "gateway-docker plans an extra Docker gateway" "rc=$rc $out"
fresh_home
out="$(with_stubs "$OSH" sync-agents --dry-run 2>&1)"; rc=$?
[[ $rc -eq 0 && "$out" == *"Dry run: nothing is installed"* ]] && pass "sync-agents --dry-run works with no agent setup" || fail "sync-agents --dry-run works" "rc=$rc $out"

echo "== real: scripts/release and install.sh against a local release"
fresh_home
dist="$(mktemp -d)"
"$REPO/scripts/release" v0.0.0-test --out "$dist" >"$HOME/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "scripts/release builds this machine's archive" || { fail "scripts/release" "rc=$rc"; tail -15 "$HOME/out.log"; }
arch="$(uname -m)"; [[ "$arch" == arm64 ]] && arch=aarch64
archive="$dist/osh-v0.0.0-test-linux-$arch.tar.gz"
check "release writes the archive and its checksum" bash -c "test -f '$archive' && test -f '$archive.sha256'"
install_env=(OSH_VERSION=v0.0.0-test "OSH_RELEASE_BASE=file://$dist" "OSH_BIN_DIR=$HOME/bin" OSH_NO_SETUP=1)
env "${install_env[@]}" sh "$REPO/install.sh" >"$HOME/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "install.sh installs from a local release" || { fail "install.sh" "rc=$rc"; tail -15 "$HOME/out.log"; }
check "the installed osh runs" bash -c "'$HOME/bin/osh' doctor | grep -q openshell"
rm -rf "$HOME/bin"; echo "0000  file" > "$archive.sha256"
env "${install_env[@]}" sh "$REPO/install.sh" >"$HOME/out.log" 2>&1 && fail "install.sh rejects a bad checksum" || pass "install.sh rejects a bad checksum"
check "a bad checksum installs nothing" test ! -e "$HOME/bin/osh"

echo "== launchd agents parse as real property lists (macOS cannot run here)"
plists="$(mktemp -d)"
(cd "$REPO/tui" && source ../dev-env && osh_use_zig && OSH_DUMP_PLISTS="$plists" cargo test dump_plists_when_asked >"$HOME/out.log" 2>&1) \
  || { fail "dumping the plists" ; tail -15 "$HOME/out.log"; }
for k in vm docker podman; do
  python3 - "$plists/openshell-$k.plist" <<'PY' && pass "the $k agent plist parses and runs gateway-service" || fail "the $k agent plist parses"
import plistlib, sys
d = plistlib.load(open(sys.argv[1], "rb"))
name = sys.argv[1].rsplit("/", 1)[1][:-6]
assert d["Label"] == "dev.guilospanck." + name, d["Label"]
assert d["ProgramArguments"][1:] == ["gateway-service", name], d["ProgramArguments"]
assert d["ProgramArguments"][0] == "/Users/me/My Apps/a&b/osh", d["ProgramArguments"]
assert d["RunAtLoad"] is True and "PATH" in d["EnvironmentVariables"]
PY
done

echo
[[ $skips -eq 0 ]] || echo "$skips scenario(s) skipped (see above)"
if [[ $fails -eq 0 ]]; then echo "all Linux tests passed"; else echo "$fails Linux test(s) failed"; exit 1; fi
