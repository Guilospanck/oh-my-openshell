#!/bin/bash
# Runs inside the disposable macOS VM tests/macos/vm.sh boots, as its user `admin`, against the
# real Homebrew, `brew services`, OpenShell's installer and launchd. Nothing is stubbed. The Docker
# engine is the host's (vm.sh forwards a disposable Colima's socket to ~/.colima/default/docker.sock).
# /bin/bash here is 3.2: no bash 4 features.
set -uo pipefail

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1${2:+: $2}"; fails=$((fails + 1)); }
check() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi; }
# with_timeout SECONDS CMD...: macOS has no timeout(1). The explicit <&0 keeps a piped answer
# (a background job's stdin is otherwise /dev/null).
with_timeout() {
  local s="$1"; shift
  "$@" <&0 & local p=$!
  local i=0
  while kill -0 "$p" 2>/dev/null; do
    i=$((i + 1)); [ "$i" -gt "$s" ] && { kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; return 124; }
    sleep 1
  done
  wait "$p"
}
connected() { openshell -g "$1" status 2>&1 | grep -q Connected; }
# A gateway launchd just (re)started can take a few seconds to listen.
wait_connected() { local i; for i in $(seq 30); do connected "$1" && return 0; sleep 1; done; openshell -g "$1" status 2>&1 | tail -3 >&2; return 1; }
uid="$(id -u)"
agent() { echo "gui/$uid/dev.guilospanck.openshell-$1"; }
agent_pid() { launchctl print "$(agent "$1")" 2>/dev/null | awk '$1 == "pid" {print $3; exit}'; }
data="$HOME/.local/share/osh"
log="$HOME/out.log"
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/bin:/bin:/usr/sbin:/sbin"
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
shared="/Volumes/My Shared Files"

echo "== the guest: $(sw_vers -productName) $(sw_vers -productVersion), hypervisor support: $(sysctl -n kern.hv_support)"
[ "$(sysctl -n kern.hv_support)" = 0 ] && pass "no hypervisor in a macOS guest (kern.hv_support is 0)" || fail "expected no hypervisor in the guest"

echo "== install osh from the local release"
rm -rf /tmp/dist && cp -R "$shared/dist" /tmp/dist
OSH_VERSION=v0.0.0-macvm OSH_RELEASE_BASE=file:///tmp/dist OSH_NO_SETUP=1 sh "$shared/repo/install.sh" >"$log" 2>&1 \
  && pass "install.sh installs the release" || { fail "install.sh"; tail -10 "$log"; }
check "osh runs on this macOS" osh --version

echo "== the Docker CLI on the host's engine, as Colima would set it up"
brew install docker >"$log" 2>&1 || { fail "brew install docker"; tail -10 "$log"; }
ln -sf "$HOME/.colima/default/docker.sock" "$HOME/.colima/docker.sock"
docker context create colima --docker "host=unix://$HOME/.colima/default/docker.sock" >/dev/null 2>&1 || true
docker context use colima >/dev/null 2>&1
check "docker answers through the forwarded socket" docker info

out="$(osh doctor 2>&1)"; rc=$?
[ $rc -eq 1 ] && grep -q 'MISSING  openshell' <<<"$out" && pass "doctor reports openshell missing" || fail "doctor before setup" "rc=$rc $out"

echo "== setup --dry-run plans without changing anything"
out="$(osh setup --gateways vm,docker --dry-run </dev/null 2>&1)"; rc=$?
[ $rc -eq 0 ] && pass "dry-run exits 0" || fail "dry-run exits 0" "rc=$rc $out"
grep -q 'colima start' <<<"$out" && fail "Docker answers, so Colima is not needed" "$out" || pass "Docker answers, so Colima is not needed"
check "dry-run installed nothing" bash -c '! command -v openshell'

if [ -n "${OSH_PRESEED:-}" ]; then
  # KNOWN BUG workaround, opt-in: see tests/linux/real-suite.sh.
  echo "== OSH_PRESEED: writing a vm gateway.toml before setup (known-bug workaround)"
  mkdir -p "$HOME/.config/openshell"
  printf '[openshell]\nversion = 2\n\n[openshell.gateway]\ncompute_driver = "vm"\n' > "$HOME/.config/openshell/gateway.toml"
