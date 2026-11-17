# osh MCP server, Part 2 (background jobs) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `sandbox_exec` over MCP waits `wait_secs` and then hands back a job id instead of being cut off; `job_status`, `job_output` and `job_cancel` follow the job; a job survives a restart of the server and shows in the MCP tab.

**Architecture:** A job is a process the server starts inside the sandbox with `setsid`/`nohup`, writing `out`, `err`, `pid` and `exit` files under `/tmp/.osh-jobs/<id>/`; nothing runs in the server. The server keeps one small JSON file per job in `<data>/mcp-jobs/` (gateway, sandbox, command name, state), which is how a fresh process finds a job again and how the tab lists them. All sandbox access goes through `openshell sandbox exec` with fixed scripts whose inputs are positional parameters.

**Tech Stack:** Rust 2021, `serde_json`, the existing `ops`/`cli`/`mcp`/`settings`/`state` modules and ratatui tab; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-17-mcp-server-design.md` (Part 2). Part 1 is merged.

## Global Constraints

- No new dependencies in `tui/core/Cargo.toml`; `osh-core` has no TUI and no Zig.
- `sandbox_exec` gains `wait_secs` (default 30, `0` returns a job at once) and `max_secs`; done within the wait: the result as today (`exit_code`, `stdout`, `stderr`); otherwise `{"job": "<id>", "status": "running"}`. `osh sandbox exec` (CLI) and `ops::sandbox_exec` keep their synchronous behaviour and the 600 s `--timeout`.
- New tools `job_status`, `job_output` (from offsets, returns new text and the next offsets) and `job_cancel`; every tool keeps the optional `gateway` argument and `additionalProperties: false`.
- A job survives a restart of the server: everything needed to find it again is on disk (index file) or in the sandbox (job files).
- `mcp.job_max_secs` (default 14400; `0` = no cap) is the operator's cap on a job's run time; a caller's `max_secs` can only lower it.
- Starting a job assumes `sh` with `setsid` or `nohup` in the sandbox; if neither can start it, the call fails with a clear error and nothing is left running.
- Argv elements and job ids are never spliced into a script: scripts are fixed text with positional parameters; an id must match `^j[0-9a-f]{16}$` before it is used in a path.
- Never pass credentials in argv or print them; ask before `just test-sandbox`; commit subjects lowercase and scoped; `just --justfile .justfile check` green before each commit.

## Review Focus

1. A job id or argv element crafted to escape the job directory or the launcher script (path parts, `-` prefixes, newlines, NUL) must be refused or inert (Tasks 1, 2).
2. A huge output must not be pulled whole into the server: each `job_output` call is bounded, offsets past the end return nothing with `more: false`, a multibyte character split at the chunk edge is not mangled, and bad offsets (negative, fractional, huge) are invalid params (Tasks 2, 4).
3. After the server process is replaced, `job_status`, `job_output` and `job_cancel` work from the index file alone; an unknown or pruned id says so plainly (Tasks 1, 2).
4. Cancelling a job that has already exited leaves it `exited`; cancelling twice is harmless; cancel ends the whole process group when `setsid` was used (Task 2).
5. The caller's `max_secs` never exceeds the operator's `mcp.job_max_secs`; `0` means no cap only when the operator's is `0`; `wait_secs` is clamped and a job whose sandbox was deleted reports `lost`, not a hang (Tasks 2, 3).

## File Structure

- `tui/core/src/jobs.rs` (create): index files, ids, launcher/status/output/cancel scripts, the operations.
- `tui/core/src/ops.rs` (modify): `run_in_sandbox`; `Ctx.job_max_secs`.
- `tui/core/tests/fake-openshell.sh` (modify): `OSH_FAKE_EXEC_RUN=1` runs the command for real.
- `tui/core/src/mcp.rs` (modify): `sandbox_exec` arguments, the three job tools.
- `tui/core/src/settings.rs`, `README.md` (modify): `mcp.job_max_secs`.
- `tui/src/app.rs`, `tui/src/ui.rs`, `tui/src/mcpcmd.rs` (modify): jobs in the MCP tab, pruning ticker, `Ctx` field.
- `tests/sandbox/checks.sh` (modify): jobs in a real sandbox.

---

### Task 1: Job index, ids and scripts (`jobs.rs`, pure part)

**Files:** Create `tui/core/src/jobs.rs` (add `pub mod jobs;` to `lib.rs`).

**Interfaces:**
- Produces: `pub struct Job { pub id: String, pub gateway: String, pub sandbox: String, pub command: String, pub started: u64, pub state: String, pub exit_code: Option<i64>, pub checked: u64 }` (`state`: `running`, `exited`, `cancelled`, `lost`; `command` is `argv[0]` only, never the arguments); `pub fn new_id() -> String` (`j` + 16 hex from `/dev/urandom`); `pub fn valid_id(id: &str) -> bool`; `pub fn dir(data: &Path) -> PathBuf` (`<data>/mcp-jobs`); `pub fn save(data: &Path, job: &Job) -> io::Result<()>` (atomic, `0600`); `pub fn load(data: &Path, id: &str) -> Option<Job>` (`None` for an invalid id or a missing/broken file); `pub fn list(data: &Path) -> Vec<Job>` (newest first); `pub fn prune(data: &Path, now: u64) -> usize` (removes finished jobs older than 7 days and any older than 30 days); `pub const LAUNCH: &str`, `STATUS: &str`, `READ: &str`, `CANCEL: &str` (fixed `sh` scripts, parameters positional); `pub fn parse_status(out: &str) -> Status` with `enum Status { Running, Exited(i64), Cancelled, Lost, Unknown }`.
- Consumes: nothing.

- [ ] **Step 1: Write failing tests** in `jobs.rs`: `new_id_is_valid_and_unique`; `valid_id_refuses_paths_and_odd_text` (`""`, `"j"`, `"../x"`, `"j0123456789abcdeg"`, `"j0123456789abcdef/"`, 17 hex digits, uppercase); `a_job_round_trips_through_its_file_with_mode_0600`; `load_refuses_an_invalid_id_and_a_broken_file`; `list_is_newest_first_and_skips_junk_files`; `prune_removes_old_finished_jobs_and_very_old_running_ones` (a 6-day finished job and a 20-day running job stay; an 8-day finished and a 31-day running one go); `the_command_is_the_first_word_only` (a constructor `Job::new(gateway, sandbox, argv)` stores `argv[0]` and nothing of the rest, even with a secret in a later argument); `parse_status_reads_every_word` (`running`, `exited 3`, `cancelled`, `lost`, `unknown`, junk → `Unknown`); and script-text tests that pin the safety properties: no script contains `$@` inside a `sh -c` string that is built by interpolation, each script starts with `set -u`, and `LAUNCH` mentions both `setsid` and `nohup`.
- [ ] **Step 2: Run** `cargo test -p osh-core jobs` (in `tui/`). Expected: FAIL (module missing).
- [ ] **Step 3: Implement** the types and functions above. `LAUNCH` takes `$1` job dir, `$2` max seconds, then the argv as `"$@"` after `shift 2`; it creates the dir, empties `out` and `err`, and starts `D="$d" MAX="$max" setsid sh -c '<inner>' sh "$@" >"$d/out" 2>"$d/err" </dev/null &` (else `nohup`); the inner script writes `$$` to `$D/pid`, runs the command under `timeout -k 5 "$MAX"` when `MAX` is not `0` and `timeout` exists, then writes its status to `$D/exit`. It prints `started` when the files exist. `STATUS` (`$1` dir) prints one word as in `parse_status`: `exited N` when `exit` exists, else `cancelled` when `cancelled` exists, else `running` when `pid` names a live process, `lost` when it does not, `unknown` when the directory is missing. `READ` (`$1` file, `$2` offset, `$3` limit) prints the file size on the first line then up to `limit` bytes from `offset`. `CANCEL` (`$1` dir) writes `cancelled` unless `exit` exists, then sends TERM to the process group (`kill -TERM -- -PID`, falling back to `kill -TERM PID`), waits up to 3 s, then KILL.
- [ ] **Step 4: Run** `cargo test -p osh-core jobs`. Expected: PASS.
- [ ] **Step 5: Commit** `core: add the job index and the scripts a background job runs`.

### Task 2: Running jobs through `openshell sandbox exec`

**Files:** Modify `tui/core/src/ops.rs` (extract the process-running part of `sandbox_exec`), `tui/core/tests/fake-openshell.sh`, `tui/core/src/jobs.rs`.

**Interfaces:**
- Produces in `ops`: `pub(crate) fn run_in_sandbox(ctx: &Ctx, name: &str, argv: &[String], timeout_secs: u64) -> Result<(i32, Vec<u8>, Vec<u8>), String>` (validates the name, aims at `ctx.gateway`, runs `openshell sandbox exec -n NAME --timeout T -- ARGV`, returns exit code and both streams); `sandbox_exec` is rewritten on top of it with the same JSON and the 600 s timeout. In `jobs`: `pub fn start(ctx: &Ctx, name: &str, argv: &[String], max_secs: u64) -> Result<Job, String>`; `pub fn status(ctx: &Ctx, id: &str) -> Result<Job, String>` (asks the sandbox, saves the new state); `pub struct Chunk { pub text: String, pub next: u64, pub more: bool }`; `pub fn output(ctx: &Ctx, id: &str, stream: Stream, offset: u64, limit: usize) -> Result<Chunk, String>` with `enum Stream { Out, Err }`; `pub fn cancel(ctx: &Ctx, id: &str) -> Result<Job, String>`; `pub fn exec(ctx: &Ctx, name: &str, argv: &[String], wait_secs: u64, max_secs: Option<u64>) -> Result<Value, String>`; `pub const DEFAULT_WAIT: u64 = 30; pub const MAX_WAIT: u64 = 900; pub const CHUNK: usize = 64 * 1024;`.
- Consumes: Task 1; `ops::Ctx`; `cli::with_gateway`.
- Fake: with `OSH_FAKE_EXEC_RUN=1` the fake's `sandbox exec` runs the arguments after `--` with `exec "$@"` (instead of printing the canned text), so a launcher really runs on the test machine.

- [ ] **Step 1: Write failing tests** (using `ops::tests::Fake` and `OSH_FAKE_EXEC_RUN=1`): `start_returns_a_job_and_records_it_in_the_index` (command name only, gateway and sandbox saved); `an_argv_with_odd_text_runs_as_given` (`["sh","-c","echo \"a b\"; echo '$x' >&2"]`, a first element starting with `-` is refused with a clear message, an empty argv is refused, a newline and a quote in an argument survive to the output); `status_follows_a_short_command_to_exited_with_its_code`; `output_returns_new_text_and_the_next_offset_in_chunks` (a 150 000 byte output read in 64 KiB chunks: `more` is true until the last; the chunks join to the original; `offset` past the end gives empty text, `more:false`, `next` equal to the size); `a_multibyte_character_is_not_split_at_a_chunk_edge` (a limit that falls inside a three-byte character moves the edge back and `next` follows); `a_fresh_process_finds_the_job_by_id` (drop every in-memory value, call `status` and `output` with only the data dir); `an_unknown_or_invalid_id_is_a_plain_error` (`../x`, a well-formed id with no file); `cancel_after_exit_keeps_exited_and_twice_is_harmless`; `exec_returns_the_result_when_the_command_finishes_within_the_wait` (same keys as `ops::sandbox_exec`: `exit_code`, `stdout`, `stderr`); `exec_with_wait_zero_returns_a_job_at_once` (`{"job":..., "status":"running"}`); `exec_gives_a_job_when_the_wait_runs_out` (`sleep 3`, `wait_secs` 1); `a_deleted_sandbox_reports_lost_without_hanging` (fake `OSH_FAKE_FAIL` on the status call). Cancel killing a whole process group is proved in Task 6 (macOS has no `setsid`).
- [ ] **Step 2: Run** `cargo test -p osh-core jobs`. Expected: FAIL.
- [ ] **Step 3: Implement** `run_in_sandbox` and the `jobs` operations. `start` validates the name and argv (non-empty; `argv[0]` must not start with `-`), creates the id, runs `LAUNCH` with `run_in_sandbox` (short timeout), saves the index file only after the launcher printed `started`, and returns the error text of a failing launcher. `status` runs `STATUS`, parses it, and saves the changed state and `checked`. `output` runs `READ` with the offset and `limit`, trims an incomplete trailing UTF-8 sequence off the chunk (and counts only the kept bytes into `next`), and sets `more` from the size line. `exec` polls `status` every 500 ms up to `wait_secs` (clamped to `MAX_WAIT`); when `exited`, it reads both streams to the end (at most 8 MiB each, noting `"truncated": true` beyond that) and builds the `exec` result; otherwise it returns the job reply. The job's own gateway (from the index) is used for every later call, whatever `ctx.gateway` says.
- [ ] **Step 4: Run** `cargo test -p osh-core jobs` and the whole `just --justfile .justfile check`. Expected: PASS.
- [ ] **Step 5: Commit** `core: run background jobs in a sandbox and read them back by id`.

### Task 3: The operator's cap

**Files:** Modify `tui/core/src/settings.rs`, `README.md`, `tui/core/src/ops.rs` (`Ctx`), and every `Ctx { .. }` literal (`grep -rn "Ctx {" tui`): `mcp.rs` tests, `gateways.rs` tests, `mcpserver/http.rs` tests, `ops.rs` `Fake::ctx`, `tui/src/mcpcmd.rs` (`server_ctx`, tests).

**Interfaces:**
- Produces: setting `mcp.job_max_secs` (`Kind::Secs { min: 0, max: 604800 }`, default `14400`, `Effect::McpRequest`); `settings::mcp_job_max_secs() -> u64`; `Ctx.job_max_secs: u64` (`0` = no cap); `jobs::effective_cap(operator: u64, asked: Option<u64>) -> u64`.
- Consumes: Task 2 `jobs::exec` takes the cap through `max_secs`.

- [ ] **Step 1: Write failing tests:** `effective_cap_never_exceeds_the_operators` (operator 100: asked `None` → 100, `Some(50)` → 50, `Some(500)` → 100, `Some(0)` → 100; operator 0: `None` → 0, `Some(500)` → 500, `Some(0)` → 0); `the_job_cap_setting_has_a_default_and_a_range` (`settings.rs`); the README-row test fails until the row exists; `the_servers_context_carries_the_cap_from_osh_toml` (`mcpcmd.rs`, with `[mcp]\njob_max_secs = 60`).
- [ ] **Step 2: Run** `just --justfile .justfile test`. Expected: FAIL.
- [ ] **Step 3: Implement** the setting, its README row (`next request (osh mcp over stdio: restart)`, env `none`), the field, and `build_ctx`/`server_ctx` reading it from the snapshot.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: PASS.
- [ ] **Step 5: Commit** `core, tui: add mcp.job_max_secs, the operator's cap on a job's run time`.

### Task 4: The MCP tools

**Files:** Modify `tui/core/src/mcp.rs` (schemas, `call`, the existing tool-list test).

**Interfaces:**
- Consumes: `jobs::{exec, status, output, cancel, Stream, DEFAULT_WAIT, MAX_WAIT, CHUNK, effective_cap}`, `Ctx.job_max_secs`.
- Produces: `sandbox_exec` arguments `name`, `argv`, `wait_secs` (integer 0 to 900), `max_secs` (integer ≥ 0); tools `job_status {job}` → `{job, state, exit_code?, sandbox, started}`; `job_output {job, stdout_offset?, stderr_offset?, max_bytes?}` → `{stdout, stderr, stdout_offset, stderr_offset, more}` (`max_bytes` default and ceiling `CHUNK`; offsets are the values a previous call returned); `job_cancel {job}` → `{job, state}`. Tool order: `sandbox_list`, `sandbox_create`, `sandbox_delete`, `sandbox_exec`, `sandbox_apply`, `job_status`, `job_output`, `job_cancel`.

- [ ] **Step 1: Write failing tests:** `tools_list_has_the_job_tools_with_closed_schemas`; `sandbox_exec_takes_wait_and_max_secs_and_refuses_bad_values` (`-1`, `1.5` and a string are invalid params; `wait_secs` 901 does not error and is clamped to 900); `over_mcp_a_quick_command_returns_the_exec_result_and_a_slow_one_a_job` (fake with `OSH_FAKE_EXEC_RUN=1`); `job_output_offsets_must_be_non_negative_integers` (`-1`, `1.5`, `"3"`, `1e30` → `-32602`); `job_tools_refuse_an_unknown_or_malformed_job_id` (isError result, not an RPC error, with the id echoed back harmlessly); `the_caller_cannot_raise_the_operators_cap` (operator 100, `max_secs` 500: the launcher is given `100`, read from the fake's call log); `a_job_call_uses_the_gateway_the_job_was_started_on` (start with `gateway` A, then `job_status` with no gateway argument: the log shows `-g A`).
- [ ] **Step 2: Run** `cargo test -p osh-core mcp`. Expected: FAIL.
- [ ] **Step 3: Implement** the schemas (descriptions say: `sandbox_exec` may return `{"job": ...}`; use `job_status` and `job_output` with it; offsets come from the previous reply) and dispatch. The old stdio behaviour for commands that finish in time is unchanged apart from the extra optional arguments.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: PASS.
- [ ] **Step 5: Commit** `core: add wait_secs to sandbox_exec and the job_status, job_output and job_cancel tools`.

### Task 5: Jobs in the MCP tab, pruning

**Files:** Modify `tui/src/app.rs` (`McpView`, `refresh_mcp`, `len`), `tui/src/ui.rs` (`draw_mcp`), `tui/src/mcpcmd.rs` (the ticker), `README.md`.

**Interfaces:**
- Consumes: `jobs::{list, prune, Job}`.
- Produces: `McpView.jobs: Vec<osh_core::jobs::Job>` read on each refresh; the tab shows a "Jobs" table (JOB short id, SANDBOX, COMMAND, STATE, STARTED as `ago`) under the clients, only when there are jobs, with `ago`-style text for the checked time on a running one; `jobs::prune(&dir, now)` runs when `osh mcp serve` starts and then every hour from its ticker thread.

- [ ] **Step 1: Write failing tests:** `the_mcp_tab_lists_jobs_with_their_state` (`ui.rs`, a `Job` set on `app.mcp.jobs`: id prefix, sandbox, command name and `running` shown; a job with a secret-looking later argument shows only the command name because only the name is stored); `the_tab_has_no_jobs_table_when_there_are_none`; `refresh_reads_the_jobs_folder` (`app.rs`, with a job file in a temp data dir; call the pure part, `McpView::read_jobs(dir)`); `prune_runs_at_start_and_hourly` as a unit test of a small pure helper `due(last: Option<u64>, now: u64) -> bool`.
- [ ] **Step 2: Run** `just --justfile .justfile test`. Expected: FAIL.
- [ ] **Step 3: Implement** the view, drawing and the helper; the footer keys do not change.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: PASS.
- [ ] **Step 5: Commit** `tui: list MCP jobs in the MCP tab and prune old ones`.

### Task 6: Proof in a real sandbox

**Files:** Modify `tests/sandbox/checks.sh` (a `jobs_checks` function called from `headless_checks` after the existing `osh mcp` check), `README.md`, `AGENTS.md`.

- [ ] **Step 1: Add `jobs_checks`**, driving `osh mcp --gateway "$main"` once per call (each call is a fresh process, so it also proves a job is found again after a restart). Calls pipe one JSON-RPC line to `osh mcp`; the token is not involved. Checks: a `sandbox_exec` of `echo fast` with `wait_secs` 30 returns `exit_code` 0 and `fast`; `sh -c 'sleep 2; echo hi; exit 3'` with `wait_secs` 0 returns a job id; `job_status` becomes `exited` with `exit_code` 3 within 20 s; `job_output` has `hi` and a `next` offset, and a second call from that offset is empty; a `sleep 600` job is cancelled, `job_status` says `cancelled`, and `ps` in the sandbox shows no `sleep 600` left (process group ended); `max_secs` 2 on `sleep 30` ends `exited` with a non-zero code (124 or 137) within 20 s; the index file exists in `$data/mcp-jobs` with mode `600` and holds no argument text.
- [ ] **Step 2: Syntax-check** `bash -n tests/sandbox/checks.sh`. Expected: no output.
- [ ] **Step 3: Update** README (`### MCP server`: `wait_secs`, jobs, the three tools, `mcp.job_max_secs`, that a job's files live in `/tmp/.osh-jobs/` in the sandbox) and AGENTS.md (`jobs.rs`).
- [ ] **Step 4: Ask the user**, then run `just test-sandbox` (a disposable podman machine; no dashboard key changed, so no new end-to-end scenario). Expected: the new checks pass.
- [ ] **Step 5: Commit** `tests: prove MCP jobs in a real sandbox`.

---

## Self-review notes

- Spec coverage: `wait_secs` and the job reply (Tasks 2, 4), `job_status`/`job_output`/`job_cancel` (4), survives a restart (1, 2, 6), `max_secs` and `mcp.job_max_secs` (3, 4), the tab (5), the `setsid`/`nohup` assumption and its check (1, 6). The CLI and `ops::sandbox_exec` are unchanged on purpose.
- `Job.command` stores `argv[0]` only, because arguments may carry secrets and the index is readable by anything running as the user.
- Cancel killing a whole group cannot be proved on macOS (no `setsid`): Task 6 does it in the real sandbox.
