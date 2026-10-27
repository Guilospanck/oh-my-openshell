# Gateway wizard and multi-gateway Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A first-run, step-by-step TUI wizard that lets the user tick gateway types (VM, Docker, Podman) for their machine, installs what is missing, and an "add gateway" flow on the Gateways tab; every gateway is named after its type; Linux can do everything macOS can.

**Architecture:** A pure model (`gwmodel.rs`: kinds, registry, availability from `Facts`) feeds the existing step planner (`setup.rs`) and gateway step builders (`gateway.rs`); a ratatui wizard (`wizard.rs`: a pure state machine, pure draw functions, and a thin runner that executes the plan on a thread and streams progress) is opened on first run and from the Gateways tab. Everything host-touching stays behind `Host` and returns `Step`s.

**Tech Stack:** Rust 2021, ratatui 0.30 (already used), crossterm, serde_json. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-27-gateway-wizard-design.md` (read it first; it holds the approved screens, the evidence, and "Calls made after the builder check"). Approved mockup: `.superpowers/brainstorm/1601-1793115800/content/flow-b-v3.html` (screens reproduced in the spec).

## Global Constraints

- `just check` (fmt, clippy `-D warnings`, tests) green after every task; add unit tests in the file's `mod tests` for all new behaviour; keep README and footer keys in step (`a` add, `x` remove on the Gateways tab).
- Never pass credentials in argv or print them. No `--no-verify`. Commit subjects lowercase, scoped (`tui: ...`, `tests: ...`).
- Host-touching code goes through `Host` and returns `Step`s (tested with `FakeHost`). No new crates.
- Gateway names: `openshell-vm`, `openshell-docker`, `openshell-podman`. Main port 17670; extras lowest free from 17671, health +10, metrics +20.
- Never rename, restart or reconfigure a gateway that is not in the registry (legacy `openshell`, `openshell-docker`).
- Every command is shown before it runs; without a terminal nothing runs unless `--yes`.
- The wizard must not panic and must not overlap text at 80x24 and at 60x20 (stacked fallback under 70 columns).
- Linux real tests (`just test-linux-real`) must keep passing; extend them for two gateways.

## Review Focus

1. Re-running the wizard after a "log out and back in" stop resumes (pending selection in `wizard.json`) and does not duplicate gateways.
2. Two gateways whose ports/names collide with an existing legacy gateway or a port already bound.
3. `Esc`/`q` at any wizard step leaves the machine exactly as it was (before Install) and `osh` still opens the dashboard if OpenShell exists.
4. A type that is unavailable (no `/dev/kvm`) cannot be ticked, and `--gateways vm` on such a machine fails with the reason, not a half install.
5. Removing the main while extras exist is refused; removing an extra leaves the main and the others answering.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `tui/src/gwmodel.rs` (new) | `GwKind`, `GatewaySpec`, `Registry` (file `<data>/gateways.json`), port allocation, `Availability`/`Need` from `Facts`, wizard persistence file |
| `tui/src/platform.rs` | `Facts` gains `kvm`, `colima`, `docker_desktop`, `podman_machine`; `Host::run_streaming` |
| `tui/src/gateway.rs` | per-gateway config text, systemd unit / launchd agent text, add/remove/register/reset steps, `osh gateway-service <name>` generalised |
| `tui/src/setup.rs` | `Options.gateways`, requirement steps per type, ordering, `execute` progress events |
| `tui/src/wizard.rs` (new) | state machine, draw functions, runner, persistence glue |
| `tui/src/main.rs`, `app.rs`, `ui.rs` | first-run hook, `--gateways`, `gateway add|remove` subcommands, Gateways tab keys and table |
| `tests/linux/real-suite.sh`, `tests/linux/suite.sh`, `README.md`, `AGENTS.md` | extended tests and docs |

---

### Task 1: Model and registry (Sonnet)

**Files:** Create `tui/src/gwmodel.rs`; Modify `tui/src/main.rs` (`mod gwmodel;`).

**Interfaces:**
- Produces:
  - `#[derive(Clone, Copy, PartialEq, Eq, Debug)] enum GwKind { Vm, Docker, Podman }` with `fn name(self) -> &'static str` (`openshell-vm|docker|podman`), `fn label(self) -> &'static str` (`VM|Docker|Podman`), `fn describe(self) -> &'static str` (one sentence, see spec), `fn parse(s: &str) -> Option<GwKind>` (accepts `vm|docker|podman` and the full names), `const ALL: [GwKind; 3]`, `fn from_driver(d: &str) -> Option<GwKind>` (`vm|docker|podman`).
  - `struct GatewaySpec { name: String, kind: GwKind, port: u16, main: bool }` with `fn health_port(&self) -> u16` (+10), `fn metrics_port(&self) -> u16` (+20).
  - `struct Registry { gateways: Vec<GatewaySpec> }` with `Registry::load(dir: &Path) -> Registry` (missing/garbled file = empty), `fn save(&self, dir: &Path) -> io::Result<()>` (`<dir>/gateways.json`), `fn main(&self) -> Option<&GatewaySpec>`, `fn get(&self, name: &str) -> Option<&GatewaySpec>`, `fn has_kind(&self, k: GwKind) -> bool`, `fn add(&mut self, s: GatewaySpec)`, `fn remove(&mut self, name: &str)`.
  - `fn next_port(taken: &[u16], is_bound: &dyn Fn(u16) -> bool) -> u16` (lowest from 17671 not in `taken`, not bound, and with port+10 and port+20 also free of `taken`).
  - `struct Pending { kinds: Vec<GwKind> }` with `Pending::load(dir)`, `save(dir)`, `clear(dir)` (`<dir>/wizard.json`).
