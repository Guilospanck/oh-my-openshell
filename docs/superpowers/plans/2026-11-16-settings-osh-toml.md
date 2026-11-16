# Settings and osh.toml Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One registry of every general osh setting, a documented `osh.toml` edited without losing comments, and three ways to use it: `osh config`, a dashboard Settings screen, and the user's editor.

**Architecture:** `tui/core/src/settings.rs` (crate `osh-core`, no TUI) holds the registry, resolution (env > `osh.toml` > default), validation, the commented template, `toml_edit` edits and migration of two old files. The binary calls `settings::init(dir)` once; accessors read a shared snapshot. `tui/src/headless.rs`-style `osh config` and a Settings view in `app.rs`/`ui.rs` sit on top. Existing hardcoded numbers, `exted.rs`, `tmuxpass.rs` and `mcpcmd.rs` read the accessors.

**Tech Stack:** Rust 2021, `toml` (parse, already used), `toml_edit` (new, in `tui/core` only), ratatui (existing).

**Spec:** `docs/superpowers/specs/2026-11-16-settings-design.md` (read it first; it is binding).

## Global Constraints

- Resolution: env variable > `osh.toml` > default. Env overrides force one direction only: `OSH_TMUX_PASSTHROUGH=0` forces `shell.tmux_keys=false`; `OSH_MCP_ALLOW_SHARE=1` forces `mcp.allow_share=true`; any other value is "not overridden". `OSH_MCP_SHARE_ROOT` (PATH-style) and `OSH_MCP_REPO_VARS` (comma list) replace the file list.
- No new environment variables. The env variable is shown everywhere (template comment, `osh config` incl. `--json` fields `env` and `env_active`, Settings screen, README table).
- Registry (key, type, default, takes effect): `general.editor` auto|own|builtin, `auto`, live; `shell.tmux_keys` bool, true, live; `refresh.gateway_secs` 1..=60, 2, live; `refresh.ports_secs` 1..=60, 3, live; `refresh.usage_secs` 1..=300, 5, live; `refresh.usage_max_backoff_secs` 5..=600, 40, live; `refresh.alerts_secs` 1..=60, 4, restart; `alerts.settle_secs` 0..=3600, 60, live; `ui.toast_secs` 1..=60, 5, live; `ui.status_secs` 1..=120, 6, live; `ui.status_allowed_secs` 1..=300, 20, live; `mcp.allow_share` bool, false, restart of `osh mcp`; `mcp.share_roots` absolute-path list, `[]`, restart of `osh mcp`; `mcp.repo_vars` valid-variable-name list, `[]`, restart of `osh mcp`. Plus one read-only data-folder row (`OSH_DIR`, else `$XDG_DATA_HOME/osh`, else `~/.local/share/osh`). Defaults equal today's behavior. Interacting numbers are clamped: effective max backoff is `max(usage_max_backoff_secs, usage_secs)`.
- The registry validates shape only (absolute paths, valid variable names); existence is checked when `osh mcp` starts, as today.
- `Settings::load(dir, env)` takes an explicit folder and an env lookup function. Accessors return defaults under `osh_core::testmode::under_test()`; tests install snapshots through a lock-guarded helper. Tests never read the process env or `~/.local/state/osh`.
- Writes: only dashboard start and `osh config set|unset|edit|path` create or extend `osh.toml`. `osh config`, `osh config get`, headless commands and `osh mcp` never write. `set` refuses on an invalid `osh.toml` and prints line:column. `unset` of a missing key is a no-op. Migration deletes `editor-mode` / `.tmux-passthrough-off` only after a successful write; if both exist, `osh.toml` wins.
- Output contract for `osh config`: success one JSON line (with `--json`), exit 0; failure `{"ok":false,"error":...}` exit 1; usage error exit 2.
- Never bypass hooks; the gitleaks hook flags secret-like words and literals, so reword. Never touch `.gitleaks.toml`. Do not run `osh`, `openshell` writes or `just test-sandbox` (the controller asks first). Never read `~/.local/state/osh`. Hard time limits on every command (`gtimeout -k 5`). Stop your own background shells before reporting.
- `just check` green before every commit (run from the checkout; `cargo test -p osh-core` in `tui/` needs no Zig). Commit subjects lowercase, scoped (`core: ...`, `tui: ...`).

