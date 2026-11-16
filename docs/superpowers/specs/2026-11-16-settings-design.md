# Settings and `osh.toml`: design

Project 1 of 2. Project 2 (a long-running MCP server hosted by `osh-core`, with an MCP tab in the dashboard) comes after this one and will reuse the settings system described here.

## Intent

A person using osh should be able to see every general setting osh has, what each one does, what value is in effect and where that value comes from, and change it, without reading the code or the README. The motivating failure is: "why isn't the MCP server doing what I want?" followed by "oh, there is a setting I did not know about". Settings today are scattered (a marker file, a mode file, environment variables, numbers hardcoded in the source) and none of them is discoverable.

The model is VS Code's settings: one file you can open and edit by hand, and one screen where you browse, search and change settings one at a time. Both edit the same file.

## Scope

Only **general** settings, the ones that apply to osh as a whole. Nothing that belongs to one sandbox.

## Goals

- One registry in `osh-core` declares every setting once: key, type, default, allowed values or range, description, when a change takes effect, and an optional environment variable that overrides it.
- `osh.toml` in the data folder holds the values you set. osh creates it with every setting documented as comments, so opening the file shows what exists.
- Three ways in, all backed by the registry and the same file:
  - the file itself, opened in the editor osh already has (yours or the built-in one);
  - a Settings screen in the dashboard: browse, filter, change one setting, reset one setting, open the file;
  - `osh config` on the command line.
- Every value is shown with its source: default, `osh.toml`, or an environment variable.
- Editing from the screen or `osh config set` keeps your comments, spacing and unknown keys in `osh.toml` untouched.
- The numbers that are hardcoded today become settings, with today's values as the defaults.
- After an upgrade that adds settings, the new ones appear in `osh.toml` as documented comments, so users learn about them.

## Non-goals

- Per-sandbox settings.
- Settings that already have their own editable file and their own tooling: `toolchains.toml`, `sandbox-policy.yaml`, the gateway configs, provider profiles, Dockerfiles. The Settings screen only links to them (it names them and opens them in the editor).
- Install-time variables of `install.sh` (`OSH_VERSION`, `OSH_BIN_DIR`, `OSH_RELEASE_BASE`, `OSH_NO_SETUP`), and test or debug variables.
- The MCP server's `enabled` and `port` settings and the MCP tab: project 2. The MCP settings that exist today are included here.
- Storing secrets. Settings never hold credentials. `mcp.repo_vars` holds variable names only.

## Anti-goals (a failure even if it technically works)

- A setting that exists in the code but not in the registry, or declared in two places.
- A typo in `osh.toml` that is silently ignored.
- An upgrade that overwrites the user's `osh.toml`, or loses their comments.
- A value from an environment variable silently overriding the file without the screen saying so.
- Unit tests that read or write the developer's real `osh.toml`.

## Constraints

- Repo rules in `AGENTS.md` apply: `just check` green; unit tests for behavior; README tables and key lists in step with the code; every key in a tab footer or the global footer is in `tests/e2e/actions.txt` with an end-to-end scenario (palette actions and `osh` subcommands get one too where `actions.txt` asks for it).
- The registry and file handling live in `osh-core` (no TUI dependency, builds without Zig), so `osh mcp`, the headless commands and the future MCP server use the same code.
- Tests must never touch the real data folder: use `osh_core::testmode::under_test()` and temp folders.
- New crate allowed: `toml_edit` (see Approaches). No other new dependencies.
- Portable code only; no hard-coded `/opt/homebrew` outside macOS paths.

## Approaches considered

How to store settings and write changes back from the screen:

- **A. Registry in code, generated documented `osh.toml`, hand-written line edits.** No new crate. Rejected: the editing code has many edge cases (spacing, trailing comments, arrays) to get right and keep right.
- **B. Same registry, edits through the `toml_edit` crate.** Preserves the whole file exactly. Costs one dependency. **Chosen by the user.**
- **C. Rewrite the whole file from the registry on every change.** Rejected: it erases the user's comments, which defeats opening the file and editing it.

## Decisions made with the user