- [ ] **Step 1: Write failing tests** in `gwmodel.rs`: `kind_names_and_parsing` (`parse("podman")`, `parse("openshell-vm")`, `parse("lxc")==None`), `ports_derive_from_the_gateway_port`, `registry_round_trips_through_a_file`, `garbled_registry_loads_empty`, `next_port_skips_taken_and_bound_ports` (taken [17671], bound 17672 → 17673; also skips a port whose +10 is taken), `main_is_the_first_added_flagged_main`, `pending_selection_round_trips_and_clears`.
- [ ] **Step 2:** `just test` → FAIL (module missing). **Step 3:** implement with `serde_json`; `GwKind::describe` texts: VM "Each sandbox in its own microVM. Strongest isolation.", Docker "Sandboxes run as containers. Honours CPU and memory limits.", Podman "Like Docker, but daemonless and rootless." **Step 4:** `just test` PASS. **Step 5:** commit `tui: model gateway kinds and a registry`.

### Task 2: Machine facts and availability (Sonnet)

**Files:** Modify `tui/src/platform.rs` (Facts, `gather_for`, tests), `tui/src/gwmodel.rs`.

**Interfaces:**
- Consumes: `Facts`, `Host`, `GwKind`.
- Produces:
  - `enum Kvm { Absent, NoAccess, Usable }`, `enum PodmanMachine { None, Stopped, Running }`; `Facts` gains `kvm: Kvm`, `colima: bool`, `docker_desktop: bool`, `podman_machine: PodmanMachine`, `has_e2fsprogs: bool`.
  - In `gwmodel.rs`: `struct Need { text: String, ok: bool, action: String }` (action e.g. "will be installed", "will be enabled", "start it yourself", or empty when ok); `enum State { Ready, NeedsInstall, Unavailable(String) }`; `struct Availability { state: State, needs: Vec<Need> }`; `fn availability(f: &Facts, k: GwKind) -> Availability`.
  - Detection rules: Kvm via `h.read`/`h.run("test",["-r","/dev/kvm"])`-style probe: `Absent` when `/dev/kvm` missing, `NoAccess` when present but `test -r -w` fails, else `Usable`; macOS always `Usable` for VM (Hypervisor.framework) and `has_e2fsprogs` from `brew --prefix e2fsprogs` / `which mke2fs`; `colima` = `which colima`; `docker_desktop` = macOS `/Applications/Docker.app` exists (`Host::read` cannot test dirs: add `Host::exists(&self, p:&Path)->bool`); `podman_machine` from `podman machine list --format {{.Running}}` output (macOS only).
  - `availability` per spec table: VM Linux: Absent → `Unavailable("no /dev/kvm: virtualization is off or unsupported on this machine")`, NoAccess → needs a `kvm` group step, plus e2fsprogs need; Docker: ready when `runtimes_ready` has Docker; needs "docker" (install) and "docker group" (Linux) otherwise; macOS needs Colima (install `colima docker`, then start) unless Docker Desktop present (then need "start Docker Desktop" with `action: "start it yourself"` and state `NeedsInstall` only for the non-ready part); Podman: ready when `runtimes_ready` has Podman; macOS adds the machine (`init`/`start`).
