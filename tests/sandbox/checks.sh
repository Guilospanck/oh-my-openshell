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
  # The policy osh writes for a new sandbox with two toolchain groups chosen ("one":
  # example.com and www.example.org; "two": www.example.org and git host codeberg.org): the base
  # plus one rule per host. A unit test (preload) keeps this block equal to what osh generates.
  { printf '%s\n\n' "$(cat "$data/sandbox-policy.yaml")"; cat /repo/tests/sandbox/toolchains-close.yaml; } >"$HOME/tc-policy.yaml"
  timeout 900 openshell -g "$main" sandbox create --name "$name" --from "$tag" \
    --policy "$HOME/tc-policy.yaml" --detach >"$HOME/sandbox.log" 2>&1 \
    && pass "a sandbox is created with the policy and two toolchain groups" || { fail "a sandbox is created with the policy and two toolchain groups"; tail -8 "$HOME/sandbox.log"; return; }
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

  # The workmux config copy (cli::upload_workmux_conf; text from agents::sandbox_workmux_conf with no
  # host config): without an agent pane `workmux add -P` fails with "no pane is configured to run
  # the agent", which is what /orchestrate hit in a sandbox. Upload the minimal config, then
  # dispatch a prompt from a throwaway repo in a tmux session of its own.
  printf 'agent: claude\nnerdfont: false\npanes:\n  - command: <agent>\n    focus: true\n  - split: horizontal\n' >"$HOME/osh-workmux.yaml"
  ex 'mkdir -p "$HOME/.config/workmux"' >/dev/null
  out="$(openshell -g "$main" sandbox upload "$name" "$HOME/osh-workmux.yaml" "$home/.config/workmux/config.yaml" --no-git-ignore 2>&1)" \
    && pass "the workmux config uploads to a nested path" || fail "the workmux config uploads to a nested path" "home=[$home] $out"
  out="$(ex 'export PATH="$HOME/.local/bin:$PATH"; D=$(mktemp -d /tmp/wmchk.XXXX); cd "$D" && git init -q . && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m init && echo "do x" > p.md && tmux -L wmchk -f /dev/null new-session -d -s chk -c "$D" && tmux -L wmchk send-keys -t chk "workmux add chk-task -b -P p.md > out.txt 2>&1 </dev/null; echo DONE >> out.txt" Enter; for i in 1 2 3 4 5 6 7 8 9 10; do grep -q DONE out.txt 2>/dev/null && break; sleep 1; done; cat out.txt; tmux -L wmchk kill-server; cd /; rm -rf "$D"')"
  [[ "$out" == *DONE* && "$out" != *"no pane is configured"* && "$out" != *Error* ]] && pass "workmux add -P finds the agent pane from the uploaded config" \
    || fail "workmux add -P finds the agent pane from the uploaded config" "$out"

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

  # The toolchain host list on a running sandbox (preload::update_commands): one update per host,
  # each into its own osh_tc_ rule, the git fetch POST in a later one (OpenShell refuses both flags
  # together), after which a clone over https works.
  openshell -g "$main" policy update "$name" --add-endpoint github.com:443:read-only:rest:enforce \
    --rule-name osh_tc_github_com --binary '/**' >"$HOME/toolchain-1.log" 2>&1 \
    && openshell -g "$main" policy update "$name" --add-endpoint codeload.github.com:443:read-only:rest:enforce \
    --rule-name osh_tc_codeload_github_com --binary '/**' >>"$HOME/toolchain-1.log" 2>&1 \
    && pass "the toolchain endpoints updates are accepted" || fail "the toolchain endpoints updates are accepted" "$(tail -5 "$HOME/toolchain-1.log")"
  openshell -g "$main" policy update "$name" --rule-name osh_tc_github_com --binary '/**' \
    --add-allow 'github.com:443:POST:/**/git-upload-pack' --wait --timeout 60 >"$HOME/toolchain-2.log" 2>&1 \
    && pass "the git fetch allow update is accepted" || fail "the git fetch allow update is accepted" "$(tail -5 "$HOME/toolchain-2.log")"
  out="$(ex 'rm -rf /tmp/hw && git clone --depth 1 -q https://github.com/octocat/Hello-World /tmp/hw && ls /tmp/hw | head -1')"
  [[ -n "$out" ]] && pass "git clone over https works once the list is applied" || fail "git clone over https works once the list is applied" "$out"

  # Closing toolchain group "one" while "two" stays marked (preload::close_plan/close_commands):
  # osh removes only example.com's own rule; www.example.org stays because "two" lists it, and the
  # providers' rules and the user's own (the ietf.org allow above) are left alone.
  code() { ex "curl -s -o /dev/null -w '%{http_code}' --max-time 30 https://$1/"; }
  out="$(code example.com)"
  [[ "$out" == 200 ]] && pass "a host of toolchain group one is open from creation" || fail "a host of toolchain group one is open from creation" "$out"
  openshell -g "$main" policy get "$name" --full -o json >"$HOME/tc-before.json" 2>&1 \
    && grep -q '"osh_tc_example_com"' "$HOME/tc-before.json" && grep -q '"_provider_rust"' "$HOME/tc-before.json" \
    && pass "policy get shows osh's per-host rule and the provider's rule" || fail "policy get shows osh's per-host rule and the provider's rule" "$(head -c 400 "$HOME/tc-before.json")"
  openshell -g "$main" policy update "$name" --remove-rule osh_tc_example_com --wait --timeout 60 >"$HOME/tc-close.log" 2>&1 \
    && pass "the close osh issues for group one is accepted" || fail "the close osh issues for group one is accepted" "$(tail -5 "$HOME/tc-close.log")"
  out="$(code example.com)"
  [[ "$out" != 200 ]] && pass "the closed group's host is refused" || fail "the closed group's host is refused" "$out"
  out="$(code www.example.org)"
  [[ "$out" == 200 ]] && pass "the host group two shares stays open" || fail "the host group two shares stays open" "$out"
  out="$(code www.ietf.org)"
  [[ "$out" == 200 ]] && pass "a rule that is not osh's still works after the close" || fail "a rule that is not osh's still works after the close" "$out"
  openshell -g "$main" policy get "$name" --full -o json >"$HOME/tc-after.json" 2>&1
  ! grep -q '"osh_tc_example_com"' "$HOME/tc-after.json" && grep -q '"_provider_rust"' "$HOME/tc-after.json" \
    && grep -q '"osh_tc_www_example_org"' "$HOME/tc-after.json" && grep -q '"osh_tc_codeberg_org"' "$HOME/tc-after.json" \
    && pass "only the closed rule is gone; provider and group two rules are intact" || fail "only the closed rule is gone; provider and group two rules are intact" "$(head -c 400 "$HOME/tc-after.json")"

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

  # The resource meter (usage.rs): the exact script osh runs (usage::script(true); a unit test
  # keeps this copy equal to it) reads /proc/stat twice, /proc/meminfo and df under the shipped
  # policy, with the same exec flags.
  local meter='{ read -r l < /proc/stat && echo "$l"; } 2>/dev/null; sleep 1; { read -r l < /proc/stat && echo "$l"; } 2>/dev/null; cat /proc/meminfo 2>/dev/null; df -Pk /sandbox / 2>/dev/null; true'
  out="$(openshell -g "$main" sandbox exec --name "$name" --timeout 10 --no-tty -- sh -c "$meter" 2>/dev/null)"
  [[ "$(grep -c '^cpu ' <<<"$out")" == 2 ]] && pass "the meter reads two CPU samples from /proc/stat" \
    || fail "the meter reads two CPU samples from /proc/stat" "$out"
  grep -q '^MemTotal:' <<<"$out" && grep -q '^MemAvailable:' <<<"$out" \
    && pass "the meter reads MemTotal and MemAvailable" || fail "the meter reads MemTotal and MemAvailable" "$out"
  grep -Eq '^.* [0-9]+ +[0-9]+ +[0-9]+ +[0-9]+% +/(sandbox)?$' <<<"$out" \
    && pass "the meter reads df for /sandbox or /" || fail "the meter reads df for /sandbox or /" "$out"

  openshell -g "$main" sandbox delete "$name" >/dev/null 2>&1 && pass "the sandbox is deleted" || fail "the sandbox is deleted"

  headless_checks "$tag"
  settings_checks
}