fi

echo "== setup --gateways vm,docker --yes (real Homebrew, installer, brew services, launchd)"
with_timeout 1800 osh setup --gateways vm,docker --yes </dev/null >"$log" 2>&1; rc=$?
[ $rc -eq 0 ] && pass "setup exits 0" || {
  fail "setup exits 0" "rc=$rc"
  grep -E '^(\[[0-9]+/[0-9]+\]|failed:|skipped|Error|openshell: error)' "$log" | head -40
  tail -5 /opt/homebrew/var/log/openshell/openshell-gateway.err.log 2>/dev/null
}
check "OpenShell is a Homebrew formula" brew list openshell
svc="$(brew services info openshell --json 2>/dev/null)"
grep -q '"running": true' <<<"$svc" && pass "the Homebrew service runs" || fail "the Homebrew service runs" "$(grep -E '"(status|exit_code)"' <<<"$svc" | tr -d ' \n')"
connected openshell-vm && pass "openshell-vm (Homebrew service, main) answers" || fail "openshell-vm answers"
check "the VM main is on 17670" bash -c "grep -A3 '\"openshell-vm\"' '$data/gateways.json' | grep -q 17670"
check "the VM main's config is gateway.toml" grep -q 'compute_driver = "vm"' "$HOME/.config/openshell/gateway.toml"
plist="$HOME/Library/LaunchAgents/dev.guilospanck.openshell-docker.plist"
check "the Docker extra's agent plist exists" plutil -lint -s "$plist"
state="$(launchctl print "$(agent docker)" 2>&1 | awk '$1 == "state" {print $3; exit}')"
[ "$state" = running ] && pass "launchd runs the Docker extra's agent" || fail "launchd runs the Docker extra's agent" "state=$state"
wait_connected openshell-docker && pass "openshell-docker (launchd agent, extra) answers" || { fail "openshell-docker answers"; tail -10 "$HOME/.local/state/openshell/gateway-docker/"*.log 2>/dev/null; }
grep -qx openshell <<<"$(openshell gateway list 2>/dev/null | awk 'NR>1 {print ($1 == "*" ? $2 : $1)}')" \
  && fail "the installer's own 'openshell' registration is replaced" || pass "the installer's own 'openshell' registration is replaced"
out="$(osh doctor 2>&1)"; rc=$?
[ $rc -eq 0 ] && pass "doctor is green after setup" || fail "doctor is green after setup" "$out"

echo "== setup again does nothing"
out="$(with_timeout 300 osh setup --gateways vm,docker --yes </dev/null 2>&1)"; rc=$?
[ $rc -eq 0 ] && [[ "$out" == *"Nothing to do"* ]] && pass "a second setup is a no-op" || fail "a second setup is a no-op" "rc=$rc $(tail -5 <<<"$out")"

echo "== launchd brings a killed gateway back (KeepAlive)"
pid="$(agent_pid docker)"
if [ -n "$pid" ]; then
  kill -9 "$pid"
  new=""
  for _ in $(seq 30); do new="$(agent_pid docker)"; [ -n "$new" ] && [ "$new" != "$pid" ] && break; sleep 1; done
  [ -n "$new" ] && [ "$new" != "$pid" ] && pass "launchd restarted the agent ($pid -> $new)" || fail "launchd restarted the agent" "pid=$pid new=$new"
  wait_connected openshell-docker && pass "openshell-docker answers again" || fail "openshell-docker answers again"
else
  fail "the Docker agent has a pid"
fi

echo "== gateway-reset of the agent-run extra and of the Homebrew main"
printf 'reset\n' | with_timeout 300 osh gateway-reset openshell-docker >"$log" 2>&1; rc=$?
[ $rc -eq 0 ] && pass "resetting the Docker extra exits 0" || { fail "resetting the Docker extra" "rc=$rc"; tail -15 "$log"; }
wait_connected openshell-docker && pass "the extra answers after its reset" || fail "the extra answers after its reset"
printf 'reset\n' | with_timeout 300 osh gateway-reset openshell-vm >"$log" 2>&1; rc=$?
[ $rc -eq 0 ] && pass "resetting the VM main exits 0" || { fail "resetting the VM main" "rc=$rc"; tail -15 "$log"; }
wait_connected openshell-vm && pass "the main answers after its reset (brew services stop/start)" || fail "the main answers after its reset"