- General settings only. `osh.toml` in the data folder. Edit one by one from the TUI, or open the file in osh's editor.
- Include every setting, including the ones hardcoded today.
- Edits go through `toml_edit`.
- The registry lives in `osh-core`.
- Order: settings first, MCP server second.

## Design (the builder's starting point; calls marked *call* were made by the author, not the user)

### Resolution order *call*

For each setting: environment variable (when the setting has one) > `osh.toml` > default. The Settings screen and `osh config` show the winning source. A row whose value comes from an environment variable cannot be changed from the screen or `osh config set`; the attempt is refused with a message naming the variable.

### Initial settings *call* (key, type, default, takes effect, environment override, what it does)

| Key | Type | Default | Takes effect | Env override | Replaces / meaning |
|---|---|---|---|---|---|
| `general.editor` | `auto` / `own` / `builtin` | `auto` | live | none | Which editor edit actions use. Replaces the `editor-mode` file. |
| `shell.tmux_keys` | bool | `true` | live | `OSH_TMUX_PASSTHROUGH=0` forces `false` | Host tmux keys inside sandbox shells. Replaces the `.tmux-passthrough-off` marker. |
| `refresh.gateway_secs` | integer seconds, 1..=60 | 2 | live | none | How often gateway, sandbox and provider state is read (`cli::spawn_poller`). |
| `refresh.ports_secs` | integer seconds, 1..=60 | 3 | live | none | How often the sandbox in view is asked for listening ports (`ports.rs`). |
| `refresh.usage_secs` | integer seconds, 1..=300 | 5 | live | none | How often the CPU/memory/disk meter reads (`usage.rs`). |
| `refresh.usage_max_backoff_secs` | integer seconds, 5..=600 | 40 | live | none | Longest wait after meter reads keep failing. |
| `refresh.alerts_secs` | integer seconds, 1..=60 | 4 | restart | none | How often shared folders are scanned for risky or changed files (`alerts::spawn_watch`). |
| `alerts.settle_secs` | integer seconds, 0..=3600 | 60 | live | none | After an allow, repeats of the same denial within this time are treated as in flight, not a new request (`watch.rs`). |
| `ui.toast_secs` | integer seconds, 1..=60 | 5 | live | none | How long an alert toast stays (`TOAST_FOR`). |
| `ui.status_secs` | integer seconds, 1..=120 | 6 | live | none | How long a status message shows over a shell. |
| `ui.status_allowed_secs` | integer seconds, 1..=300 | 20 | live | none | The same for "Allowed ..." messages (the cue to retry). |
| `mcp.allow_share` | bool | `false` | restart of `osh mcp` | `OSH_MCP_ALLOW_SHARE=1` forces `true` | Whether agents may mount host folders. |
| `mcp.share_roots` | list of absolute paths | `[]` | restart of `osh mcp` | `OSH_MCP_SHARE_ROOT` (PATH-style) | Folders agents may share. |
| `mcp.repo_vars` | list of variable names | `[]` | restart of `osh mcp` | `OSH_MCP_REPO_VARS` (comma-separated) | Variables agents may name as a git credential. |

Also shown as one **read-only row**: the data folder (`OSH_DIR`, else `$XDG_DATA_HOME/osh`, else `~/.local/share/osh`). It cannot be in `osh.toml` because the file lives inside it.

The ranges and exact key names above are the author's proposal; the builder may adjust a range if the code shows a reason, and records it in the registry. Defaults must equal today's behavior. "Takes effect" is a registry field with three values: `live` (the running dashboard applies it), `restart` (after the dashboard restarts), `restart of osh mcp` (the next `osh mcp` start).

### `osh.toml`

- Path: `<data>/osh.toml`. Created on first use (dashboard start, or any `osh config` command) if missing; never overwritten.
- The template lists every setting as a commented block: a description comment, then the key commented out with its default, grouped under its table:

  ```toml
  [general]
  # Which editor the edit actions use: "auto" (yours when found, else the built-in one),
  # "own" (always yours), "builtin" (always the built-in one).
  # editor = "auto"
  ```
