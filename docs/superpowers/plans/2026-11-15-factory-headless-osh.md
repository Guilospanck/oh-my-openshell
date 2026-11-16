# Headless osh for a factory app and agents: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a factory application (Rust, via a library) and LLM agents (via MCP, the Model Context Protocol) create, list, run commands in and delete OpenShell sandboxes with the same opinionated setup the dashboard gives (policy, toolchain hosts, providers, repo access, resource limits), without the TUI.

**Architecture:** A new library crate `osh-core` takes the TUI-free modules and a new `sandbox` module (a `SandboxSpec` and a pure `plan_create`) extracted from `App::sandbox_steps`. A new `ops` module is the one operations layer (list, create, delete, exec, apply) that returns JSON values. Two thin adapters call it: headless `osh sandbox ...` / `osh apply` subcommands, and `osh mcp` (stdio JSON-RPC). The dashboard's `App` keeps working through an adapter over the same extracted function.

**Tech Stack:** Rust 2021, serde_json (already used), a Cargo workspace rooted at `tui/`. No MCP SDK: the protocol subset needed is small, hand-written. No new runtime dependency beyond `serde` with `derive` if the spec parser wants it.

**Spec:** none written; the design is the "Design decisions" section below (agreed in conversation, 2026-11-15). Memory note: `project-factory-headless-plan`.

## Design decisions

- **Workspace, not one crate with a lib target.** `libghostty-vt` needs Zig 0.15 to build. A factory app depending on the library must not need Zig, so TUI-only code stays in `osh-tui` and `osh-core` has no `ratatui`, `libghostty-vt` or `portable-pty` dependency. `tui/Cargo.toml` keeps its `[package]` (name `osh-tui`, binary `osh-tui`) and gains `[workspace] members = ["core"]`, so `tui/target/release/osh-tui` (used by `osh`, `scripts/release`) does not move.
- **Moved modules keep their paths for the binary** with `use osh_core::{cli, ...};` at the top of `tui/src/main.rs`: a private `use` at the crate root makes `crate::cli::...` resolve everywhere in the binary, so no file other than `main.rs` needs path edits.
- **Sandbox spec is JSON** (YAGNI: no YAML writer needed; `serde_json` exists). Credentials never appear in a spec: a repo grant names an environment variable (`token_env`).
- **One operations layer** (`ops`): the CLI and MCP adapters only parse input and print its `serde_json::Value`. They never build `openshell` argv themselves.
- **Output contract** for every headless command and MCP tool: success prints one JSON value on stdout and exits 0; failure prints `{"ok":false,"error":"<text>"}` on stdout and exits 1; usage errors exit 2. stderr is free text for humans.
- **`apply` is create-if-absent, not a diff.** A sandbox named `spec.name` that already exists yields `{"action":"unchanged"}`. Reconciling a running sandbox is out of scope.
- **MCP exposes less than the CLI.** `sandbox_create` over MCP rejects specs with `share` (a host folder mounted into the sandbox) unless the server runs with `OSH_MCP_ALLOW_SHARE=1`, because an agent must not choose which host folders it can reach.

## Global Constraints

- `just check` (cargo fmt --check, clippy `-D warnings`, cargo test) green after every task; every task adds unit tests in the changed file's `mod tests`.
- Use `just build` / `just test` / `just check`; a bare `cargo` in `tui/` fails with the wrong Zig. `cargo test -p osh-core` needs no Zig.
- Never pass credentials in argv or print them; credentials come from the environment (`cli.rs` convention). No `--no-verify`; the pre-commit gitleaks hook must pass.
- Commit subjects lowercase, scoped: `tui: ...`, `core: ...`, `tests: ...`. One logical change per commit.
- New host-touching code goes through `Host` / returns `Step`s so it is testable with `FakeHost`; shell out to `openshell` only through `cli::exec` / `cli::os` so `-g <gateway>` handling stays in one place.
- Do NOT run `just test-sandbox` per task. It runs once, in Task 7 (user decision), and only after asking, per `AGENTS.md`.
- Do not run `osh`, `openshell sandbox|provider|policy` write commands or `just release` against the live gateway at any point; unit tests use `FakeHost` or a recording stand-in for `openshell` on `PATH`.
- Keep `README.md` and `AGENTS.md` layout table in step (Task 6).
- Before `just check` or a commit, run `git status`: an uncommitted change to `toolchains.toml`, `sandbox-policy.yaml` or `providers/` may be the user's, made through osh. Do not commit or discard it.

