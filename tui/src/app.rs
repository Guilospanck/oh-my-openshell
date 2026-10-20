//! Application state, modal flows and key handling.
use crate::cli::{self, Snapshot};
use crate::logview::LogView;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{
    path::{Path, PathBuf},
    sync::{mpsc::Sender, Arc, Mutex},
    thread,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Sandboxes,
    Providers,
    Profiles,
    Images,
    Gateways,
}

pub const TABS: [(Tab, &str); 5] = [
    (Tab::Sandboxes, "Sandboxes"),
    (Tab::Providers, "Providers"),
    (Tab::Profiles, "Profiles"),
    (Tab::Images, "Images"),
    (Tab::Gateways, "Gateways"),
];

#[derive(Clone, Default)]
pub struct Plan {
    pub image: String,
    pub providers: Vec<String>,
    pub name: String,
    /// Kubernetes-style limits such as `500m`, `2`, `4Gi`; empty = gateway default.
    pub cpu: String,
    pub memory: String,
    /// Non-empty: run once via `sh -c` and delete the sandbox afterwards (`--no-keep`).
    pub command: String,
}

#[derive(Clone)]
pub enum SelKind {
    Preset,
    Image,
    Providers(Plan),
    AttachProvider(String),
    DetachProvider(String),
    NewProvider,
    Gateway,
    /// How to obtain the Claude Code setup token; carries the credential flow to resume.
    ClaudeToken(InKind),
}

#[derive(Clone)]
pub enum InKind {
    OtherImage,
    Name(Plan),
    Cpu(Plan),
    Memory(Plan),
    Command(Plan),
    /// Prompt for each key in turn; `next` resumes a sandbox plan afterwards.
    Cred {
        provider: String,
        keys: Vec<String>,
        values: Vec<(String, String)>,
        update: bool,
        next: Option<(Plan, Vec<String>)>,
    },
}

#[derive(Clone)]
pub enum ConfKind {
    CreateSandbox(Plan),
    DeleteSandbox(String),
    DeleteProvider(String),
    ResetGateway(String),
}

pub enum Modal {
    Select {
        title: String,
        items: Vec<String>,
        cursor: usize,
        marked: Vec<bool>,
        multi: bool,
        kind: SelKind,
    },
    Input {
        title: String,
        buf: String,
        mask: bool,
        kind: InKind,
    },
    Confirm {
        text: String,
        kind: ConfKind,
    },
    Info(String),
}

/// A command run with the terminal handed back to it (attach, build, create).
pub struct Step {
    pub prog: String,
    pub args: Vec<String>,
}

pub struct Done {
    pub msg: String,
    pub next: Option<(Plan, Vec<String>)>,
}

pub struct App {
    pub dir: PathBuf,
    /// Whether an `osh` on PATH resolves to this checkout's launcher.
    pub linked: bool,
    /// Installed gateway.toml differs from the repo copy (`just install` copies it).
    pub config_stale: bool,
    pub tab: Tab,
    pub snap: Snapshot,
    pub cursor: [usize; 5],
    pub modal: Option<Modal>,
    /// Full-screen live log viewer; takes over the keyboard while open.
    pub logview: Option<LogView>,
    pub status: String,
    pub quit: bool,
    pub job: Option<Vec<Step>>,
    /// Credential flow waiting on a running `claude setup-token`.
    token_flow: Option<InKind>,
    selected: Arc<Mutex<Option<String>>>,
    wake: Sender<()>,
    done_tx: Sender<Done>,
}

pub struct ProfileRow {
    pub id: String,
    pub in_repo: bool,
    pub gateway_version: Option<u64>,
}

const PRESETS: [(&str, &str, &str); 2] = [
    ("Claude Code (subscription)", "claude-code:latest", "claude-code"),
    ("Custom...", "", ""),
];

/// Trimmed limit value; `None` when it could be mistaken for a flag or has spaces.
fn limit(text: &str) -> Option<String> {
    let t = text.trim();
    (!t.starts_with('-') && !t.contains(char::is_whitespace)).then(|| t.to_string())
}

