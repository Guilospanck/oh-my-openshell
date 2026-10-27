# AGENTS.md

Portable OpenShell config plus `osh`, a ratatui terminal dashboard for managing
sandboxes, providers, profiles, images and gateways. `README.md` is the user
manual; this file is what an agent needs to build, test and change the repo.
Also consumed as a submodule by the dotfiles repo: change it here, never in the
dotfiles checkout.

## Setup and verify

Building and testing work on macOS and Linux (see Platforms). Check prerequisites first (read-only):

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
| `tui/src/` | Rust TUI (crate `osh-tui`). `main.rs` event loop and subcommands (`doctor`, `setup`, `upgrade`, `uninstall`, `gateway-docker`, `gateway-reset`, `sync-agents`), `app.rs` state and tabs, `ui.rs` drawing, `cli.rs` wrappers around the `openshell` and container runtime CLIs, `term.rs` embedded shells (libghostty-vt + portable-pty), `watch.rs` log watcher, `logview.rs`, `clip.rs` clipboard |
| `tui/src/platform.rs` | `Os`, `PkgMgr`, `Runtime` (Docker/Podman), the `Host` trait (the only door to the host; `FakeHost` in tests) and `Facts` |
| `tui/src/setup.rs` | Pure planner (`plan`: Facts in, `Step`s out), `execute` with consent, `doctor`, `hint` |
| `tui/src/gateway.rs` | Gateway config install, service restart, macOS Docker gateway (launchd), reset, upgrade, uninstall: all as `Step`s |
| `tui/src/gwmodel.rs` | `GwKind` (vm/docker/podman, named `openshell-<kind>`), `GatewaySpec`, the registry `<data>/gateways.json` (what osh created), port allocation, `availability(Facts, kind)` (what each type needs on this machine), the wizard's pending selection |
| `tui/src/gwops.rs` | Per-gateway config text, systemd unit / launchd agent text, and the add / main / remove / reset steps for a gateway of any kind; `osh gateway-service NAME` |
| `tui/src/wizard.rs` | The set-up wizard: pure state machine (`Wizard::on_key`), `draw` (ratatui, measured columns; tested on `TestBackend`), and the runner that executes the plan on a thread with live progress |
| `tui/src/assets.rs` | Files embedded with `include_str!` (providers, Dockerfiles, gateway configs, `.gitleaks.toml`) and written to the data dir; user edits are kept |
| `tui/src/agents.rs` | The `sync-agents` logic (manifest, policy preflight, generated profile); golden fixture in `tui/fixtures/` |
| `osh`, `dev-env` | Developer launcher: rebuilds `tui/` when sources are newer than the binary, sets `OSH_DIR` to the checkout, execs the binary. `dev-env` is the shared Zig 0.15 lookup |
| `.justfile` | Developer entry points: checks and build, thin wrappers over `osh setup|upgrade|uninstall|gateway-docker`, hooks |
| `scripts/release` | Local release build (archives per platform); the GitHub workflow runs the same script |
| `providers/*.yaml` | OpenShell provider profiles (network policy per service). Imported with `openshell provider profile import` |
| `images/Dockerfile-*` | Sandbox images built from the Images tab |
| `gateway.toml` (macOS vm), `gateway-linux.toml`, `gateway-podman.toml`, `gateway-docker.toml`, `gateway.env` | Gateway configs, embedded in the binary |
| `git-hooks/check-secrets`, `.githooks/`, `.gitleaks.toml` | Pre-commit secret/PII scan (Gitleaks) |

## Platforms

- macOS: vm gateway (Homebrew service, `gateway.toml`) plus an optional
  Docker gateway (launchd agent that runs `osh gateway-service docker`, Colima,
  `gateway-docker.toml`).
- Linux: one gateway, the packaged `openshell-gateway` systemd user service,
  on the Docker driver (`gateway-linux.toml`) or the Podman driver
  (`gateway-podman.toml`, untested on a real host). `osh setup` installs a
  runtime only when neither Docker nor Podman exists. Linux needs Zig 0.15
  (PATH or Linuxbrew) to build and `wl-paste` or `xclip` for image paste.
- New host-touching code goes through `Host` and returns `Step`s, so it is
  unit-tested with `FakeHost`. Gateways are named after their type
  (`openshell-vm|docker|podman`); the first one set up is the main (17670), the rest are
  extras. Never rename, restart or reconfigure a gateway that is not in the registry. Branch on `Os` / `PkgMgr` / `cfg!(target_os)`.
- `just test-linux` (Docker; `tests/linux/`) runs the real build, clippy and
  tests on Ubuntu, Fedora and Arch, and `osh setup` with real package installs
  (OpenShell installer, systemd and the container runtime daemons stubbed); `just test-linux-real
  podman|docker` boots a systemd machine with nothing stubbed and runs the two-gateway
  scenarios (add, remove, reset, uninstall). It
  proves our commands are right for each distro, not that OpenShell's packages
  and systemd unit behave as assumed; a real Linux host is still untested.
- Keep new code portable: no BSD-only or GNU-only flags without a branch on
  `uname` / `cfg!(target_os)`, no hard-coded `/opt/homebrew` outside the macOS
  code paths.

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
  OpenShell state: `osh setup|upgrade|uninstall|gateway-docker` (and the `just`
  wrappers), `osh gateway-reset` (wipes a gateway's sandboxes and providers), `osh` itself
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