## Review Focus

- A spec with an empty or invalid `name` (empty string, spaces, `../x`, over 63 chars): rejected with a JSON error before any `openshell` call; `apply` requires a name.
- Spec JSON with an unknown field or a wrong type (`"cpu": 2`): error names the field; unknown fields are rejected so typos do not silently drop a limit.
- `openshell` missing from `PATH` or the gateway down: JSON error, exit 1, no panic, no partial `sandbox create`.
- `sandbox exec` command that exits non-zero or prints non-UTF-8: exit code and output are returned faithfully, not as a transport error.
- Two concurrent `osh apply` for the same name: the second sees the sandbox exists or the create error; neither deletes the other's sandbox.
- MCP: malformed JSON line, unknown method, `tools/call` with unknown tool or missing arguments: JSON-RPC error object, server keeps running; stdout carries only protocol messages (no stray logging).
- A `token_env` naming an unset or empty variable: error before creating the provider; the token never appears in argv, stdout, stderr or the JSON error.
- Specs written for a gateway of another driver: `share` needs the gateway's driver key (`snap.driver`); an unknown driver yields an error, not a mount for the wrong key.

---

### Task 1: Create the `osh-core` crate and move the TUI-free modules

**Files:**
- Modify: `tui/Cargo.toml` (add `[workspace] members = ["core"]` and `osh-core = { path = "core" }`; move dependencies that only moved modules use, e.g. `psl`, `saphyr`, `toml`, `serde_json`, to core and keep them in the binary only if it still uses them)
- Create: `tui/core/Cargo.toml` (package `osh-core`, edition 2021, same license), `tui/core/src/lib.rs`
- Move (`git mv`) from `tui/src/` to `tui/core/src/`: `cli.rs`, `platform.rs`, `preload.rs`, `ghrepo.rs`, `assets.rs`, `agents.rs`, `watch.rs` (for `is_ip`; if `watch.rs` uses `term` or `ratatui`, move only what `preload`/`cli` need into a new `core/src/hosts.rs` and leave the rest), and any further module the compiler shows they need that has no `ratatui`, `libghostty-vt` or `portable-pty` use
- Move: `tui/fixtures/agents-golden.json` to `tui/core/fixtures/` (the `agents.rs` test reads it); fix the `include_str!("../../…")` paths in `assets.rs` (one more `../`) and re-run `just check`
- Modify: `tui/src/main.rs` (declare `use osh_core::{...}` for each moved module in place of its `mod` line)

**Interfaces:**
- Consumes: nothing.
- Produces: crate `osh_core` exposing `pub mod cli, platform, preload, ghrepo, assets, agents, ...` with unchanged item names and visibility (items that were `pub` stay `pub`; anything the binary used through `pub(crate)` becomes `pub`). Task 2 adds `pub mod sandbox`.

- [ ] **Step 1: Record the baseline.** Run `just check` and note the passing test count; it must not drop except for tests that moved with their file (they now run under `-p osh-core`).
- [ ] **Step 2: Create the crate and move files** as listed. `editor::error_summary` is called from `App::creation_policy_groups` and `editor.rs` stays in the binary; move `error_summary` (pure string helper) into `osh_core::cli` or a new `osh_core::text` and have `editor.rs` call it from there.
- [ ] **Step 3: Fix compile errors until `cargo build` passes** by moving only what the errors demand into core. Rule: core must not name `ratatui`, `libghostty_vt` or `portable_pty`. A module that does stays in the binary, and any core code that wanted it takes a parameter or a trait instead.
- [ ] **Step 4: Prove core is Zig-free.** Run `cargo test -p osh-core` with Zig off `PATH` (`env PATH=/usr/bin:/bin:/opt/homebrew/bin cargo test -p osh-core`, with `zig` not in those dirs; check `command -v zig` first). Expected: PASS.
- [ ] **Step 5: Run `just check`.** Expected: green, same total test count as Step 1. Also run `./scripts/release --help` or read `scripts/release` to confirm it still points at `tui/target/release/osh-tui` (it must; the binary name did not change).
- [ ] **Step 6: Commit** `core: move the TUI-free modules into the osh-core crate`.

