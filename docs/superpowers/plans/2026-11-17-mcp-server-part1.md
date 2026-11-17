# osh MCP server, Part 1 (server, service, dashboard tab) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `osh mcp serve` is a long-running HTTP MCP server run as a launchd/systemd user service, with an optional per-call `gateway` argument on every tool, switched on and off from the dashboard and `osh mcp start|stop|status`, and an MCP tab listing connected clients.

**Architecture:** The server logic lives in `osh-core` (new module `mcpserver/`): a hand-written HTTP/1.1 parser and accept loop on `std::net`, bearer-token check, a client registry written to a state file; each request body goes through the existing `mcp::handle`. The `osh-tui` crate adds the service unit text and `Step`s (patterned on `gwops.rs`), the `osh mcp` subcommands and the MCP tab, which only reads the state file. Per-call gateway needs the gateway selection in `cli.rs` to be per-thread.

**Tech Stack:** Rust 2021, `std::net` and threads, `serde_json`, existing `Host`/`Step`/`FakeHost`, ratatui (dashboard only). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-17-mcp-server-design.md` (Part 1). Part 2 (jobs) is a separate plan.

## Global Constraints

- No new dependencies in `tui/core/Cargo.toml`; `osh-core` has no TUI and no Zig.
- Binds `127.0.0.1` by default; another address is the `mcp.server_bind` setting.
- Every request needs `Authorization: Bearer <token>`; compared in constant time; token file read on every request; token file mode `0600` in the data folder; never on a command line, in a log or in the state file.
- Transport: `POST /mcp`, JSON in, JSON out; notification gets `202`, no body; no batches; limits on body size, header size, read time, simultaneous connections.
- An unknown `Mcp-Session-Id` is accepted and added to the client list.
- Host-touching code returns `Step`s tested with `FakeHost`; macOS and Linux only.
- New settings: one `Def` each in `settings.rs` plus a README `## Settings` row (a test checks this); env > `osh.toml` > default.
- Every new dashboard key is in `tests/e2e/actions.txt` with a scenario; README tables and footer keys agree.
- Never pass credentials in argv or print them. Ask before `just test-sandbox`, `just test-linux-real`, and anything that installs a service on the host.
- stdio `osh mcp` keeps working; commit subjects lowercase and scoped (`core: ...`, `tui: ...`); `just --justfile .justfile check` green before each commit.

## Review Focus

1. Two concurrent calls naming different gateways must not aim at each other's gateway (Task 1 pins it).
2. A client that sends garbage, an oversized body or header, a slow trickle, or opens many connections must not hang the server, panic it, or exhaust its threads (Task 4).
3. The token file with loose permissions, missing, or replaced while the server runs: loose is tightened or refused, missing is recreated at start, replacement takes effect on the next request; the token never appears in logs or the state file (Task 3).
4. A crashed server leaves a state file with a dead pid: the dashboard shows "off". A second `serve` on a taken port fails with a clear message and does not overwrite the first's state file (Task 5).
5. A `gateway` argument that is empty, starts with `-`, contains a path or is unknown is rejected before any `openshell` call (Task 2).

## File Structure

- `tui/core/src/cli.rs` (modify): per-thread gateway override.
- `tui/core/src/ops.rs` (modify): `Ctx` derives `Clone`; `target` uses the per-thread setting.
- `tui/core/src/gateways.rs` (create): names osh knows, from `<data>/gateways.json`.
- `tui/core/src/mcp.rs` (modify): `gateway` argument on every tool.
- `tui/core/src/settings.rs` (modify): `Kind::Text`, `Kind::Count`; four `mcp.server_*` settings.
- `tui/core/src/mcpserver/{mod,token,http,state}.rs` (create): token file, HTTP, client registry and state file.
- `tui/src/mcpsvc.rs` (create): service unit text, install/start/stop/status `Step`s.
- `tui/src/mcpcmd.rs`, `tui/src/main.rs` (modify): `osh mcp serve|start|stop|status|token`.
- `tui/src/app.rs`, `tui/src/ui.rs`, `tui/src/palette.rs` (modify): MCP tab.
- `README.md`, `AGENTS.md`, `tests/e2e/*`, `tests/linux/real-suite.sh` (modify).

---

### Task 1: Per-thread gateway selection