- [ ] **Step 1: Failing tests** (`platform.rs`): `kvm_absent_when_dev_kvm_is_missing`, `kvm_no_access_when_present_but_not_usable`, `colima_and_docker_desktop_are_detected`, `podman_machine_state_is_parsed`; (`gwmodel.rs`): `vm_is_unavailable_without_kvm_with_a_reason`, `vm_needs_the_kvm_group_when_present_but_not_accessible`, `docker_ready_when_the_daemon_answers`, `docker_on_linux_needs_install_group_and_login`, `docker_on_macos_offers_colima_or_asks_to_start_docker_desktop`, `podman_on_macos_needs_the_machine`, `a_need_that_is_met_has_no_action`.
- [ ] **Step 2-4:** implement, update the `facts()` literal helpers in `setup.rs`/`gateway.rs` tests, `just check`. **Step 5:** commit `tui: detect kvm, colima and podman machine and describe what each gateway type needs`.

### Task 3: Per-gateway config, services and registration steps (Opus: state and system-service critical)

**Files:** Modify `tui/src/gateway.rs`, `tui/src/assets.rs` (templates if any), `tui/src/main.rs` (`gateway-service`).

**Interfaces:**
- Consumes: `GatewaySpec`, `GwKind`, `Registry`, `Facts`, `Paths`, `Host`, `Step`.
- Produces (all pure, tested with `FakeHost`):
  - `fn gateway_toml(spec: &GatewaySpec, os: Os) -> String`: `[openshell] version = 2`; `[openshell.gateway] name, bind_address/health/metrics (extras only; main keeps the packaged defaults), compute_driver`; driver table with `allow_driver_config = true`, `enable_bind_mounts = true`, `[openshell.drivers.<k>.resource_admission] enabled = false` for docker/podman/vm as in the existing files; macOS Docker extra adds `grpc_endpoint = "https://host.docker.internal:<port>"`. Every generated text must pass `openshell-gateway config preflight --path`.
  - `fn systemd_unit(spec: &GatewaySpec, exe: &Path, data: &Path) -> String` derived from the packaged unit: `ExecStart=<exe> gateway-service <name>`, `Environment=OPENSHELL_LOCAL_TLS_DIR=%h/.local/state/openshell/tls`, `EnvironmentFile=-%E/openshell/gateway.env`, `Restart=on-failure`, `WantedBy=default.target`.
  - `fn launchd_plist(spec, exe, logs) -> String` (generalise `docker_gateway_plist`; label `dev.guilospanck.<name>`).
  - `fn add_gateway_steps(f: &Facts, p: &Paths, h: &dyn Host, reg: &Registry, spec: &GatewaySpec) -> Vec<Step>`: for an **extra**: write `<data>/gateways/<name>.toml` (kept if user-edited), write unit/plist (+`systemctl --user daemon-reload`, `enable --now` / `launchctl bootstrap`), copy mTLS dir from the main's `gateways/<main>/mtls` (or `openshell` for a legacy main), `openshell gateway add https://localhost:<port> --local --name <name>` unless already listed (restore the previously selected gateway), wait step for `-g <name>`, and a registry write step.
  - `fn main_gateway_steps(f, p, h, spec: &GatewaySpec, installing: bool) -> Vec<Step>`: write the main's `gateway.toml` with `name`, restart the packaged service (existing `plan` logic), and when `installing`: copy certs `gateways/openshell/mtls` → `gateways/<name>/mtls`, `openshell gateway add https://127.0.0.1:17670 --local --name <name>`, `openshell gateway select <name>`, `openshell gateway remove openshell`, registry write. macOS non-VM main: apply spec call 8 (own launchd agent on 17670 with Homebrew service stopped); document the choice in the ledger.
  - `fn remove_gateway_steps(f, p, reg: &Registry, name: &str, delete_data: bool) -> Result<Vec<Step>, String>`: refuses the main while extras exist and gateways not in the registry; stops/disables the service/agent, removes unit/plist, `openshell gateway remove <name>`, deletes generated config (and the database dir only when `delete_data`), registry write.
  - `reset_steps` generalised: `reset_steps(f, p, gateway, kind: Option<GwKind>, rt)` uses the gateway's own database dir (`gateway` for the main, `gateway-<kind>` for extras, legacy dirs unchanged) and its own service stop/start.
  - `osh gateway-service <name>` execs `openshell-gateway --config <data>/gateways/<name>.toml` with `OPENSHELL_LOCAL_TLS_DIR` and `OPENSHELL_DB_URL=sqlite:<state>/gateway-<kind>/openshell.db?mode=rwc`; the legacy `docker` argument keeps working.
  - `fn registry_write_step(reg_after: &Registry, data: &Path) -> Step` (a `StepKind::Write` of the JSON).
