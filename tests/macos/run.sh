#!/usr/bin/env bash
# The macOS test: runs on a Mac and changes nothing on it. The macOS code paths (Homebrew, Colima,
# Podman machine, launchd) are chosen with cfg!(target_os) when gathering facts, so they only run
# from a macOS build; this runs that build with recording stand-ins for brew, launchctl, colima,
# docker, podman, openshell, curl, sudo and pkill (tests/macos/shims and installable), in a
# throwaway HOME, under sandbox-exec, which refuses any file write outside that folder (and the
# per-user temp dir mktemp uses) and refuses to start the real launchctl, sudo, pkill, curl or anything
# under /opt/homebrew, /usr/local or /Applications. It checks the exact command order osh runs,
# that each plist osh hands to `launchctl bootstrap` passes `plutil -lint` and names an
# executable, and launchd's rules (no bootstrap of a loaded label).
#
#   tests/macos/run.sh        (or: just test-macos)
#
# What it cannot show: that the real launchd starts the agent, that Homebrew's openshell service,
# Colima and Podman machines really start, and Docker Desktop (osh looks for /Applications/Docker.app,
# a fixed path the sandbox cannot fake).
set -uo pipefail

[[ "$(uname)" == Darwin ]] || { echo "tests/macos/run.sh runs on macOS only" >&2; exit 2; }
command -v sandbox-exec >/dev/null 2>&1 || { echo "sandbox-exec is missing; refusing to run without it" >&2; exit 2; }
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SHIMS="$repo/tests/macos/shims"
work="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/osh-macos-test.XXXXXX")" && pwd -P)"
trap '[[ -n "${OSH_MACOS_KEEP:-}" ]] || rm -rf "$work"' EXIT
user_tmp="$(cd "$(getconf DARWIN_USER_TEMP_DIR)" && pwd -P)"
# A stand-in that is not executable is skipped by PATH lookup and the real tool runs instead (the
# sandbox still blocks its writes, but the test would be meaningless).
for s in "$SHIMS"/* "$repo"/tests/macos/installable/*; do
  [[ -x "$s" ]] || { echo "$s is not executable (git update-index --chmod=+x)" >&2; exit 2; }
done
fails=0

pass() { echo "ok   $1"; }
fail() { echo "FAIL $1${2:+: $2}"; fails=$((fails + 1)); }
check() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi; }
logged() { grep -qx -- "$1" "$STUB_LOG"; }
line_of() { grep -n -x -m1 -- "$1" "$STUB_LOG" | cut -d: -f1; }
before() { # before NAME FIRST-LINE SECOND-LINE: both logged, FIRST earlier
  local a b; a="$(line_of "$2")"; b="$(line_of "$3")"
  [[ -n "$a" && -n "$b" && "$a" -lt "$b" ]] && pass "$1" || fail "$1" "'$2' at ${a:-never}, '$3' at ${b:-never}"
}

cat > "$work/profile.sb" <<EOF
(version 1)
(allow default)
(deny file-write*
  (require-all (require-not (subpath "$work")) (require-not (subpath "/dev"))
               (require-not (subpath "$user_tmp"))))
(deny process-exec (literal "/bin/launchctl") (literal "/usr/bin/sudo") (literal "/usr/bin/pkill")
  (literal "/usr/bin/curl")
  (subpath "/opt/homebrew") (subpath "/usr/local") (subpath "/Applications"))
EOF

echo "== build (macOS binary)"
just --justfile "$repo/.justfile" build >"$work/build.log" 2>&1 || { tail -20 "$work/build.log"; exit 1; }
OSH="$repo/tui/target/release/osh-tui"

# A fresh HOME per scenario. `osh_mac` runs the macOS build sandboxed with only the stand-ins and
# the system's own /usr/bin:/bin on PATH, so a tool is there only once the brew stand-in installed it.
fresh_home() {
  HOME_T="$(mktemp -d "$work/home.XXXXXX")"
  export STUB_LOG="$HOME_T/stub.log"
  agents="$HOME_T/Library/LaunchAgents"
  : > "$STUB_LOG"
}
osh_mac() {
  sandbox-exec -f "$work/profile.sb" env -i HOME="$HOME_T" XDG_CONFIG_HOME="$HOME_T/.config" \
    TMPDIR="$work" STUB_LOG="$STUB_LOG" USER="$USER" LANG=en_US.UTF-8 \
    PATH="$SHIMS:$HOME_T/.installed:/usr/bin:/bin:/usr/sbin:/sbin" "$OSH" "$@"
}
uid="$(id -u)"

echo "== launchd agents pass plutil -lint"
plists="$work/plists"; mkdir -p "$plists"
(cd "$repo/tui" && source ../dev-env && osh_use_zig && OSH_DUMP_PLISTS="$plists" cargo test dump_plists_when_asked) \
  >"$work/plists.log" 2>&1 || { fail "dumping the plists"; tail -15 "$work/plists.log"; }
for k in vm docker podman; do
  p="$plists/openshell-$k.plist"
  check "the $k agent plist passes plutil -lint" plutil -lint -s "$p"
  [[ "$(plutil -extract ProgramArguments.0 raw -o - "$p" 2>/dev/null)" == "/Users/me/My Apps/a&b/osh" ]] \
    && pass "the $k plist keeps a path with spaces and & intact" || fail "the $k plist path"
done

echo "== the real Mac's facts, dry run (sandboxed: nothing can be written outside the test folder)"
fresh_home
out="$(sandbox-exec -f "$work/profile.sb" env HOME="$HOME_T" XDG_CONFIG_HOME="$HOME_T/.config" TMPDIR="$work" \
  "$OSH" setup --gateways vm,docker --dry-run </dev/null 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "setup --dry-run exits 0 on this Mac" || fail "setup --dry-run on this Mac" "rc=$rc $out"
grep -q 'systemctl' <<<"$out" && fail "the macOS plan mentions systemctl" "$out" || pass "the macOS plan has no systemctl"
grep -q 'launchctl bootstrap' <<<"$out" && pass "the Docker extra is a launchd agent" || fail "the Docker extra is a launchd agent" "$out"

echo "== emulated bare Mac with Homebrew: setup --gateways vm,docker"
fresh_home
osh_mac setup --gateways vm,docker --yes </dev/null >"$HOME_T/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup exits 0" || { fail "setup exits 0" "rc=$rc"; grep -E '^(failed|skipped|declined)' "$HOME_T/out.log" | head -20; }
check "the OpenShell installer ran" logged installer-ran
check "no sudo on macOS" bash -c "! grep -q '^sudo ' '$STUB_LOG'"
check "no systemctl on macOS" bash -c "! grep -q 'systemctl' '$STUB_LOG' '$HOME_T/out.log'"
before "OpenShell is installed before its Homebrew service restarts" installer-ran "brew services restart openshell"
before "Colima and Docker are installed before Colima starts" "brew install colima docker" "colima start"
docker_plist="$agents/dev.guilospanck.openshell-docker.plist"
before "a stale agent is booted out before the bootstrap" \
  "launchctl bootout gui/$uid/dev.guilospanck.openshell-docker" "launchctl bootstrap gui/$uid $docker_plist"
before "Colima runs before the Docker gateway starts" "colima start" "launchctl bootstrap gui/$uid $docker_plist"
check "the VM main is the Homebrew service: no agent for it" test ! -e "$agents/dev.guilospanck.openshell-vm.plist"
check "the VM main's config is gateway.toml" grep -q 'compute_driver = "vm"' "$HOME_T/.config/openshell/gateway.toml"
check "the Docker extra's agent was loaded" test -e "$HOME_T/.launchd/dev.guilospanck.openshell-docker"
check "its plist runs this osh binary" bash -c "[ \"\$(plutil -extract ProgramArguments.0 raw -o - '$docker_plist')\" = '$OSH' ]"
check "its log directory exists" test -d "$HOME_T/.local/state/openshell/gateway-docker"
check "Colima's socket is in gateway.env" grep -q colima "$HOME_T/.config/openshell/gateway.env"
check "the Docker config reaches the host through host.docker.internal" grep -q 'host.docker.internal:17671' "$HOME_T/.local/share/osh/gateways/openshell-docker.toml"
check "the CLI knows openshell-vm on 17670" grep -qx 'openshell-vm https://127.0.0.1:17670' "$HOME_T/.openshell-gateways"
check "the CLI knows openshell-docker on 17671" grep -qx 'openshell-docker https://localhost:17671' "$HOME_T/.openshell-gateways"
check "the installer's own registration is replaced" bash -c "! grep -q '^openshell ' '$HOME_T/.openshell-gateways'"
check "the registry has both" bash -c "grep -q openshell-vm '$HOME_T/.local/share/osh/gateways.json' && grep -q openshell-docker '$HOME_T/.local/share/osh/gateways.json'"

echo "== setup again does nothing"
: > "$STUB_LOG"
out="$(osh_mac setup --gateways vm,docker --yes </dev/null 2>&1)"; rc=$?
[[ $rc -eq 0 && "$out" == *"Nothing to do"* ]] && pass "a second setup is a no-op" || fail "a second setup is a no-op" "rc=$rc $out"
check "a no-op run touches neither brew nor launchd state" bash -c "! grep -E '^(brew (install|services)|launchctl (bootstrap|bootout))' '$STUB_LOG'"

echo "== a Podman extra on a Podman machine"
: > "$STUB_LOG"
osh_mac gateway add podman --yes </dev/null >"$HOME_T/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "gateway add podman exits 0" || { fail "gateway add podman exits 0" "rc=$rc"; tail -20 "$HOME_T/out.log"; }
podman_plist="$agents/dev.guilospanck.openshell-podman.plist"
before "Podman is installed before its machine is created" "brew install podman" "podman machine init"
before "the machine is created before it starts" "podman machine init" "podman machine start"
before "the machine runs before the Podman gateway starts" "podman machine start" "launchctl bootstrap gui/$uid $podman_plist"
check "the Podman extra's agent was loaded" test -e "$HOME_T/.launchd/dev.guilospanck.openshell-podman"
check "the Podman extra got the next port" grep -qx 'openshell-podman https://localhost:17672' "$HOME_T/.openshell-gateways"

echo "== gateway-reset of an agent-run extra and of the Homebrew main"
: > "$STUB_LOG"
printf 'reset\n' | osh_mac gateway-reset openshell-podman >"$HOME_T/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "resetting the Podman extra exits 0" || { fail "resetting the Podman extra" "rc=$rc"; tail -20 "$HOME_T/out.log"; }
before "the extra is booted out, then bootstrapped again" \
  "launchctl bootout gui/$uid/dev.guilospanck.openshell-podman" "launchctl bootstrap gui/$uid $podman_plist"
# The sweep sources gateway.env, whose PATH lists only Homebrew's folders; the Podman stand-in sits
# outside them (in the test HOME), as a Podman from its own installer would. osh passes the path it
# found, so the sweep still reaches it.
ran() { grep -n -m1 "^\[[0-9]*/[0-9]*\] $1" "$HOME_T/out.log" | cut -d: -f1; }
a="$(ran 'Stop the gateway')"; b="$(ran 'Remove leftover sandbox runtimes')"; c="$(ran 'Start the gateway')"
[[ -n "$a" && -n "$b" && -n "$c" && "$a" -lt "$b" && "$b" -lt "$c" ]] && pass "the sweep runs between stop and start" || fail "the sweep runs between stop and start"
check "the sweep reached the Podman outside Homebrew" bash -c "! grep -q 'podman unreachable' '$HOME_T/out.log'"
check "the sweep removed the leftover containers" logged "podman rm -f def456"
# The Docker extra drives a runtime too, so the sweep is the shared one: stopped leftovers only.
check "the shared sweep only lists stopped containers" grep -q -- '^podman ps -aq .*--filter status=exited' "$STUB_LOG"
check "the shared sweep keeps the volumes" bash -c "! grep -q '^podman volume rm' '$STUB_LOG'"
check "the reset never touches the Homebrew service" bash -c "! grep -q '^brew services' '$STUB_LOG'"
: > "$STUB_LOG"
printf 'reset\n' | osh_mac gateway-reset openshell-vm >"$HOME_T/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "resetting the VM main exits 0" || { fail "resetting the VM main" "rc=$rc"; tail -20 "$HOME_T/out.log"; }
before "the VM main stops before its microVMs are swept" "brew services stop openshell" "pkill -f $HOME_T/.local/state/openshell/vm-driver/sandboxes"
before "and starts again after" "pkill -f $HOME_T/.local/state/openshell/vm-driver/sandboxes" "brew services start openshell"
check "the VM reset leaves the agents alone" bash -c "! grep -q '^launchctl boot' '$STUB_LOG'"