# MCP jobs (tui/core/src/jobs.rs): a command that outlives the request that started it. Every
# call below is a fresh `osh mcp` process, so finding a job again by its id also proves that a
# restarted server still has it. Runs in the sandbox $1 that headless_checks made.
jobs_checks() {
  local hn="$1" out id st o c i code
  echo "== MCP jobs: wait_secs, a job found again by id, output offsets, cancel, max_secs"
  : >"$HOME/jobs-mcp.err"
  # What went wrong, for a failure line: the answer, then the tail of what osh mcp said on stderr.
  why() { printf '%s | stderr: %s' "$1" "$(tail -c 300 "$HOME/jobs-mcp.err" 2>/dev/null | tr '\n' ' ')"; }
  # One tools/call through a fresh `osh mcp`; prints the tool's JSON text.
  mcpcall() {
    printf '%s\n%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize"}' \
      "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}" \
      | timeout 120 osh mcp --gateway "$main" 2>>"$HOME/jobs-mcp.err" \
      | python3 -c 'import json,sys; l=[json.loads(x) for x in sys.stdin.read().splitlines() if x.strip()]; print(l[1]["result"]["content"][0]["text"])'
  }
  # A top-level field of the JSON on stdin.
  jf() { python3 -c 'import json,sys; v=json.loads(sys.stdin.read()); print(v.get(sys.argv[1],""))' "$1"; }
  # Poll job_status until the job is no longer running (20 s); the last answer is left in $st.
  jobwait() {
    local k
    for k in $(seq 1 40); do
      st="$(mcpcall job_status "{\"job\":\"$1\"}")"
      [[ "$(jf state <<<"$st")" == running ]] || return 0
      sleep 0.5
    done
  }

  out="$(mcpcall sandbox_exec "{\"name\":\"$hn\",\"argv\":[\"echo\",\"fast\"],\"wait_secs\":30}")"
  [[ "$(jf exit_code <<<"$out")" == 0 && "$(jf stdout <<<"$out")" == fast ]] \
    && pass "a quick command returns its result within wait_secs" || fail "a quick command returns its result within wait_secs" "$(why "$out")"

  out="$(mcpcall sandbox_exec "{\"name\":\"$hn\",\"argv\":[\"sh\",\"-c\",\"sleep 2; echo hi; exit 3\"],\"wait_secs\":0}")"
  id="$(jf job <<<"$out")"
  [[ "$id" == j* && "$(jf status <<<"$out")" == running ]] \
    && pass "wait_secs 0 returns a job at once" || fail "wait_secs 0 returns a job at once" "$(why "$out")"
  jobwait "$id"
  [[ "$(jf state <<<"$st")" == exited && "$(jf exit_code <<<"$st")" == 3 ]] \
    && pass "the job ends exited with its exit code, found again by id" || fail "the job ends exited with its exit code" "$(why "$st")"
  o="$(mcpcall job_output "{\"job\":\"$id\"}")"
  [[ "$(jf stdout <<<"$o")" == hi && "$(jf stdout_offset <<<"$o")" == 3 ]] \
    && pass "job_output returns the text and the next offset" || fail "job_output returns the text and the next offset" "$(why "$o")"
  o="$(mcpcall job_output "{\"job\":\"$id\",\"stdout_offset\":3}")"
  [[ -z "$(jf stdout <<<"$o")" && "$(jf more <<<"$o")" == False ]] \
    && pass "reading from the next offset returns nothing more" || fail "reading from the next offset returns nothing more" "$o"
  out="$(timeout 60 openshell -g "$main" sandbox exec -n "$hn" --no-tty -- ls /tmp/.osh-jobs 2>&1)"
  [[ "$out" == *"$id"* ]] && pass "the job's files are in the sandbox" || fail "the job's files are in the sandbox" "$out"

  out="$(mcpcall sandbox_exec "{\"name\":\"$hn\",\"argv\":[\"sleep\",\"600\"],\"wait_secs\":0}")"
  id="$(jf job <<<"$out")"
  c="$(mcpcall job_cancel "{\"job\":\"$id\"}")"
  [[ "$(jf state <<<"$c")" == cancelled ]] && pass "job_cancel ends a running job" || fail "job_cancel ends a running job" "$(why "$c")"
  # /proc is read instead of `ps` (not in every image); [s]leep keeps this check from matching itself.
  code=1
  for i in 1 2 3 4 5; do
    out="$(timeout 60 openshell -g "$main" sandbox exec -n "$hn" --no-tty -- sh -c 'for p in /proc/[0-9]*/cmdline; do tr "\0" " " < "$p" 2>/dev/null; echo; done | grep -c "[s]leep 600"' 2>&1)"
    [[ "$out" == 0 ]] && { code=0; break; }
    sleep 1
  done
  [[ $code -eq 0 ]] && pass "no process of the cancelled job is left in the sandbox" || fail "no process of the cancelled job is left in the sandbox" "still running: $out"

  # The whole process group goes, not only the program: a command with children of its own. With
  # no cap (mcp.job_max_secs 0) there is no `timeout` between the job and its children either.
  osh config set mcp.job_max_secs 0 >/dev/null 2>&1
  out="$(mcpcall sandbox_exec "{\"name\":\"$hn\",\"argv\":[\"sh\",\"-c\",\"sleep 601 & sleep 602 & wait\"],\"wait_secs\":0}")"
  id="$(jf job <<<"$out")"
  sleep 1
  c="$(mcpcall job_cancel "{\"job\":\"$id\"}")"
  code=1
  for i in 1 2 3 4 5; do
    out="$(timeout 60 openshell -g "$main" sandbox exec -n "$hn" --no-tty -- sh -c 'for p in /proc/[0-9]*/cmdline; do tr "\0" " " < "$p" 2>/dev/null; echo; done | grep -c "[s]leep 60[12]"' 2>&1)"
    [[ "$out" == 0 ]] && { code=0; break; }
    sleep 1
  done
  [[ "$(jf state <<<"$c")" == cancelled && $code -eq 0 ]] \
    && pass "cancel ends the grandchildren of a job with no cap too" || fail "cancel ends the grandchildren of a job with no cap too" "state=$(jf state <<<"$c") left=$out"
  osh config unset mcp.job_max_secs >/dev/null 2>&1

  out="$(mcpcall sandbox_exec "{\"name\":\"$hn\",\"argv\":[\"sleep\",\"30\"],\"wait_secs\":0,\"max_secs\":2}")"
  id="$(jf job <<<"$out")"
  jobwait "$id"
  code="$(jf exit_code <<<"$st")"
  [[ "$(jf state <<<"$st")" == exited && ( "$code" == 124 || "$code" == 137 ) ]] \
    && pass "max_secs ends a job that runs too long" || fail "max_secs ends a job that runs too long" "$(why "$st")"

  check "the job index folder exists" test -d "$data/mcp-jobs"
  out="$(stat -c %a "$data"/mcp-jobs/*.json 2>/dev/null | sort -u)"
  [[ "$out" == 600 ]] && pass "every job index file is owner-only" || fail "every job index file is owner-only" "$out"
  check "no job index file holds an argument" bash -c "! grep -rqE 'sleep 600|sleep 30|echo hi' '$data/mcp-jobs'"
}

