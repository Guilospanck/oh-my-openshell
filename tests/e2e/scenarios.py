#!/usr/bin/env python3
"""End-to-end scenarios for the osh dashboard. Each @scenario id appears in tests/e2e/actions.txt,
which lists every key and command the dashboard has; a unit test keeps the two in step.
usage: scenarios.py [id ...]   (no ids = all)"""
import os, shutil, subprocess, sys, tempfile, time, traceback
sys.path.insert(0, os.path.dirname(__file__))
from driver import Osh, Timeout
import pyte.graphics

SCENARIOS = {}

def scenario(sid):
    def deco(fn):
        SCENARIOS[sid] = fn
        return fn
    return deco

def sh(cmd, timeout=120):
    r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout)
    return r.returncode, (r.stdout + r.stderr)

GW = os.environ.get("OSH_GATEWAY", "")
TABS = {"1": "PHASE", "2": "CREDENTIAL KEYS", "3": "ON GATEWAY", "4": "DOCKERFILE", "5": "GATEWAY"}

def osc(args, timeout=120):
    return sh(f"openshell {'-g ' + GW if GW else ''} {args}", timeout)

def sandboxes():
    return osc("sandbox list")[1]

def wait_cli(pred, what, timeout=120, every=2):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return
        time.sleep(every)
    raise AssertionError(f"timed out waiting for {what}")

def ready(name):
    return lambda: any(name in l and "Ready" in l for l in sandboxes().splitlines())

def gone(name):
    return lambda: not any(l.split()[:1] == [name] for l in sandboxes().splitlines())

def clean(*names):
    for n in names:
        osc(f"sandbox delete {n}")

def editor_mode(mode):
    """Set osh's editor setting (the palette's `Editor: switch`): "builtin", "external" or "auto".
    Scenarios that type into the built-in editor force it, so an editor on PATH does not open."""
    path = os.path.join(data_dir(), "editor-mode")
    if mode == "auto":
        if os.path.exists(path):
            os.remove(path)
    else:
        with open(path, "w") as f:
            f.write(mode + "\n")

def dashboard(env=None, editor="builtin"):
    editor_mode(editor)
    o = Osh(env=env)
    # After osh is replaced it says once that it found data of an earlier install.
    o.wait(("PHASE", "Press Enter to continue"), 60)
    if "Press Enter to continue" in o.text():
        o.press("enter")
        o.wait("PHASE", 60)
    return o

# osh's accent (ratatui Color::Cyan) as pyte reports it: crossterm sends it as 256-colour index 6
# (38;5;6), which pyte stores as that palette entry's hex; plain SGR 36 would read "cyan".
ACCENT = ("cyan", pyte.graphics.FG_BG_256[6])

def highlighted(o):
    """Text of the row under the cursor. A modal's cursor is drawn in reverse video (the lowest
    such row: the tab bar, row 0, is reversed for the active tab, so it is skipped); with no modal
    it is the table's cursor row, drawn bold in the accent colour (cyan) from its first cell, and
    only that panel's part of the line is returned (the Sandboxes tab has details beside it)."""
    found = ""
    for y in range(1, o.rows):
        row = o.screen.buffer[y]
        if any(row[x].reverse for x in range(o.cols) if x in row):
            found = o.screen.display[y]
    if found:
        return found.strip(" \u2502")
    for y in range(1, o.rows):
        row, line = o.screen.buffer[y], o.screen.display[y]
        x = next((x for x in range(o.cols) if line[x] not in " \u2502"), None)
        if x is not None and row[x].bold and row[x].fg in ACCENT:
            end = line.find("\u2502", x)
            return line[x:end if end >= 0 else None].strip()
    return ""

def choose(o, text, limit=12):
    """Move the cursor down to the row containing `text`, then press Enter."""
    for _ in range(limit):
        if text in highlighted(o):
            break
        o.press("down")
    else:
        raise AssertionError(f"no row {text!r} to choose; screen:\n{o.text()}")
    o.press("enter")

def create_sandbox(o, name, share=None, rw=False):
    """The wizard, the way a person walks it: Custom, the claude-code image, no providers."""
    o.press("n"); o.wait("New sandbox")
    choose(o, "Custom...")
    o.wait("Image"); choose(o, "claude-code:latest")
    o.wait("Providers (space"); o.press("enter")
    o.wait("Toolchain hosts this sandbox"); o.press("enter")
    o.wait("Sandbox name"); o.type(name); o.press("enter")
    o.wait("CPU limit"); o.press("enter")
    o.wait("Memory limit"); o.press("enter")
    o.wait("Share a host folder?")
    if share is None:
        choose(o, "No folder")
    else:
        choose(o, "Other folder...")
        o.wait("Host folder to share"); o.type(share); o.press("enter")
        o.wait("Access to the shared folder")
        choose(o, "Read-write" if rw else "Read-only")
    o.wait("One-shot command"); o.press("enter")
    o.wait("Continue?"); o.press("enter")
    finish_job(o)

