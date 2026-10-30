#!/usr/bin/env bash
# Sourced by tests/linux/real-suite.sh inside the disposable systemd machine that
# `just test-sandbox` (tests/linux/real.sh) boots, never on the developer's own machine.
# Creates one sandbox on the machine's main gateway from the policy and Dockerfile osh installed
# in its data directory, and checks what the dashboard relies on inside it: a terminal can be
# opened, tmux starts a session and keeps it, and the sandbox user can write to its home and /tmp. Needs from the suite: $main (gateway name), $runtime (docker|podman), $data (osh data
# dir), and its pass/fail helpers.
# Why it exists: unit tests passed while tmux could not start in any sandbox, because OpenShell's
# default policy denies opening a terminal. Only a real sandbox shows that kind of failure.

sandbox_checks() {
  local name=osh-test tag=claude-code out
  echo "== a sandbox made from osh's policy and image: terminal, tmux, writes"
  check "osh wrote the sandbox policy" test -f "$data/sandbox-policy.yaml"
  "$runtime" build -f "$data/images/Dockerfile-claude_code" -t "$tag" "$data/images" >"$HOME/image.log" 2>&1 \
    && pass "the claude-code image builds" || { fail "the claude-code image builds"; tail -8 "$HOME/image.log"; return; }
  timeout 900 openshell -g "$main" sandbox create --name "$name" --from "$tag" \
    --policy "$data/sandbox-policy.yaml" --detach >"$HOME/sandbox.log" 2>&1 \
    && pass "a sandbox is created with the policy" || { fail "a sandbox is created with the policy"; tail -8 "$HOME/sandbox.log"; return; }
  ex() { timeout 120 openshell -g "$main" sandbox exec -n "$name" -- sh -c "$1" 2>&1; }

  out="$(ex 'python3 -c "import os; os.openpty(); print(\"openpty-ok\")"')"
  [[ "$out" == *openpty-ok* ]] && pass "a pseudo-terminal can be opened" || fail "a pseudo-terminal can be opened" "$out"

  out="$(ex 'tmux new-session -d -s osh bash && sleep 1 && tmux ls')"
  [[ "$out" == *"osh:"* ]] && pass "tmux starts a session and keeps it" || fail "tmux starts a session and keeps it" "$out"

  out="$(ex 'tmux send-keys -t osh "echo osh-in-tmux > /tmp/osh-tmux-out" Enter; sleep 1; cat /tmp/osh-tmux-out')"
  [[ "$out" == *osh-in-tmux* ]] && pass "a command typed into the tmux shell runs" || fail "a command typed into the tmux shell runs" "$out"

  # Landlock allows the system directories; plain file modes still apply to the sandbox user.
  out="$(ex 'touch /tmp/osh-w "$HOME/osh-w" /dev/shm/osh-w && echo write-ok')"
  [[ "$out" == *write-ok* ]] && pass "the sandbox user can write to its home, /tmp and /dev/shm" || fail "the sandbox user can write to its home, /tmp and /dev/shm" "$out"

  openshell -g "$main" sandbox delete "$name" >/dev/null 2>&1 && pass "the sandbox is deleted" || fail "the sandbox is deleted"
}