/// True when an `osh` found on PATH is this checkout's launcher.
fn on_path(dir: &Path) -> bool {
    let Ok(ours) = dir.join("osh").canonicalize() else { return false };
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| d.join("osh").canonicalize().is_ok_and(|c| c == ours))
    })
}

/// True when `gateway.toml` in the user config dir exists and differs from the repo's.
fn config_stale(dir: &Path) -> bool {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    match (
        std::fs::read(dir.join("gateway.toml")),
        std::fs::read(base.join("openshell/gateway.toml")),
    ) {
        (Ok(repo), Ok(installed)) => repo != installed,
        (Ok(_), Err(_)) => true,
        _ => false,
    }
}

impl App {
    pub fn new(
        dir: PathBuf,
        selected: Arc<Mutex<Option<String>>>,
        wake: Sender<()>,
        done_tx: Sender<Done>,
    ) -> Self {
        App {
            linked: on_path(&dir),
            config_stale: config_stale(&dir),
            dir,
            tab: Tab::Sandboxes,
            snap: Snapshot::default(),
            cursor: [0; 5],
            modal: None,
            logview: None,
            status: "Loading...".into(),
            quit: false,
            job: None,
            token_flow: None,
            selected,
            wake,
            done_tx,
        }
    }

    fn idx(&self) -> usize {
        TABS.iter().position(|(t, _)| *t == self.tab).unwrap_or(0)
    }

    pub fn profile_rows(&self) -> Vec<ProfileRow> {
        let mut ids = cli::profile_ids(&self.dir);
        for p in &self.snap.profiles {
            if !ids.contains(&p.id) {
                ids.push(p.id.clone());
            }
        }
        ids.sort();
        ids.into_iter()
            .map(|id| ProfileRow {
                in_repo: cli::profile_path(&self.dir, &id).exists(),
                gateway_version: self.snap.profiles.iter().find(|p| p.id == id).map(|p| p.version),
                id,
            })
            .collect()
    }

    fn len(&self) -> usize {
        match self.tab {
            Tab::Sandboxes => self.snap.sandboxes.len(),
            Tab::Providers => self.snap.providers.len(),
            Tab::Profiles => self.profile_rows().len(),
            Tab::Images => self.snap.images.len(),
            Tab::Gateways => self.snap.gateways.len(),
        }
    }

    pub fn current(&self) -> usize {
        self.cursor[self.idx()]
    }

    pub fn selected_sandbox(&self) -> Option<&cli::Sandbox> {
        self.snap.sandboxes.get(self.cursor[0])
    }

    pub fn on_snapshot(&mut self, snap: Snapshot) {
        self.snap = snap;
        self.config_stale = config_stale(&self.dir);
        for i in 0..4 {
            let len = [
                self.snap.sandboxes.len(),
                self.snap.providers.len(),
                self.profile_rows().len(),
                self.snap.images.len(),
            ][i];
            self.cursor[i] = self.cursor[i].min(len.saturating_sub(1));
        }
        self.sync_selection();
    }

    fn sync_selection(&mut self) {
        let name = self.selected_sandbox().map(|s| s.name.clone());
        if let Ok(mut g) = self.selected.lock() {
            if *g != name {
                *g = name;
                let _ = self.wake.send(());
            }
        }
    }

    pub fn refresh(&self) {
        let _ = self.wake.send(());
    }

    fn bg<F>(&self, f: F)
    where
        F: FnOnce() -> Result<Done, String> + Send + 'static,
    {
        let tx = self.done_tx.clone();
        thread::spawn(move || {
            let d = f().unwrap_or_else(|e| Done {
                msg: format!("Error: {e}"),
                next: None,
            });
            let _ = tx.send(d);
        });
    }

    fn bg_msg<F>(&self, f: F)
    where
        F: FnOnce() -> Result<String, String> + Send + 'static,
    {
        self.bg(move || f().map(|msg| Done { msg, next: None }));
    }

    pub fn on_done(&mut self, d: Done) {
        self.status = d.msg;
        self.refresh();
        if let Some((plan, mut missing)) = d.next {
            if self.status.starts_with("Error") {
                return;
            }
            if missing.is_empty() {
                self.ask_name(plan);
            } else {
                let p = missing.remove(0);
                self.start_creds(p, false, Some((plan, missing)));
            }
        }
    }