def finish_job(o, timeout=900):
    """A job (create, build) runs on the normal screen and waits for Enter before the dashboard returns."""
    o.wait("Press Enter to return to osh", timeout)
    o.clear()
    o.press("enter")
    o.wait("5 Gateways", 30)                      # the tab bar: the dashboard is back on whatever tab

@scenario("nav-tabs")
def nav_tabs():
    with dashboard() as o:
        for k, title in TABS.items():
            o.press(k); o.wait(title, 10)
        o.press("1"); o.wait(TABS["1"], 10)
        o.press("tab"); o.wait(TABS["2"], 10)
        o.press("backtab"); o.wait(TABS["1"], 10)
        o.press("backtab"); o.wait(TABS["5"], 10)

@scenario("nav-move")
def nav_move():
    with dashboard() as o:
        o.press("3"); o.wait("claude-marketplace", 15)
        first = highlighted(o)
        assert first.startswith("claude-code"), first
        o.press("j"); second = highlighted(o)
        assert second.startswith("claude-marketplace"), second
        o.press("k"); assert highlighted(o) == first, "k moves back"
        o.press("down"); assert highlighted(o) == second
        o.press("up"); assert highlighted(o) == first

@scenario("nav-refresh")
def nav_refresh():
    with dashboard() as o:
        o.press("r"); o.wait(("Refreshing", "connected"), 10)
        o.wait("connected", 20)

@scenario("nav-alerts")
def nav_alerts():
    with dashboard() as o:
        o.press("!"); o.wait("Nothing suspicious", 10)
        o.press("esc"); o.wait_gone("Nothing suspicious", 5)

@scenario("nav-help")
def nav_help():
    with dashboard() as o:
        o.press("?"); o.wait("Everywhere on the dashboard", 10)
        o.press("esc"); o.wait_gone("Everywhere on the dashboard", 5)

@scenario("nav-palette")
def nav_palette():
    with dashboard() as o:
        o.press(":"); o.wait("Commands", 10)
        o.type("go to g"); o.wait("Go to Gateways", 5)
        o.press("enter"); o.wait(TABS["5"], 10)
        o.press(":"); o.wait("Commands", 10)
        o.press("esc"); o.wait_gone("Commands", 5)

def data_dir():
    """osh's data directory, found the way osh finds it (assets::data_dir)."""
    if os.environ.get("OSH_DIR"):
        return os.environ["OSH_DIR"]
    xdg = os.environ.get("XDG_DATA_HOME", "")
    if os.path.isabs(xdg):
        return os.path.join(xdg, "osh")
    return os.path.expanduser("~/.local/share/osh")

@scenario("editor-toolchains")
def editor_toolchains():
    """The built-in editor from the palette: a broken file is refused, a fixed one is saved,
    find and go-to-line work, and closing with changes asks first."""
    path = os.path.join(data_dir(), "toolchains.toml")
    with open(path) as f:
        before = f.read()
    def now():
        with open(path) as f:
            return f.read()
    try:
        with dashboard() as o:
            o.press(":"); o.wait("Commands", 10)
            o.type("edit toolchains"); o.wait("Edit toolchains.toml", 5)
            o.press("enter"); o.wait("Ctrl-S save", 10)
            o.press("ctrl-end", "enter")
            o.type("[e2e")
            o.press("ctrl-s"); o.wait("Not saved: TOML parse error", 10)
            assert now() == before, "a broken file is not written"
            o.press("end"); o.type("]"); o.press("enter")
            o.type("enabled = false"); o.press("enter")
            o.type('hosts = ["e2e.example.com"]')
            o.press("ctrl-s"); o.wait("Saved toolchains.toml", 10)
            assert "[e2e]" in now() and "e2e.example.com" in now(), now()
            o.press("ctrl-g"); o.type("1"); o.press("enter"); o.wait("Ln 1, Col 1", 5)
            o.press("ctrl-f"); o.type("e2e.example"); o.wait("Match 1 of 1", 5)
            o.press("enter")
            o.type("x"); o.wait("[modified]", 5)
            o.press("esc"); o.wait("Unsaved changes", 5)
            o.press("d"); o.wait_gone("Ctrl-S save", 10)
            assert "e2e.example.com" in now(), "discarding keeps the saved file"
            o.wait("PHASE", 10)
    finally:
        with open(path, "w") as f:
            f.write(before)