- [ ] **Step 1: Failing tests:** `extra_toml_has_its_own_name_and_ports`, `main_toml_keeps_the_packaged_ports_and_sets_the_name`, `macos_docker_extra_points_sandboxes_at_host_docker_internal`, `systemd_unit_runs_gateway_service_with_the_shared_tls_dir`, `plist_label_is_per_gateway`, `adding_an_extra_copies_certs_registers_and_waits`, `adding_an_extra_does_not_change_the_selected_gateway`, `fresh_main_replaces_the_installers_registration`, `existing_openshell_is_left_alone_when_it_was_not_installed_by_this_run`, `remove_refuses_the_main_while_extras_exist`, `remove_refuses_gateways_not_in_the_registry`, `remove_keeps_data_unless_asked`, `reset_uses_the_gateways_own_database_dir`, `generated_config_is_kept_when_the_user_edited_it`.
- [ ] **Step 2:** FAIL. **Step 3:** implement each signature above (one line of approach per function in a code comment, bodies follow the existing `docker_gateway_steps`/`plan` style). **Step 4:** `just check`; additionally run `openshell-gateway config preflight --path` on each generated TOML inside a Linux container with the real package (one-off, recorded in the ledger). **Step 5:** commit `tui: build per-gateway config, services and registration steps`.

### Task 4: Planner, flags and subcommands (Sonnet; Opus review of ordering)

**Files:** Modify `tui/src/setup.rs`, `tui/src/main.rs`.

**Interfaces:**
- Consumes: Tasks 1-3.
- Produces:
  - `Options.gateways: Vec<GwKind>` (first = main); `plan(f, o)` turns each wanted kind's `availability` needs into `Step`s (reusing the runtime/kvm/colima/podman-machine/e2fsprogs steps), keeps the existing Linux ordering (curl → runtime and its user-level needs → OpenShell → main gateway → extras → waits), and with `o.runtime` set and `gateways` empty behaves as `gateways = [that runtime]`.
  - New step ids: `kvm-group` (+ `kvm-relogin` Manual after it, OpenShell/VM gateway wait after it), `colima`, `colima-start`, `podman-machine-init`, `podman-machine-start`, `e2fsprogs`.
  - `execute_events(steps, h, o, ask, on_event: &mut dyn FnMut(Event))` with `enum Event { Start(usize), Line(usize, String), Done(usize, Outcome) }`; `execute` becomes a wrapper. Steps run through `Host::run_streaming(prog, args, &mut dyn FnMut(&str)) -> Result<(), String>` when `o.capture` is set (RealHost: spawn, merge stdout/stderr, call the sink per line; default impl of the trait method falls back to `run_inherit`); with `o.sudo_noninteractive` the planner/executor rewrite `sudo X` to `sudo -n X`.
  - CLI: `osh setup --gateways docker,podman` (comma list, order = main first; `vm` on a machine without KVM exits 2 with the reason), `osh gateway add <vm|docker|podman> [--yes]`, `osh gateway remove <name>` (typed confirmation `remove`; `--delete-data` needs typed `remove and delete data`), `osh gateway-docker` = `gateway add docker`.
- [ ] **Step 1: Failing tests:** `gateways_flag_orders_main_first`, `vm_without_kvm_is_refused_with_the_reason`, `plan_for_docker_and_podman_installs_both_runtimes_before_openshell_on_linux`, `plan_for_vm_needs_kvm_group_then_relogin_before_the_gateway`, `plan_installs_colima_and_starts_it_for_docker_on_macos`, `plan_inits_and_starts_the_podman_machine_on_macos`, `existing_main_makes_every_ticked_type_an_extra`, `runtime_flag_is_an_alias_for_gateways`, `execute_events_reports_start_lines_and_done`, `sudo_noninteractive_rewrites_sudo`, `gateway_add_builds_one_extra_plan_and_picks_a_free_port`.
- [ ] **Step 2-4:** implement; `just check`. **Step 5:** commit `tui: plan gateways by type and add gateway subcommands`.

