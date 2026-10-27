#!/usr/bin/env bash
# Runs inside the machine tests/linux/real.sh boots, as user `tester` with sudo. Nothing is stubbed.
#   podman machine: one gateway, openshell-podman (main), plus the VM type reported unavailable.
#   docker machine: two gateways, openshell-docker (main) and openshell-podman (extra), then add,
#                   remove and reset of the extra.
set -uo pipefail

runtime="${1:-podman}"
phase="${2:-}"
fails="${PREV_FAILS:-0}"
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1${2:+: $2}"; fails=$((fails + 1)); }
check() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi; }
show_gateway_log() { journalctl --user -u "${1:-openshell-gateway}" --no-pager 2>/dev/null | tail -25; }
connected() { openshell -g "$1" status 2>&1 | grep -q Connected; }
gateway_names() { openshell gateway list -o json 2>/dev/null | grep -oE '"name": ?"[^"]+"' | sed -E 's/.*"([^"]+)"$/\1/'; }
data="$HOME/.local/share/osh"

export PATH="$HOME/.local/bin:$PATH"
log="$HOME/out.log"

echo "== install osh from the local release"
OSH_VERSION=v0.0.0-real OSH_RELEASE_BASE=file:///dist OSH_NO_SETUP=1 sh /repo/install.sh >"$log" 2>&1 \
  && pass "install.sh installs the release" || { fail "install.sh" ; tail -10 "$log"; }
check "osh is on PATH" command -v osh

echo "== doctor before setup"
out="$(osh doctor 2>&1)"; rc=$?
[[ $rc -eq 1 ]] && pass "doctor exits 1 on a bare machine" || fail "doctor exits 1 on a bare machine" "rc=$rc"
grep -q 'MISSING  openshell' <<<"$out" && pass "doctor reports openshell missing" || fail "doctor reports openshell missing" "$out"

if [[ "$runtime" == docker ]]; then gateways="docker,podman"; main=openshell-docker; extra=openshell-podman
else gateways="podman"; main=openshell-podman; extra=""; fi

echo "== setup --dry-run plans without changing anything"
out="$(osh setup --gateways "$gateways" --dry-run </dev/null 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "dry-run exits 0" || fail "dry-run exits 0" "rc=$rc"
check "dry-run installed nothing" bash -c '! command -v openshell'
without_yes="$(osh setup --gateways "$gateways" </dev/null 2>&1)"; rc=$?
[[ $rc -ne 0 ]] && grep -q -- '--yes' <<<"$without_yes" && pass "without a terminal or --yes, nothing runs" || fail "setup refuses without a terminal" "rc=$rc"
check "refusing installed nothing" bash -c '! command -v openshell'
out="$(osh setup --gateways vm --yes </dev/null 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && grep -q '/dev/kvm' <<<"$out" && pass "the VM type is refused with the reason where there is no KVM" || fail "VM refused without KVM" "rc=$rc $out"
out="$(osh setup --gateways lxc --yes </dev/null 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && pass "an unknown gateway type is refused" || fail "unknown type refused" "rc=$rc $out"

if [[ "$runtime" == docker && "$phase" != after-login ]]; then
  # Joining the docker group only reaches the user's systemd services after a new login, and the
  # gateway is one of them, so osh must stop and say so instead of installing OpenShell too early.
  echo "== docker: the first setup stops for a new login"
  osh setup --gateways "$gateways" --yes >"$log" 2>&1; rc=$?
  [[ $rc -ne 0 ]] && grep -q 'log out and back in' "$log" \
    && pass "the first setup asks for a new login" || { fail "the first setup asks for a new login" "rc=$rc"; tail -15 "$log"; }
  check "OpenShell was not installed before the new login" bash -c '! command -v openshell'
  # What a new login does for the user manager: restart it with the user's current groups.
  sudo systemctl restart "user@$(id -u).service"
  for _ in $(seq 20); do [[ -S "$XDG_RUNTIME_DIR/bus" ]] && break; sleep 1; done
  # And a new login is a new process tree with the new groups: carry on in one.
  exec sudo -u tester -H env "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR" \
    "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS" "PATH=$PATH" "PREV_FAILS=$fails" \
    bash "$0" docker after-login
