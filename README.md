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