echo "== removing gateways"
: > "$STUB_LOG"
out="$(printf 'remove\n' | osh_mac gateway remove openshell-vm 2>&1)"; rc=$?
[[ $rc -ne 0 && "$out" == *"remove openshell-"* ]] && pass "the main cannot go while extras exist" || fail "main removal refused" "rc=$rc $out"
printf 'remove\n' | osh_mac gateway remove openshell-docker >"$HOME_T/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "removing the Docker extra exits 0" || { fail "removing the Docker extra" "rc=$rc"; tail -20 "$HOME_T/out.log"; }
check "its agent was booted out" logged "launchctl bootout gui/$uid/dev.guilospanck.openshell-docker"
check "its plist is gone" test ! -e "$docker_plist"
check "the CLI forgot it" bash -c "! grep -q '^openshell-docker ' '$HOME_T/.openshell-gateways'"
check "the registry forgot it" bash -c "! grep -q openshell-docker '$HOME_T/.local/share/osh/gateways.json'"
check "the other agent is still loaded" test -e "$HOME_T/.launchd/dev.guilospanck.openshell-podman"

echo "== uninstall"
: > "$STUB_LOG"
printf 'uninstall\n' | osh_mac uninstall >"$HOME_T/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "uninstall exits 0" || { fail "uninstall exits 0" "rc=$rc"; tail -20 "$HOME_T/out.log"; }
check "the remaining agent was booted out" logged "launchctl bootout gui/$uid/dev.guilospanck.openshell-podman"
check "no agents are left loaded" bash -c "[ -z \"\$(ls '$HOME_T/.launchd')\" ]"
check "no plists are left" bash -c "! ls '$agents'/dev.guilospanck.*.plist"
before "the Homebrew service stops before the formula goes" "brew services stop openshell" "brew uninstall openshell"
check "the config dir is gone" test ! -e "$HOME_T/.config/openshell"
check "the state dir is gone" test ! -e "$HOME_T/.local/state/openshell"
check "the registry is gone" test ! -e "$HOME_T/.local/share/osh/gateways.json"