fi

echo "== setup --gateways $gateways --yes (real packages, installer, systemd)"
osh setup --gateways "$gateways" --yes >"$log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup exits 0" || {
  fail "setup exits 0" "rc=$rc"
  grep -E '^(\[[0-9]+/[0-9]+\]|failed:|skipped|do yourself|declined|openshell:|Failed|cp:|E:)' "$log" | head -70
}
check "openshell is installed" openshell --version
check "the runtime is installed" command -v "$runtime"
check "the gateway service is enabled" systemctl --user is-enabled openshell-gateway
check "the gateway service is active" systemctl --user is-active openshell-gateway
if [[ "$runtime" == podman ]]; then check "the podman socket is active" systemctl --user is-active podman.socket; fi

echo "== the main gateway is named after its type"
connected "$main" && pass "$main answers" || { fail "$main answers"; openshell -g "$main" status 2>&1 | head -5; show_gateway_log; }
names="$(gateway_names)"
grep -qx "$main" <<<"$names" && pass "the CLI lists $main" || fail "the CLI lists $main" "$names"
grep -qx "openshell" <<<"$names" && fail "the installer's own 'openshell' registration is replaced" "$names" || pass "the installer's own 'openshell' registration is replaced"
check "the main's config names it" grep -q "name = \"$main\"" "$HOME/.config/openshell/gateway.toml"
check "the main's config drives the right runtime" grep -q "compute_driver = \"$runtime\"" "$HOME/.config/openshell/gateway.toml"
real_validate="$(openshell-gateway config preflight --path "$HOME/.config/openshell/gateway.toml" 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "the real gateway accepts the installed config" || { fail "gateway config preflight"; echo "$real_validate" | head -20; }
check "the registry records the main on 17670" bash -c "grep -q '\"$main\"' '$data/gateways.json' && grep -q 17670 '$data/gateways.json'"
out="$(osh doctor 2>&1)"; rc=$?
[[ $rc -eq 0 ]] && pass "doctor is green after setup" || fail "doctor is green after setup" "$out"

if [[ -n "$extra" ]]; then
  echo "== the second gateway runs next to the main one"
  connected "$extra" && pass "$extra answers" || { fail "$extra answers"; openshell -g "$extra" status 2>&1 | head -5; show_gateway_log "osh-gateway-$extra"; }
  grep -qx "$extra" <<<"$(gateway_names)" && pass "the CLI lists $extra" || fail "the CLI lists $extra"
  check "$extra runs as its own user service" systemctl --user is-active "osh-gateway-$extra.service"
  check "$extra has its own config" test -f "$data/gateways/$extra.toml"
  check "the registry records $extra as an extra on 17671" bash -c "grep -q '\"$extra\"' '$data/gateways.json' && grep -q 17671 '$data/gateways.json'"
  connected "$main" && pass "the main still answers" || fail "the main still answers"
  check "the real gateway accepts the extra's config" openshell-gateway config preflight --path "$data/gateways/$extra.toml"
fi

echo "== setup again does nothing"
out="$(osh setup --gateways "$gateways" --yes 2>&1)"; rc=$?
[[ $rc -eq 0 && "$out" == *"Nothing to do"* ]] && pass "a second setup is a no-op" || fail "a second setup is a no-op" "rc=$rc $out"

echo "== a stopped gateway is brought back by setup"
systemctl --user stop openshell-gateway
osh setup --gateways "$gateways" --yes >"$log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "setup resumes after the service was stopped" || { fail "setup resumes" "rc=$rc"; tail -20 "$log"; }
check "the service is active again" systemctl --user is-active openshell-gateway
connected "$main" && pass "the main answers again" || fail "the main answers again"