**Files:** Modify `tui/core/src/cli.rs` (near `GATEWAY`, `set_gateway`, `gateway`, `with_gateway`), `tui/core/src/ops.rs` (`Ctx`, `target`). Test in each `mod tests`.

**Interfaces:**
- Produces: `cli::set_gateway(name)` keeps its signature but now sets a per-thread value that wins over the process-wide one; `cli::gateway() -> String` returns the per-thread value if set, else the process-wide one. `cli::set_process_gateway(name: &str)` sets the process-wide value (the dashboard keeps using it). `#[derive(Clone)] pub struct Ctx` in `ops.rs`.
- Consumes: nothing.

- [ ] **Step 1: Write the failing test** `gateway_choice_is_per_thread` in `cli.rs`: two threads each call `set_gateway("a")` / `set_gateway("b")`, wait on a `Barrier`, then each asserts `gateway()` returns its own name; the main thread, which set nothing, still sees the process-wide value from `set_process_gateway("main")`.
- [ ] **Step 2: Run** `cargo test -p osh-core gateway_choice_is_per_thread` (in `tui/`). Expected: FAIL (threads see each other's value).
- [ ] **Step 3: Implement** a `thread_local!` `Option<String>` consulted first by `gateway()`; `set_gateway` writes it; rename the old global writer to `set_process_gateway` and switch the dashboard call sites (`grep set_gateway` in `tui/src`, and `ops.rs` lines that reset to `""`) to the right one: dashboard selection uses `set_process_gateway`; `ops::target` keeps `set_gateway`. Derive `Clone` on `Ctx`.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: all green.
- [ ] **Step 5: Commit** `core: select the gateway per thread so concurrent calls cannot cross`.

### Task 2: Per-call `gateway` argument

**Files:** Create `tui/core/src/gateways.rs` (add `pub mod gateways;` to `lib.rs`). Modify `tui/core/src/mcp.rs` (`tools`, `args_of` callers, `call`). Tests in both.

**Interfaces:**
- Produces: `gateways::names(dir: &Path) -> Vec<String>` (names listed in `<dir>/gateways.json`, same validity rules as `gwmodel::Registry::load`: name must equal `openshell-vm|docker|podman`, port in range); `gateways::resolve(ctx: &Ctx, arg: Option<&str>) -> Result<Option<String>, String>`: `None` → `ctx.gateway.clone()`; `Some(n)` → `Err` if `n` is empty, starts with `-`, contains `/` or is not in `names(&ctx.dir)`, else `Ok(Some(n))`.
- Consumes: `ops::Ctx` (Clone, Task 1), `mcp::call`.

- [ ] **Step 1: Write failing tests.** In `gateways.rs`: `names_reads_valid_entries_only` (a file with one good entry, one with a wrong name, one with port 0 → one name); `resolve_rejects_unusable_names` for `""`, `"-x"`, `"a/b"`, `"nope"`, each `Err` with the name in the message; `resolve_none_keeps_the_default`. In `mcp.rs`: `every_tool_accepts_an_optional_gateway` (each schema in `tools/list` has a `gateway` string property and stays `additionalProperties:false`), and `an_unknown_gateway_is_invalid_params_and_runs_nothing` (use the fake `openshell` of `tests/fake-openshell.sh` and assert it was not called).
- [ ] **Step 2: Run** `cargo test -p osh-core gateway`. Expected: FAIL (module missing).
- [ ] **Step 3: Implement** the module; in `call`, allow `gateway` in every tool's `args_of` list, call `resolve`, and run the op with a `Ctx` clone whose `gateway` is the result (an `Err` is `invalid("...")`, code `-32602`). Add the property to each tool schema.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: green.
- [ ] **Step 5: Commit** `core: let every MCP tool name a gateway, limited to the ones osh knows`.

### Task 3: Settings and token file

**Files:** Modify `tui/core/src/settings.rs`, `README.md` (Settings table). Create `tui/core/src/mcpserver/mod.rs` (`pub mod token;` and the other modules as they land; `pub mod mcpserver;` in `lib.rs`) and `mcpserver/token.rs`.

**Interfaces:**
- Produces settings: `mcp.server_bind` (`Kind::Text`, default `"127.0.0.1"`, valid only if it parses as an `IpAddr`), `mcp.server_port` (`Kind::Count{min:1024,max:65535}`, default `17800`), `mcp.server_gateway` (`Kind::Text`, default `""` = the main/active gateway), `mcp.client_idle_secs` (`Secs{10,86400}`, default `600`); accessors `settings::mcp_server_bind() -> String`, `mcp_server_port() -> u16`, `mcp_server_gateway() -> Option<String>`, `mcp_client_idle() -> Duration`, all `Effect::RestartMcp` except `client_idle_secs` (Live). Token: `token::path(dir) -> PathBuf` (`<dir>/mcp-token`), `token::ensure(dir) -> io::Result<()>` (create 32 random bytes hex from `/dev/urandom`, mode `0600`, atomic; existing file with a looser mode is `chmod 0600`; empty file is replaced), `token::read(dir) -> Option<String>`, `token::matches(dir, presented: &str) -> bool` (reads the file each call, constant-time compare, `false` if unreadable).
- Consumes: the settings `Def` pattern; the `Kind`-exhaustive matches (`text_of`, `parse_value`, `check`, `row_json`, `settingsview`) all gain the two new kinds.

- [ ] **Step 1: Write failing tests.** `settings.rs`: `server_defaults_and_validation` (defaults above; `mcp.server_bind = "not an ip"` is a problem and keeps the default; port `80` is out of range), plus the existing README-row test must fail until the README rows exist. `token.rs`: `ensure_creates_a_0600_file_once` (second call keeps the content), `ensure_tightens_a_loose_file`, `matches_reads_the_file_every_time` (write a new token, old one stops matching without any restart), `matches_is_false_without_a_file`, `the_token_never_appears_in_debug_output_of_its_holder` (if any struct holds it).
- [ ] **Step 2: Run** `cargo test -p osh-core server_ token`. Expected: FAIL.
- [ ] **Step 3: Implement** as described; add the four README `## Settings` rows (env column `none`); extend every `match` on `Kind` the compiler flags.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: green (settings screen shows the new rows).
- [ ] **Step 5: Commit** `core: add the MCP server settings and the token file`.

### Task 4: HTTP server

**Files:** Create `tui/core/src/mcpserver/http.rs`. Test in the same file.

**Interfaces:**
- Consumes: `token::matches`, `mcp::handle(line, &Ctx) -> Option<String>`, `Ctx`.
- Produces: `pub struct Limits { pub max_header: usize, pub max_body: usize, pub read_timeout: Duration, pub max_conns: usize }` with `Default` (16 KiB, 4 MiB, 10 s, 64); `pub struct Request { pub method: String, pub path: String, pub headers: Vec<(String, String)>, pub body: Vec<u8> }`; `pub fn read_request(r: &mut impl Read, l: &Limits) -> Result<Request, HttpError>`; `pub fn write_response(w: &mut impl Write, status: u16, headers: &[(&str, &str)], body: &[u8]) -> io::Result<()>`; `pub trait Observer: Send + Sync { fn seen(&self, session: &str, peer: &str, client: Option<(String, String)>); }`; `pub fn serve(listener: TcpListener, dir: PathBuf, ctx: Arc<dyn Fn() -> Ctx + Send + Sync>, obs: Arc<dyn Observer>, limits: Limits, stop: Arc<AtomicBool>) -> io::Result<()>` (one thread per connection up to `max_conns`, over that cap the connection is answered `503` and closed; `ctx()` is called per request so settings changes apply).
- Behavior: only `POST /mcp` (else `404`/`405`); `Content-Length` required (`411` without; chunked → `501`); wrong or missing bearer → `401` with `WWW-Authenticate: Bearer`; body must be UTF-8 → else JSON-RPC parse error body with `200`; `initialize` replies carry a new `Mcp-Session-Id` header; a request's own `Mcp-Session-Id` (any value, trimmed to 64 printable ASCII) is reported to the observer; keep-alive supported, connection closed after an error.

- [ ] **Step 1: Write failing tests** (loopback socket on port 0 with a fake `Ctx`; `tests/fake-openshell.sh` as in `mcp.rs` tests): `a_valid_initialize_returns_json_and_a_session_id`; `no_bearer_is_401_and_wrong_bearer_is_401` and the body is never handed to `handle` (observer not called); `get_is_405_and_unknown_path_404`; `an_oversized_body_is_413_without_reading_it_all`; `an_oversized_header_is_431`; `a_slow_client_is_cut_off_at_the_read_timeout` (a 200 ms timeout in the test `Limits`); `garbage_bytes_get_400_and_the_server_keeps_serving`; `connections_over_the_cap_get_503`; `a_notification_gets_202_and_no_body`; `chunked_is_501`; `a_replaced_token_works_on_the_next_request`.
- [ ] **Step 2: Run** `cargo test -p osh-core mcpserver::http`. Expected: FAIL (module empty).
- [ ] **Step 3: Implement** parser, writer and accept loop. The accept loop polls `stop` (non-blocking accept with a short sleep, or a self-connection on shutdown). Never panic on input: every size is checked before allocation, header names compared case-insensitively.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: green.
- [ ] **Step 5: Commit** `core: serve MCP over HTTP with a token, limits and per-request settings`.

### Task 5: Client registry and state file

**Files:** Create `tui/core/src/mcpserver/state.rs`. Test in the same file.

**Interfaces:**
- Produces: `pub struct Client { pub session: String, pub peer: String, pub name: String, pub version: String, pub first_seen: u64, pub last_seen: u64 }` (Unix seconds); `pub struct State { pub pid: u32, pub listen: String, pub started: u64, pub version: String, pub clients: Vec<Client> }`; `pub struct Registry` implementing `http::Observer` with `new(dir: PathBuf, listen: String, version: String, idle: impl Fn() -> Duration + Send + Sync + 'static) -> Registry`, `flush(&self)` (atomic write of `<dir>/mcp-server.json`, mode `0600`; drops clients idle longer than `idle()`; rewrites only when something changed, at most once a second), `remove(&self)` (delete the file on clean stop); `pub fn read(dir: &Path) -> Option<State>` returning `None` when the file is missing, unparsable or `pid` is not alive (`kill(pid, 0)` via `libc`-free `Command`? use `std::fs::metadata("/proc/<pid>")` on Linux and `ps -p` through `Host` on macOS: the builder picks, behind one function `pid_alive(pid) -> bool`); `pub fn claim(dir, listen) -> Result<(), String>` that refuses when `read(dir)` is `Some` (another live server) and otherwise records ownership.
- Consumes: `http::Observer`.

- [ ] **Step 1: Write failing tests:** `a_new_session_is_listed_and_a_repeat_updates_last_seen`; `initialize_name_and_version_are_recorded`; `idle_clients_are_dropped`; `the_state_file_has_no_token_and_is_0600` (grep the file for the token string); `a_dead_pid_reads_as_off`; `claim_refuses_when_a_live_server_owns_the_file` and succeeds over a stale file; `flush_writes_atomically` (no partial file visible: write to `.tmp` then rename).
- [ ] **Step 2: Run** `cargo test -p osh-core mcpserver::state`. Expected: FAIL.
- [ ] **Step 3: Implement** with a `Mutex<Vec<Client>>`.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: green.
- [ ] **Step 5: Commit** `core: keep the MCP clients in a state file the dashboard can read`.

### Task 6: `osh mcp serve|token`

**Files:** Modify `tui/src/mcpcmd.rs` (`parse`, `run`, `USAGE`), `tui/src/main.rs` (the `mcp` arm already dispatches to `mcpcmd::run`). Tests in `mcpcmd.rs`.

**Interfaces:**
- Consumes: `mcpserver::{http, state, token}`, `build_ctx(gateway, &Snapshot) -> (Ctx, Vec<String>)`, `settings::{reload_if_changed, snapshot}` and the Task 3 accessors.
- Produces: `osh mcp serve` (foreground: `token::ensure`, `state::claim`, bind `mcp_server_bind():mcp_server_port()`, per-request ctx from the current settings with `gateway` = `mcp_server_gateway()`, SIGTERM/SIGINT set `stop`, then `Registry::remove`); `osh mcp token` prints the token and nothing else (the operator asked for it); the existing `osh mcp [--gateway NAME]` stdio mode is unchanged. A bind that is not loopback prints one stderr line saying the port is reachable by anything on that network and is protected by the token. A bind failure exits 1 with the address in the message and leaves an existing state file alone.

- [ ] **Step 1: Write failing tests:** `parse_accepts_serve_and_token_and_keeps_stdio` (`[]`, `["--gateway","x"]`, `["serve"]`, `["token"]`, `["serve","--bogus"]` is an error); `serve_per_request_context_follows_settings_changes` (the ctx closure reflects a changed `mcp.allow_share` without restart); `a_taken_port_is_a_clear_error_and_keeps_the_other_state_file`.
- [ ] **Step 2: Run** `cargo test -p osh-tui mcpcmd` through `just --justfile .justfile test`. Expected: FAIL.
- [ ] **Step 3: Implement.** `std` has no signal handling: use `libc` only if it is already a dependency of `osh-tui` (check `tui/Cargo.toml`) to set `stop` on SIGTERM and SIGINT; otherwise let SIGTERM end the process (the state file is then stale and reads as off, which Task 5 handles).
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: green.
- [ ] **Step 5: Commit** `tui: add osh mcp serve and token`.

### Task 7: The service and `osh mcp start|stop|status`

**Files:** Create `tui/src/mcpsvc.rs` (`mod mcpsvc;` in `main.rs`); make `gwops::agent_plist` `pub(crate)`; modify `mcpcmd.rs` to dispatch the three commands. Tests in `mcpsvc.rs`.

**Interfaces:**
- Produces: `mcpsvc::unit_name() -> &'static str` = `osh-mcp.service`; `agent_label() -> &'static str` = `dev.guilospanck.osh-mcp`; `systemd_unit(p: &Paths) -> String` (ExecStart `"{exe}" mcp serve`, `Restart=on-failure`, `UMask=0077`, `Environment` `OSH_DIR` and a `PATH` that includes the folders `gwops::service_env` uses, never the token); `launchd_plist(p: &Paths, logs: &Path) -> String` through `gwops::agent_plist` (extend it to take the stdout/stderr file names, `mcp.out.log`/`mcp.err.log`); `install_steps(f: &Facts, p: &Paths) -> Vec<Step>` (write the unit or plist, then enable and start: `systemctl --user daemon-reload`, `enable --now`; `launchctl bootstrap gui/<uid> <plist>`), `start_steps`, `stop_steps` (`systemctl --user stop` / `launchctl bootout`), `remove_steps`; `status(h: &dyn Host, p: &Paths) -> Svc` where `enum Svc { NotInstalled, Stopped, Running }` from the unit/plist file existing and the state file being live (Task 5 `read`).
- Consumes: `Paths`, `Facts`, `Step`, `StepKind::{Cmd, Write}`, `Host`, `FakeHost`.

- [ ] **Step 1: Write failing tests with `FakeHost`:** `the_systemd_unit_runs_mcp_serve_and_never_holds_the_token`; `the_plist_lints_and_has_label_args_and_path` (same shape as the gateway plist tests; `plutil` is not available off macOS, so compare structure); `install_steps_write_then_enable_in_order` for Linux and macOS (`after` chain); `stop_steps_exist_for_both_os`; `status_is_not_installed_without_the_unit`, `stopped_with_a_unit_and_no_live_state`, `running_with_a_live_state`.
- [ ] **Step 2: Run** `just --justfile .justfile test`. Expected: FAIL.
- [ ] **Step 3: Implement.** `osh mcp start` installs when needed then starts; `stop` stops; `status` prints `off|on`, the address and the client count; all print the consent text of each step like `osh gateway add` does and run through `setup::execute`.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: green.
- [ ] **Step 5: Commit** `tui: run the MCP server as a launchd or systemd service`.

### Task 8: The MCP tab (view)

**Files:** Modify `tui/src/app.rs` (`Tab`, `TABS`, `switch`, `idx`, refresh), `tui/src/ui.rs` (`draw` for the tab, `footer_help`, `GLOBAL_HELP` `1-5` → `1-6`), `tui/src/palette.rs` (`a(Global, "6", ..., "Go to MCP")` next to the other tab keys; widen the `'1'..='5'` key match). Test in `app.rs` / `ui.rs` `mod tests`.

**Interfaces:**
- Consumes: `mcpserver::state::read`, `mcpsvc::status`, `Svc`.
- Produces: `Tab::Mcp` and `("MCP")` in `TABS` (now 6); `App.mcp: McpView { svc: Svc, state: Option<State>, selected: usize }` refreshed on the dashboard's normal refresh tick (read-only file reads, no network); the tab shows the status line (`on at 127.0.0.1:17800` / `off` / `not installed`), then a table of clients (name, version, peer, first seen, last seen) with `j/k`.

- [ ] **Step 1: Write failing tests:** `the_mcp_tab_shows_off_when_nothing_runs`, `it_lists_clients_from_the_state_file` (a temp data dir with a state file for the current pid), `six_tabs_switch_with_tab_and_6`, `the_footer_help_and_global_help_agree_with_actions_txt` (existing test `every_footer_key_has_an_end_to_end_scenario` will fail until Task 9 adds rows).
- [ ] **Step 2: Run** `just --justfile .justfile test`. Expected: FAIL.
- [ ] **Step 3: Implement** the tab and drawing.
- [ ] **Step 4: Run** the unit tests. Expected: all but the manifest test pass; that one is fixed in Task 9, so commit after Task 9's manifest rows (do Task 8 and 9's manifest step together before running `check`).
- [ ] **Step 5: Commit** together with Task 9 (see there).

