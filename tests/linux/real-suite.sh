#!/usr/bin/env bash
# Runs inside the machine tests/linux/real.sh boots, as user `tester` with sudo. Nothing is stubbed.
set -uo pipefail

runtime="${1:-podman}"
fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1${2:+: $2}"; fails=$((fails + 1)); }
check() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi; }
show_gateway_log() { journalctl --user -u openshell-gateway --no-pager 2>/dev/null | tail -25; }

export PATH="$HOME/.local/bin:$PATH"
log="$HOME/out.log"

echo "== install osh from the local release"
OSH_VERSION=v0.0.0-real OSH_RELEASE_BASE=file:///dist OSH_NO_SETUP=1 sh /repo/install.sh >"$log" 2>&1 \
  && pass "install.sh installs the release" || { fail "install.sh" ; tail -10 "$log"; }
check "osh runs" osh doctor

echo "== doctor before setup"
out="$(osh doctor 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && pass "doctor exits 1 on a bare machine" || fail "doctor exits 1 on a bare machine" "rc=$rc"
grep -q 'MISSING  openshell' <<<"$out" && pass "doctor reports openshell missing" || fail "doctor reports openshell missing" "$out"

echo "== setup --dry-run plans without changing anything"
out="$(osh setup --dry-run --runtime "$runtime" </dev/null 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "dry-run exits 0" || fail "dry-run exits 0" "rc=$rc"
check "dry-run installed nothing" bash -c '! command -v openshell'
without_yes="$(osh setup --runtime "$runtime" </dev/null 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && grep -q -- '--yes' <<<"$without_yes" && pass "without a terminal or --yes, nothing runs" || fail "setup refuses without a terminal" "rc=$rc"
check "refusing installed nothing" bash -c '! command -v openshell'

echo "== setup --yes --runtime $runtime (real packages, installer, systemd)"
osh setup --yes --runtime "$runtime" >"$log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup exits 0" || { fail "setup exits 0" "rc=$rc"; tail -40 "$log"; show_gateway_log; }
check "openshell is installed" openshell --version
check "the runtime is installed" command -v "$runtime"
case "$runtime" in podman) want=gateway-podman.toml ;; docker) want=gateway-linux.toml ;; esac
check "the $want config is installed" cmp /repo/"$want" "$HOME/.config/openshell/gateway.toml"
check "the gateway service is enabled" systemctl --user is-enabled openshell-gateway
check "the gateway service is active" systemctl --user is-active openshell-gateway
if [[ "$runtime" == podman ]]; then check "the podman socket is active" systemctl --user is-active podman.socket; fi
out="$(openshell status 2>&1)"
grep -q 'Connected' <<<"$out" && pass "openshell status says Connected" || { fail "openshell status says Connected" "$out"; show_gateway_log; }
out="$(osh doctor 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "doctor is green after setup" || fail "doctor is green after setup" "$out"
real_validate="$(openshell-gateway config preflight --path "$HOME/.config/openshell/gateway.toml" 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "the real gateway accepts the installed config" || fail "gateway config preflight" "$real_validate"

echo "== setup again does nothing"
out="$(osh setup --yes --runtime "$runtime" 2>&1)"; rc=$?
[[ $rc -eq 0 && "$out" == *"Nothing to install"* ]] && pass "a second setup is a no-op" || fail "a second setup is a no-op" "rc=$rc $out"

echo "== a stopped gateway is brought back by setup"
systemctl --user stop openshell-gateway
osh setup --yes --runtime "$runtime" >"$log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup resumes after the service was stopped" || { fail "setup resumes" "rc=$rc"; tail -20 "$log"; }
check "the service is active again" systemctl --user is-active openshell-gateway

echo "== gateway-reset"
printf 'no\n' | osh gateway-reset openshell >/dev/null 2>&1 && fail "reset aborts on a wrong answer" || pass "reset aborts on a wrong answer"
printf 'reset\n' | osh gateway-reset openshell >"$log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "gateway-reset exits 0" || { fail "gateway-reset exits 0" "rc=$rc"; tail -30 "$log"; }
out="$(openshell status 2>&1)"
grep -q 'Connected' <<<"$out" && pass "the gateway answers after a reset" || { fail "the gateway answers after a reset" "$out"; show_gateway_log; }

echo "== uninstall"
printf 'uninstall\n' | osh uninstall >"$log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "uninstall exits 0" || { fail "uninstall exits 0" "rc=$rc"; tail -30 "$log"; }
check "the service is disabled" bash -c '! systemctl --user is-enabled openshell-gateway'
check "the config dir is gone" test ! -e "$HOME/.config/openshell"
check "the state dir is gone" test ! -e "$HOME/.local/state/openshell"

echo
if [[ $fails -eq 0 ]]; then echo "all real Linux tests passed ($runtime)"; else echo "$fails real Linux test(s) failed ($runtime)"; exit 1; fi
