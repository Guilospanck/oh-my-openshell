# AGENTS.md

Portable OpenShell config plus `osh`, a ratatui terminal dashboard for managing
sandboxes, providers, profiles, images and gateways. `README.md` is the user
manual; this file is what an agent needs to build, test and change the repo.
Also consumed as a submodule by the dotfiles repo: change it here, never in the
dotfiles checkout.

## Setup and verify

Building and testing work on macOS and Linux; the gateway services and
installer are macOS-first (see Platforms). Check prerequisites first (read-only):

```sh
just doctor      # cargo, just, gitleaks, Zig 0.15 required; openshell, docker, pngpaste optional
just install-hooks   # once per clone: pre-commit secret/PII scan
just check       # cargo fmt --check, clippy -D warnings, cargo test. Run before every commit
```

- Build and tests need only `cargo` and Zig 0.15 (`brew install zig@0.15`).
  They do not need OpenShell, a gateway or Docker; the unit tests pass with
  `openshell` off PATH.
- Zig must be 0.15 exactly (libghostty-vt). Use `just build` / `just test` /
  `just check`: they source `dev-env`, which finds Zig 0.15 (on PATH, else
  Homebrew or Linuxbrew `zig@0.15`) and sets `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast`.
  A bare `cargo` in `tui/` fails with the wrong Zig.
- A bare `just` only lists recipes. Run recipes as
  `just --justfile .justfile <recipe>` from any directory.

## Layout

| Path | What |
| --- | --- |
| `tui/src/` | Rust TUI (crate `osh-tui`). `main.rs` event loop, `app.rs` state and tabs, `ui.rs` drawing, `cli.rs` wrappers around the `openshell` CLI, `term.rs` embedded shells (libghostty-vt + portable-pty), `watch.rs` log watcher for blocked requests, `logview.rs`, `clip.rs` clipboard |
| `osh`, `dev-env` | Bash launcher: rebuilds `tui/` when sources are newer than the binary, sets `OSH_DIR`, execs the binary. `dev-env` is the shared Zig 0.15 lookup, sourced by `osh` and the justfile |
| `.justfile` | All entry points (install, gateways, providers, hooks, dev checks) |
| `providers/*.yaml` | OpenShell provider profiles (network policy per service). Imported with `openshell provider profile import` |
| `images/Dockerfile-*` | Sandbox images built from the Images tab |
| `sync-agents` | Python 3: manifest of the host's Claude Code / pi setup to reproduce in a sandbox |
| `gateway*.toml`, `gateway-docker-service`, `gateway-reset` | Gateway config and services |
| `git-hooks/check-secrets`, `.githooks/`, `.gitleaks.toml` | Pre-commit secret/PII scan (Gitleaks) |

## Platforms

- macOS: everything works. Linux: `osh`, `just build|test|check|doctor`, the
  clipboard code (`wl-paste`/`xclip`, util-linux `script`) and the sandbox
  features are written for it, but only macOS has been run by hand; the Linux
  build and tests were not run on a Linux host.
- macOS only: `just gateway-docker`, the Docker gateway in `gateway-reset`,
  `gateway-docker-service` (launchd, Colima, `/opt/homebrew` paths). `just
  install` and `uninstall` assume Homebrew (Linuxbrew counts) and are untested on Linux.
- Keep new code portable: no BSD-only or GNU-only flags without a branch on
  `uname` / `cfg!(target_os)`, no hard-coded `/opt/homebrew` outside the macOS
  files above.

## Rules

- Always: add or update a unit test (in the file's `mod tests`) for TUI
  behavior you change; keep `just check` green; keep README tables and key
  lists in step with code changes (the footer keys and the README tab table
  must agree).
- Never pass credentials in argv or print them; credentials come from the
  environment or a hidden prompt (see `cli.rs`). Never commit `.env` files or
  tokens: the pre-commit hook blocks them, and a missing `gitleaks` blocks the
  commit too. Do not bypass with `--no-verify`.
- Ask first before running anything that changes the host or running
  OpenShell state: `just install`, `upgrade`, `uninstall`, `gateway-docker`,
  `gateway-reset` (wipes a gateway's sandboxes and providers), `osh` itself
  (it is interactive and talks to the live gateway), and any
  `openshell sandbox|provider|policy` write command. Read-only ones
  (`openshell status`, `... list`, `provider profile lint`) are fine.
- Dev builds land in `tui/target/` (git-ignored). Do not commit it.

## Conventions

- Rust 2021, `cargo fmt` default style, no clippy warnings. Doc comments
  explain why, not what; follow the surrounding density.
- Shell scripts: `bash`, `set -euo pipefail`, quote everything.
- Commit subjects are lowercase, scoped by area: `tui: ...`, `providers: ...`,
  `justfile: ...`, `gateways, tui: ...`. One logical change per commit.
- Provider profiles: grant the narrowest host, port, method and binary
  needed; keep the header comment explaining what is and is not allowed, as in
  `providers/npm.yaml`. Lint with
  `openshell provider profile lint -f providers/<name>.yaml` (needs openshell).