EXT_EDITOR = r"""#!/bin/sh
# A scripted stand-in for the user's editor: the file is the last argument, and each run does
# the next step (counted in a file next to this script).
for f; do :; done
here=$(dirname "$0")
n=$(cat "$here/count" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$here/count"
echo "fake editor run $n on $(basename "$f") args $1"
sleep 2
case $n in
1) printf '\n[e2e-ext\n' >> "$f" ;;
2) grep -v '^\[e2e-ext$' "$f" > "$here/fixed"; cat "$here/fixed" > "$f"
   printf '\n[e2e-ext]\nenabled = false\nhosts = ["e2e-ext.example.com"]\n' >> "$f" ;;
3) : ;;
*) printf 'junk\n' >> "$f"; exit 1 ;;
esac
"""

@scenario("editor-external")
def editor_external():
    """$EDITOR (with an argument) runs inside osh: a broken result is refused and reopened with the
    edits, a fixed one is saved, an unchanged file and a failing editor write nothing, and the
    palette's editor setting forces the built-in editor."""
    path = os.path.join(data_dir(), "toolchains.toml")
    with open(path) as f:
        before = f.read()
    def now():
        with open(path) as f:
            return f.read()
    tmp = tempfile.mkdtemp(prefix="osh-e2e-ed-")
    script = os.path.join(tmp, "fake editor")
    with open(script, "w") as f:
        f.write(EXT_EDITOR)
    os.chmod(script, 0o755)
    env = {"VISUAL": "", "EDITOR": f"'{script}' --e2e"}
    def edit(o):
        o.press(":"); o.wait("Commands", 10)
        o.type("edit toolchains"); o.wait("Edit toolchains.toml", 5)
        o.press("enter")
    try:
        with dashboard(env=env, editor="auto") as o:
            edit(o); o.wait("fake editor run 1 on toolchains.toml args --e2e", 10)
            o.wait("save and quit the editor", 5)
            o.wait("toolchains.toml is not valid: not saved", 15)
            o.wait("TOML parse error", 5)
            assert now() == before, "a broken file is not written"
            o.press("r"); o.wait("fake editor run 2", 10)
            o.wait("Saved toolchains.toml (edited in fake editor)", 15)
            assert "e2e-ext.example.com" in now(), now()
            saved = now()
            edit(o); o.wait("fake editor run 3", 10)
            o.wait("toolchains.toml unchanged: nothing written", 15)
            edit(o); o.wait("fake editor run 4", 10)
            o.wait("Cancelled: fake editor exited with an error", 15)
            assert now() == saved, "a failing editor writes nothing"
            o.press(":"); o.wait("Commands", 10)
            o.type("editor: switch"); o.press("enter"); o.wait("Editor: always yours", 10)
            o.press(":"); o.wait("Commands", 10)
            o.type("editor: switch"); o.press("enter"); o.wait("Editor: always osh's built-in", 10)
            edit(o); o.wait("Ctrl-S save", 10)
            o.wait("built-in editor (chosen in the palette)", 5)
            o.press("esc"); o.wait_gone("Ctrl-S save", 10)
            o.press(":"); o.wait("Commands", 10)
            o.type("editor: switch"); o.press("enter"); o.wait("Editor: auto", 10)
            assert not os.path.exists(os.path.join(data_dir(), "editor-mode"))
    finally:
        with open(path, "w") as f:
            f.write(before)
        editor_mode("builtin")
        shutil.rmtree(tmp, ignore_errors=True)

@scenario("nav-quit")
def nav_quit():
    o = dashboard()
    o.press("q")
    assert o.exit_status() == 0, "q exits 0"

@scenario("nav-gateway")
def nav_gateway():
    with dashboard() as o:
        o.press("g"); o.wait("Gateway", 10)
        o.press("esc")

def row(o, text):
    """The table row that starts with `text` (a confirm box may mention it too). A running
    shell's marker is part of the returned row but not of the match."""
    for l in o.text().splitlines():
        t = l.strip(" \u2502")
        if t.removeprefix("\u25cf").strip().startswith(text):
            return t
    return ""

def move_to(o, text, limit=12):
    for _ in range(limit):
        if text in highlighted(o):
            return
        o.press("down")
    raise AssertionError(f"no row {text!r}; screen:\n{o.text()}")

@scenario("profile-import")
def profile_import():
    osc("provider profile delete npm")
    with dashboard() as o:
        o.press("3"); o.wait("npm", 15)
        move_to(o, "npm")
        assert "not imported" in row(o, "npm"), row(o, "npm")
        o.press("u"); o.wait("Imported profile npm", 60)
        o.wait_gone("npm                 not imported", 30)
        o.press("enter")                  # Enter on an imported profile updates it
        o.wait(("Imported profile npm", "Updated profile npm", "up to date", "unchanged"), 60)
    rc, out = osc("provider profile describe npm")
    assert rc == 0 and "npm" in out, out

