# Self-contained osh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A person downloads one `osh` binary, runs it, and osh detects the OS, installs or guides everything it needs (OpenShell, a container runtime, clipboard tool, gateway service), then opens the dashboard.

**Architecture:** Static data (providers, Dockerfiles, gateway configs) is embedded in the binary and written to a data directory on first run. The bash installer (`.justfile`, `gateway-reset`, `osh` launcher) and the Python `sync-agents` move into Rust modules. Setup is a pure *plan* function (facts about the host in, ordered steps out) plus a thin executor that asks consent per step, so every OS/distro path is unit-testable without touching the host.

**Tech Stack:** Rust 2021, existing crates (ratatui, portable-pty, libghostty-vt, serde_json). No new crates unless a task says so. Zig 0.15 stays a build-time-only dependency.

**Spec:** the user's answers in the conversation of 2026-10-27, recorded here:
1. Container runtime: if Docker or Podman (or another OCI runtime) already exists, install nothing. If none exists, the user picks which to install (Docker or Podman).
2. Port everything to Rust, including `sync-agents`.
3. Work on branch `self-contained-osh`.
4. Linux is tested in containers.

## Global Constraints

- Zig must be exactly 0.15 for building (libghostty-vt); users of release binaries need no Zig, cargo or just.
- `just check` (fmt, clippy `-D warnings`, tests) stays green after every task.
- Never pass credentials in argv or print them. No `--no-verify`.
- Every host-changing step (package install, service enable, sudo, file writes outside the data dir) needs per-step consent, or `--yes`. `--dry-run` prints the plan and changes nothing.
- Keep code portable: branch on `cfg!(target_os)` / detected distro; no hard-coded `/opt/homebrew` outside macOS code paths.
- Commit subjects lowercase, scoped: `tui: ...`, `justfile: ...`. One logical change per commit. Add or update unit tests in the file's `mod tests` for every behavior change; keep README tables and footer keys in step.
- Hard requirements at runtime: `openshell` and, for image builds and container gateways, one OCI runtime. tmux, gh, script, clipboard tool are optional and degrade with an "install X to enable" hint.
- Model per task is noted; Opus for security/state-critical tasks (5, 4 executor).

## Review Focus

1. No network (curl fails mid-install): setup stops with the failed step named; re-running resumes (steps are idempotent).
2. `sudo` absent or user declines the password prompt: setup reports it, leaves the host unchanged for that step, continues with independent steps.
3. Unknown distro / no package manager (e.g. Alpine, NixOS): plan lists manual instructions instead of failing.
4. User edited a materialized provider YAML or Dockerfile, then upgrades osh: edit is preserved, not overwritten.
5. Both Docker and Podman installed, or Docker installed but daemon stopped: Docker preferred when its daemon answers, else Podman if its socket answers, else a "start your runtime" hint; nothing is installed.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `tui/src/platform.rs` (new) | `Os`, `Distro`, `PkgMgr`, `Runtime` detection; `Host` trait (which / run / exists) with `RealHost` and a test `FakeHost` |
| `tui/src/assets.rs` (new) | embedded files, `data_dir()`, `materialize()` with hash manifest |
| `tui/src/setup.rs` (new) | `Step`, `plan(&Facts, &Options) -> Vec<Step>`, executor with consent, `doctor()` report |
| `tui/src/gateway.rs` (new) | gateway config copy, service start/stop/restart/reset, macOS Docker gateway (launchd), uninstall, link |
| `tui/src/agents.rs` (new) | Rust port of `sync-agents` (manifest, preflight, generated profile) |
| `tui/src/main.rs` | subcommands `doctor`, `setup`, `uninstall`, `sync-agents`; startup flow replaces `ensure_installed` |
| `gateway-podman.toml` (new) | Podman-driver gateway config |
| `tests/linux/` | distro matrix Dockerfiles and suite updates |
| `.github/workflows/release.yml`, `install.sh` (new) | release binaries and curl-pipe installer |

---

### Task 1: Platform detection and Host trait (Sonnet)

**Files:**
- Create: `tui/src/platform.rs`; Modify: `tui/src/main.rs` (`mod platform;`)