    // ---- flows -----------------------------------------------------------

    fn select(&mut self, title: &str, items: Vec<String>, multi: bool, kind: SelKind) {
        if items.is_empty() {
            self.modal = Some(Modal::Info(format!("Nothing to choose from for: {title}")));
            return;
        }
        let marked = vec![false; items.len()];
        self.modal = Some(Modal::Select {
            title: title.into(),
            items,
            cursor: 0,
            marked,
            multi,
            kind,
        });
    }

    fn input(&mut self, title: &str, mask: bool, kind: InKind) {
        self.modal = Some(Modal::Input {
            title: title.into(),
            buf: String::new(),
            mask,
            kind,
        });
    }

    fn start_creds(
        &mut self,
        provider: String,
        update: bool,
        next: Option<(Plan, Vec<String>)>,
    ) {
        let keys = if update {
            self.snap
                .providers
                .iter()
                .find(|p| p.name == provider)
                .map(|p| p.keys.clone())
                .unwrap_or_default()
        } else {
            cli::profile_env_keys(&self.dir, &provider)
        };
        self.cred_step(InKind::Cred { provider, keys, values: vec![], update, next });
    }

    /// Ask for the next key, or run the create/update once all are collected.
    fn cred_step(&mut self, kind: InKind) {
        let InKind::Cred { provider, keys, mut values, update, next } = kind else { return };
        // Take values already exported in the TUI's environment instead of prompting.
        while let Some(key) = keys.get(values.len()) {
            match std::env::var(key) {
                Ok(v) if !v.is_empty() => values.push((key.clone(), v)),
                _ => break,
            }
        }
        if let Some(key) = keys.get(values.len()).cloned() {
            if !update && provider == "claude-code" && key == "CLAUDE_CODE_OAUTH_TOKEN" {
                let items = vec![
                    "Generate a new token (runs claude setup-token, then paste it)".to_string(),
                    "Paste an existing token".to_string(),
                ];
                self.select(
                    "Claude Code token",
                    items,
                    false,
                    SelKind::ClaudeToken(InKind::Cred { provider, keys, values, update, next }),
                );
                return;
            }
            let mask = key != "FORGEJO_HOST";
            let title = format!("{provider}: {key}{}", if mask { " (hidden)" } else { "" });
            self.input(&title, mask, InKind::Cred { provider, keys, values, update, next });
            return;
        }
        let dir = self.dir.clone();
        self.status = format!("Working on provider {provider}...");
        self.bg(move || {
            let msg = if update {
                cli::update_credentials(&provider, &values)?
            } else {
                cli::create_provider(&dir, &provider, &values)?
            };
            Ok(Done { msg, next })
        });
    }

    /// Run `claude setup-token` with the terminal handed over; `finish_job`
    /// then asks for the printed token (the command has no machine-readable output).
    fn generate_token(&mut self, flow: InKind) {
        self.job = Some(vec![Step { prog: "claude".into(), args: vec!["setup-token".into()] }]);
        self.token_flow = Some(flow);
    }

    /// Called after a terminal hand-over: resume a token flow with the paste prompt.
    pub fn finish_job(&mut self) {
        let Some(flow) = self.token_flow.take() else { return };
        let InKind::Cred { provider, keys, values, .. } = &flow else { return };
        let title = format!("Paste the token claude printed: {provider}: {} (hidden)", keys[values.len()]);
        self.input(&title, true, flow);
    }

    fn ask_name(&mut self, plan: Plan) {
        self.input("Sandbox name (empty = generated)", false, InKind::Name(plan));
    }

    fn start_wizard(&mut self) {
        let items = PRESETS.iter().map(|p| p.0.to_string()).collect();
        self.select("New sandbox", items, false, SelKind::Preset);
    }

    /// Providers a plan needs that the gateway does not have yet.
    fn missing(&self, providers: &[String]) -> Vec<String> {
        providers
            .iter()
            .filter(|p| !self.snap.providers.iter().any(|g| &g.name == *p))
            .cloned()
            .collect()
    }