---

### Task 2: Extract `SandboxSpec` and `plan_create` from `App::sandbox_steps`

**Files:**
- Create: `tui/core/src/sandbox.rs` (`pub mod sandbox` in `lib.rs`)
- Modify: `tui/src/app.rs` (`Step`, `Plan`, `sandbox_steps`, `creation_policy`, `creation_policy_groups`, `share_config` at roughly lines 172-194, 546-549, 2811-2879, 2881-2960, 5025-5037 become adapters; existing tests `a.sandbox_steps(...)` near lines 9013 and 9140 stay and must still pass untouched)
- Test: `tui/core/src/sandbox.rs` (`mod tests`)

**Interfaces:**
- Consumes: `cli::with_gateway`, `cli::runtime_bin`, `cli::build_args`, `cli::share_key`, `cli::SHARE_LABEL`, `ghrepo::provider_name`, `preload::{parse, select, creation_policy}`, `assets::embedded`.
- Produces (in `osh_core::sandbox`):
  - `pub struct Step { pub prog: String, pub args: Vec<String> }` (moved from `app.rs`; `app.rs` does `pub use osh_core::sandbox::Step;`)
  - `pub struct SandboxSpec { pub name: String, pub image: String, pub providers: Vec<String>, pub cpu: String, pub memory: String, pub command: String, pub toolchains: Option<Vec<String>>, pub share: Option<Share>, pub repo: Option<RepoGrant> }` with `Share { pub path: String, pub rw: bool }` and `RepoGrant` with two public `String` fields, `url` and `token_env` (the name of the environment variable that holds the token, never the token)
  - `impl SandboxSpec { pub fn from_json(v: &serde_json::Value) -> Result<SandboxSpec, String> }` (unknown field rejected; error names the field; validates `name` per Review Focus: non-empty is NOT required for the dashboard, so `from_json` does not require it, `ops` does)
  - `pub struct CreateCtx<'a> { pub dir: &'a Path, pub driver: &'a str, pub images: &'a [cli::Image], pub rebuild: bool }`
  - `pub struct Created { pub steps: Vec<Step>, pub policy_note: Option<String>, pub toolchain_groups: Option<Vec<String>> }`
  - `pub fn plan_create(spec: &SandboxSpec, ctx: &CreateCtx) -> Result<Created, String>` (Err for a `share` when `driver` is empty or unknown)
  - `pub fn validate_name(name: &str) -> Result<(), String>` (lowercase letters, digits, `-`, 1 to 63 chars, starts with a letter or digit)

- [ ] **Step 1: Write failing tests in `sandbox.rs`.**
  - `from_json_rejects_unknown_field`: `{"name":"a","cpus":2}` is `Err` and the message contains `cpus`.
  - `from_json_rejects_wrong_type`: `{"cpu":2}` is `Err` containing `cpu`.
  - `validate_name_rejects_path_and_long`: `""`, `"a b"`, `"../x"`, 64 x `"a"` are `Err`; `"factory-1"` is `Ok`.
  - `plan_matches_dashboard_args`: for the spec `{name:"w1", image:"img:1", providers:["npm"], cpu:"2", memory:"4Gi"}` and a temp dir holding `sandbox-policy.yaml` and `toolchains.toml`, the last step is `openshell sandbox create --policy <dir>/.sandbox-policy.generated.yaml --name w1 --from img:1 --cpu 2 --memory 4Gi --provider npm --detach` (assert the order exactly as `App::sandbox_steps` emits it today).
  - `command_spec_is_one_shot`: a non-empty `command` ends with `--no-keep -- sh -c <command>` and has no `--detach`.
  - `share_without_driver_errors`: a `share` with `driver == ""` is `Err`.
  - `share_uses_driver_key_and_label`: with driver `docker`, the args contain `--driver-config-json` whose JSON has `docker.mounts[0]` with `type` `bind`, `read_only` the inverse of `rw`, and `--label <SHARE_LABEL>=<share_key>`.
  - `rebuild_step_first_when_image_outdated`: an unbuilt `cli::Image` with the spec's tag yields a runtime build step before the create step.