### Task 5: Wizard state machine (Sonnet)

**Files:** Create `tui/src/wizard.rs` (state only in this task); `main.rs` (`mod wizard;`).

**Interfaces:**
- Consumes: Tasks 1, 2, 4.
- Produces:
  - `enum Mode { First, Add }`, `enum Screen { Machine, Gateways, Review, Install, Done }`.
  - `struct Item { kind: GwKind, avail: Availability, ticked: bool, existing: bool }`.
  - `struct Wizard { mode, screen, items: Vec<Item>, cursor: usize, tick_order: Vec<GwKind>, steps: Vec<Step>, facts: Facts, ... }` with `Wizard::new(mode: Mode, f: Facts, existing: &[GwKind], pending: Option<Pending>) -> Wizard` (pre-ticks the pending selection and starts at `Review`), `fn on_key(&mut self, k: KeyCode) -> Action` where `enum Action { None, Quit, Install(Vec<Step>, Vec<GwKind>), Finish }`, `fn wanted(&self) -> Vec<GwKind>` (tick order), `fn main_kind(&self) -> Option<GwKind>`.
  - Behaviour: space toggles; unavailable and `existing` items cannot be ticked (`on_key` returns `None`, sets `self.note` with the reason); `Enter` on Gateways with nothing ticked sets a note "pick at least one"; `Esc`/`b` go back (Machine → Quit); `q` quits at any screen before Install; Review builds `steps` via `plan` for the ticked kinds.
- [ ] **Step 1: Failing tests:** `first_tick_becomes_main`, `unticking_the_main_promotes_the_next`, `unavailable_types_cannot_be_ticked_and_say_why`, `existing_types_are_dimmed_and_not_tickable_in_add_mode`, `enter_needs_at_least_one_tick`, `back_and_quit_keys`, `review_lists_the_plan_for_the_ticked_types`, `pending_selection_opens_at_review_with_ticks`, `install_action_carries_steps_and_kinds`.
- [ ] **Step 2-4:** implement, `just check`. **Step 5:** commit `tui: wizard state machine`.

### Task 6: Wizard drawing with measured layout (Sonnet)

**Files:** `tui/src/wizard.rs` (draw functions), `tui/src/ui.rs` only for shared colour helpers if needed.

**Interfaces:**
- Produces: `fn draw(f: &mut Frame, w: &Wizard, progress: &Progress)` where `struct Progress { rows: Vec<RowState>, now: Vec<String>, note: String }`, `enum RowState { Pending, Running, Done(Duration), Failed(String), Manual(String) }`; one private `draw_*` per screen following the approved mockup (Block borders, accent colour, green/amber/red, dim hints, `[x]`/`[ ]`/`[-]` ticks, "What it needs" lines `✓/✗ text  action`, columns padded to the widest cell, "1st ticked = main"); width < 70 → stacked list without the details pane; height < 20 → hints dropped before content.
- [ ] **Step 1: Failing tests** on ratatui `TestBackend` (render to a `Buffer`, convert rows to strings): `gateways_screen_shows_list_and_details_for_the_highlighted_type`, `every_box_row_has_equal_width_at_80_columns` (scan for `┌ │ └` rows and compare right edges), `columns_line_up_in_the_review_and_install_screens`, `unavailable_row_is_dim_and_shows_the_reason`, `stacked_layout_under_70_columns_has_no_details_pane`, `nothing_panics_at_60x20_and_80x24_on_every_screen`, `done_screen_lists_gateways_with_name_kind_port_status`.
- [ ] **Step 2-4:** implement; `just check`. **Step 5:** commit `tui: draw the wizard screens`.

### Task 7: Runner, first-run hook and Gateways tab (Opus review)

**Files:** `tui/src/wizard.rs` (runner), `tui/src/main.rs`, `tui/src/app.rs`, `tui/src/ui.rs`, `tui/src/cli.rs` (gateway rows), `README.md`, `AGENTS.md`.

