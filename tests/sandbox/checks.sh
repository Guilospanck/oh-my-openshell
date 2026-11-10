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
  echo "== a sandbox made from osh's policy and image: terminal, tmux, writes, git config"
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

  # The host git config copy (cli::install_sandbox_confs): upload to a nested path, then the
  # include script from agents::git_include_script, run twice to prove it adds the line once.
  printf '[alias]\n\toshhi = !echo osh-git-alias\n' >"$HOME/osh-gitconfig"
  local home; home="$(ex 'printf %s "$HOME"')"
  ex 'mkdir -p "$HOME/.config/osh"' >/dev/null
  out="$(openshell -g "$main" sandbox upload "$name" "$HOME/osh-gitconfig" "$home/.config/osh/gitconfig" --no-git-ignore 2>&1)" \
    && pass "the git config uploads to a nested path" || fail "the git config uploads to a nested path" "home=[$home] $out"
  local inc="if [ -e \"\$HOME/.gitconfig\" ]; then git config --global --get-all include.path | grep -qxF '~/.config/osh/gitconfig' || git config --global --add include.path '~/.config/osh/gitconfig'; else printf '[include]\\n\\tpath = ~/.config/osh/gitconfig\\n' > \"\$HOME/.gitconfig\"; fi"
  out="$(ex "$inc; $inc; git oshhi; git config --global --get-all include.path | grep -c osh/gitconfig")"
  [[ "$out" == *osh-git-alias* && "$out" == *$'\n'1* ]] && pass "a host git alias works and the include is added once" \
    || fail "a host git alias works and the include is added once" "$out"

  # Opt-in profiles (providers/{rust,go,js}.yaml): import, create and attach each to the same
  # sandbox (the gateway refuses endpoints that overlap with conflicting metadata, so this proves
  # they compose). The attach is checked with --wait because the gateway saves an attachment
  # whose composed policy the sandbox later rejects (an invalid host wildcard, for instance), and
  # only the wait reports that.
  local pr
  for pr in rust go js; do
    openshell -g "$main" provider profile import --file "$data/providers/$pr.yaml" >"$HOME/prof-$pr.log" 2>&1 \
      && openshell -g "$main" provider create --name "$pr" --type "$pr" >>"$HOME/prof-$pr.log" 2>&1 \
      && openshell -g "$main" sandbox provider attach "$name" "$pr" --wait --timeout 60 >>"$HOME/prof-$pr.log" 2>&1 \
      && ! grep -q 'did not complete' "$HOME/prof-$pr.log" \
      && pass "the $pr profile imports, creates and attaches" || { fail "the $pr profile imports, creates and attaches" "$(tail -5 "$HOME/prof-$pr.log")"; }
  done

  # What the "Allow web reads ... until the session ends" prompt option runs for each new host
  # (watch::allow): curl is refused, the exact allow osh issues is applied, curl then succeeds.
  out="$(ex 'curl -s -o /dev/null -w "%{http_code}" --max-time 20 https://www.iana.org/')"
  [[ "$out" != 200 ]] && pass "an unlisted host is refused before the allow" || fail "an unlisted host is refused before the allow" "$out"
  openshell -g "$main" policy update "$name" --add-endpoint www.iana.org:443:read-only:rest:enforce \
    --binary /usr/bin/curl --wait --timeout 60 >"$HOME/auto-allow.log" 2>&1 \
    && pass "the read-only allow osh issues is accepted" || fail "the read-only allow osh issues is accepted" "$(tail -5 "$HOME/auto-allow.log")"
  out="$(ex 'curl -s -o /dev/null -w "%{http_code}" --max-time 30 https://www.iana.org/')"
  [[ "$out" == 200 ]] && pass "curl reads the host once it is allowed" || fail "curl reads the host once it is allowed" "$out"

  # The "any program" auto-allow (watch::allow_domain): the registrable domain and its
  # subdomains in one update, for every program. `**.D` does not match D itself, so both go in.
  out="$(ex 'curl -s -o /dev/null -w "%{http_code}" --max-time 20 https://www.ietf.org/')"
  [[ "$out" != 200 ]] && pass "a host of another domain is refused before the domain allow" || fail "a host of another domain is refused before the domain allow" "$out"
  openshell -g "$main" policy update "$name" --add-endpoint ietf.org:443:read-only:rest:enforce \
    --add-endpoint '**.ietf.org:443:read-only:rest:enforce' --binary '/**' --wait --timeout 60 >"$HOME/domain-allow.log" 2>&1 \
    && pass "the domain-wide allow osh issues is accepted" || fail "the domain-wide allow osh issues is accepted" "$(tail -5 "$HOME/domain-allow.log")"
  out="$(ex 'curl -s -o /dev/null -w "%{http_code}" --max-time 30 https://www.ietf.org/')"
  [[ "$out" == 200 ]] && pass "curl reads a subdomain of the allowed domain" || fail "curl reads a subdomain of the allowed domain" "$out"
  out="$(ex 'python3 -c "import urllib.request as u; print(u.urlopen(\"https://datatracker.ietf.org/\", timeout=30).status)"')"
  [[ "$out" == 200 ]] && pass "another program reads a second subdomain on its first try" || fail "another program reads a second subdomain on its first try" "$out"
  out="$(ex 'curl -s -o /dev/null -w "%{http_code}" -X POST --max-time 30 https://www.ietf.org/')"
  [[ "$out" == 403 ]] && pass "a POST to the allowed domain is still denied" || fail "a POST to the allowed domain is still denied" "$out"

  # The toolchain host list on a running sandbox (preload::update_commands): the endpoints in
  # one update, the git fetch POST in a second (OpenShell refuses both flags together), after
  # which a clone over https works.
  openshell -g "$main" policy update "$name" --add-endpoint github.com:443:read-only:rest:enforce \
    --add-endpoint codeload.github.com:443:read-only:rest:enforce --binary '/**' --wait --timeout 60 >"$HOME/toolchain-1.log" 2>&1 \
    && pass "the toolchain endpoints update is accepted" || fail "the toolchain endpoints update is accepted" "$(tail -5 "$HOME/toolchain-1.log")"
  openshell -g "$main" policy update "$name" --rule-name allow_github_com_443 --binary '/**' \
    --add-allow 'github.com:443:POST:/**/git-upload-pack' --wait --timeout 60 >"$HOME/toolchain-2.log" 2>&1 \
    && pass "the git fetch allow update is accepted" || fail "the git fetch allow update is accepted" "$(tail -5 "$HOME/toolchain-2.log")"
  out="$(ex 'rm -rf /tmp/hw && git clone --depth 1 -q https://github.com/octocat/Hello-World /tmp/hw && ls /tmp/hw | head -1')"
  [[ -n "$out" ]] && pass "git clone over https works once the list is applied" || fail "git clone over https works once the list is applied" "$out"

  # Port forwarding offers (ports.rs): the scan osh runs must list a new listener, and the
  # forward osh starts must reach it from the host.
  ex 'nohup python3 -m http.server 18765 --bind 127.0.0.1 >/dev/null 2>&1 & sleep 2' >/dev/null
  out="$(openshell -g "$main" sandbox exec --name "$name" --timeout 10 --no-tty -- cat /proc/net/tcp /proc/net/tcp6 2>/dev/null)"
  # 18765 is 494D in hex.
  [[ "$out" == *":494D "* ]] && pass "a listener in the sandbox shows in /proc/net/tcp as osh's scan reads it" \
    || fail "a listener in the sandbox shows in /proc/net/tcp as osh's scan reads it" "$out"
  openshell -g "$main" forward start -d 18765 "$name" >"$HOME/forward.log" 2>&1 \
    && pass "forward start -d accepts the port osh offers" || fail "forward start -d accepts the port osh offers" "$(tail -5 "$HOME/forward.log")"
  out="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 http://localhost:18765/ 2>&1)"
  [[ "$out" == 200 ]] && pass "the forwarded port answers on the host" || fail "the forwarded port answers on the host" "$out"
  openshell -g "$main" forward stop 18765 "$name" >/dev/null 2>&1 && pass "forward stop ends it" || fail "forward stop ends it"

  openshell -g "$main" sandbox delete "$name" >/dev/null 2>&1 && pass "the sandbox is deleted" || fail "the sandbox is deleted"
}
