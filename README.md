# oh-my-openshell

Portable [OpenShell](https://github.com/NVIDIA/OpenShell) config plus `osh`, a
terminal dashboard for managing gateways, sandboxes, providers, profiles and
images.

Also consumed as a submodule by
[dotfiles](https://github.com/Guilospanck/dotfiles) at `openshell/`. Change it
here, not in the dotfiles checkout.

## osh

```sh
osh            # the dashboard; the first time, a guided screen sets up your gateways
osh doctor     # read-only: what is installed, what is missing
osh setup      # the guided screen again (needs a terminal); or, without one:
osh setup --gateways docker,podman --yes   # the first listed is the main gateway; --dry-run only prints the steps
osh gateway add vm|docker|podman [--yes] [--dry-run]   # one more gateway
osh gateway remove NAME [--delete-data]    # remove a gateway osh created (typed confirmation)
osh upgrade    # reinstall OpenShell, refresh the gateway config, restart the main gateway and its extras
osh gateway-docker          # same as: osh gateway add docker
osh uninstall  # remove OpenShell, its services and folders (asks first; sandboxes are lost)
osh gateway-reset NAME      # wipe a gateway's sandboxes and database (typed confirmation)
osh sync-agents --dry-run   # preview what a sandbox would get from your agent setup
```

Install with Homebrew (macOS or Linux):

```sh
brew install Guilospanck/tap/oh-my-openshell   # taps Guilospanck/homebrew-tap on first use
osh setup                                      # then install what osh needs (asks before each step)
```

Homebrew installs only the `osh` binary; it does not install OpenShell, a container runtime or a gateway until you
run `osh setup`. Upgrade with `brew upgrade oh-my-openshell`. Releases currently ship Apple Silicon macOS and arm64
Linux archives, so Intel Macs and x86_64 Linux need to build from source until those archives are added.

Install from a release (Linux or macOS, aarch64; x86_64 once those archives are published): `curl -fsSL
https://raw.githubusercontent.com/Guilospanck/oh-my-openshell/main/install.sh | sh` downloads
`osh-VERSION-OS-ARCH.tar.gz`, checks its `.sha256`, puts `osh` in `~/.local/bin` and runs `osh setup`
(asking before each step, on the terminal even when piped). Environment: `OSH_VERSION` (release tag; default the
latest), `OSH_BIN_DIR`, `OSH_RELEASE_BASE` (a directory or URL holding the archives; needs `OSH_VERSION`)
and `OSH_NO_SETUP=1` (binary only). Linux archives are built on Ubuntu 24.04 and need glibc 2.39 or newer.

`osh` is a single binary. The provider profiles, sandbox Dockerfiles and gateway
configs are embedded in it and written to `~/.local/share/osh` on first run
(your edits to those files are kept on upgrade, and a file you delete stays deleted; remove `.osh-manifest.json` there to restore the defaults; add your own `images/Dockerfile-*`
and `providers/*.yaml` there). The first time you run `osh` (and with `osh setup`) a guided screen walks you
through it: **Machine** (what was found), **Gateways** (tick the types you want;
each says what it needs and what osh will install for it), **Review** (every
step, with `sudo` marked), **Install** (live progress; `Esc` stops after the step that is running, and the rest are left for the next `osh setup`) and **Done**. Setup
detects the OS and package manager (Homebrew, apt, dnf, pacman, zypper) and
installs OpenShell, the gateway configs and services, whatever each ticked type
needs (Docker Engine, Podman, Colima, a Podman machine, `e2fsprogs`, KVM access),
and the optional tools: `tmux`, `gh` and a clipboard tool. `--runtime docker|podman`
is an alias for `--gateways docker|podman`. It never installs silently: each step is shown first, and what
it cannot do for you (no sudo, unknown distro, a stopped Docker daemon) is
printed as the command to run yourself. Without a terminal (a pipe, CI) nothing runs unless you pass `--yes`. Only OpenShell is required to open the
dashboard; image builds need a container runtime.

Release archives: `scripts/release vX.Y.Z [--linux amd64 arm64]` builds them into `dist/` with a `.sha256` each
and prints the `gh release create` command; it never uploads. Pushing a `v*` tag runs the same script in
`.github/workflows/release.yml` (one runner per platform, then publish), which has not yet run on GitHub.

Releasing from this machine, the same steps as the workflow plus the Homebrew formula: `just release-setup`
(read-only check of `gh` login, push access to the tap, Docker, Zig), then `just release-dry-run vX.Y.Z` (check,
version bump, archives, formula lint; pushes nothing, reverts the bump), then `just release vX.Y.Z ["notes"]`
(commits the `tui/Cargo.toml` bump, tags, pushes, `gh release create`, pushes `Formula/oh-my-openshell.rb` to
`Guilospanck/homebrew-tap`). It needs a clean `main` level with `origin`, and never runs `osh` or `osh setup`.
On Apple Silicon the Linux arm64 archive builds in a separate Colima instance (`scripts/release-colima`; needs
`colima` and `lima`), created for the run and deleted after it (`--keep-vm` keeps it, `--host-docker` uses your own
Docker instead); your own Colima is not touched. The x86_64 Linux archive is opt-in (`--linux amd64`): under QEMU
on Apple Silicon the linker crashed in a test, so it is left to CI's native runner and the formula omits that block.
Users then run `brew install Guilospanck/tap/oh-my-openshell`; the formula installs only the binary. A Mac builds
only its own architecture, so an Apple Silicon release has no Intel macOS archive and the formula omits that block.
`just release-setup-ci-key` is the one-time deploy key (stored as the repo secret `HOMEBREW_TAP_DEPLOY_KEY`) the workflow uses to push the tap; once Actions run again, release by pushing the tag only, since `just release` also creates the GitHub release and would make the workflow fail. From a
checkout, `./osh` is a launcher that builds the TUI with cargo when its sources
change and sets `OSH_DIR` to the checkout, so edits to the repo's providers and
Dockerfiles apply with no copy. Build needs `cargo` and Zig 0.15.

The embedded sandbox shell uses libghostty-vt (Ghostty's terminal engine),
which builds only with Zig 0.15 (`brew install zig@0.15`). The launcher puts
`zig@0.15` on PATH for the build; building by hand needs
`PATH="$(brew --prefix zig@0.15)/bin:$PATH" LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast cargo build --release`.

| Tab | What you can do |
| --- | --- |
| Sandboxes | create (`n`), open a shell inside osh (`Enter`; `Ctrl-]` for the menu, `●` marks a running one), delete (`d`), stop/start (`s`/`S`), attach/detach providers (`a`/`x`), live logs (`l`), blocked requests (`p`), renew a GitHub repo token (`t`), install skills and plugins (`K`, below) |
| Providers | create from a `providers/*.yaml` profile or a read/write GitHub repo token (`n`), update credentials (`u`; renews a repo token), delete (`d`) |
| Profiles | import or refresh a profile on the gateway (`u`) |
| Images | build `images/Dockerfile-*` (`b`); shows when a build is outdated |
| Gateways | switch (`g` / `Enter`), add a gateway (`a`), remove one osh created (`x`), wipe and restart empty (`d`) |

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
sandbox closes. The host needs `pngpaste` (macOS; `osh setup` installs it),
`wl-paste` (Wayland) or `xclip` (X11).

## Gateways

A gateway is where sandboxes run, and each is named after its type: `openshell-vm`,
`openshell-docker`, `openshell-podman`. You can have several side by side, on
macOS and on Linux. The first one you create is the **main** gateway (port 17670, the
one the CLI uses by default); the others get the next free ports (17671 and up). Each gateway also uses the port +10 (health) and +20 (metrics).

| Type | What it is | Needs |
| --- | --- | --- |
| VM | each sandbox in its own microVM; strongest isolation, fixed size per VM | macOS: `e2fsprogs`. Linux: `/dev/kvm` usable by your user (osh adds you to the `kvm` group and asks for a new login) and `e2fsprogs`; unavailable, with the reason, where there is no KVM |
| Docker | sandboxes as containers; honours per-sandbox CPU/memory | Linux: Docker Engine, the `docker` service, your user in the `docker` group (new login). macOS: Colima (installed and started) or Docker Desktop (you start it; untested) |
| Podman | like Docker, daemonless and rootless | Linux: `podman` and its user `podman.socket`. macOS: `podman` and a Podman machine (created and started) |

The Gateways tab lists them with their type and port. `a` adds one (the same screen,
showing only the types you do not have), `x` removes one osh created, `d` wipes one.
The main gateway cannot be removed while others exist. A gateway you had before osh
managed it (for example `openshell` or `openshell-docker` from an older setup) is left
exactly as it is: it is listed, can be selected and wiped, but osh will not rename,
reconfigure or remove it. One repair is the exception: the `openshell-docker` launchd agent an
older `just gateway-docker` set up runs a script osh no longer ships, so `osh setup`, `osh upgrade`
and `osh gateway-docker` point it at `osh gateway-service docker` (same config, database and
logs) and start it again; any osh launchd agent whose program is gone is repaired the same way.

osh keeps each gateway's config in `~/.local/share/osh/gateways/NAME.toml` (edit it; your
changes are kept). The main gateway on Linux, and the VM main on macOS, use OpenShell's own
`gateway.toml` instead: osh rewrites it when it differs from what it would write and keeps the
previous copy next to it as `gateway.toml.osh-bak` (then `.osh-bak.1`, `.2`, never overwriting
an earlier one). osh remembers the ones it made in `gateways.json` there. Extras run as
a systemd user unit `osh-gateway-NAME` (Linux) or a launchd agent (macOS); the main runs as
OpenShell's own service (on macOS a Docker or Podman main runs as its own launchd agent
instead of the Homebrew service). Logs on Linux:
`journalctl --user -u openshell-gateway` or `-u osh-gateway-NAME`. Providers and
profiles are per gateway. The Podman and VM gateways have only been tested up to a
connected gateway on Linux, never booting a real microVM, and nothing here has run
on a real Linux desktop.

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
CLI to `SKILLS_CLI` in `tui/src/agents.rs`, graphify to the host's version, and
superpowers for pi to the commit installed on the host.

If the sandbox's policy blocks an install, `K` offers to create (if needed) and attach the anonymous profiles that grant the access, then checks again. The profiles are: `npm`, `pypi`
(graphify) and `claude-marketplace` (read-only github.com for plugins, `npx
skills` and `pi install`). You can also create them from the Providers tab. Preview the list
with `osh sync-agents --dry-run`.

When access is missing, `K` offers either the built-in profiles above or a
generated one: `osh sync-agents --gen-profile` writes a profile that grants only
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
(Contents, Pull requests and Workflows: Read and write) filled in. GitHub cannot
preselect the repository, so the steps tell you to choose "Only select
repositories" and pick it (Workflows is needed to push
`.github/workflows`; an organization may have to approve the token first). Paste the token (hidden);
osh checks it can see the repo before using it.