### Task 9: MCP tab actions, README, end-to-end

**Files:** Modify `tui/src/app.rs` (`tab_key` for `Tab::Mcp`), `tui/src/ui.rs` (`footer_help` for the tab), `tui/src/palette.rs`, `tests/e2e/actions.txt`, `tests/e2e/scenarios.py`, `README.md` (tab table, Settings), `AGENTS.md` (layout table: `mcpserver/`, `mcpsvc.rs`, MCP tab).

**Interfaces:**
- Consumes: `mcpsvc::{status, install_steps, start_steps, stop_steps}`, `setup::execute` consent flow used by the Gateways tab.
- Produces: tab keys `Enter` (turn the server on when off or not installed: install, start; turn it off when on: stop), `t` (copy the address). There is no token key: the token is only printed by `osh mcp token`. Footer for the tab: `Enter on/off  t copy address`.

- [ ] **Step 1: Write failing tests:** `enter_on_the_mcp_tab_offers_the_install_and_start_steps_when_off` (modal lists the `Step` titles, no host command runs before consent), `enter_when_on_offers_the_stop_step`, `t_copies_the_address` (clipboard stub as the other tabs' tests).
- [ ] **Step 2: Run** `just --justfile .justfile test`. Expected: FAIL.
- [ ] **Step 3: Implement** the keys; add rows `6`-tab (`global | 1-6 | nav-tabs`; replace the `1-5` row), `MCP | Enter | mcp-onoff`, `MCP | t | SKIP: copies to the clipboard, which no test machine has; covered by a unit test` to `actions.txt`; write scenario `mcp-onoff` in `scenarios.py` that presses `6`, checks the status line, and stops there (installing a service is outside the e2e machine's remit: assert the consent screen appears, then `esc`). Update README and AGENTS.md.
- [ ] **Step 4: Run** `just --justfile .justfile check`. Expected: green.
- [ ] **Step 5: Commit** Task 8 and 9 together: `tui: add the MCP tab with an on/off switch and the client list`.

### Task 10: The real thing on Linux

**Files:** Modify `tests/linux/real-suite.sh` (and `tests/sandbox/checks.sh` only if a sandbox is needed: it is not).

- [ ] **Step 1: Add a section** to `real-suite.sh` after the gateway scenarios: `osh mcp start` (consent given by the suite's existing non-interactive flag), poll `osh mcp status` for `on` (timeout 30 s), `curl` an `initialize` with the token read through `osh mcp token` into a shell variable (never argv: pass the header through `-H @file`), assert a 200 with `serverInfo`, assert a request without the token is 401, assert the client appears in `osh mcp status`, `osh mcp stop`, assert `off`.
- [ ] **Step 2: Ask the user**, then run `just test-linux-real podman` (it boots a disposable systemd machine; nothing on the host changes). Expected: the new section passes.
- [ ] **Step 3: Commit** `tests: prove the MCP service on a real systemd machine`.

---

## Self-review notes

- Spec coverage: server, HTTP limits, token, bind, sessions, client list, state file, settings, service on both OSes, dashboard switch and tab, `osh mcp start|stop|status|token`, per-call gateway: Tasks 1–10. Not here by design: jobs (Part 2).
- `Kind::Text` and `Kind::Count` are introduced in Task 3 and used in Tasks 6 and 8.
- Task 6 leaves signal handling to the builder because `std` has none; the fallback (stale state read as off) is covered by Task 5.