**Interfaces:**
- Produces:
  - `enum Os { MacOs, Linux }`, `enum PkgMgr { Brew, Apt, Dnf, Pacman, Zypper, None }`
  - `enum Runtime { Docker, Podman }` with `fn bin(self) -> &'static str`
  - `trait Host { fn which(&self, bin: &str) -> bool; fn run(&self, prog: &str, args: &[&str]) -> Result<String, String>; fn read(&self, path: &Path) -> Option<String>; }`
  - `struct Facts { os: Os, pkg: PkgMgr, is_root: bool, has_sudo: bool, runtimes_ready: Vec<Runtime>, runtimes_installed: Vec<Runtime>, … }` and `fn gather(h: &dyn Host) -> Facts`
  - `fn parse_os_release(text: &str) -> PkgMgr` (maps `ID`/`ID_LIKE`: ubuntu/debian→Apt, fedora/rhel/centos→Dnf, arch→Pacman, opensuse→Zypper, else None)

- [ ] **Step 1: Failing tests** in `platform.rs`: `os_release_ubuntu_is_apt`, `os_release_fedora_is_dnf`, `os_release_alpine_is_none`, `ready_runtime_requires_answering_daemon` (FakeHost where `docker` exists but `docker info` errors → in `installed`, not `ready`), `prefers_docker_when_both_ready`.
- [ ] **Step 2:** `just test` → FAIL (module missing).
- [ ] **Step 3:** Implement. "Ready" = `docker info` / `podman info` exit 0. `RealHost::run` uses `std::process::Command`, never a shell.
- [ ] **Step 4:** `just test` → PASS. **Step 5:** commit `tui: detect os, package manager and container runtime`.

### Task 2: Embedded assets and data directory (Sonnet)

**Files:** Create `tui/src/assets.rs`; Modify `tui/src/main.rs:30` (`config_dir`), `tui/src/cli.rs:162,348,365,380,1111-1125`

**Interfaces:**
- Produces: `fn data_dir() -> PathBuf` (`OSH_DIR` if set, else `$XDG_DATA_HOME/osh`, else `~/.local/share/osh`); `fn materialize(dir: &Path) -> io::Result<Report>` where `Report { written: Vec<PathBuf>, kept_user_edits: Vec<PathBuf> }`.
- Embedded set (via `include_str!("../../providers/npm.yaml")` etc.): all `providers/*.yaml`, `images/Dockerfile-*`, the three existing gateway TOMLs. `dir.join("images")` and `dir.join("providers")` listings in `cli.rs` keep working because files are real on disk, so user-added files are still listed.
- Manifest `.osh-manifest.json` in the data dir stores the hash last written per file.

- [ ] **Step 1: Failing tests:** `materialize_writes_all_files_into_empty_dir`; `materialize_overwrites_file_when_disk_hash_equals_manifest` (simulates upgrade); `materialize_keeps_user_edited_file_and_reports_it` (Review Focus 4); `materialize_leaves_unknown_user_files_alone`; `data_dir_prefers_osh_dir_env`.
- [ ] **Step 2:** fail. **Step 3:** implement; call `materialize` from `main()` before the poller starts. `script_dir` in `cli.rs` goes away in Task 7.
- [ ] **Step 4:** `just check`. **Step 5:** commit `tui: embed providers, images and gateway configs in the binary`.

### Task 3: Container runtime abstraction (Sonnet)

**Files:** Modify `tui/src/cli.rs:167,695,787-812`, `tui/src/app.rs:1743,2586,2900`; Create `gateway-podman.toml`; Modify `tui/src/assets.rs` (embed it).

**Interfaces:**
- Consumes: `Runtime`, `Facts` from Task 1.
- Produces: `cli::runtime() -> Option<Runtime>` (cached; ready Docker preferred, then ready Podman) and replaces each literal `"docker"` program with `runtime.bin()`. `build_args` and the freshness `image inspect` work with both (same flags). `gateway-podman.toml`: `compute_driver = "podman"`, `[openshell.drivers.podman] allow_driver_config = true, enable_bind_mounts = true`, resource admission off. The key names were seen in the gateway binary strings; verify with `openshell-gateway --help`/lint once on a host before merging.
- `app.rs:2586` (`matches!(driver, "vm" | "docker")`) gains `"podman"` for the per-sandbox resource options if the gateway honours `--cpu/--memory` there; check the Podman driver's `resource_limits` strings in the binary.