osh then imports a profile `gh-<owner>-<repo>`: the `github` profile's read
access plus `POST .../<owner>/<repo>.git/git-receive-pack`, so pushes reach
that repo only. The token lives in a provider created from it: `github-<sandbox>-<repo>`
for one sandbox, or `github-<repo>` when you pick "shared by sandboxes" (also
`n` on the Providers tab, "GitHub repo, read/write"; attach it to other
sandboxes with `a` or pick it in the wizard). A sandbox's own provider is deleted
with the sandbox, and osh then offers to open GitHub's token list and names the token to delete there
(GitHub has no API for that); a shared one stays until you delete it.

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
service. `just test-linux` checks them in Docker (Ubuntu, Fedora, Arch) with real
package installs and stubbed OpenShell and systemd, and `just test-linux-real
podman|docker|vm` in a systemd container with nothing stubbed (vm needs /dev/kvm
on the Docker host and is skipped without it); they have not run on a real Linux
host. `just test-macos` runs the macOS paths on a Mac against recording stand-ins
for Homebrew, launchd, Colima and Podman without changing the Mac, and
`just test-release` runs the release workflow's Linux jobs with act. On a Mac,
with disposable VMs that are deleted afterwards: `just test-linux-kvm` runs the
VM gateway scenario on real KVM (a separate Colima instance with nested
virtualization, M3 or later; its sandbox checks show as KNOWN-FAIL while OpenShell
bug [#4014](https://github.com/NVIDIA/OpenShell/issues/4014) stops every microVM
sandbox at start), `just test-macos-vm` runs real Homebrew,
`brew services` and launchd in a Tart macOS VM clone, and
`just test-macos-runtimes` runs osh's real Colima and Podman machine steps in a
throwaway HOME, leaving your own Colima untouched. Docker Desktop is not tested.

## Setup without the dashboard

Everything above runs from the shell (`osh setup`, `upgrade`, `uninstall`,
`gateway add|remove`, `gateway-docker`, `gateway-reset`). The justfile keeps thin developer wrappers for
them (`just install`, `upgrade`, `uninstall`, `gateway-docker`, `tui`) and
`just install-hooks` (below). Run recipes with `just --justfile .justfile <recipe>`.

## Secret and PII hook

`just install-hooks` (once per clone) enables a pre-commit hook that scans the
whole staged snapshot with [Gitleaks](https://github.com/gitleaks/gitleaks)
(`brew install gitleaks`) and refuses `.env` files. Rules live in
`.gitleaks.toml`. A missing scanner or scan error blocks the commit.

## License

[MIT](LICENSE).