echo "== emulated Mac: a Docker main runs as osh's own agent, not the Homebrew service"
fresh_home
osh_mac setup --gateways docker --yes </dev/null >"$HOME_T/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup --gateways docker exits 0" || { fail "setup --gateways docker" "rc=$rc"; grep -E '^(failed|skipped)' "$HOME_T/out.log" | head; }
main_plist="$agents/dev.guilospanck.openshell-docker.plist"
before "the Homebrew service is stopped before the agent starts" "brew services stop openshell" "launchctl bootstrap gui/$uid $main_plist"
check "the Docker main's agent was loaded" test -e "$HOME_T/.launchd/dev.guilospanck.openshell-docker"
check "the Docker main is on 17670" grep -qx 'openshell-docker https://127.0.0.1:17670' "$HOME_T/.openshell-gateways"
check "it never restarts the Homebrew service" bash -c "! grep -q '^brew services restart' '$STUB_LOG'"

echo "== the sandbox held: nothing outside the test folder changed"
check "no stand-in was bypassed by a real brew or launchctl" bash -c "! grep -E 'Operation not permitted' '$work'/home.*/out.log"

echo
if [[ $fails -eq 0 ]]; then echo "all macOS tests passed"; else echo "$fails macOS test(s) failed"; exit 1; fi