- Setting a value uncomments or inserts that key under its table; resetting a value removes the key line (the documented block stays). Comments, spacing, unknown keys and tables are preserved.
- When osh later adds settings, their commented blocks are appended to the matching tables on the next start. Values are never added, changed or removed by an upgrade.
- Unknown keys and keys of the wrong type are **not** silently ignored: `osh config` and the Settings screen list them with the file and line, and the rest of the file still applies. A file that is not valid TOML is ignored as a whole, the last good values stay in effect in a running dashboard, and the screen shows the parse error with line and column and offers the editor (the same pattern `toolchains.toml` uses).
- `osh.toml` appears under `Data files` like the other data files (view, edit, compare, restore). "Restore" regenerates the template.

### Migration *call*

On first load, if `editor-mode` or `.tmux-passthrough-off` exists in the data folder and `osh.toml` has no value for the matching key, write the value into `osh.toml` and delete the old file. If `osh.toml` cannot be written, keep reading the old file and do not delete it. If both exist, `osh.toml` wins. The migration is idempotent. The existing palette actions "Editor: switch auto / your own / built-in" and "Host tmux keys in sandbox shells: switch on or off" stay and now write to `osh.toml`.

### Live reload *call*

The dashboard re-reads `osh.toml` when its modification time changes (checked in the existing event loop) and applies `live` settings at once. A setting whose value in the file differs from the value the running process started with, and whose "takes effect" is not `live`, shows an "applies after restart" mark in the Settings screen.

### `osh config` *call*