- [ ] **Step 2: Run** `cargo test -p osh-core sandbox` (through `just test` if Zig is needed). Expected: FAIL, items not defined.
- [ ] **Step 3: Implement** the types and `plan_create` by moving the bodies of `creation_policy_groups`, `sandbox_steps` and `share_config` out of `App` and replacing `self.dir`, `self.snap.images`, `self.snap.driver` with `ctx` fields. Behavior must be byte-identical for the dashboard; do not "improve" the arg order. The `repo` grant only adds `--provider <ghrepo::provider_name(name, repo)>`; creating the provider is `ops`' job (Task 3).
- [ ] **Step 4: Adapt `App`.** `Plan` stays (it carries UI-only state such as `resume` and the pasted token); add `impl From<&Plan> for SandboxSpec` and have `sandbox_steps` / `creation_policy*` call `plan_create` and return the same `Vec<Step>` / tuple they returned. Remove the moved code from `app.rs`.
- [ ] **Step 5: Run** `just check`. Expected: PASS, including the two existing `sandbox_steps` tests unchanged (they are the regression guard for byte-identical behavior).
- [ ] **Step 6: Commit** `core, tui: extract SandboxSpec and plan_create from the dashboard`.

---

### Task 3: The `ops` layer

**Files:**
- Create: `tui/core/src/ops.rs` (`pub mod ops` in `lib.rs`)
- Test: `tui/core/src/ops.rs` (`mod tests`), using a recording `openshell` stand-in script placed first on `PATH` in a temp dir (tests that touch `PATH` must be serialized with a `static Mutex`)

**Interfaces:**
- Consumes: `sandbox::{SandboxSpec, CreateCtx, plan_create, validate_name}`, `cli::{os, exec, collect, create_repo_provider, set_gateway}`, `ghrepo` URL parsing (`GhRepo` from a URL; find the parser in `ghrepo.rs` and reuse it).
- Produces (all in `osh_core::ops`):
  - `pub struct Ctx { pub dir: PathBuf, pub gateway: Option<String>, pub allow_share: bool }`
  - `pub fn sandbox_list(ctx: &Ctx) -> Result<Value, String>`: JSON array of `{"name","phase","created"}`
  - `pub fn sandbox_create(ctx: &Ctx, spec: &Value, dry_run: bool) -> Result<Value, String>`: validates name (required), parses the spec, rejects `share` unless `ctx.allow_share`, resolves `repo.token_env` from the process environment (error if unset or empty, message must not contain the value), creates the repo provider via `cli::create_repo_provider` before the sandbox, runs the steps. Returns `{"name","steps":[...]}` on `dry_run` (each step is `{"prog","args"}`; tokens never enter argv, so nothing needs redacting), else `{"name","phase"}`.
  - `pub fn sandbox_delete(ctx: &Ctx, name: &str) -> Result<Value, String>`: `{"name","deleted":true}`
  - `pub fn sandbox_exec(ctx: &Ctx, name: &str, argv: &[String]) -> Result<Value, String>`: `{"exit_code","stdout","stderr"}`; a non-zero exit code is `Ok`, only a failure to run is `Err`. Output is lossy-UTF-8.
  - `pub fn apply(ctx: &Ctx, spec: &Value) -> Result<Value, String>`: `{"action":"created"|"unchanged","name"}`
  - `pub fn envelope(r: Result<Value, String>) -> (Value, i32)`: the output contract: `Ok(v)` is `(v, 0)`; `Err(e)` is `({"ok":false,"error":e}, 1)`.