# The headless path (osh sandbox/apply/mcp, tui/src/headless.rs and mcpcmd.rs): the same binary
# the dashboard uses creates a second sandbox from a JSON spec, with the policy it generates per
# call. The reason this is checked for real: that policy must still let the sandbox open a
# terminal and run tmux, which no unit test can show. Uses `osh` on PATH, the machine's main
# gateway and the image built above; the sandbox is deleted at the end, also when a check fails.
headless_checks() {
  local tag="$1" hn=osh-headless out rc spec="$HOME/headless-spec.json" hshare="$HOME/headless-share"
  echo "== the headless osh path: apply, list, exec, delete, mcp, share refusal"
  # True when the JSON on stdin has the given top-level key equal to the given JSON value.
  jfield() { python3 -c 'import json,sys; d=json.loads(sys.stdin.read()); sys.exit(0 if d.get(sys.argv[1])==json.loads(sys.argv[2]) else 1)' "$1" "$2"; }
  printf '{"name":"%s","image":"%s"}\n' "$hn" "$tag" >"$spec"

  out="$(timeout 900 osh apply -f "$spec" --gateway "$main" 2>&1)"; rc=$?
  [[ $rc -eq 0 ]] && jfield action '"created"' <<<"$out" \
    && pass "osh apply creates a sandbox from a spec" || fail "osh apply creates a sandbox from a spec" "rc=$rc $out"

  out="$(timeout 120 osh apply -f "$spec" --gateway "$main" 2>&1)"; rc=$?
  [[ $rc -eq 0 ]] && jfield action '"unchanged"' <<<"$out" \
    && pass "osh apply again reports unchanged" || fail "osh apply again reports unchanged" "rc=$rc $out"

  out="$(timeout 120 osh sandbox list --gateway "$main" 2>&1)"
  python3 -c 'import json,sys; sys.exit(0 if any(s.get("name")==sys.argv[1] for s in json.loads(sys.stdin.read())) else 1)' "$hn" <<<"$out" \
    && pass "osh sandbox list shows the sandbox" || fail "osh sandbox list shows the sandbox" "$out"

  out="$(timeout 180 osh sandbox exec "$hn" --gateway "$main" -- sh -c 'python3 -c "import os; os.openpty(); print(\"openpty-ok\")"; tmux -V' 2>&1)"; rc=$?
  [[ $rc -eq 0 ]] && jfield exit_code 0 <<<"$out" && [[ "$out" == *openpty-ok* && "$out" == *tmux* ]] \
    && pass "a sandbox from a spec opens a terminal and runs tmux" || fail "a sandbox from a spec opens a terminal and runs tmux" "rc=$rc $out"

  out="$(timeout 120 osh sandbox exec "$hn" --gateway "$main" -- sh -c 'exit 7' 2>&1)"; rc=$?
  [[ $rc -eq 7 ]] && jfield exit_code 7 <<<"$out" \
    && pass "osh sandbox exec exits with the command's code" || fail "osh sandbox exec exits with the command's code" "rc=$rc $out"

  # MCP over stdio: initialize, then tools/call sandbox_list; one JSON line per request.
  out="$(printf '%s\n%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"osh-check","version":"0"}}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"sandbox_list","arguments":{}}}' \
    | timeout 120 osh mcp --gateway "$main" 2>&1)"
  python3 -c '
import json,sys
l=[json.loads(x) for x in sys.stdin.read().splitlines() if x.strip()]
ok=len(l)==2 and l[0].get("id")==1 and l[1].get("id")==2 and sys.argv[1] in l[1]["result"]["content"][0]["text"]
sys.exit(0 if ok else 1)' "$hn" <<<"$out" \
    && pass "osh mcp answers initialize and a sandbox_list call" || fail "osh mcp answers initialize and a sandbox_list call" "$out"

  jobs_checks "$hn"

  # A share needs --allow-share: refused without it, and nothing is created.
  mkdir -p "$hshare"
  printf '{"name":"%s-share","image":"%s","share":{"path":"%s"}}\n' "$hn" "$tag" "$hshare" >"$HOME/headless-share-spec.json"
  out="$(timeout 120 osh apply -f "$HOME/headless-share-spec.json" --gateway "$main" 2>&1)"; rc=$?
  [[ $rc -eq 1 ]] && jfield ok false <<<"$out" \
    && pass "a spec with a share is refused without --allow-share" || fail "a spec with a share is refused without --allow-share" "rc=$rc $out"
  out="$(timeout 120 osh sandbox list --gateway "$main" 2>&1)"
  [[ "$out" != *"$hn-share"* ]] && pass "the refused share sandbox was not created" || fail "the refused share sandbox was not created" "$out"

  out="$(timeout 300 osh sandbox delete "$hn" --gateway "$main" 2>&1)"; rc=$?
  [[ $rc -eq 0 ]] && jfield deleted true <<<"$out" \
    && pass "osh sandbox delete removes it" || { fail "osh sandbox delete removes it" "rc=$rc $out"; openshell -g "$main" sandbox delete "$hn" >/dev/null 2>&1; }
  # OpenShell deletes in the background: the list shows the sandbox with phase Deleting for a
  # while. Poll every 2 s for up to 60 s; fail with the last list only after the deadline.
  local gone=0 deadline=$((SECONDS + 60))
  while :; do
    out="$(timeout 30 osh sandbox list --gateway "$main" 2>&1)"
    if [[ "$out" != *"\"$hn\""* ]]; then gone=1; break; fi
    ((SECONDS < deadline)) || break
    sleep 2
  done
  [[ $gone -eq 1 ]] && pass "a following list no longer shows it" || fail "a following list no longer shows it" "$out"
  openshell -g "$main" sandbox delete "$hn-share" >/dev/null 2>&1
  rm -rf "$hshare" "$spec" "$HOME/headless-share-spec.json"
}

# `osh config` (tui/core/src/settings.rs): the settings registry and osh.toml, edited from the
# command line on the real machine. Nothing here needs a gateway; it runs against the data
# folder of the user the suite runs as. A comment planted in the file must survive the edits,
# and an environment variable that decides a setting must refuse the edit, naming itself.
settings_checks() {
  local f out rc
  echo "== osh config: every setting, a comment-keeping edit, the environment variable wins"
  f="$(osh config path 2>&1)"
  check "osh config path creates osh.toml" test -f "$f"
  out="$(osh config --json 2>&1)"
  python3 -c 'import json,sys; d=json.loads(sys.stdin.read()); k={r["key"]:r for r in d["settings"]}; sys.exit(0 if len(k)>=14 and k["mcp.allow_share"]["env"]=="OSH_MCP_ALLOW_SHARE" and k["general.editor"]["env"] is None else 1)' <<<"$out" \
    && pass "osh config --json lists every setting with its environment variable" || fail "osh config --json lists every setting with its environment variable" "$out"
  { echo "# planted comment"; cat "$f"; } >"$f.new" && mv "$f.new" "$f"
  out="$(osh config set refresh.ports_secs 7 2>&1)"; rc=$?
  [[ $rc -eq 0 && "$(osh config get refresh.ports_secs)" == 7 ]] \
    && pass "osh config set changes a setting" || fail "osh config set changes a setting" "rc=$rc $out"
  grep -q '^# planted comment$' "$f" && grep -q '^ports_secs = 7$' "$f" \
    && pass "the edit keeps the comments in osh.toml" || fail "the edit keeps the comments in osh.toml" "$(head -20 "$f")"
  out="$(OSH_MCP_ALLOW_SHARE=1 osh config set mcp.allow_share false --json 2>&1)"; rc=$?
  [[ $rc -eq 1 && "$out" == *OSH_MCP_ALLOW_SHARE* ]] \
    && pass "an environment variable that decides a setting refuses the edit" || fail "an environment variable that decides a setting refuses the edit" "rc=$rc $out"
  out="$(osh config set refresh.ports_secs 0 2>&1)"; rc=$?
  [[ $rc -eq 1 ]] && pass "a value out of range is refused" || fail "a value out of range is refused" "rc=$rc $out"
  osh config unset refresh.ports_secs >/dev/null 2>&1
  [[ "$(osh config get refresh.ports_secs)" == 3 ]] && grep -q '^# ports_secs = 3$' "$f" \
    && pass "osh config unset puts the default back" || fail "osh config unset puts the default back" "$(grep ports_secs "$f")"
}