@scenario("image-build")
def image_build():
    with dashboard() as o:
        o.press("4"); o.wait("claude-code:latest", 15)
        move_to(o, "claude-code:latest")
        o.press("b")
        finish_job(o, 1500)                       # the build runs on the normal screen
        o.press("4"); o.wait("up to date", 30)
        assert "up to date" in row(o, "claude-code:latest"), row(o, "claude-code:latest")
        o.press("enter")                          # Enter builds too (cached, so quick)
        finish_job(o, 1500)
        o.press("4"); o.wait("up to date", 30)

@scenario("profile-file")
def profile_file():
    """e on a profile lists what can be done with its file (view, compare, restore); n starts a
    new profile in the built-in editor from a template, written on Ctrl-S."""
    path = os.path.join(data_dir(), "providers", "e2e-prof.yaml")
    try:
        with dashboard() as o:
            o.press("3"); o.wait("npm", 15)
            move_to(o, "npm")
            o.press("e"); o.wait("providers/npm.yaml (", 10)
            choose(o, "View"); o.wait("id: npm", 10)
            o.press("esc"); o.wait("providers/npm.yaml (", 10)
            o.press("esc"); o.wait_gone("providers/npm.yaml (", 5)
            o.press("n"); o.wait("Name of the new provider profile", 10)
            o.type("e2e-prof"); o.press("enter"); o.wait("Ctrl-S save", 10)
            assert not os.path.exists(path), "nothing is written before the save"
            o.press("ctrl-s"); o.wait("Saved providers/e2e-prof.yaml", 15)
            assert "id: e2e-prof" in open(path).read()
            o.press("esc"); o.wait("providers/e2e-prof.yaml (your own file)", 10)
            choose(o, "Delete this file"); o.wait("Delete providers/e2e-prof.yaml?", 10)
            o.press("y"); o.wait("Deleted providers/e2e-prof.yaml", 10)
            assert not os.path.exists(path)
    finally:
        if os.path.exists(path):
            os.remove(path)

@scenario("image-file")
def image_file():
    """e on an image opens its Dockerfile's actions; n starts a new Dockerfile from pi's."""
    path = os.path.join(data_dir(), "images", "Dockerfile-e2e")
    try:
        with dashboard() as o:
            o.press("4"); o.wait("claude-code:latest", 15)
            move_to(o, "claude-code:latest")
            o.press("e"); o.wait("images/Dockerfile-claude_code (", 10)
            choose(o, "View"); o.wait("FROM", 10)
            o.press("esc"); o.press("esc")
            o.press("n"); o.wait("Name of the new image", 10)
            o.type("e2e"); o.press("enter"); o.wait("Ctrl-S save", 10)
            o.press("ctrl-s"); o.wait("Saved images/Dockerfile-e2e", 15)
            assert "FROM" in open(path).read()
            o.press("esc"); o.wait("images/Dockerfile-e2e (your own file)", 10)
            o.press("esc")
            o.press("r"); o.wait("e2e:latest", 30)        # the Images tab lists the new image
    finally:
        if os.path.exists(path):
            os.remove(path)

@scenario("data-files")
def data_files():
    """The palette's Data files: an edited policy shows as edited, compares against the shipped
    default and is restored from inside osh (the edit is kept as .osh-bak)."""
    path = os.path.join(data_dir(), "sandbox-policy.yaml")
    with open(path) as f:
        before = f.read()
    try:
        with open(path, "a") as f:
            f.write("# e2e edit\n")
        with dashboard() as o:
            o.press(":"); o.wait("Commands", 10)
            o.type("data files"); o.wait("Data files:", 5); o.press("enter")
            o.wait("Data files in", 10)
            assert "sandbox-policy.yaml" in o.text() and "edited by you" in o.text(), o.text()
            choose(o, "sandbox-policy.yaml", 25); o.wait("sandbox-policy.yaml (edited by you", 10)
            choose(o, "Compare with the shipped default"); o.wait("+ # e2e edit", 10)
            o.press("esc"); o.wait("sandbox-policy.yaml (edited by you", 10)
            choose(o, "Restore the shipped default"); o.wait("Put the shipped default", 10)
            o.press("y"); o.wait("Restored the shipped sandbox-policy.yaml", 10)
        assert "# e2e edit" not in open(path).read()
        assert "# e2e edit" in open(path + ".osh-bak").read()
    finally:
        with open(path, "w") as f:
            f.write(before)
        if os.path.exists(path + ".osh-bak"):
            os.remove(path + ".osh-bak")

def providers_out():
    return osc("provider list")[1]

