# MCP tab jobs list: cursor, scrolling, search

> Execute inline (native), then merge to `main` locally with `git merge --ff-only`. Do not push.

**Goal:** the Jobs table on the MCP tab can be walked through (j/k, up/down arrows), shows every job newest first, scrolls with the cursor and can be searched with `/`.

**Out of scope (pass 2, separate plan):** the same search for Sandboxes, Providers, Profiles, Images, Gateways. They already scroll with j/k; none has a filter yet. Pass 2 reuses the filter helper written in Task 2.

**Decisions (from the user):** page size = as many rows as fit; no job limit; no PageUp/PageDown (j/k and arrows only); Jobs first.

## Design

- Today: `McpView.jobs` is cut at 50, the table is drawn with cursor 0 (so nothing moves), and its height is capped at half the area.
- ratatui's `TableState` already scrolls to keep the selected row visible, so "paging" is the cursor walking the list. The panel title already shows `i/n` (`panel(title, Some((cursor, n)))`).
- On the MCP tab, `j`/`k`/arrows move the **jobs** cursor when there are jobs (the clients list has no actions); with no jobs they move the clients cursor as before.
- `/` starts a filter (case-insensitive substring over id, gateway, sandbox, command, state). Typing edits it, Enter keeps it, Esc clears it. While a filter is set the title shows it, `i/n` counts the matches.
- The jobs panel takes all the height the clients panel does not need (clients: `min(n+4, 8)` lines).

## Tasks

### Task 1: a jobs cursor and uncapped list

Files: `tui/src/app.rs` (`McpView`, `read_jobs`, `mv`, `len`, `refresh_mcp`), `tui/src/ui.rs` (`draw_mcp`).

- [ ] Tests (app.rs `mod tests`): `jobs_are_not_cut_at_fifty` (60 job files → 60 rows, newest first); `j_and_k_move_the_jobs_cursor_on_the_mcp_tab` (with jobs; Down/Up arrows too); `the_jobs_cursor_stays_in_range_when_jobs_disappear`; `without_jobs_j_moves_the_client_cursor_as_before`. Watch them fail.
- [ ] Add `McpView.job_cursor: usize`; `read_jobs` drops `truncate(50)` (jobs are pruned after a week already; hard ceiling 2000 as a guard).
- [ ] `mv`: on `Tab::Mcp` with non-empty visible jobs, move `job_cursor` (clamped) and return; `refresh_mcp` clamps it to the visible count.
- [ ] `draw_mcp`: pass `app.mcp.job_cursor` to `draw_table` (so the row is highlighted and the table scrolls); jobs area = all of `rows[1]` below the clients panel. Add a `TestBackend` test in ui.rs: 30 jobs in a 24-line terminal, cursor on job 25 → that row is on screen and the title reads `26/30` style `25/30`.
- [ ] `just --justfile .justfile check`; commit `tui: walk through the MCP tab's jobs with j/k and arrows`.

### Task 2: search

Files: `tui/src/app.rs`, `tui/src/ui.rs`, `tui/core/src/text.rs` (or a new small `tui/src/listfilter.rs`) for `matches(query, fields) -> bool`.

- [ ] Tests: `matches` (case-insensitive, empty query matches all, any field); app: `slash_starts_a_job_filter_and_typing_narrows_the_list`, `enter_keeps_the_filter_and_esc_clears_it`, `j_and_k_are_text_while_typing_the_filter`, `a_filter_with_no_match_shows_no_jobs_and_a_hint`, `the_cursor_resets_to_the_first_match`.
- [ ] `McpView.filter: String`, `McpView.filtering: bool`; `visible_jobs()` returns the matching jobs. `on_key` handles the filter's keys before the global ones while `filtering` (so `q`, `1`-`6`, `j` type text; Backspace deletes; Enter ends typing; Esc clears and ends).
- [ ] `/` on the MCP tab starts filtering (only when the tab has jobs or a filter is set). Footer for the tab: `Enter on/off  t copy address  / search jobs`; while typing show `/<text>_` in the jobs panel title.
- [ ] Docs and rules: README tab table row and key list, help screen and palette entry (`palette.rs`, one action `Search MCP jobs`), `tests/e2e/actions.txt` + a scenario in `tests/e2e/scenarios.py` (seed three job files, press `6`, `/`, type, check the screen). Unit test that checks footer keys vs `actions.txt` must pass.
- [ ] `just check`; commit `tui: search the MCP tab's jobs with /`.

### Task 3: real screen check and merge

- [ ] Ask before running it: the e2e scenario on the disposable podman machine (`OSH_E2E_ONLY=<id> tests/linux/real.sh podman`).
- [ ] Whole-branch review of the diff (one reviewer, Opus, because key handling and state are involved); fix Critical/Important only.
- [ ] `git merge --ff-only` into `main`.

## Review focus

- Typing `q`, `1`-`6`, `g`, `c`, `r`, `:` inside the filter must not trigger their global actions.
- Jobs appearing or finishing during the 2-second refresh while a filter is set or the cursor is deep in the list: cursor stays on a valid row.
- A filter that matches nothing, then Esc.
- Narrow terminal: the table drops columns but the cursor row still shows its full line.