## Review Focus

- A comment, blank line, unknown table or unknown key in a hand-edited `osh.toml` survives `set`, `unset` and an upgrade append.
- A file with `refresh.ports_secs = 0`, a wrong type, an unknown key, or invalid TOML: the rest still applies (or last good values for invalid TOML); problems list file:line:column; nothing panics.
- `OSH_MCP_ALLOW_SHARE=0` or `=true` does not enable sharing; only `=1` does.
- Two running dashboards / a headless command while `osh.toml` is being rewritten: the write is atomic (temp file in the same folder, then rename), so a reader never sees a half file.
- Migration run twice, with only one old file present, with both present, and with an unwritable folder.

---

### Task 1: Registry, resolution, validation (read-only)

**Files:**
- Create: `tui/core/src/settings.rs`
- Modify: `tui/core/src/lib.rs` (`pub mod settings;`), `tui/core/Cargo.toml` (add `toml_edit` at a pinned version at least two weeks old; used from Task 2)
- Test: `mod tests` in `settings.rs`

**Interfaces:**
- Produces:
  - `pub enum Kind { Bool, Secs { min: u64, max: u64 }, Choice(&'static [&'static str]), PathList, NameList }`
  - `pub enum Effect { Live, Restart, RestartMcp }`
  - `pub struct Def { pub key: &'static str, pub kind: Kind, pub default: &'static str /* TOML literal */, pub effect: Effect, pub env: Option<&'static str>, pub desc: &'static str }`
  - `pub fn registry() -> &'static [Def]` (exact rows in Global Constraints, in that order; table = key prefix before the dot)
  - `pub enum Value { Bool(bool), Int(u64), Str(String), List(Vec<String>) }`
  - `pub enum Source { Default, File, Env }`
  - `pub struct Row { pub def: &'static Def, pub value: Value, pub source: Source, pub env_active: bool, pub in_file: bool }`
  - `pub struct Problem { pub key: Option<String>, pub line: Option<usize>, pub col: Option<usize>, pub msg: String }`
  - `pub struct Snapshot { pub rows: Vec<Row>, pub problems: Vec<Problem>, pub file_valid: bool, pub data_dir: PathBuf }`
  - `pub fn load(dir: &Path, env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Snapshot`
  - `pub fn parse_value(def: &Def, text: &str) -> Result<Value, String>` (the same validation `set` and the screen use; lists take comma-separated text)

- [ ] **Step 1: Write failing tests** in `settings.rs`: `defaults_when_no_file` (every row `Source::Default`, values equal the table); `file_value_wins_over_default`; `env_forces_only_one_direction` (`OSH_TMUX_PASSTHROUGH` = `0` → false/`Env`/`env_active`; `1`, `x`, unset → file/default; same for `OSH_MCP_ALLOW_SHARE` with `1`, `0`, `true`); `list_env_replaces_file_list` (PATH-style split for share roots, comma split for repo vars, empty entries dropped); `out_of_range_and_wrong_type_are_problems_not_panics` (`refresh.ports_secs = 0`, `= "x"`, `mcp.share_roots = ["rel/path"]`, `mcp.repo_vars = ["bad name"]` each give a `Problem` with key and line, and the row keeps its default); `unknown_key_is_listed_with_line`; `invalid_toml_gives_line_and_col_and_file_valid_false`; `registry_keys_unique_and_defaults_parse` (every default passes `parse_value`); `backoff_never_below_usage_secs` for the clamp helper `pub fn effective_backoff(usage: u64, max: u64) -> u64`.
- [ ] **Step 2: Run** `cd tui && gtimeout -k 5 300 cargo test -p osh-core settings` → FAIL (module missing).
- [ ] **Step 3: Implement** the interfaces. Parse with `toml`'s `Table`; take problem line/col from `toml::de::Error::span()` converted to line/col over the file text; for key-level problems find the key's line by scanning the text for the key under its table (best effort is acceptable, `None` allowed). Data-folder row is exposed as `Snapshot::data_dir` (resolved from the `OSH_DIR` / `XDG_DATA_HOME` / `HOME` env lookups), not a `Def`.
- [ ] **Step 4: Run** the same command → PASS; then `just --justfile .justfile check` from the checkout → green.
- [ ] **Step 5: Commit** `core: add the settings registry and its resolution`.