- [ ] **Step 1: Write failing tests** (stand-in `openshell` records argv to a file and answers `sandbox list -o json`):
  - `create_requires_valid_name` (empty and `../x` → `Err`, stand-in log is empty).
  - `create_dry_run_does_not_call_openshell_write` (log has no `sandbox create`).
  - `create_runs_plan_steps_in_order` (log shows the `sandbox create` line from Task 2's test).
  - `share_rejected_unless_allowed` and `share_allowed_with_flag`.
  - `token_env_unset_is_error_and_value_never_leaks` (set `OSH_TEST_TOK` to a value the test builds at run time, e.g. `format!("tok-{}", "x".repeat(24))`, so no literal secret-like string sits in the source; point `token_env` at it and make the stand-in fail; assert the returned error and the recorded argv do not contain that value).
  - `apply_unchanged_when_exists` (stand-in lists a sandbox named `w1`; log has no `create`).
  - `apply_creates_when_absent`.
  - `exec_nonzero_is_ok_with_exit_code` (stand-in exits 3 → `Ok` with `"exit_code":3`).
  - `envelope_maps_err_to_ok_false_exit_1`.
- [ ] **Step 2: Run** `cargo test -p osh-core ops`. Expected: FAIL.
- [ ] **Step 3: Implement** the functions above. `sandbox_list` reuses the parsing already behind `cli::collect` / its sandbox JSON parser rather than adding a second one. Set the gateway with `cli::set_gateway` from `ctx.gateway` before any call. For `exec`, find how `cli.rs` already invokes `openshell sandbox exec` (the calls near `cli.rs:1486`, `:1513`, `:1815`) and use the same argument shape.
- [ ] **Step 4: Run** `just check`. Expected: PASS.
- [ ] **Step 5: Commit** `core: add the ops layer (list, create, delete, exec, apply) with a JSON contract`.

---

### Task 4: Headless subcommands in `osh`

**Files:**
- Modify: `tui/src/main.rs` (`subcommand()` near line 316: add `"sandbox"` and `"apply"` arms; dispatch to a new function `headless(args: &[String]) -> i32`)
- Create: `tui/src/headless.rs` (argument parsing only, no logic beyond calling `ops`)
- Test: `tui/src/headless.rs` (`mod tests`)

**Interfaces:**
- Consumes: `ops::*` from Task 3.
- Produces: the commands
  - `osh sandbox list [--gateway G]`
  - `osh sandbox create --spec FILE|- [--gateway G] [--dry-run] [--allow-share]`
  - `osh sandbox delete NAME [--gateway G]`
  - `osh sandbox exec NAME [--gateway G] -- CMD...` (process exit code equals the command's exit code when the call ran, with the JSON on stdout)
  - `osh apply -f FILE|- [--gateway G] [--allow-share]`
  - `pub fn headless(args: &[String]) -> i32`; `pub fn parse(args: &[String]) -> Result<Command, String>` where `Command` is a private enum, tested directly.
  - Usage error → message on stderr, exit 2.

- [ ] **Step 1: Write failing tests** on `parse`: each form above parses; `--spec` with no value is `Err`; `exec` without `--` before the command is `Err`; unknown flag is `Err`; `-` reads stdin (inject the reader so the test passes a string).
- [ ] **Step 2: Run** `just test headless`. Expected: FAIL.
- [ ] **Step 3: Implement** `parse` and `headless`: print `ops::envelope`'s value with `println!` (one line, compact JSON), return its code. Existing arms must keep working (`osh doctor`, `osh setup`, etc.); a bare `osh` still opens the dashboard.
- [ ] **Step 4: Smoke test without a gateway.** With a stand-in `openshell` on `PATH` (the same script Task 3 uses; put it under `tests/` if both need it, e.g. `tui/core/tests/fake-openshell.sh`), run `osh sandbox create --spec spec.json --dry-run` and check the stdout is valid JSON (`| python3 -m json.tool`). Expected: exit 0.
- [ ] **Step 5: Run** `just check`. Expected: PASS.
- [ ] **Step 6: Commit** `tui: add headless osh sandbox and osh apply with JSON output`.

---

### Task 5: `osh mcp` (stdio MCP server)

**Files:**
- Create: `tui/core/src/mcp.rs` (protocol + tool table, pure: `handle(line: &str, ctx: &Ctx) -> Option<String>`), wired from `tui/src/main.rs` as `osh mcp [--gateway G]` (loop reading stdin lines, writing stdout lines; `OSH_MCP_ALLOW_SHARE=1` sets `Ctx.allow_share`)
- Test: `tui/core/src/mcp.rs` (`mod tests`)

**Interfaces:**
- Consumes: `ops::*`.
- Produces: `pub fn handle(line: &str, ctx: &ops::Ctx) -> Option<String>`: `Some(response_line)` for requests, `None` for notifications (`notifications/initialized`). Methods: `initialize` (answers with the client's `protocolVersion`, `serverInfo {name:"osh", version: env!("CARGO_PKG_VERSION")}`, `capabilities {tools:{}}`), `ping`, `tools/list`, `tools/call`. Tools and their `inputSchema` (JSON Schema, `additionalProperties:false`): `sandbox_list`, `sandbox_create` (`spec` object, `dry_run` bool), `sandbox_delete` (`name`), `sandbox_exec` (`name`, `argv` array of strings), `sandbox_apply` (`spec`). A tool result is `{"content":[{"type":"text","text":"<compact JSON from ops>"}],"isError":<bool>}`; a failing operation sets `isError` true and is not a JSON-RPC error. Protocol faults use JSON-RPC errors: -32700 parse error, -32601 unknown method, -32602 invalid params.

- [ ] **Step 1: Write failing tests** on `handle`: initialize echoes the version; `tools/list` returns exactly the five tool names; `tools/call` for `sandbox_list` returns text that parses as JSON; unknown tool → `-32602`; unknown method → `-32601`; garbage line → `-32700` with `"id":null`; a notification returns `None`; `sandbox_create` with a `share` returns `isError:true` unless `allow_share`; the response line contains no newline (framing).
- [ ] **Step 2: Run** `cargo test -p osh-core mcp`. Expected: FAIL.
- [ ] **Step 3: Implement** `handle` and the stdin/stdout loop. Nothing but protocol lines may reach stdout: route any diagnostics to stderr. The loop exits cleanly on stdin EOF.
- [ ] **Step 4: Protocol smoke test.** Pipe `initialize`, `notifications/initialized`, `tools/list`, one `tools/call` into `osh mcp` with the stand-in `openshell` on `PATH`; expected: one valid JSON line per request, none for the notification, exit 0 at EOF.
- [ ] **Step 5: Run** `just check`. Expected: PASS.
- [ ] **Step 6: Commit** `core, tui: add osh mcp, a stdio MCP server over the ops layer`.

---

### Task 6: Documentation

**Files:**
- Modify: `README.md` (new section "Headless use and MCP": the spec schema with one full example, every command above, the output contract, the `.mcp.json` snippet `{"mcpServers":{"osh":{"command":"osh","args":["mcp"]}}}`, and that `share` over MCP needs `OSH_MCP_ALLOW_SHARE=1`), `AGENTS.md` (layout table: `tui/core/` and what moved, `headless.rs`, `mcp.rs`, `ops.rs`, `sandbox.rs`; setup section: `cargo test -p osh-core` needs no Zig)
- Check: `tests/e2e/actions.txt` and the unit test that enforces it: new subcommands are not footer keys, so no entry is required; if `just check` fails on it, add the stated-reason entry the file's format asks for.

- [ ] **Step 1: Write the docs** from the finished behavior; copy command lines from `headless.rs`'s usage text so they cannot drift.
- [ ] **Step 2: Run** `just check`. Expected: PASS.
- [ ] **Step 3: Commit** `docs: describe the headless commands, the spec and osh mcp`.

---

### Task 7: Real-sandbox proof (runs once, at the end)

**Files:**
- Modify: `tests/sandbox/checks.sh` (add a section for the headless path)

**Interfaces:**
- Consumes: the finished `osh` binary inside the disposable test container (`just test-sandbox`).

- [ ] **Step 1: Add checks** to `tests/sandbox/checks.sh`, following the file's existing pattern: `osh apply -f spec.json` creates a sandbox with the generated policy; running it twice prints `"action":"unchanged"` the second time; `osh sandbox exec NAME -- sh -c 'tmux -V; echo ok'` returns `"exit_code":0` and `ok` (this is the check that the PTY/tmux policy still applies to specs); `osh sandbox list` shows it; `osh sandbox delete NAME` removes it; piping `initialize` + `tools/call sandbox_list` into `osh mcp` returns valid JSON.
- [ ] **Step 2: Ask the user for permission** to run `just test-sandbox` (it boots the disposable test container and creates a sandbox only inside it, never on the host). Do not run it before they agree.
- [ ] **Step 3: Run** `just test-sandbox`. Expected: all checks PASS. Fix and re-run on failure; a failing check blocks the commit.
- [ ] **Step 4: Run** `just check`. Expected: PASS.
- [ ] **Step 5: Commit** `tests: prove the headless osh path in a real sandbox`.