@scenario("provider-create")
def provider_create():
    osc("provider delete forgejo"); osc("provider delete npm")
    secret = "e2e-secret-token-xyz"
    with dashboard() as o:
        o.press("2"); o.wait("Providers", 10)
        o.press("n"); o.wait("Create provider from profile", 10)
        choose(o, "npm")                       # a profile with no credentials: created at once
        o.wait("Created provider npm", 30)
        o.press("n"); o.wait("Create provider from profile", 10)
        choose(o, "forgejo")
        o.wait("FORGEJO_HOST"); o.type("git.example.test"); o.press("enter")
        o.wait("FORGEJO_TOKEN (hidden)"); o.type(secret)
        assert secret not in o.text(), "a hidden prompt must not echo the token"
        o.press("enter"); o.wait("Created provider forgejo", 30)
        assert secret not in o.text()
    out = providers_out()
    assert "npm" in out and "forgejo" in out, out
    # Checked after the fact, so only a lingering process could show it; the unit tests cover argv.
    rc, ps = sh("ps -axo args")
    assert secret not in ps, "the token must not be in any command line"

@scenario("provider-update")
def provider_update():
    with dashboard() as o:
        o.press("2"); o.wait("forgejo", 15)
        move_to(o, "npm")
        o.press("u"); o.wait("npm stores no credentials", 10)
        o.press("esc"); o.settle(0.5)
        move_to(o, "forgejo")
        o.press("u"); o.wait("FORGEJO_TOKEN", 10)
        o.type("e2e-second-token"); o.press("enter")
        o.wait(("Updated", "updated"), 30)

@scenario("provider-delete")
def provider_delete():
    with dashboard() as o:
        o.press("2"); o.wait("forgejo", 15)
        move_to(o, "forgejo")
        o.press("d"); o.wait("Delete provider 'forgejo'?", 10)
        o.press("n"); o.wait_gone("Delete provider", 5)
        assert "forgejo" in providers_out(), "n keeps it"
        o.press("d"); o.wait("Delete provider 'forgejo'?", 10)
        o.press("y"); o.wait("Deleted provider forgejo", 30)
        move_to(o, "npm")
        o.press("d"); o.wait("Delete provider 'npm'?", 10)
        o.press("y"); o.wait("Deleted provider npm", 30)
    assert "forgejo" not in providers_out() and "npm" not in providers_out()

SB = "e2e-sb"
DATA = os.path.expanduser("~/.local/share/osh")

def ensure_npm_profile():
    osc(f"provider profile import --file {DATA}/providers/npm.yaml")

def ensure_sandbox(name=SB):
    """A ready sandbox made through the CLI, as the wizard would (same image and policy)."""
    if ready(name)():
        return
    if name in sandboxes():
        osc(f"sandbox start {name}")
        wait_cli(ready(name), f"{name} Ready", 300)
        return
    rc, out = osc(f"sandbox create --name {name} --from claude-code:latest --policy {DATA}/sandbox-policy.yaml --detach", 900)
    assert rc == 0, out
    wait_cli(ready(name), f"{name} Ready", 300)

@scenario("sandbox-create")
def sandbox_create():
    clean(SB); wait_cli(gone(SB), "old sandbox gone", 120)
    with dashboard() as o:
        create_sandbox(o, SB)
        wait_cli(ready(SB), "the new sandbox Ready", 300)
        o.press("r"); o.wait("Ready", 60)
        assert "Ready" in row(o, SB), o.text()
    rc, out = osc(f"sandbox exec -n {SB} -- sh -c 'python3 -c \"import os; os.openpty(); print(1)\"'")
    assert out.strip().endswith("1"), f"created from osh's policy, so a terminal opens: {out}"

def select_sandbox(o, name=SB):
    o.press("1"); o.wait(name, 30)
    move_to(o, name)

@scenario("sandbox-shell")
def sandbox_shell():
    ensure_sandbox()
    with dashboard() as o:
        select_sandbox(o)
        o.press("enter"); o.wait("node@", 60)
        o.wait("[osh]", 30)                       # the tmux status line: the shell runs in tmux
        o.type('echo hello-$((40+2)); [ -n "$TMUX" ] && echo IN-TMUX'); o.press("enter")
        o.wait("IN-TMUX", 15); assert "hello-42" in o.text()
        o.press("ctrl-]"); o.wait("Shell in e2e-sb", 10)
        choose(o, "Close the session")
        o.wait("PHASE", 15)
        select_sandbox(o)
        o.press("enter"); o.wait("hello-42", 60)   # tmux -A reattached to the same session
        o.press("ctrl-]"); o.wait("Shell in e2e-sb", 10); choose(o, "Close the session")
        o.wait("PHASE", 15)