### Task 2: Template, edits, migration (toml_edit)

**Files:**
- Modify: `tui/core/src/settings.rs`
- Test: `mod tests` in `settings.rs`

**Interfaces:**
- Consumes: Task 1 registry, `parse_value`, `load`.
- Produces:
  - `pub fn template() -> String` (per table header, per setting: description comment lines, an `# Overridden by the environment variable <VAR>…` line when `def.env` is set, then the key commented out at its default)
  - `pub fn ensure_file(dir: &Path) -> Result<(), String>` (create from template when missing; when present and valid, append commented blocks for registry keys whose block is absent, never touching values; skip silently and return `Err` text only for I/O failure; do nothing when the file is invalid TOML; runs migration first)
  - `pub fn set(dir: &Path, key: &str, text: &str, env: &dyn Fn(&str) -> Option<OsString>) -> Result<(), String>` (unknown key, validation failure, active env override naming the variable, or invalid file → `Err`; otherwise uncomment/insert the key under its table, preserving everything else)
  - `pub fn unset(dir: &Path, key: &str) -> Result<(), String>` (removes the key line; missing key is `Ok`; invalid file → `Err`)
  - `pub fn migrate(dir: &Path) -> Result<(), String>`
  - All writes: temp file in `dir`, then `rename`.

- [ ] **Step 1: Write failing tests**: `template_parses_and_lists_every_key_commented`; `template_names_env_variables`; `set_uncomments_in_place_and_keeps_comments_blank_lines_unknown_keys`; `set_inserts_under_existing_table_or_creates_table`; `set_refuses_invalid_toml_with_line_col_and_leaves_file_byte_identical`; `set_refuses_env_forced_key_naming_variable`; `set_rejects_bad_value_reason`; `unset_removes_only_that_line_and_missing_is_ok`; `append_adds_only_new_commented_blocks_and_never_values`; `ensure_file_skips_invalid_file`; `migrate_editor_mode_and_tmux_marker` (external→`own`, builtin→`builtin`, marker file→`shell.tmux_keys=false`; old files deleted after write); `migrate_idempotent_and_osh_toml_wins_when_both`; `migrate_unwritable_dir_keeps_old_files` (read-only dir via permissions).
- [ ] **Step 2: Run** `cd tui && gtimeout -k 5 300 cargo test -p osh-core settings` → FAIL.
- [ ] **Step 3: Implement** using `toml_edit::DocumentMut`. Because the template keeps keys as comments, `set` must locate the commented line for the key (`# key = …`) under its table, replace it with the live line, and otherwise insert via `DocumentMut`; keep the surrounding decor.
- [ ] **Step 4: Run** tests → PASS; `just check` → green.
- [ ] **Step 5: Commit** `core: edit osh.toml with comments kept, and migrate the old files`.

### Task 3: Snapshot, accessors, `osh config`