- [ ] **Step 1: Failing tests:** `build_args_use_selected_runtime` (pure function taking `Runtime`), `no_runtime_disables_image_build_with_hint`.
- [ ] **Step 2-4:** implement, `just check`. **Step 5:** commit `tui: build images with docker or podman`.

### Task 4: Setup engine, `osh doctor`, `osh setup` (Opus for executor, Sonnet for the rest)

**Files:** Create `tui/src/setup.rs`; Modify `tui/src/main.rs` (subcommands, remove `ensure_installed` at main.rs:204-223).

**Interfaces:**
- Consumes: `Facts`, `Host`, `Runtime`, `PkgMgr` (Task 1); `assets::data_dir` (Task 2).
- Produces:
  - `struct Options { yes: bool, dry_run: bool, runtime: Option<Runtime> }`
  - `enum StepKind { Pkg(Vec<&'static str>), Script(String), Service(String), WriteFile(PathBuf), Manual(String) }`, `struct Step { id: &'static str, title: String, kind: StepKind, needs_sudo: bool, optional: bool }`
  - `fn plan(f: &Facts, o: &Options) -> Vec<Step>`
  - `fn execute(steps: &[Step], h: &dyn Host, ask: &mut dyn FnMut(&Step) -> Answer) -> Vec<StepResult>`
  - `fn doctor(f: &Facts) -> String` (read-only table: ok / MISSING / optional)
- Plan content by case (steps are idempotent; each skips itself when its check already passes):
  - macOS: brew present or Manual("install Homebrew"); `brew install e2fsprogs pngpaste`; OpenShell via the official `install.sh` (`curl … | sh`, same command as `.justfile:73`); gateway steps come from Task 5. Container runtime is **not** required for the default vm gateway; offered only as optional (Colima+docker or podman).
  - Linux: OpenShell via `install.sh` (sudo); runtime step only if `runtimes_installed` is empty, using `o.runtime` or an interactive pick; Docker: Apt `docker.io`, Dnf `docker`/`moby-engine`, Pacman `docker` + `systemctl enable --now docker` + `usermod -aG docker $USER` (warn: re-login); Podman: `podman` + `systemctl --user enable --now podman.socket`; clipboard: `wl-clipboard` or `xclip`; optional `tmux`, `gh`.
  - PkgMgr::None: every package step becomes `Manual` text.
  - Installed-but-not-ready runtime (Review Focus 5): `Manual("start docker: …")`, nothing installed.
- Executor never builds shell strings from user input; `Script` steps run a fixed program with fixed args; sudo steps run `sudo <pkg-manager …>` and surface the prompt on the real terminal.

- [ ] **Step 1: Failing tests (table-driven over Facts):** `plan_macos_fresh_has_brew_and_openshell`, `plan_ubuntu_no_runtime_installs_chosen_runtime`, `plan_skips_runtime_when_podman_ready`, `plan_installed_but_stopped_docker_is_manual_hint`, `plan_unknown_distro_is_all_manual`, `plan_without_sudo_marks_pkg_steps_manual`, `plan_is_empty_when_everything_present`; executor: `execute_continues_after_failed_independent_step`, `execute_dry_run_runs_nothing` (FakeHost records calls), `declined_step_is_not_run`.
- [ ] **Step 2:** fail. **Step 3:** implement. Startup: if `plan()` has required steps, show a numbered list and `[Y/n]` per step before the dashboard; `--yes` accepts all; `osh setup` forces the flow; `osh doctor` prints and exits non-zero when a hard requirement is missing.
- [ ] **Step 4:** `just check`. **Step 5:** commit `tui: add setup planner, doctor and installer`.

### Task 5: Gateway management in Rust (Opus)

**Files:** Create `tui/src/gateway.rs`; Modify `tui/src/app.rs:631-660` (`on_path`, `config_stale`), `app.rs:2758` (gateway-reset call), `tui/src/setup.rs` (steps).