    fn proceed(&mut self, plan: Plan) {
        let mut missing = self.missing(&plan.providers);
        if missing.is_empty() {
            self.ask_name(plan);
        } else {
            let first = missing.remove(0);
            self.start_creds(first, false, Some((plan, missing)));
        }
    }

    fn sandbox_steps(&self, plan: &Plan) -> Vec<Step> {
        let mut steps = Vec::new();
        if let Some(img) = self.snap.images.iter().find(|i| i.tag == plan.image && !i.built) {
            steps.push(Step {
                prog: "docker".into(),
                args: cli::build_args(&self.dir, &img.file, &img.tag),
            });
        }
        let mut args: Vec<String> = vec!["sandbox".into(), "create".into()];
        if !plan.name.is_empty() {
            args.extend(["--name".into(), plan.name.clone()]);
        }
        if !plan.image.is_empty() {
            args.extend(["--from".into(), plan.image.clone()]);
        }
        if !plan.cpu.is_empty() {
            args.extend(["--cpu".into(), plan.cpu.clone()]);
        }
        if !plan.memory.is_empty() {
            args.extend(["--memory".into(), plan.memory.clone()]);
        }
        for p in &plan.providers {
            args.extend(["--provider".into(), p.clone()]);
        }
        if plan.command.is_empty() {
            // The detached shell stays the main process; sessions come from `exec`.
            args.push("--detach".into());
        } else {
            args.extend(["--no-keep".into(), "--".into(), "sh".into(), "-c".into(), plan.command.clone()]);
        }
        steps.push(Step { prog: "openshell".into(), args: cli::with_gateway(args) });
        steps
    }

    fn on_select(&mut self, kind: SelKind, picks: Vec<String>) {
        let first = picks.first().cloned().unwrap_or_default();
        match kind {
            SelKind::Preset => {
                let Some(p) = PRESETS.iter().find(|p| p.0 == first) else { return };
                if p.1.is_empty() {
                    let mut items: Vec<String> =
                        self.snap.images.iter().map(|i| i.tag.clone()).collect();
                    items.push("Other image reference...".into());
                    items.push("Gateway default image".into());
                    self.select("Image", items, false, SelKind::Image);
                } else {
                    let plan = Plan {
                        image: p.1.into(),
                        providers: vec![p.2.into()],
                        ..Plan::default()
                    };
                    self.proceed(plan);
                }
            }
            SelKind::Image => {
                let plan = Plan::default();
                match first.as_str() {
                    "Other image reference..." => self.input("Image reference", false, InKind::OtherImage),
                    "Gateway default image" => self.pick_providers(plan),
                    _ => self.pick_providers(Plan { image: first, ..plan }),
                }
            }
            SelKind::Providers(mut plan) => {
                plan.providers = picks;
                self.proceed(plan);
            }
            SelKind::AttachProvider(sb) => {
                self.bg_msg(move || {
                    cli::os(&["sandbox", "provider", "attach", &sb, &first, "--wait", "--timeout", "30"])
                        .map(|_| format!("Attached {first} to {sb}"))
                });
            }
            SelKind::DetachProvider(sb) => {
                self.bg_msg(move || {
                    cli::os(&["sandbox", "provider", "detach", &sb, &first, "--wait", "--timeout", "30"])
                        .map(|_| format!("Detached {first} from {sb}"))
                });
            }
            SelKind::Gateway => {
                let name = first.split_whitespace().next().unwrap_or_default();
                cli::set_gateway(name);
                self.snap = Snapshot::default();
                self.snap.gateway = name.to_string();
                self.cursor = [0; 5];
                self.status = format!("Switching to {name}...");
                self.sync_selection();
                self.refresh();
            }
            SelKind::ClaudeToken(flow) => {
                if first.starts_with("Paste") {
                    let InKind::Cred { provider, keys, values, .. } = &flow else { return };
                    let key = &keys[values.len()];
                    let title = format!("{provider}: {key} (hidden)");
                    self.input(&title, true, flow);
                } else {
                    self.generate_token(flow);
                }
            }
            SelKind::NewProvider => {
                if self.snap.providers.iter().any(|p| p.name == first) {
                    self.modal = Some(Modal::Info(format!(
                        "Provider {first} exists. Use u on the Providers tab to update credentials."
                    )));
                } else {
                    self.start_creds(first, false, None);
                }
            }
        }
    }