**Interfaces:**
- Produces:
  - `pub fn run(terminal: &mut DefaultTerminal, mode: Mode, facts: Facts, existing: &[GwKind], data: &Path) -> io::Result<Outcome>` with `enum Outcome { Installed(Vec<GatewaySpec>), Cancelled, NeedsAction(String) }`: runs the key loop; on `Action::Install` writes `wizard.json`, leaves the alternate screen once for `sudo -v` when any step is privileged, spawns the executor thread (`execute_events` with `capture` and `sudo_noninteractive`), a sudo keep-alive (`sudo -n -v` every 50 s), and redraws on a channel until all steps end; clears `wizard.json` only when every required step succeeded; a Manual step ends in `NeedsAction` with its text on the Done screen.
  - `main.rs`: `ensure_installed` is replaced by `first_run_wizard()`: auto-open when OpenShell is missing or `openshell gateway list` is empty and the registry is empty; `osh setup` (interactive, no `--gateways`) opens it; with a flag it stays non-interactive.
  - `app.rs`: Gateways tab keys `a` (sets `self.wizard_request = Some(Mode::Add)`; main loop runs the wizard on the same terminal then refreshes), `x` (typed-confirmation modal for registry gateways, refuses others with a plain message), `d` generalised to any gateway (kind from registry or detected from the legacy name/config); the table gets TYPE and PORT columns and a `main` mark; footer text and README tab table updated.
- [ ] **Step 1: Failing tests:** `wizard_request_is_set_by_a_on_the_gateways_tab`, `x_on_a_legacy_gateway_explains_it_is_not_managed`, `x_on_an_extra_asks_for_typed_confirmation`, `reset_is_offered_for_registry_and_legacy_gateways`, `gateway_rows_show_kind_and_port`, `first_run_auto_opens_only_without_openshell_or_gateways`, `runner_reports_needs_action_for_a_manual_step` (runner logic split into a pure `fn finish(results, steps) -> Outcome` tested here), `pending_file_is_cleared_only_on_success`, `footer_keys_match_the_readme_table` (reads README, as the existing key/README rule requires).
- [ ] **Step 2-4:** implement; `just check`; manual smoke in a pty is not possible here, so `wizard.rs` keeps the loop thin. **Step 5:** commit `tui: run the wizard on first start and from the Gateways tab`.

### Task 8: Real and stubbed Linux tests, docs (Sonnet)

**Files:** `tests/linux/real-suite.sh`, `tests/linux/suite.sh`, `README.md`, `AGENTS.md`.

- [ ] **Step 1:** extend `real-suite.sh` (podman and docker machines): `osh setup --gateways <rt>,<other> --yes` (two gateways, first is main: `openshell -g openshell-<first> status` and the second both `Connected`, main on 17670, extra on 17671, `openshell gateway list` names, registry file content), `osh gateway add vm` fails with the KVM reason (container has no `/dev/kvm`), `osh gateway remove` of the extra (typed confirmation), the main refuses removal while an extra exists, reset of the extra leaves the main answering, uninstall removes extra units. Fix whatever the real run reveals (spec calls 1 and Linux Docker `grpc_endpoint` are decided here; record in the ledger).
- [ ] **Step 2:** stubbed `suite.sh`: `osh setup --gateways docker,podman --dry-run` output lists both gateways' steps for each distro; `--gateways vm` exits 2 without `/dev/kvm`.
- [ ] **Step 3:** README (first-run wizard, Gateways tab `a`/`x`, flags, types per OS), AGENTS.md layout table (`gwmodel.rs`, `wizard.rs`). **Step 4:** run `just check`, `just test-linux`, `just test-linux-real podman`, `just test-linux-real docker`. **Step 5:** commit `tests, docs: multi-gateway and wizard coverage`.

---

## Self-review

- Spec coverage: wizard screens → T5/T6/T7; types and needs → T2; naming/main/ports/registry → T1/T3; add/remove/reset → T3/T4/T7; resume → T1 (`Pending`) + T5/T7; Linux parity incl. VM → T2/T4; real tests → T8. macOS-only mechanisms are unit-tested only (stated in the spec).
- Types used across tasks are defined once: `GwKind`, `GatewaySpec`, `Registry`, `Pending`, `Availability`, `Need`, `State`, `Kvm`, `PodmanMachine`, `Event`, `Progress`, `RowState`, `Outcome`, `Mode`, `Screen`, `Action`.
- Proportion: bodies are left to the implementer; signatures, test names and the exact values the spec pins are given.