@scenario("shell-menu")
def shell_menu():
    ensure_sandbox()
    with dashboard() as o:
        select_sandbox(o)
        o.press("enter"); o.wait("node@", 60)
        def menu():
            o.press("ctrl-]"); o.wait("Shell in e2e-sb", 10)
        menu(); o.press("esc"); o.wait_gone("Shell in e2e-sb", 5)          # Esc is Back
        menu(); choose(o, "Back to the session"); o.wait_gone("Shell in e2e-sb", 5)
        menu(); choose(o, "Providers and their permissions")
        o.wait(("Providers", "permissions", "no providers", "No providers"), 15); o.press("esc")
        menu(); choose(o, "Blocked requests")
        o.wait(("blocked", "Blocked", "No blocked"), 15); o.press("esc")
        menu(); choose(o, "Alerts (0)"); o.wait("Nothing suspicious", 10); o.press("esc")
        menu(); choose(o, "Dashboard (keep the session running)")
        o.wait("PHASE", 15)
        # A running shell is marked after the name (the phase has a dot of its own).
        assert row(o, SB).startswith(SB + " \u25b8"), f"a running shell is marked: {row(o, SB)!r}"
        o.press("enter"); o.wait("node@", 30)                  # back into the kept shell
        menu(); choose(o, "Close the session"); o.wait("PHASE", 15)
        assert "\u25b8" not in row(o, SB), row(o, SB)

@scenario("sandbox-logs")
def sandbox_logs():
    ensure_sandbox()
    with dashboard() as o:
        select_sandbox(o)
        o.press("l"); o.settle(2)
        assert "PHASE" not in o.text().splitlines()[0], "the log view takes the screen"
        o.press("q"); o.wait("PHASE", 10)

@scenario("sandbox-blocked")
def sandbox_blocked():
    ensure_sandbox()
    with dashboard() as o:
        select_sandbox(o)
        o.press("p"); o.wait("Blocked in e2e-sb", 20)
        o.wait("Nothing blocked yet", 10)
        o.press("esc"); o.wait_gone("Blocked in e2e-sb", 5)

@scenario("sandbox-stop-start")
def sandbox_stop_start():
    ensure_sandbox()
    with dashboard() as o:
        select_sandbox(o)
        o.press("s")                               # no question: it stops at once
        o.wait(("Stopping", "Stopped"), 30)
        wait_cli(lambda: SB in sandboxes() and not ready(SB)(), "stopped", 180)
        o.press("S")
        wait_cli(ready(SB), "started again", 300)
        o.press("r"); o.wait_gone("Stopped", 60)
        o.wait("Ready", 60)

@scenario("sandbox-providers")
def sandbox_providers():
    ensure_sandbox()
    ensure_npm_profile()
    osc("provider delete e2e-npm")
    rc, out = osc("provider create --name e2e-npm --type npm")
    assert rc == 0, out
    with dashboard() as o:
        select_sandbox(o)
        o.press("a"); o.wait("e2e-npm", 15); choose(o, "e2e-npm")
        wait_cli(lambda: "e2e-npm" in osc(f"sandbox provider list {SB}")[1], "attached", 60)
        o.press("x"); o.wait("e2e-npm", 15); choose(o, "e2e-npm")
        wait_cli(lambda: "e2e-npm" not in osc(f"sandbox provider list {SB}")[1], "detached", 60)
    osc("provider delete e2e-npm")

@scenario("sandbox-skills")
def sandbox_skills():
    ensure_sandbox()
    with dashboard() as o:
        select_sandbox(o)
        o.press("K"); o.wait("Add skills and plugins for", 20)
        assert "claude-code" in o.text() and "pi" in o.text() and "both" in o.text()
        o.press("esc"); o.wait_gone("Add skills and plugins for", 5)

@scenario("sandbox-transfer")
def sandbox_transfer():
    ensure_sandbox()
    other = SB + "-xfer"
    clean(other); wait_cli(gone(other), "old target gone", 120)
    rc, out = osc(f"sandbox exec -n {SB} -- sh -c 'echo moved-$((40+2)) > \"$HOME/transfer-probe.txt\"'")
    assert rc == 0, out
    with dashboard() as o:
        create_sandbox(o, other)
        wait_cli(ready(other), "the target Ready", 300)
        o.press("r"); o.wait("Ready", 60)
        select_sandbox(o)
        o.press("T"); o.wait("Transfer the state of", 20)
        choose(o, other)
        o.wait("overwritten", 10); o.press("y")
        screen = o.wait(("Transferred", "Error:"), 600)
        assert "Transferred" in screen, screen
    rc, out = osc(f"sandbox exec -n {other} -- sh -c 'cat \"$HOME/transfer-probe.txt\"'")
    assert "moved-42" in out, out
    clean(other)

@scenario("sandbox-delete")
def sandbox_delete():
    ensure_sandbox()
    with dashboard() as o:
        select_sandbox(o)
        o.press("d"); o.wait("Delete sandbox", 10); o.press("n"); o.wait_gone("Delete sandbox", 5)
        assert ready(SB)(), "n keeps the sandbox"
        o.press("d"); o.wait("Delete sandbox", 10); o.press("y")
        wait_cli(gone(SB), "deleted", 180)

