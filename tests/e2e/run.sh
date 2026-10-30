#!/usr/bin/env bash
# Sourced by tests/linux/real-suite.sh inside the disposable systemd machine, after the gateway,
# the claude-code image and the sandbox checks. Runs every scenario in tests/e2e/scenarios.py
# against the real dashboard (pseudo-terminal) and the real gateway. Needs $main, pass/fail.
e2e_run() {
  echo "== end to end: the dashboard, every action"
  local out rc
  out="$(OSH_GATEWAY="$main" python3 /repo/tests/e2e/scenarios.py ${OSH_E2E_ONLY:-} 2>&1)"; rc=$?
  echo "$out" | grep -E '^(ok|FAIL|SKIP)'
  [[ $rc -eq 0 ]] && pass "every end-to-end scenario passed" || { fail "end-to-end scenarios" "rc=$rc"; echo "$out" | grep -v '^ok' | tail -60; }
}