- `osh config`: a table of key, value, source, default, **environment variable**, description. The environment column always names the variable that overrides the setting (blank when it has none), and the source column says `env` when that variable is the one currently winning. `--json` prints the same as a JSON array (fields include `env`: the variable name or `null`, and `env_active`: bool) (for factory apps), following the existing headless output contract (exit 0 with JSON; `{"ok":false,"error":...}` and exit 1 on failure; exit 2 for usage errors).
- `osh config get KEY`, `osh config set KEY VALUE`, `osh config unset KEY`, `osh config path` (prints the `osh.toml` path), `osh config edit` (opens it in the user's editor, else the built-in one).
- `set` validates the value against the registry type and range and refuses with the reason; it refuses keys that an environment variable currently overrides, naming the variable.

### Dashboard Settings screen *call*

A full-screen view opened from the command palette (`Settings`), plus a key if a free one exists. Rows show key, value, source, the environment variable that overrides it (when it has one, shown whether or not it is set, and highlighted when it is the active source) and description. The detail line of a selected row spells out the precedence: `<VAR> (if set) > osh.toml > default`. Enter edits the selected row (bool toggles, enum chooses from a list, numbers and lists take typed input with the same validation as `set`). `r` resets the row, `e` opens `osh.toml` in the editor, `/` filters rows by key or description. Rows that an environment variable controls are marked and cannot be edited. The file-based settings that live elsewhere (`toolchains.toml`, policy, gateway configs) appear in a short "Other files" list that opens each in the editor. The layout details (column widths, narrow-terminal fallback, colours) are the builder's, following the existing views.

### Where the code goes

- `tui/core/src/settings.rs`: the registry, typed accessors, resolution with sources, parse and validation, template generation, edit and migration through `toml_edit`. Typed accessors (for example `settings::refresh_ports()`) read a shared, thread-safe snapshot that the dashboard refreshes on file change; code that has a hardcoded constant today (`ports.rs`, `usage.rs`, `alerts::spawn_watch` call in `main.rs`, `TOAST_FOR`, the status durations in `app.rs`, `watch::SETTLE`, the poller wait in `cli.rs`) reads the accessor instead.
- `exted.rs` (`PREF_FILE`) and `tmuxpass.rs` (`OFF_FILE`) read and write through the settings module.
- `tui/src/mcpcmd.rs` builds its share and repo settings from the resolved settings (environment still wins).
- `tui/src/` gets the Settings view and its wiring; `tui/src/main.rs` gets the `config` subcommand arm and, for the headless commands, calls the same loader.
- Docs: a README "Settings" section whose table lists every registry entry with its default, takes-effect value and environment variable, with a unit test that fails when the README table and the registry disagree (like the footer-key test). `AGENTS.md` layout rows. `tests/e2e/scenarios.py` gets a scenario for the Settings screen and `actions.txt` its entries.

### Environment variables are visible *call* (asked for by the user)

The environment variable that can supersede a setting is part of the registry entry and is shown everywhere the setting is: the `osh.toml` template (an extra comment line, for example `# Overridden by the environment variable OSH_MCP_ALLOW_SHARE=1.`), `osh config` and `--json`, the Settings screen, and the README table. Today only `shell.tmux_keys`, the three `mcp.*` settings and the data folder (`OSH_DIR`) have one. This change adds no new environment variables; a setting gets one only by a later decision.

### Further calls (from the builder check; all *call*, the author's)

- **Environment overrides force one direction only.** `OSH_TMUX_PASSTHROUGH=0` forces `shell.tmux_keys = false`; any other value (unset, `1`, garbage) is "not overridden". `OSH_MCP_ALLOW_SHARE=1` forces `mcp.allow_share = true`; any other value is "not overridden". This matches how the code reads them today. A row is locked (not editable) only while its variable actually forces a value.
- **List settings: the environment variable replaces the file list; it does not merge.** `OSH_MCP_SHARE_ROOT` is read PATH-style, `OSH_MCP_REPO_VARS` comma-separated, trimmed, empty entries dropped, exactly as `osh mcp` does today. The Settings screen shows the effective list and its source. A malformed entry (for example a non-absolute share root) is dropped with a warning line, as today.
- **Registry validation vs runtime checks.** The registry validates shape only: `mcp.share_roots` entries must be absolute paths; `mcp.repo_vars` entries must be valid variable names (no `=`, no NUL). Whether a share root exists is checked when `osh mcp` starts (today's behavior, a stderr line per dropped entry), not when the value is saved.
- **The settings module is explicit about its data folder, and tests stay isolated.** `Settings::load(dir)` reads one folder and never touches global state. The binary calls `settings::init(dir)` once at start to install the shared snapshot that the typed accessors read; the dashboard replaces the snapshot when `osh.toml` changes. Under `under_test()` the accessors return the defaults and never read disk. A test that needs other values installs a snapshot with a helper that holds a lock for its duration, so parallel tests cannot interfere. The detail of the API is the builder's, but these three properties are required: explicit directory, defaults under test, no cross-test interference.
- **The "applies after restart" mark covers the dashboard process only.** The dashboard has no knowledge of a separate `osh mcp` process, so `mcp.*` rows show their "takes effect" text ("next start of `osh mcp`") and no mark. Project 2 revisits this once the server is hosted by osh.
- **Interacting numbers are clamped, not cross-validated.** The effective maximum backoff is `max(refresh.usage_max_backoff_secs, refresh.usage_secs)`. Each setting is still validated against its own range.
- **Write commands and invalid files.** `osh config set` on an `osh.toml` that is not valid TOML refuses and prints the parse error with line and column; it never rewrites an invalid file. `osh config unset KEY` for a key that is not in the file succeeds as a no-op. Creating the file and appending new commented blocks happens only on dashboard start and on `osh config set`, `unset`, `edit` and `path`; it is skipped when the file is invalid. Read-only commands (`osh config`, `osh config get`, the headless commands, `osh mcp`) never write; when the file is missing they use the defaults.
- **One edit path for files.** The Settings screen's "Other files" list and its "open `osh.toml`" action go through the existing `Data files` machinery, so they use the same editor choice and the same per-file check before saving. `osh.toml` gets a registered check there that runs the settings validator and reports errors as file:line:column.
- **Settings screen key.** Palette-only unless a free footer key exists. If a key is added, `tests/e2e/actions.txt` and a scenario cover it; the README key tables follow the code either way.

## Left to the builder

Exact key names and ranges where the code shows a reason to change them; the Settings screen layout and its narrow-terminal behavior; how typed accessors cache the snapshot; the template wording of each description; the choice of a free key for the Settings screen, or palette-only; test structure.