**Files:**
- Modify: `tui/core/src/settings.rs`, `tui/src/main.rs` (`"config"` arm in `subcommand`, before `"mcp"`; calls `settings::ensure_file` only for `set|unset|edit|path`), `tui/src/headless.rs` or new `tui/src/configcmd.rs` (follow the headless pattern: argv → core call → JSON, exit 0/1/2)
- Test: `mod tests` in the touched files

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces:
  - `pub fn init(dir: &Path)` (loads once with the real env, stores globally), `pub fn reload_if_changed() -> bool` (re-reads when mtime of `osh.toml` changed; keeps last good values when the file is invalid), `pub fn snapshot() -> Arc<Snapshot>`, and typed accessors `refresh_gateway() / refresh_ports() / refresh_usage() / usage_max_backoff() / refresh_alerts() / alerts_settle() / toast_for() / status_for() / status_allowed_for() -> Duration`, `editor_pref() -> &'static str`, `tmux_keys() -> bool`, `mcp_allow_share() -> bool`, `mcp_share_roots() -> Vec<PathBuf>`, `mcp_repo_vars() -> Vec<String>`
  - Under `under_test()` accessors return registry defaults unless a snapshot was installed through `pub fn install_for_test(Snapshot) -> impl Drop` (holds a global lock).
  - `osh config [--json]`, `osh config get KEY`, `set KEY VALUE`, `unset KEY`, `path`, `edit` (opens via the existing editor selection: `$VISUAL`/`$EDITOR`/fallbacks, else prints the path and exits 1 with the reason; `edit` never opens the built-in editor headless).
  - Table columns: key, value, source, default, env variable, description. `--json` rows: `key, value, source ("default"|"file"|"env"), default, env (string|null), env_active, takes_effect, description`, and a final `problems` array in an object `{"settings":[…],"problems":[…],"data_dir":"…"}`.

- [ ] **Step 1: Write failing tests**: accessor defaults under test; `install_for_test` isolation (two tests with different snapshots cannot interleave); `reload_if_changed_applies_live_and_survives_invalid_file`; for `osh config`: `config_json_lists_every_registry_key_with_env_fields`, `config_get_prints_value`, `config_set_then_get_roundtrip_in_temp_dir`, `config_set_refused_by_env_exits_1_names_variable`, `config_unknown_key_exit_1`, `config_missing_args_exit_2`, `read_only_commands_do_not_create_the_file`. Run against a temp data dir through `OSH_DIR`-style explicit dir parameter, never the real data folder.
- [ ] **Step 2: Run** `cd tui && gtimeout -k 5 600 cargo test -p osh-core settings && cargo test -p osh-tui config` (osh-tui needs Zig: use `just --justfile .justfile test`) → FAIL.
- [ ] **Step 3: Implement.** The `config` arm resolves the folder like the other arms (`assets::data_dir()`), without `materialize_data()`.
- [ ] **Step 4: Run** → PASS; `just check` green.
- [ ] **Step 5: Commit** `tui, core: add osh config and the settings snapshot`.

### Task 4: Use the settings where the numbers and files live today

**Files:**
- Modify: `tui/src/main.rs` (call `settings::init(&dir)` and `settings::ensure_file(&dir)` right after `materialize_notes()`; `alerts::spawn_watch(…, settings::refresh_alerts())`; call `settings::reload_if_changed()` once per event-loop tick, cheap mtime check), `tui/core/src/cli.rs:1239` (poller wait), `tui/src/ports.rs:19`, `tui/src/usage.rs:21,23`, `tui/core/src/watch.rs:211`, `tui/src/app.rs:50,1404,1406` (toast, status durations), `tui/src/exted.rs` (`load_pref`/`save_pref` through settings; keep `Pref` and `PREF_FILE` only for migration), `tui/src/tmuxpass.rs` (`load`/`toggle`/`enabled` through settings; `OSH_TMUX_PASSTHROUGH=0` still wins), `tui/src/mcpcmd.rs` (`run` builds share flag, roots, repo vars from the resolved settings; the pure helpers keep their signatures and tests), `tests/e2e/scenarios.py` (`editor_mode` helper writes `general.editor` into `osh.toml` instead of `editor-mode`)
- Test: existing tests plus new ones listed below

**Interfaces:**
- Consumes: Task 3 accessors. Produces: no new public API.