echo "== removing the extra, then adding it back"
out="$(printf 'remove\n' | osh gateway remove openshell-vm 2>&1)"; rc=$?
[ $rc -ne 0 ] && [[ "$out" == *"remove openshell-"* ]] && pass "the main cannot go while an extra exists" || fail "main removal refused" "rc=$rc $out"
printf 'remove\n' | with_timeout 300 osh gateway remove openshell-docker >"$log" 2>&1; rc=$?
[ $rc -eq 0 ] && pass "removing the Docker extra exits 0" || { fail "removing the Docker extra" "rc=$rc"; tail -15 "$log"; }
check "launchd no longer knows its agent" bash -c "! launchctl print '$(agent docker)'"
check "its plist is gone" test ! -e "$plist"
grep -q openshell-docker <<<"$(openshell gateway list 2>/dev/null)" && fail "the CLI forgot it" || pass "the CLI forgot it"
connected openshell-vm && pass "the main still answers" || fail "the main still answers"
with_timeout 600 osh gateway add docker --yes </dev/null >"$log" 2>&1; rc=$?
[ $rc -eq 0 ] && pass "gateway add docker exits 0" || { fail "gateway add docker" "rc=$rc"; tail -15 "$log"; }
wait_connected openshell-docker && pass "the re-added extra answers" || fail "the re-added extra answers"

echo "== what cannot run in a macOS guest: microVMs, Colima, Podman machines"
out="$(with_timeout 300 openshell -g openshell-vm sandbox create --name vm-probe --no-keep -- true 2>&1)"; rc=$?
if [ $rc -ne 0 ]; then pass "a microVM sandbox does not boot without a hypervisor (as expected)"; else fail "a microVM booted in a macOS guest?" "$out"; fi
grep -h -m1 -E 'krun|VmCreate' "$HOME/.local/state/openshell/"*/gateway.err.log /opt/homebrew/var/log/openshell/*.log 2>/dev/null | sed 's/^/     reason: /' | head -2
brew install colima >"$log" 2>&1 || fail "brew install colima"
out="$(with_timeout 300 colima start -p probe --vm-type vz 2>&1)"; rc=$?
[ $rc -ne 0 ] && pass "Colima cannot start a VM in a macOS guest (as expected)" || fail "Colima started in a macOS guest?"
grep -h -o 'Virtualization is not available on this hardware' "$HOME/.colima/_lima/colima-probe/ha.stderr.log" 2>/dev/null | head -1 | sed 's/^/     reason: /'
colima delete -p probe --data --force >/dev/null 2>&1
brew install podman >"$log" 2>&1 || fail "brew install podman"
podman machine init >/dev/null 2>&1
out="$(with_timeout 300 podman machine start 2>&1)"; rc=$?
[ $rc -ne 0 ] && pass "a Podman machine cannot start in a macOS guest (as expected)" || fail "a Podman machine started in a macOS guest?"
tail -1 <<<"$out" | sed 's/^/     reason: /'
podman machine rm -f >/dev/null 2>&1

echo "== uninstall"
printf 'uninstall\n' | with_timeout 600 osh uninstall >"$log" 2>&1; rc=$?
[ $rc -eq 0 ] && pass "uninstall exits 0" || { fail "uninstall exits 0" "rc=$rc"; tail -20 "$log"; }
check "launchd no longer knows the Docker agent" bash -c "! launchctl print '$(agent docker)'"
check "no osh agents are left" bash -c "! ls '$HOME/Library/LaunchAgents'/dev.guilospanck.*.plist"
check "the Homebrew service is gone" bash -c "! brew services list | grep -q '^openshell '"
check "the formula is uninstalled" bash -c '! brew list openshell'
check "the config dir is gone" test ! -e "$HOME/.config/openshell"
check "the state dir is gone" test ! -e "$HOME/.local/state/openshell"
check "the registry is gone" test ! -e "$data/gateways.json"

echo
if [ $fails -eq 0 ]; then echo "all macOS VM tests passed"; else echo "$fails macOS VM test(s) failed"; exit 1; fi
