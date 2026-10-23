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

| Tab | What you can do |
| --- | --- |
| Sandboxes | create (`n`), attach (`Enter`), delete (`d`), stop/start (`s`/`S`), attach/detach providers (`a`/`x`), live logs (`l`) |
| Providers | create from a `providers/*.yaml` profile (`n`), update credentials (`u`), delete (`d`) |
| Profiles | import or refresh a profile on the gateway (`u`) |
| Images | build `images/Dockerfile-*` (`b`); shows when a build is outdated |
| Gateways | switch (`g` / `Enter`), wipe and restart empty (`d`) |

Credentials are read from the environment when exported, else prompted hidden;
they never go through argv. The footer shows the keys for the current tab.

## Gateways

- **vm** (default, Homebrew service, libkrun): fixed size per VM.
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
and the language-server plugins are left out.

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
overlap errors between built-in profiles.

Profiles that need a secret (for example `github`, for private skill sources)
are offered the same way. For a GitHub token, `osh` offers to read it from
`gh auth token` on the host (never shown, never on a command line), or you can
paste one (hidden; use a fine-grained, read-only token from
github.com/settings/personal-access-tokens/new). Tokens exported in the shell
that started `osh` are used without asking. As with Docker Sandboxes, the
real secret stays on the host. Profiles that define the same endpoint
differently, such as `pypi` and `github` (both define `api.github.com`),
cannot be attached together; detach one first.

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