    fn pick_providers(&mut self, plan: Plan) {
        let mut names: Vec<String> = self.snap.providers.iter().map(|p| p.name.clone()).collect();
        for id in cli::profile_ids(&self.dir) {
            if !names.contains(&id) {
                names.push(id);
            }
        }
        names.sort();
        self.select("Providers (space to mark, Enter to confirm)", names, true, SelKind::Providers(plan));
    }

    fn on_input(&mut self, kind: InKind, text: String) {
        match kind {
            InKind::OtherImage => {
                if !text.trim().is_empty() {
                    self.pick_providers(Plan { image: text.trim().into(), ..Plan::default() });
                }
            }
            InKind::Name(mut plan) => {
                plan.name = text.trim().into();
                if self.limits_supported() {
                    self.input("CPU limit, e.g. 500m, 2 (empty = unlimited)", false, InKind::Cpu(plan));
                } else {
                    self.ask_command(plan);
                }
            }
            InKind::Cpu(mut plan) => {
                let Some(v) = limit(&text) else { return self.bad_limit("CPU") };
                plan.cpu = v;
                self.input("Memory limit, e.g. 512Mi, 4Gi (empty = unlimited)", false, InKind::Memory(plan));
            }
            InKind::Memory(mut plan) => {
                let Some(v) = limit(&text) else { return self.bad_limit("Memory") };
                plan.memory = v;
                self.ask_command(plan);
            }
            InKind::Command(mut plan) => {
                plan.command = text.trim().into();
                let steps = self.sandbox_steps(&plan);
                let summary = steps
                    .iter()
                    .map(|s| format!("{} {}", s.prog, s.args.join(" ")))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.modal = Some(Modal::Confirm {
                    text: format!("Run:\n{summary}\n\nContinue?"),
                    kind: ConfKind::CreateSandbox(plan),
                });
            }
            InKind::Cred { provider, keys, mut values, update, next } => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    self.status = "Cancelled: empty value".into();
                    return;
                }
                let key = keys[values.len()].clone();
                values.push((key, text));
                self.cred_step(InKind::Cred { provider, keys, values, update, next });
            }
        }
    }

    /// The vm driver ignores per-sandbox limits; its size is fixed gateway-wide.
    fn limits_supported(&self) -> bool {
        !self.snap.driver.is_empty() && self.snap.driver != "vm"
    }

    fn ask_command(&mut self, plan: Plan) {
        self.input(
            "One-shot command (empty = keep sandbox running)",
            false,
            InKind::Command(plan),
        );
    }

    pub fn switch_gateway(&mut self) {
        let items: Vec<String> = self
            .snap
            .gateways
            .iter()
            .map(|g| {
                let cur = if g.name == self.snap.gateway { "  (current)" } else { "" };
                format!("{}  {}{cur}", g.name, g.endpoint)
            })
            .collect();
        if items.is_empty() {
            self.status = "No gateways registered".into();
        } else {
            self.select("Gateway", items, false, SelKind::Gateway);
        }
    }

    fn bad_limit(&mut self, what: &str) {
        self.status = format!("Cancelled: invalid {what} limit");
    }

    fn on_confirm(&mut self, kind: ConfKind) {
        match kind {
            ConfKind::CreateSandbox(plan) => self.job = Some(self.sandbox_steps(&plan)),
            ConfKind::DeleteSandbox(name) => self.bg_msg(move || {
                cli::os(&["sandbox", "delete", &name]).map(|_| format!("Deleted sandbox {name}"))
            }),
            ConfKind::ResetGateway(name) => {
                self.job = Some(vec![Step {
                    prog: self.dir.join("gateway-reset").to_string_lossy().into(),
                    args: vec![name],
                }]);
            }
            ConfKind::DeleteProvider(name) => self.bg_msg(move || {
                cli::os(&["provider", "delete", &name]).map(|_| format!("Deleted provider {name}"))
            }),
        }
    }

    // ---- keys ------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some(lv) = &mut self.logview {
            if !lv.on_key(key) {
                self.logview = None;
            }
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.modal_key(modal, key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.switch((self.idx() + 1) % TABS.len()),
            KeyCode::BackTab => self.switch((self.idx() + TABS.len() - 1) % TABS.len()),
            KeyCode::Char(c @ '1'..='5') => self.switch(c as usize - '1' as usize),
            KeyCode::Char('j') | KeyCode::Down => self.mv(1),
            KeyCode::Char('k') | KeyCode::Up => self.mv(-1),
            KeyCode::Char('g') => self.switch_gateway(),
            KeyCode::Char('r') => {
                self.status = "Refreshing...".into();
                self.refresh();
            }
            _ => self.tab_key(key.code),
        }
    }

    fn switch(&mut self, i: usize) {
        self.tab = TABS[i].0;
    }

    fn mv(&mut self, d: i32) {
        let len = self.len();
        let i = self.idx();
        if len > 0 {
            self.cursor[i] = (self.cursor[i] as i32 + d).clamp(0, len as i32 - 1) as usize;
        }
        self.sync_selection();
    }

    fn tab_key(&mut self, code: KeyCode) {
        match self.tab {
            Tab::Sandboxes => self.sandbox_key(code),
            Tab::Providers => self.provider_key(code),
            Tab::Profiles => self.profile_key(code),
            Tab::Gateways => self.gateway_key(code),
            Tab::Images => {
                if code == KeyCode::Char('b') || code == KeyCode::Enter {
                    if let Some(img) = self.snap.images.get(self.current()) {
                        self.job = Some(vec![Step {
                            prog: "docker".into(),
                            args: cli::build_args(&self.dir, &img.file, &img.tag),
                        }]);
                    }
                }
            }
        }
    }

    fn sandbox_key(&mut self, code: KeyCode) {
        if code == KeyCode::Char('n') {
            self.start_wizard();
            return;
        }
        let Some(name) = self.selected_sandbox().map(|s| s.name.clone()) else { return };
        match code {
            KeyCode::Char('l') => match LogView::open(name) {
                Ok(lv) => self.logview = Some(lv),
                Err(e) => self.status = format!("Error: {e}"),
            },
            KeyCode::Enter => {
                self.job = Some(vec![Step {
                    prog: "openshell".into(),
                    args: cli::with_gateway(vec![
                        "sandbox".into(),
                        "exec".into(),
                        "-n".into(),
                        name,
                        "--tty".into(),
                        "--".into(),
                        "bash".into(),
                    ]),
                }])
            }
            KeyCode::Char('d') => {
                self.modal = Some(Modal::Confirm {
                    text: format!("Delete sandbox '{name}'? Its contents are lost."),
                    kind: ConfKind::DeleteSandbox(name),
                })
            }
            KeyCode::Char('s') => self.bg_msg(move || {
                cli::os(&["sandbox", "stop", &name]).map(|_| format!("Stopped {name}"))
            }),
            KeyCode::Char('S') => self.bg_msg(move || {
                cli::os(&["sandbox", "start", &name]).map(|_| format!("Started {name}"))
            }),
            KeyCode::Char('a') => {
                let items = self.snap.providers.iter().map(|p| p.name.clone()).collect();
                self.select("Attach provider", items, false, SelKind::AttachProvider(name));
            }
            KeyCode::Char('x') => {
                let attached = cli::os(&["sandbox", "provider", "list", &name, "--output", "json"])
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .map(|v| {
                        v["providers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| p["name"].as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.select("Detach provider", attached, false, SelKind::DetachProvider(name));
            }
            _ => {}
        }
    }

    fn gateway_key(&mut self, code: KeyCode) {
        let Some(g) = self.snap.gateways.get(self.current()) else { return };
        match code {
            KeyCode::Char('d') => {
                if !matches!(g.name.as_str(), "openshell" | "openshell-docker") {
                    self.modal = Some(Modal::Info(format!(
                        "{} is not managed by this repo; only openshell and openshell-docker can be wiped.",
                        g.name
                    )));
                    return;
                }
                let what = if g.name == "openshell" {
                    "microVMs"
                } else {
                    "Docker containers and volumes"
                };
                self.modal = Some(Modal::Confirm {
                    text: format!(
                        "WIPE gateway {name}?\n\nThis permanently deletes EVERYTHING it holds:\n\
                         - all sandboxes, with their {what} and workspace data\n\
                         - all providers and their stored credentials\n\
                         - all imported profiles and gateway settings\n\n\
                         The gateway is restarted empty. Setup tokens must be created again.\n\
                         This cannot be undone. Continue?",
                        name = g.name
                    ),
                    kind: ConfKind::ResetGateway(g.name.clone()),
                });
            }
            KeyCode::Enter => {
                let name = g.name.clone();
                cli::set_gateway(&name);
                self.snap = Snapshot::default();
                self.snap.gateway = name.clone();
                self.cursor = [0; 5];
                self.status = format!("Switching to {name}...");
                self.sync_selection();
                self.refresh();
            }
            _ => {}
        }
    }

    fn provider_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('n') => {
                let ids = cli::profile_ids(&self.dir);
                self.select("Create provider from profile", ids, false, SelKind::NewProvider);
            }
            KeyCode::Char('u') => {
                if let Some(p) = self.snap.providers.get(self.current()).cloned() {
                    if p.keys.is_empty() {
                        self.modal = Some(Modal::Info(format!("{} stores no credentials.", p.name)));
                    } else {
                        self.start_creds(p.name, true, None);
                    }
                }
            }
            KeyCode::Char('d') => {
                if let Some(p) = self.snap.providers.get(self.current()) {
                    self.modal = Some(Modal::Confirm {
                        text: format!("Delete provider '{}'?", p.name),
                        kind: ConfKind::DeleteProvider(p.name.clone()),
                    });
                }
            }
            _ => {}
        }
    }

    fn profile_key(&mut self, code: KeyCode) {
        if !matches!(code, KeyCode::Char('u') | KeyCode::Enter) {
            return;
        }
        let rows = self.profile_rows();
        let Some(row) = rows.get(self.current()) else { return };
        if !row.in_repo {
            self.modal = Some(Modal::Info(format!("No {}.yaml in providers/ to sync from.", row.id)));
            return;
        }
        if cli::profile_env_keys(&self.dir, &row.id).first().map(String::as_str) == Some("FORGEJO_HOST") {
            self.modal = Some(Modal::Info(
                "This profile needs FORGEJO_HOST rendered in. Create it from the Providers tab (n).".into(),
            ));
            return;
        }
        let (dir, id) = (self.dir.clone(), row.id.clone());
        self.status = format!("Syncing profile {id}...");
        self.bg_msg(move || cli::sync_profile(&dir, &id, None));
    }

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) {
        match modal {
            Modal::Info(_) => {}
            Modal::Confirm { text, kind } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.on_confirm(kind),
                KeyCode::Char('n') | KeyCode::Esc => {}
                _ => self.modal = Some(Modal::Confirm { text, kind }),
            },
            Modal::Input { title, mut buf, mask, kind } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.on_input(kind, buf),
                code => {
                    match code {
                        KeyCode::Char(c) => buf.push(c),
                        KeyCode::Backspace => {
                            buf.pop();
                        }
                        _ => {}
                    }
                    self.modal = Some(Modal::Input { title, buf, mask, kind });
                }
            },
            Modal::Select { title, items, mut cursor, mut marked, multi, kind } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let mut picks: Vec<String> = items
                        .iter()
                        .zip(&marked)
                        .filter(|(_, m)| **m)
                        .map(|(i, _)| i.clone())
                        .collect();
                    if picks.is_empty() && !multi {
                        picks.push(items[cursor].clone());
                    }
                    self.on_select(kind, picks);
                }
                code => {
                    match code {
                        KeyCode::Char('j') | KeyCode::Down => cursor = (cursor + 1).min(items.len() - 1),
                        KeyCode::Char('k') | KeyCode::Up => cursor = cursor.saturating_sub(1),
                        KeyCode::Char(' ') if multi => marked[cursor] = !marked[cursor],
                        _ => {}
                    }
                    self.modal = Some(Modal::Select { title, items, cursor, marked, multi, kind });
                }
            },
        }
    }
}