@scenario("alerts-toast")
def alerts_toast():
    share = os.path.expanduser("~/e2e-share")
    name = "e2e-share-sb"
    sh(f"rm -rf {share}; mkdir -p {share}")
    clean(name); wait_cli(gone(name), "old sandbox gone", 120)
    with dashboard() as o:
        # A risky folder is flagged in the access question, and as an alert.
        o.press("n"); o.wait("New sandbox"); choose(o, "Custom..."); o.wait("Image")
        choose(o, "claude-code:latest"); o.wait("Providers (space"); o.press("enter")
        o.wait("Toolchain hosts this sandbox"); o.press("enter")
        for _ in range(3):
            o.wait(("Sandbox name", "CPU limit", "Memory limit")); o.press("enter")
        o.wait("Share a host folder?"); choose(o, "Other folder...")
        o.wait("Host folder to share"); o.type(os.path.expanduser("~")); o.press("enter")
        o.wait("RISKY", 10)
        assert "whole disk" not in o.text() and "home folder" in o.text(), o.text()
        lines = o.text().splitlines()
        box = next(i for i, l in enumerate(lines) if "\u256d Alert " in l)
        assert box <= 3, "the alert is at the top"
        assert any("Esc dismiss \u00b7 ! list" in l for l in lines[box:]), "the toast says how to close it"
        o.press("esc"); o.settle(0.5)
        o.press("esc"); o.settle(0.5)
        # Now a real read-write share of a project folder.
        create_sandbox(o, name, share=share, rw=True)
        wait_cli(ready(name), f"{name} Ready", 300)
        o.press("r"); o.settle(8)                 # the watcher takes its baseline within a few seconds
        open(f"{share}/.envrc", "w").write("export X=1\n")
        o.wait(".envrc appeared in e2e-share", 30)
        text = o.text().splitlines()
        hit = next(i for i, l in enumerate(text) if ".envrc appeared" in l)
        assert hit <= 3 and text[hit].index(".envrc") > 60, f"top-right, got row {hit}: {text[hit]!r}"
        o.settle(6)                                # it goes away by itself after 5 s
        assert ".envrc appeared" not in o.text(), "the alert timed out"
        os.makedirs(f"{share}/.git/hooks", exist_ok=True)
        open(f"{share}/.git/hooks/pre-commit", "w").write("#!/bin/sh\ntrue\n")
        o.wait("pre-commit appeared in e2e-share", 30)
        o.press("esc"); o.wait_gone("pre-commit appeared", 3)       # Esc dismisses it at once
        # Alerts about shared files list each file to read, then "Read all" shows every alert.
        o.press("!"); o.wait("Alerts (", 5)
        t = o.text()
        assert "View " + f"{share}/.git/hooks/pre-commit" in t and f"View {share}/.envrc" in t, t
        choose(o, "Read all"); o.wait("[HIGH]", 5)
        t = o.text()
        assert ".git/hooks/pre-commit" in t and ".envrc" in t, t
        o.press("esc"); o.wait("Read all", 5)                       # Esc goes back to the list
        o.press("esc"); o.wait_gone("Alerts (", 5)
        assert not any("\u256d Alert " in l or "Esc dismiss \u00b7" in l for l in o.text().splitlines()), \
            "closing the list leaves nothing behind"
    clean(name); sh(f"rm -rf {share}")

@scenario("cmd-doctor")
def cmd_doctor():
    rc, out = sh("osh doctor")
    assert rc == 0, out
    assert "openshell" in out.lower(), out

@scenario("cmd-sync-agents")
def cmd_sync_agents():
    rc, out = sh("osh sync-agents --dry-run")
    assert rc == 0, out
    rc, out = sh("osh sync-agents --json")
    assert rc == 0 and out.lstrip().startswith("{"), out
    import json
    json.loads(out)
    rc, out = sh("osh sync-agents --bogus")
    assert rc == 2 and "usage" in out.lower(), "a bad flag prints the usage and exits 2: " + out

def gateway_rows(o):
    return [l.strip(" \u2502") for l in o.text().splitlines()
            if l.lstrip(" \u2502").startswith("openshell-")]

@scenario("gateway-select")
def gateway_select():
    with dashboard() as o:
        o.press("5"); o.wait("GATEWAY", 10)
        rows = gateway_rows(o)
        assert rows and "selected" in rows[0], rows
        if len(rows) == 1:
            o.press("enter"); o.settle(1)             # the one gateway is already selected
            assert "selected" in gateway_rows(o)[0]
            return
        other = next(r for r in rows if "selected" not in r).split()[0]
        move_to(o, other); o.press("enter")
        o.wait(f"{other} [", 30)                      # the header names the gateway now in use
        o.press("g"); o.wait("Gateway", 10)
        move_to(o, rows[0].split()[0]); o.press("enter")
        o.wait(f"{rows[0].split()[0]} [", 30)