- [ ] **Step 1: Write failing tests**: `editor_pref_roundtrips_through_osh_toml` (`save_pref` then `load_pref` in a temp dir; `Auto` removes the key; the old file is not recreated); `tmux_toggle_persists_in_osh_toml_and_env_zero_still_forces_off`; `old_editor_mode_file_is_migrated_on_load`; `mcp_run_reads_settings_and_env_still_wins` (pure function over a snapshot: env share roots replace file roots); `ports_and_usage_use_settings_intervals` (constructors/helpers take the `Duration` from the accessor under an installed test snapshot); `usage_backoff_is_clamped`. Existing tests stay unchanged and green.
- [ ] **Step 2: Run** `just --justfile .justfile test` → new tests FAIL.
- [ ] **Step 3: Implement.** Replace each constant with its accessor at the use site (read per iteration so `live` settings apply at once; `refresh.alerts_secs` is read once at start). No behavior change at defaults.
- [ ] **Step 4: Run** `just --justfile .justfile check` → green.
- [ ] **Step 5: Commit** `tui, core: read intervals, editor, tmux keys and mcp limits from osh.toml`.

### Task 5: Settings screen, Data files entry, docs, tests

**Files:**
- Modify: `tui/src/palette.rs` (command `Settings`), `tui/src/app.rs` (state, key handling, "Other files" list reusing `edit_file`), `tui/src/ui.rs` (draw), `core/src/assets.rs::check_for` (a check for `osh.toml` running the settings validator, errors as file:line:column) and `assets::files` (list `osh.toml`; "Restore" regenerates `settings::template()`), `README.md` (Settings section: table of every registry entry with default, takes effect, environment variable; update the two palette rows and the Headless section; remove mentions of `editor-mode` and the marker file except under migration), `AGENTS.md` layout rows, `tests/e2e/scenarios.py` + `tests/e2e/actions.txt` (scenario `settings-screen`: open via palette, filter with `/`, change a number, see it in `osh.toml`, reset with `r`, `e` opens the file), `tests/sandbox/checks.sh` (headless check: `osh config --json` lists all keys; `osh config set refresh.ports_secs 7` then `get` returns 7 and the file keeps a planted comment)
- Test: `mod tests` in `app.rs`/`ui.rs` (use `TestBackend` as the wizard tests do)

**Interfaces:**
- Consumes: Tasks 1–4. Produces: `Palette::Settings` command; screen state `Screen::Settings { sel: usize, filter: String, editing: Option<String> }`.

- [ ] **Step 1: Write failing tests**: `settings_screen_lists_every_registry_key_with_source_and_env_column`; `enter_toggles_bool_and_writes_file`; `enter_on_number_validates_and_shows_reason`; `r_resets_row_by_removing_the_key`; `env_forced_row_is_marked_and_edit_refused_naming_variable`; `slash_filters_by_key_or_description`; `invalid_file_shows_parse_error_line_col_and_e_opens_editor`; `restart_mark_shown_for_non_live_row_whose_file_value_differs_from_start`; `narrow_terminal_still_renders_without_panic`; `readme_settings_table_matches_registry` (fails when a registry key, default or env variable is missing from or extra in the README table); `osh_toml_check_reports_line_col`; `actions_txt_lists_the_screen_entries`.
- [ ] **Step 2: Run** `just --justfile .justfile test` → FAIL.
- [ ] **Step 3: Implement** per spec "Dashboard Settings screen". Palette-only (empty key) unless a free footer key exists; if one is added, `actions.txt` and the scenario cover it. The detail line of the selected row reads `<VAR> (if set) > osh.toml > default` when the setting has a variable.
- [ ] **Step 4: Run** `just --justfile .justfile check` → green. Do NOT run `just test-sandbox`; report that the controller must ask the user.
- [ ] **Step 5: Commit** `tui: add the Settings screen, osh.toml in Data files, and the settings docs`.
