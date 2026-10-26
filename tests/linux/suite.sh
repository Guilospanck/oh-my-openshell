#!/usr/bin/env bash
# Linux test suite. Runs inside the container built by tests/linux/Dockerfile, in a copy of the
# repo at /work (see run.sh). Real: just doctor, build, check. Stubbed (tests/linux/stubs on PATH,
# every call logged to $STUB_LOG): the install, uninstall and gateway-reset paths, whose real
# tools (OpenShell packages, systemd, Docker) are not in the container.
set -uo pipefail

REPO="${REPO:-/work}"
STUBS="$REPO/tests/linux/stubs"
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
with_stubs() { PATH="$STUBS:$PATH" "$@"; }

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

echo "== stubbed: just install"
fresh_home
with_stubs "${J[@]}" install >"$HOME/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "install exits 0" || { fail "install exits 0" "rc=$rc"; tail -15 "$HOME/out.log"; }
check "install copies gateway-linux.toml as gateway.toml" cmp "$REPO/gateway-linux.toml" "$XDG_CONFIG_HOME/openshell/gateway.toml"
check "install links osh" test -L "$HOME/.local/bin/osh"
check "install ran the official installer" logged installer-ran
check "install enables the user service" logged "systemctl --user enable openshell-gateway"
check "install restarts the user service" logged "systemctl --user restart openshell-gateway"
check "install does not touch Homebrew or launchd" bash -c "! grep -E '^(brew|launchctl) ' '$STUB_LOG'"
a="$(line_of installer-ran)"; b="$(line_of 'systemctl --user restart openshell-gateway')"
[[ -n "$a" && -n "$b" && "$a" -lt "$b" ]] && pass "installer runs before the service restart" || fail "installer runs before the service restart"
grep -q 'Docker Engine not found' "$HOME/out.log" && fail "install warns about Docker although a docker stub exists" || pass "no Docker warning when docker exists"

echo "== stubbed: just uninstall"
printf 'uninstall\n' | with_stubs "${J[@]}" uninstall >"$HOME/out.log" 2>&1; rc=$?
[[ $rc -eq 0 ]] && pass "uninstall exits 0" || { fail "uninstall exits 0" "rc=$rc"; tail -15 "$HOME/out.log"; }
check "uninstall disables the user service" logged "systemctl --user disable --now openshell-gateway"
check "uninstall removes the config dir" test ! -e "$XDG_CONFIG_HOME/openshell"
check "uninstall removes the osh link" test ! -e "$HOME/.local/bin/osh"
check "uninstall prints the package removal commands" grep -q 'apt remove' "$HOME/out.log"
fresh_home; mkdir -p "$XDG_CONFIG_HOME/openshell"
printf 'no\n' | with_stubs "${J[@]}" uninstall >/dev/null 2>&1 && fail "uninstall aborts on a wrong answer" || pass "uninstall aborts on a wrong answer"
check "an aborted uninstall keeps the config dir" test -d "$XDG_CONFIG_HOME/openshell"

echo "== stubbed: gateway-reset"
fresh_home; mkdir -p "$HOME/.local/state/openshell/gateway" "$HOME/.local/state/openshell/docker-sandbox-tokens"
touch "$HOME/.local/state/openshell/gateway/openshell.db" "$HOME/.local/state/openshell/docker-sandbox-tokens/t"
with_stubs "$REPO/gateway-reset" openshell >"$HOME/out.log" 2>&1; rc=$?
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
with_stubs "$REPO/gateway-reset" openshell-docker >/dev/null 2>&1; rc=$?
[[ $rc -eq 2 ]] && pass "gateway-reset openshell-docker is refused on Linux (exit 2)" || fail "gateway-reset openshell-docker is refused" "rc=$rc"
with_stubs "$REPO/gateway-reset" >/dev/null 2>&1; rc=$?
[[ $rc -eq 2 ]] && pass "gateway-reset without a name prints usage (exit 2)" || fail "gateway-reset without a name" "rc=$rc"

echo "== stubbed: other recipes"
out="$(with_stubs "${J[@]}" gateway-docker 2>&1)"; rc=$?
[[ $rc -eq 0 && "$out" == *"Not needed on Linux"* ]] && pass "gateway-docker is a no-op on Linux" || fail "gateway-docker is a no-op on Linux" "rc=$rc $out"
fresh_home
nodocker="$(mktemp -d)"
for f in "$STUBS"/*; do [[ "$(basename "$f")" == docker ]] || ln -s "$f" "$nodocker/"; done
PATH="$nodocker:$PATH" "${J[@]}" install >"$HOME/out.log" 2>&1
grep -q 'Docker Engine not found' "$HOME/out.log" && pass "install warns when Docker is missing" || fail "install warns when Docker is missing"

echo
if [[ $fails -eq 0 ]]; then echo "all Linux tests passed"; else echo "$fails Linux test(s) failed"; exit 1; fi
