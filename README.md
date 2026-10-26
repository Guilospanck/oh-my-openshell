# oh-my-openshell

Portable [OpenShell](https://github.com/NVIDIA/OpenShell) config plus `osh`, a
terminal dashboard for managing gateways, sandboxes, providers, profiles and
images.

Also consumed as a submodule by
[dotfiles](https://github.com/Guilospanck/dotfiles) at `openshell/`. Change it
here, not in the dotfiles checkout.

## osh

```sh
just --justfile .justfile link   # once: symlinks osh into ~/.local/bin
osh                              # then run it from anywhere
```

`just install` also links it. Without linking, run `./osh` from the checkout
(or `~/.config/openshell/osh` when linked by dotfiles). The link points at the
checkout, so edits and rebuilds apply with no re-link. Remove it with
`just unlink`.

Builds the ratatui TUI (`tui/`) with cargo on first run and whenever its
sources change. Offers to install OpenShell if missing. Needs `cargo`; Docker
for image builds.

The embedded sandbox shell uses libghostty-vt (Ghostty's terminal engine),
which builds only with Zig 0.15 (`brew install zig@0.15`). `osh` puts
`zig@0.15` on PATH for the build; building by hand needs
`PATH="$(brew --prefix zig@0.15)/bin:$PATH" LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast cargo build --release`.

| Tab | What you can do |
| --- | --- |
| Sandboxes | create (`n`), open a shell inside osh (`Enter`; `Ctrl-]` for the menu, `●` marks a running one), delete (`d`), stop/start (`s`/`S`), attach/detach providers (`a`/`x`), live logs (`l`), blocked requests (`p`), renew a GitHub repo token (`t`) |
| Providers | create from a `providers/*.yaml` profile or a read/write GitHub repo token (`n`), update credentials (`u`; renews a repo token), delete (`d`) |
| Profiles | import or refresh a profile on the gateway (`u`) |
| Images | build `images/Dockerfile-*` (`b`); shows when a build is outdated |
| Gateways | switch (`g` / `Enter`), wipe and restart empty (`d`) |

Credentials are read from the environment when exported, else prompted hidden;
they never go through argv. The footer shows the keys for the current tab.

### Embedded shells

`Enter` on a sandbox opens `bash` in it, inside osh. Every key goes to the
shell; `Ctrl-]` opens a menu: back to the session, the providers attached to the sandbox and what
each allows (credentials, hosts and access level, binaries; scroll with `j`/`k`), dashboard (the shell keeps
running, marked `●`; `Enter` on it returns), or close it. Shells keep their
size when the window resizes, focused or not. `Shift-PgUp`/`PgDn` and the
mouse wheel scroll back on the main screen; programs that ask for the mouse
(vim, `less --mouse`) get it.

Under tmux, `Shift-Enter` reaches the shell when tmux has
`set -g extended-keys on` (or `always`) and `set -g extended-keys-format csi-u`:
tmux does not answer the kitty keyboard query, so while a shell has focus osh
asks tmux for modifyOtherKeys instead. The `xterm` format is not used (osh's
input library cannot read it); there `Shift-Enter` arrives as `Enter`.

`Alt` works as Meta (sent as an `Esc` prefix: `Alt-b`/`Alt-f` move by word in
bash). On macOS the host terminal must send it, e.g. "Use Option as Meta key"
in Terminal.app or "Esc+" for Option in iTerm2; that setting is outside osh.

### Blocked requests

While a sandbox has a shell open, osh follows its logs. When the network
policy blocks a program from reaching a host, a prompt opens (over the shell,
or the dashboard): `curl was blocked from reaching example.com:443`, with
allow read-only (GET/HEAD/OPTIONS), allow all methods, ignore this time,
ignore until the session ends, or never allow that `host:port` in this sandbox
(any program; saved in `~/.local/state/osh/never`, and `p` then Enter asks
again). When a profile in `providers/` (no credential needed) declares the
blocked host, the first choice is `Attach the <profile> provider (recommended)`:
the profile carries details a plain allow cannot, such as `npm`'s encoded
slashes for scoped packages. For other hosts, a request rejected for an encoded
`/` (`@scope%2fname`) opens a prompt to allow encoded slashes: osh replaces the
ad-hoc rule with a generated `slash-<host>` provider for that host and program.
Requests cut off by a policy reload (`policy generation is stale`) are
not shown. Allowing adds exactly that `host:port` for
exactly that binary to the running sandbox (`openshell policy update
--add-endpoint host:port:read-only:rest:enforce` or `host:port:full`); the
result shows in the status bar. Denials that a host rule cannot fix (an L7
rule rejecting the request) show a notice with the reason instead. `p` on the
Sandboxes tab lists what was seen in the selected sandbox; `Enter` on an entry
asks again.

### Selection and copy

Drag with the left button to select text in a shell (hold `Shift` when the
program uses the mouse). Releasing copies it through the terminal (OSC 52;
under tmux that needs `set -g set-clipboard on`) and `pbcopy` on macOS, and
the status bar shows `copied N chars`. The selection stays on its text while
you scroll (wheel, `Shift-PgUp`) or new output arrives, so it can span more
than one screen; text dropped from the scrollback drops it. `Esc`, typing or a
click clears it.

### Clipboard between host and sandbox

**Sandbox to host.** When a program in the focused shell copies text (OSC 52,
as Claude Code does), osh puts it on the host clipboard the same way a drag
does and shows `sandbox copied N chars`. Only plain text up to 1 MiB from the
shell in front is taken; clipboard reads and background shells are ignored.
A program in the sandbox can overwrite your clipboard this way, so check what
you paste into a host terminal.

**Host to sandbox (images).** `Ctrl-V` in a shell with an image on the host
clipboard (a screenshot, say) uploads it as a PNG to `~/.osh-paste/` in the
sandbox and types its path, which Claude Code and other agents that take image
paths attach as an image. With no image on the clipboard, `Ctrl-V` goes to the
shell as usual. The clipboard is read only when you press it, images over
20 MiB are refused, the host copy lives in a private temp directory only for
the upload, and the sandbox copies are deleted when the last shell for that
sandbox closes. The host needs `pngpaste` (macOS; `just install` installs it),
`wl-paste` (Wayland) or `xclip` (X11).

## Gateways

- **Linux**: one gateway, `openshell`, the packaged `openshell-gateway`
  systemd user service on the Docker driver (honours CPU/memory limits).
  `just install` runs the official installer (deb/rpm; needs Docker Engine and
  `sudo`) and copies `gateway-linux.toml` to `~/.config/openshell/gateway.toml`.
  Logs: `journalctl --user -u openshell-gateway`. The two gateways below are
  macOS only.
- **vm** (default on macOS, Homebrew service, libkrun): fixed size per VM.
- **docker** (`openshell-docker`, Colima): honours per-sandbox CPU/memory
  limits, weaker isolation. Needs the Homebrew gateway run once for shared TLS.

Config: `gateway.toml`, `gateway-docker.toml`. Providers and profiles are
per gateway.

## Claude Code provider

1. `claude setup-token` on the host and export `CLAUDE_CODE_OAUTH_TOKEN`.
2. In `osh`, Providers, `n`, pick `claude-code`.
3. Create a sandbox with that provider and attach.

The real token stays outside the sandbox; only `api.anthropic.com` is
allowed. Tokens expire and are not refreshed: generate a new one and use `u`
in Providers.

## Skills and plugins in a sandbox

In `osh`, Sandboxes, select a sandbox and press `K`: pick the agent
(`claude-code`, `pi` or `both`), mark the items, and confirm. The TUI checks
the sandbox's network policy first and lists any missing access. Items are
reinstalled from their sources (`npx skills`, `claude plugin install`,
`pi install`, graphify from PyPI); only your own skills and `CLAUDE.md` are
uploaded, after a secret scan (findings block the upload). The OpenShell skills
and the language-server plugins are left out. Versions are pinned: the `skills`
CLI to `SKILLS_CLI` in `sync-agents`, graphify to the host's version, and
superpowers for pi to the commit installed on the host.

If the sandbox's policy blocks an install, `K` offers to create (if needed) and attach the anonymous profiles that grant the access, then checks again. The profiles are: `npm`, `pypi`
(graphify) and `claude-marketplace` (read-only github.com for plugins, `npx
skills` and `pi install`). You can also create them by hand with `just provider-npm`,
`just provider-pypi` and `just provider-claude-marketplace`. Preview the list
with `just sync-agents --dry-run`.

When access is missing, `K` offers either the built-in profiles above or a
generated one: `sync-agents --gen-profile` writes a profile that grants only
the hosts and programs still missing (no credential), shows you a summary,
asks for a name (default `install-<sandbox>`), then imports it, creates the
provider and attaches it. After the installs it offers to detach and delete
it, since the network is only needed while installing. This also avoids the
attach conflicts between built-in profiles; if an attach still fails, it is
removed again instead of left on the gateway.

Profiles that need a secret (for example `github`, for private skill sources)
are offered the same way. For a GitHub token, `osh` offers to read it from
`gh auth token` on the host (never shown, never on a command line), or you can
paste one (hidden; use a fine-grained, read-only token from
github.com/settings/personal-access-tokens/new). Tokens exported in the shell
that started `osh` are used without asking. As with Docker Sandboxes, the
real secret stays on the host. The gateway refuses to attach a provider whose
endpoints it cannot merge with those already attached: a credentialed endpoint
(`github` injects its token on `github.com` and `api.github.com`) next to a
profile that allows the same host uninspected, or two inspected (`rest`)
entries for one host that differ in `enforcement` or `allow_encoded_slash`.
`pypi` therefore gives `github.com` and `api.github.com` the same inspected
shape as `github`, so the two attach together (re-import `pypi` on gateways
that have the old one). When an attach is refused during `K`, osh lists the
attached providers and offers to detach one and try again.

The installs run in your terminal and stop at the first failing step. osh then
says which step failed and shows the end of its output (recorded with
`script`, so the steps keep a real terminal). When that output reads like
GitHub refusing access (a private repository: `Repository not found`,
`could not read Username`, `Authentication failed`), it offers to create or
attach the `github` provider and run the installs again from that step. The
policy check cannot predict this: it is not a network block.

### GitHub read/write access to a repo

When a new sandbox shares a folder (`n`, "Share a host folder?") whose `origin`
is a GitHub repository, the wizard asks about GitHub access to it: none,
read-only (the `github` provider), an existing repo provider, or read and
write with a new fine-grained token. For read/write osh shows the steps and
opens GitHub's token page with the owner, a 30 day expiry and the permissions
(Contents and Pull requests: Read and write) filled in. GitHub cannot
preselect the repository, so the steps tell you to choose "Only select
repositories" and pick it (add Workflows only to push `.github/workflows`; an
organization may have to approve the token first). Paste the token (hidden);
osh checks it can see the repo before using it.

osh then imports a profile `gh-<owner>-<repo>`: the `github` profile's read
access plus `POST .../<owner>/<repo>.git/git-receive-pack`, so pushes reach
that repo only. The token lives in a provider created from it: `github-<sandbox>-<repo>`
for one sandbox, or `github-<repo>` when you pick "shared by sandboxes" (also
`n` on the Providers tab, "GitHub repo, read/write"; attach it to other
sandboxes with `a` or pick it in the wizard). A sandbox's own provider is deleted
with the sandbox; a shared one stays until you delete it.

Tokens expire after 30 days. The Providers tab shows the repo and time left
(yellow in the last 7 days), and a yellow banner under the tabs names the
soonest token to expire (or already expired) on every tab. Renew with `u` on
the provider, or `t` on a sandbox: osh shows the steps again and swaps the new
token in, then offers to restart the running sandboxes that have the provider
attached (stop and start: running programs end and `/tmp` is cleared, files in
`/sandbox` are kept), since programs already running keep the old token. The expiry is
kept in `~/.local/state/osh/tokens`; the token itself never touches argv or
that file. Shells opened by osh set `GIT_CONFIG_*` variables: `git@github.com:`
remotes use HTTPS, because the provider only reaches `github.com:443`, and a
credential helper gives git the provider's token as `x-access-token:<token>`
(GitHub accepts Basic auth for git, not Bearer; the gateway swaps the real
secret in). Without the helper git sends no credentials and fails with `could
not read Username`. The repo's `.git/config` on the host is not changed, and
programs that did not start in an osh shell (for example another `exec`) do not
get this setup.

## Development and agents

`AGENTS.md` (also `CLAUDE.md`, a symlink) tells coding agents how to build,
test and change the repo. For humans: `just doctor` checks the prerequisites,
`just check` runs format, clippy and tests (needs only `cargo` and Zig 0.15),
and a bare `just` lists the recipes. Both build on macOS and Linux (Zig 0.15 on
PATH, or `brew install zig@0.15` with Homebrew or Linuxbrew). The Linux
install, uninstall and gateway-reset paths follow OpenShell's packaged systemd
service. `just test-linux` checks them in Docker against stubbed system tools
(and runs the real build and tests on Ubuntu); they have not run on a real
Linux host.

## Setup without osh

`just install` (OpenShell, and links `osh` into `~/.local/bin` so it runs from
anywhere), `just link` / `just unlink` (only the `osh` link),
`just gateway-docker` (second gateway), `just install-hooks` (below),
`just uninstall` (removes OpenShell, both gateway services,
`~/.config/openshell`, state and runtime dirs, and the `osh` link; asks for
confirmation first). Run recipes with `just --justfile .justfile <recipe>`.

## Secret and PII hook

`just install-hooks` (once per clone) enables a pre-commit hook that scans the
whole staged snapshot with [Gitleaks](https://github.com/gitleaks/gitleaks)
(`brew install gitleaks`) and refuses `.env` files. Rules live in
`.gitleaks.toml`. A missing scanner or scan error blocks the commit.