@scenario("gateway-default")
def gateway_default():
    """`s` makes the highlighted gateway the OpenShell CLI default and marks its row."""
    with dashboard() as o:
        o.press("5"); o.wait("GATEWAY", 10)
        rows = gateway_rows(o)
        assert sum("default" in r for r in rows) == 1, rows
        if len(rows) == 1:
            o.press("s"); o.wait("is already the default gateway", 10)
            return
        other = next(r for r in rows if "default" not in r).split()[0]
        move_to(o, other); o.press("s")
        o.wait("is now the default gateway", 30)
        now = [r for r in gateway_rows(o) if "default" in r]
        assert len(now) == 1 and now[0].startswith(other), gateway_rows(o)
        sh(f"openshell gateway select {rows[0].split()[0]}")   # leave the machine as found

@scenario("gateway-remove")
def gateway_remove():
    with dashboard() as o:
        o.press("5"); o.wait("GATEWAY", 10)
        rows = gateway_rows(o)
        extra = next((r for r in rows if " main" not in r), None)
        if extra is None:                             # only the main gateway: it is kept
            move_to(o, rows[0].split()[0])
            o.press("x"); o.wait("Remove gateway", 10)
            o.press("n"); o.wait_gone("Remove gateway", 5)
            assert gateway_rows(o)
            return
        target = extra.split()[0]
        move_to(o, target)
        o.press("x"); o.wait(f"Remove gateway {target}?", 10)
        o.press("n"); o.wait_gone("Remove gateway", 5)
        assert any(target in r for r in gateway_rows(o)), "n keeps it"
        o.press("x"); o.wait(f"Remove gateway {target}?", 10); o.press("y")
        finish_job(o, 300)
        o.press("5"); o.wait("GATEWAY", 10)
        assert not any(target in r for r in gateway_rows(o)), gateway_rows(o)
        assert target not in sh("openshell gateway list")[1], "the OpenShell CLI forgot it too"
        main = rows[0].split()[0]
        assert "connected" in o.text(), f"{main} still answers"

@scenario("gateway-add")
def gateway_add():
    """Opens the wizard; where a type is ready to add (the docker machine, after the removal
    above), adds it for real and waits for it to answer."""
    with dashboard() as o:
        o.press("5"); o.wait("GATEWAY", 10)
        before = len(gateway_rows(o))
        o.press("a"); o.wait("Pick the gateways to add.", 20)
        t = o.text()
        assert "Docker" in t and "Podman" in t and "VM" in t, t
        assert "added" in t, "a type that exists is marked"
        ready_type = next((n for n in ("Docker", "Podman")
                           if any(l.count(n) and "[ ]" in l and "ready" in l for l in t.splitlines())), None)
        if ready_type is None:
            o.press("esc"); o.wait("GATEWAY", 10)
            return
        for _ in range(4):
            if f"\u250c {ready_type} " in o.text():      # the detail pane is titled with the cursor's type
                break
            o.press("down")
        o.press("space")
        o.press("enter"); o.wait("I will run", 20)
        o.press("enter"); o.wait("All set.", 600)
        o.press("enter"); o.wait("GATEWAY", 30)
        assert len(gateway_rows(o)) == before + 1, gateway_rows(o)
        wait_cli(lambda: "Connected" in sh(f"openshell -g openshell-{ready_type.lower()} status")[1], "it answers", 180)

@scenario("gateway-wipe")
def gateway_wipe():
    """Last: wipes the main gateway (sandboxes, providers, profiles), which then restarts empty."""
    osc(f"provider profile import --file {DATA}/providers/npm.yaml")
    osc("provider delete e2e-npm")  # an earlier scenario may have left it
    rc, out = osc("provider create --name e2e-npm --type npm")
    assert rc == 0, out
    with dashboard() as o:
        o.press("5"); o.wait("GATEWAY", 10)
        move_to(o, gateway_rows(o)[0].split()[0])
        o.press("d"); o.wait("WIPE gateway", 10)
        o.press("n"); o.wait_gone("WIPE gateway", 5)
        assert "e2e-npm" in providers_out(), "n wipes nothing"
        o.press("d"); o.wait("WIPE gateway", 10); o.press("y")
        finish_job(o, 600)
        wait_cli(lambda: "e2e-npm" not in providers_out(), "providers wiped", 300)
        o.wait("connected", 120)

if __name__ == "__main__":
    want = sys.argv[1:] or list(SCENARIOS)
    bad = 0
    for sid in want:
        t0 = time.time()
        try:
            SCENARIOS[sid]()
            print(f"ok   {sid} ({time.time() - t0:.0f}s)")
        except Exception as e:
            bad += 1
            print(f"FAIL {sid}: {type(e).__name__}: {e}")
            if not isinstance(e, (Timeout, AssertionError)):
                traceback.print_exc()
    sys.exit(1 if bad else 0)