echo "== adding a type that cannot work, and one that already exists"
out="$(osh gateway add vm --yes </dev/null 2>&1)"; rc=$?
[[ $rc -eq 2 ]] && grep -q '/dev/kvm' <<<"$out" && pass "gateway add vm is refused with the reason" || fail "gateway add vm refused" "rc=$rc $out"
out="$(osh gateway add "$runtime" --yes </dev/null 2>&1)"; rc=$?
[[ $rc -eq 0 && "$out" == *"Nothing to do"* ]] && pass "adding an existing type is a no-op" || fail "adding an existing type is a no-op" "rc=$rc $out"

echo "== gateway-reset"
printf 'no\n' | osh gateway-reset "$main" >/dev/null 2>&1 && fail "reset aborts on a wrong answer" || pass "reset aborts on a wrong answer"
printf 'reset\n' | osh gateway-reset "$main" >"$log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "gateway-reset exits 0" || { fail "gateway-reset exits 0" "rc=$rc"; tail -30 "$log"; }
connected "$main" && pass "the main answers after a reset" || { fail "the main answers after a reset"; show_gateway_log; }

if [[ -n "$extra" ]]; then
  printf 'reset\n' | osh gateway-reset "$extra" >"$log" 2>&1; rc=$?
  [[ $rc -eq 0 ]] && pass "an extra can be reset too" || { fail "an extra can be reset too" "rc=$rc"; tail -30 "$log"; }
  connected "$extra" && pass "the extra answers after its reset" || fail "the extra answers after its reset"
  connected "$main" && pass "the main is untouched by the extra's reset" || fail "the main is untouched by the extra's reset"

  echo "== removing gateways"
  out="$(printf 'remove\n' | osh gateway remove "$main" 2>&1)"; rc=$?
  [[ $rc -ne 0 ]] && grep -q "remove $extra first" <<<"$out" && pass "the main cannot be removed while an extra exists" || fail "main removal refused" "rc=$rc $out"
  connected "$main" && pass "the refused removal changed nothing" || fail "the refused removal changed nothing"
  printf 'remove\n' | osh gateway remove "$extra" >"$log" 2>&1; rc=$?
  [[ $rc -eq 0 ]] && pass "removing an extra exits 0" || { fail "removing an extra exits 0" "rc=$rc"; tail -20 "$log"; }
  check "the extra's service is gone" bash -c "! systemctl --user is-enabled osh-gateway-$extra.service"
  grep -qx "$extra" <<<"$(gateway_names)" && fail "the CLI forgot $extra" || pass "the CLI forgot $extra"
  check "the registry forgot $extra" bash -c "! grep -q '$extra' '$data/gateways.json'"
  check "its data was kept" test -d "$HOME/.local/state/openshell/gateway-podman"
  connected "$main" && pass "the main still answers after the removal" || fail "the main still answers after the removal"
  echo "== adding it back"
  osh gateway add podman --yes >"$log" 2>&1; rc=$?
  [[ $rc -eq 0 ]] && pass "gateway add podman exits 0" || { fail "gateway add podman exits 0" "rc=$rc"; tail -20 "$log"; }
  connected "$extra" && pass "the re-added extra answers" || fail "the re-added extra answers"
fi

echo "== uninstall"
printf 'uninstall\n' | osh uninstall >"$log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "uninstall exits 0" || { fail "uninstall exits 0" "rc=$rc"; tail -30 "$log"; }
check "the service is disabled" bash -c '! systemctl --user is-enabled openshell-gateway'
check "no osh gateway units are left" bash -c "! ls $HOME/.config/systemd/user/osh-gateway-*.service"
check "the config dir is gone" test ! -e "$HOME/.config/openshell"
check "the state dir is gone" test ! -e "$HOME/.local/state/openshell"
check "the registry is gone" test ! -e "$data/gateways.json"

echo
if [[ $fails -eq 0 ]]; then echo "all real Linux tests passed ($runtime)"; else echo "$fails real Linux test(s) failed ($runtime)"; exit 1; fi