**Interfaces:**
- Produces: `fn config_for(f: &Facts, rt: Option<Runtime>) -> &'static str` (macOS→`gateway.toml`; Linux→`gateway-linux.toml` for Docker, `gateway-podman.toml` for Podman); `fn install_config(data: &Path, xdg_config: &Path, name: &str)` (copy, never symlink: gateway refuses symlinks); `enum Gw { Vm, Docker }`; `fn reset(gw: &str, h: &dyn Host, confirm) -> Result<()>`; `fn docker_gateway_plist(home: &Path, exe: &Path) -> String` (pure; port of `.justfile:107-118`); `fn uninstall_plan(f:&Facts) -> Vec<Step>`.
- Ports, preserving behavior: `.justfile` `install`/`upgrade`/`gateway-docker`/`uninstall`/`link`/`unlink` and `gateway-reset`. `gateway-docker-service` becomes `osh gateway-service docker` (hidden subcommand: sets env, then execs `openshell-gateway --config <data>/gateway-docker.toml`), so launchd needs only the osh binary. `on_path`/`link` become: binary location is whatever the user installed; drop the symlink logic.
- `reset` is destructive (wipes sandboxes and providers): requires typed confirmation `reset`, even with `--yes`.

- [ ] **Step 1: Failing tests:** `config_for_linux_podman_is_podman_toml`, `install_config_copies_not_symlinks`, `plist_contains_label_and_osh_exe`, `reset_requires_typed_confirmation`, `reset_wrong_gateway_name_errors`, `uninstall_plan_linux_lists_systemd_disable_and_dirs`.
- [ ] **Step 2-4:** implement, `just check`. **Step 5:** commit `tui: manage gateways and uninstall from the binary`.

### Task 6: Graceful degradation of optional tools (Sonnet)

**Files:** Modify `tui/src/main.rs:75,142,163` (tmux, script), `tui/src/cli.rs:1097` (gh), `tui/src/clip.rs`, `tui/src/app.rs` (status line hints).

- [ ] **Step 1: Failing tests:** `missing_tmux_gives_install_hint_not_error`, `missing_gh_hides_repo_picker_with_hint`, `clipboard_hint_names_tool_for_os` (pngpaste on macOS; wl-paste/xclip on Linux), `missing_script_disables_capture_only`. Hint text comes from one function `setup::hint(tool, &Facts) -> String` (e.g. `apt install tmux`).
- [ ] **Step 2-4:** implement, `just check`. **Step 5:** commit `tui: show install hints when optional tools are missing`.

### Task 7: Port sync-agents to Rust (Sonnet; Opus review)

**Files:** Create `tui/src/agents.rs`, `tests/fixtures/agents-home/` (a fake `$HOME` with `.claude` and `.agents`); Modify `tui/src/cli.rs:978,1111-1180,1369-1405`; Delete `sync-agents` and the `just sync-agents` recipe once the port passes.

**Interfaces:** `fn scan(home:&Path, claude_dir:&Path) -> Manifest`; `fn preflight(m:&Manifest, rules:&[Rule], only:&[String]) -> Report`; `fn generated_profile(name:&str, missing:&[Need], sandbox:&str) -> String`; `Manifest` serializes to the same JSON as `sync-agents --json` (fields `group,id,name,detail,kind,default,agents,needs,note`; `stage_dir`). Port in the Python file's own order: `load/item/npx_skills/marketplace_add/plugins/packages/own_skills/memory_dir/stage_and_describe/scan` then `network_rules/allows/profile_table/covering/fixes_for/generated_profile/preflight`. Replace `cli.rs` calls that spawn the script with direct calls.

- [ ] **Step 1: Differential test first:** while the Python still exists, `agents_matches_python_on_fixture` runs `python3 -I sync-agents --json` against the fixture `HOME` and asserts the Rust manifest equals it (skip with a message if `python3` is absent). Add focused tests for `allows` (fnmatch globs, port 443 default), `covering`, and the FORGEJO-style host check.
- [ ] **Step 2-3:** implement; `fnmatch` is a small local function (no new crate). **Step 4:** differential test passes, `just check`. **Step 5:** delete the Python file, drop the differential test (keep the fixture with a recorded golden JSON instead), commit `tui: port sync-agents to rust`.

### Task 8: Launcher, justfile and docs slim-down (Sonnet)

**Files:** Modify `.justfile`, `osh`, `README.md`, `CLAUDE.md`/AGENTS.md; keep `dev-env`.

- `.justfile` recipes `install`, `upgrade`, `uninstall`, `gateway-docker`, `link`, `unlink`, `tui` call `osh setup|upgrade|uninstall|…` (thin wrappers); provider recipes stay only if `cli.rs` does not already cover them (it does: `cli.rs:954,1329`), so remove them. `osh` bash launcher stays as the *developer* entry (rebuild on change) and exports nothing the binary now needs.
- [ ] **Steps:** update README (install = download binary, `osh`), the tab/footer-key agreement check, AGENTS.md Setup section; `just check`; commit `docs: self-contained install flow`.

### Task 9: Linux tests in containers (Sonnet)

**Files:** Modify `tests/linux/Dockerfile`, `run.sh`, `suite.sh`; Create `tests/linux/Dockerfile.fedora`, `Dockerfile.arch`.

- Matrix: Ubuntu 24.04, Fedora, Arch. Each builds the real binary, runs `osh doctor` and `osh setup --dry-run --yes` and asserts the planned commands per distro (apt vs dnf vs pacman). A second scenario runs the real `osh setup --yes --runtime podman` as root with stubs only for `systemctl`, `sudo`, `curl|sh` (OpenShell installer), asserting `podman`, `tmux` and `xclip` are actually installed by the package manager and the gateway config landed in `~/.config/openshell/gateway.toml` as `gateway-podman.toml`. Existing stubs for `docker`/`openshell` stay for the gateway-reset scenarios.
- [ ] **Steps:** add scenarios failing first (binary lacks subcommand → FAIL), implement, `just test-linux`; commit `tests: linux distro matrix for setup`.
- Limit: still no real systemd or real OpenShell packages in containers; state this in the README test section.

### Task 10: Local release script, CI workflow and installer script (Sonnet)

**Files:** Create `scripts/release` (bash), `install.sh`, `.github/workflows/release.yml`; Modify `.justfile` (recipe `release`).

- GitHub Actions credits are exhausted, so the local path is primary. `scripts/release <version> [--out dist/]` builds `osh-<os>-<arch>.tar.gz` plus `.sha256` for the host platform with `cargo build --release` (via `dev-env` for Zig 0.15), and for Linux targets builds inside Docker/Podman using the Ubuntu image from `tests/linux/Dockerfile` (`--platform linux/amd64` and `linux/arm64`), so a macOS host produces both Linux archives and its own macOS archive. It prints the archive paths; uploading is a separate manual `gh release create` the script only prints, never runs.
- `.github/workflows/release.yml` (tag `v*`, matrix macos-14, macos-13, ubuntu-24.04, ubuntu-24.04-arm) runs the same `scripts/release` per runner, so CI and local share one code path. It is not exercised until credits return.
- `install.sh`: detects `uname -sm`, downloads from the release URL or from `OSH_RELEASE_BASE` (a `file://` dir, used by tests), verifies sha256, installs to `~/.local/bin/osh`, then runs `osh setup`.
- [ ] **Steps:** `shellcheck` clean on both scripts if shellcheck exists; a Linux-container test runs `install.sh` against a local `file://` release directory built by `scripts/release`; commit `release: add local release script and installer`. Homebrew tap and deb/rpm are out of scope (thin wrappers over this binary).

---

## Self-review

- **Spec coverage:** runtime pick/skip (T1, T3, T4), Rust port (T7, T5 for justfile/bash), branch (created), Linux container tests (T9).
- **Gaps stated:** Podman gateway keys and resource-limit support are inferred from binary strings, so T3 verifies them on a host before merge; real systemd/Docker Engine/Podman machine behavior is untestable in containers.
- **Order:** T1 → T2 → T3 → T4 → T5 are sequential (shared types). T6, T7 can run in parallel after T4. T8-T10 last.
