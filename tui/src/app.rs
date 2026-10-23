//! Application state, modal flows and key handling.
use crate::cli::{self, Snapshot};
use crate::logview::LogView;
use crate::term::{self, Session};
use crate::watch::{self, Blocked, Denial, Follow, Prompt, State, Watch};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc::Sender, Arc, Mutex},
    thread,
    time::{Duration, Instant},
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
    /// Whether to take a GitHub token from `gh auth token` or paste one; carries the flow to resume.
    GhToken(InKind),
    /// How to grant the network access a blocked sync is missing.
    SyncAccess { sync: SyncPlan, check: cli::SyncCheck },
    /// Which agents to add skills for, in this sandbox.
    SyncAgents(String),
    /// Items to add; `labels` are the list entries in manifest order.
    SyncItems { sandbox: String, agent: String, manifest: cli::SyncManifest, labels: Vec<String> },
    /// Ctrl-] menu over the focused shell.
    SessionMenu,
    /// What to do about a program the sandbox policy blocked.
    Blocked { sandbox: String, gateway: String, b: Blocked },
    /// Denials seen in a sandbox (`p`); choosing a blocked one asks again.
    Review { sandbox: String, denials: Vec<Denial> },
}

#[derive(Clone)]
pub enum InKind {
    OtherImage,
    Name(Plan),
    Cpu(Plan),
    Memory(Plan),
    Command(Plan),
    /// Name for a generated install-access provider.
    GenName(SyncPlan),
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
    Sync(SyncPlan),
    /// Set up a provider that needs a secret, then attach it and check again.
    SyncSecret { sync: SyncPlan, profile: String },
    /// Import, create and attach a generated profile, then check again.
    SyncGen { sync: SyncPlan, name: String, yaml: String },
    /// Detach and delete the install-access provider once the installs are done.
    DropAccess { sandbox: String, name: String },
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
    /// Read-only scrollable text; `back` is shown again when it closes.
    Viewer { title: String, text: String, scroll: u16, back: Box<Modal> },
}

/// A command run with the terminal handed back to it (build, create, setup-token).
pub struct Step {
    pub prog: String,
    pub args: Vec<String>,
}

impl Step {
    /// What the step does, for messages: the in-sandbox command without its PATH prefix.
    pub fn label(&self) -> String {
        match self.args.last() {
            Some(c) if self.args.iter().any(|a| a == "-c") => c.strip_prefix(SANDBOX_PATH).unwrap_or(c).to_string(),
            _ => format!("{} {}", self.prog, self.args.join(" ")),
        }
    }
}

/// The step a terminal job stopped at, and the end of its output when it was recorded.
pub struct Failed {
    /// Position among the steps that ran.
    pub index: usize,
    pub label: String,
    pub status: String,
    pub tail: Vec<String>,
}

/// The last `n` non-blank lines of a terminal recording, without escape sequences or `\r`.
pub fn clean_tail(raw: &str, n: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for l in raw.split('\n') {
        let mut out = String::new();
        let mut it = l.trim_end_matches('\r').chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\x1b' => {
                    // CSI: parameters up to a final byte in @..~; other escapes: one more char.
                    if it.peek() == Some(&'[') {
                        it.next();
                        for f in it.by_ref() {
                            if ('@'..='~').contains(&f) {
                                break;
                            }
                        }
                    } else {
                        it.next();
                    }
                }
                // A carriage return redraws the line (progress bars): keep what came last.
                '\r' => out.clear(),
                c if c.is_control() => {}
                c => out.push(c),
            }
        }
        if !out.trim().is_empty() {
            lines.push(out.trim_end().to_string());
        }
    }
    let from = lines.len().saturating_sub(n);
    lines.split_off(from)
}

/// Whether a failed step's output reads like git or GitHub refusing access, which a
/// GitHub token fixes; a policy block looks different and the policy check catches it.
pub fn auth_failure(tail: &[String]) -> bool {
    tail.iter().any(|l| {
        let l = l.to_lowercase();
        [
            "authentication failed",
            "could not read username",
            "could not read password",
            "terminal prompts disabled",
            "repository not found",
            "permission denied (publickey)",
            "the requested url returned error: 401",
            "the requested url returned error: 403",
            "the requested url returned error: 404",
        ]
        .iter()
        .any(|p| l.contains(p))
            || (l.contains("github") && (l.contains(" 404") || l.contains("not found")))
    })
}

pub struct Done {
    pub msg: String,
    pub next: Option<(Plan, Vec<String>)>,
    pub sync: Option<SyncStage>,
}

/// Result of a background step of the "add skills to a sandbox" flow.
pub enum SyncStage {
    Manifest { sandbox: String, agent: String, manifest: cli::SyncManifest },
    /// `offered`: fix rounds already done (0 none, 1 anonymous profiles, 2 secret profiles).
    Check { sync: SyncPlan, check: cli::SyncCheck, offered: u8 },
    /// A provider that needs a secret now exists: attach it and check again.
    Attach { sync: SyncPlan, profile: String },
    /// A generated profile for the user to review.
    Generated { sync: SyncPlan, name: String, yaml: String },
}

/// What the user chose to put into one sandbox.
#[derive(Clone)]
pub struct SyncPlan {
    pub sandbox: String,
    /// `claude-code`, `pi` or `both`.
    pub agent: String,
    pub manifest: cli::SyncManifest,
    pub ids: Vec<String>,
    /// Steps already done by an earlier run that stopped; a retry starts after them.
    pub skip: usize,
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
    /// When `status` last changed; a shell's status bar shows it for a few seconds.
    status_at: Option<(String, Instant)>,
    pub quit: bool,
    pub job: Option<Vec<Step>>,
    /// Record the job's output, so a failed step can be explained (only sync jobs:
    /// `claude setup-token` prints a secret).
    pub capture: bool,
    /// The sync plan the running job installs.
    running_sync: Option<SyncPlan>,
    /// Embedded sandbox shells; they keep running while the dashboard is shown.
    pub sessions: Vec<Session>,
    /// Session that has the screen and the keyboard.
    pub active: Option<usize>,
    /// Policy denials seen in sandboxes with a shell open, and the prompts still to show.
    pub watch: Watch,
    /// One log follower per open shell, feeding `watch`.
    follows: Vec<Follow>,
    /// When the last watcher prompt opened; keys typed just before it are not answers.
    pub prompt_at: Option<Instant>,
    /// Credential flow waiting on a running `claude setup-token`.
    token_flow: Option<InKind>,
    /// Staging directory of an in-progress "add skills" flow; removed when it ends.
    sync_stage: Option<String>,
    /// Set while a provider with a secret is being created for a blocked sync.
    sync_secret: Option<SyncPlan>,
    /// `(sandbox, provider)` of generated install access that is still attached.
    temp_access: Option<(String, String)>,
    selected: Arc<Mutex<Option<String>>>,
    wake: Sender<()>,
    done_tx: Sender<Done>,
}

pub struct ProfileRow {
    pub id: String,
    pub in_repo: bool,
    pub gateway_version: Option<u64>,
}

const PRESETS: [(&str, &str, &str); 3] = [
    ("Claude Code (subscription)", "claude-code:latest", "claude-code"),
    ("pi (attach a model provider afterwards)", "pi:latest", ""),
    ("Custom...", "", ""),
];

/// The hosts and programs a generated profile grants, short enough for a confirm box.
fn profile_summary(yaml: &str) -> String {
    let mut hosts = Vec::new();
    let mut bins = Vec::new();
    let mut in_bins = false;
    for l in yaml.lines() {
        if l.starts_with("binaries:") {
            in_bins = true;
        } else if let Some(h) = l.strip_prefix("  - host: ") {
            hosts.push(h.trim());
        } else if in_bins {
            if let Some(b) = l.strip_prefix("  - ") {
                bins.push(b.trim());
            }
        }
    }
    format!(
        "Hosts (port 443, no credential): {}\nPrograms allowed to reach them: {}\nGitHub is read-only (clone and fetch); npm is read-only.",
        hosts.join(", "),
        bins.join(", ")
    )
}

fn sync_label(i: &cli::SyncItem) -> String {
    let agents: Vec<&str> = i.agents.iter().map(|(a, _)| a.as_str()).collect();
    let tag = if i.kind == "host-only" { " (host-only)" } else { "" };
    format!("{:<11} {}  [{}]{tag}", i.group, i.name, agents.join(", "))
}

const SANDBOX_PATH: &str = "export PATH=\"$HOME/.local/bin:$PATH\"; ";

/// Commands run in the sandbox with `~/.local/bin` on PATH: uv installs there and the
/// sandbox PATH lacks it.
fn in_sandbox(sandbox: &str, command: &str) -> Step {
    Step {
        prog: "openshell".into(),
        args: cli::with_gateway(vec![
            "sandbox".into(),
            "exec".into(),
            "-n".into(),
            sandbox.into(),
            "--timeout".into(),
            "900".into(),
            "--".into(),
            "sh".into(),
            "-c".into(),
            format!("{SANDBOX_PATH}{command}"),
        ]),
    }
}

/// Steps for the chosen items, in manifest order, for the chosen agents.
pub fn sync_steps(plan: &SyncPlan) -> Vec<Step> {
    let wanted: &[&str] = match plan.agent.as_str() {
        "both" => &["claude-code", "pi"],
        "pi" => &["pi"],
        _ => &["claude-code"],
    };
    let mut steps = Vec::new();
    for item in plan.manifest.items.iter().filter(|i| plan.ids.contains(&i.id)) {
        for (_, spec) in item.agents.iter().filter(|(a, _)| wanted.contains(&a.as_str())) {
            if let Some(cmds) = spec.as_array() {
                steps.extend(cmds.iter().filter_map(|c| c.as_str()).map(|c| in_sandbox(&plan.sandbox, c)));
            } else if let (Some(src), Some(dest)) = (spec["src"].as_str(), spec["dest"].as_str()) {
                steps.push(Step {
                    prog: "openshell".into(),
                    args: cli::with_gateway(vec![
                        "sandbox".into(),
                        "upload".into(),
                        plan.sandbox.clone(),
                        src.into(),
                        dest.into(),
                        "--no-git-ignore".into(),
                    ]),
                });
            }
        }
    }
    steps.into_iter().skip(plan.skip).collect()
}

/// Trimmed limit value; `None` when it could be mistaken for a flag or has spaces.
fn limit(text: &str) -> Option<String> {
    let t = text.trim();
    (!t.starts_with('-') && !t.contains(char::is_whitespace)).then(|| t.to_string())
}

/// Keys a watcher prompt ignores right after it opens.
const PROMPT_GRACE: Duration = Duration::from_millis(800);

/// Shell pane on a `w`x`h` screen: everything above the one-row status bar.
pub fn pane(w: u16, h: u16) -> (u16, u16) {
    (w, h.saturating_sub(1))
}

fn notice_text(sandbox: &str, what: &str, reason: &str) -> String {
    format!(
        "{sandbox} blocked: {what}\nReason: {reason}\n\nThis is not a missing host rule (for example an L7 rule rejected the request), so allowing the host would not fix it. Review the policy: openshell policy get {sandbox}"
    )
}

fn review_label(s: &watch::Seen) -> String {
    let state = match s.state {
        State::Pending => "pending",
        State::Allowed(false) => "allowed (read-only)",
        State::Allowed(true) => "allowed (all)",
        State::IgnoredOnce => "ignored once",
        State::Ignored => "ignored",
        State::Notice => "notice",
    };
    let what = match &s.denial {
        Denial::Blocked(b) => format!("{} -> {}", b.binary, b.target()),
        Denial::Notice { what, reason } => format!("{what}: {reason}"),
    };
    let mut l = format!("{state:<19} {what} ({}x)", s.count);
    if l.chars().count() > 90 {
        l = l.chars().take(87).collect::<String>() + "...";
    }
    l
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
            status_at: None,
            quit: false,
            job: None,
            capture: false,
            running_sync: None,
            sessions: Vec::new(),
            active: None,
            watch: Watch::default(),
            follows: Vec::new(),
            prompt_at: None,
            token_flow: None,
            sync_stage: None,
            sync_secret: None,
            temp_access: None,
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

    /// Note a status change, so a focused shell can flash it.
    pub fn tick(&mut self) {
        if self.status_at.as_ref().is_none_or(|(s, _)| *s != self.status) {
            self.status_at = Some((self.status.clone(), Instant::now()));
        }
    }

    /// The status, while it is new enough to show over a shell.
    pub fn flash(&self) -> Option<&str> {
        self.status_at
            .as_ref()
            .filter(|(s, t)| *s == self.status && t.elapsed() < Duration::from_secs(6))
            .map(|(s, _)| s.as_str())
    }

    pub fn session(&self) -> Option<&Session> {
        self.active.and_then(|i| self.sessions.get(i))
    }

    fn session_mut(&mut self) -> Option<&mut Session> {
        self.active.and_then(|i| self.sessions.get_mut(i))
    }

    /// Whether `sandbox` on the shown gateway has a running shell.
    pub fn has_session(&self, sandbox: &str) -> bool {
        self.sessions.iter().any(|s| s.sandbox == sandbox && s.gateway == self.snap.gateway)
    }

    /// Focus the shell of `sandbox`, starting one sized to the screen if none runs.
    fn open_session(&mut self, sandbox: String) {
        let gw = cli::gateway();
        if let Some(i) = self.sessions.iter().position(|s| s.sandbox == sandbox && s.gateway == gw) {
            self.active = Some(i);
            return;
        }
        let (w, h) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
        let (cols, rows) = pane(w, h);
        match Session::open(sandbox.clone(), cols, rows) {
            Ok(s) => {
                self.sessions.push(s);
                self.active = Some(self.sessions.len() - 1);
                match Follow::spawn(sandbox.clone(), gw) {
                    Ok(f) => self.follows.push(f),
                    Err(e) => self.status = format!("Blocked-request watcher off for {sandbox}: {e}"),
                }
            }
            Err(e) => self.status = format!("Error: {e}"),
        }
    }

    fn close_session(&mut self, i: usize) -> Session {
        let s = self.sessions.remove(i);
        self.follows.retain(|f| !(f.sandbox == s.sandbox && f.gateway == s.gateway));
        if !self.sessions.iter().any(|o| o.sandbox == s.sandbox) {
            self.watch.end_session(&s.sandbox);
            if matches!(&self.modal, Some(Modal::Select { kind: SelKind::Blocked { sandbox, .. }, .. }) if *sandbox == s.sandbox) {
                self.modal = None;
            }
        }
        self.active = match self.active {
            Some(a) if a == i => {
                if matches!(self.modal, Some(Modal::Select { kind: SelKind::SessionMenu, .. })) {
                    self.modal = None;
                }
                None
            }
            Some(a) if a > i => Some(a - 1),
            a => a,
        };
        s
    }

    /// Feed every shell's output to its terminal, focused or not, and drop ended ones.
    pub fn pump_sessions(&mut self) {
        for s in &mut self.sessions {
            s.pump();
        }
        while let Some(i) = self.sessions.iter().position(|s| s.exited) {
            let s = self.close_session(i);
            let last = s.vt.last_line();
            self.status = if last.is_empty() {
                format!("Shell in {} ended", s.sandbox)
            } else {
                format!("Shell in {} ended: {last}", s.sandbox)
            };
        }
    }

    /// Size every shell, focused or not, to the pane it gets on a `w`x`h` screen.
    pub fn fit_sessions(&mut self, w: u16, h: u16) {
        let (cols, rows) = pane(w, h);
        for s in &mut self.sessions {
            s.resize(cols, rows);
        }
    }

    pub fn on_mouse(&mut self, me: MouseEvent) {
        if self.modal.is_none() {
            if let Some(text) = self.session_mut().and_then(|s| s.on_mouse(&me)) {
                self.copy(&text);
            }
        }
    }

    /// Host clipboard: OSC 52 through the terminal, plus `pbcopy` on macOS.
    fn copy(&mut self, text: &str) {
        let mut out = std::io::stdout();
        let _ = out.write_all(term::osc52(text).as_bytes()).and_then(|_| out.flush());
        if cfg!(target_os = "macos") {
            if let Ok(mut c) = Command::new("pbcopy").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                if let Some(mut i) = c.stdin.take() {
                    let _ = i.write_all(text.as_bytes());
                }
                thread::spawn(move || c.wait());
            }
        }
        self.status = format!("copied {} chars", text.chars().count());
    }

    /// Read new log lines of every open shell's sandbox, then show the next prompt when
    /// nothing else holds the screen.
    pub fn pump_watch(&mut self) {
        let mut stopped = Vec::new();
        for (i, f) in self.follows.iter_mut().enumerate() {
            match f.drain() {
                Ok(lines) => {
                    for l in lines {
                        if let Some(d) = watch::parse(&l) {
                            self.watch.push(&f.sandbox, &f.gateway, d);
                        }
                    }
                }
                Err(last) => stopped.push((i, last)),
            }
        }
        for (i, last) in stopped.into_iter().rev() {
            let f = self.follows.remove(i);
            self.status = format!("Blocked-request watcher for {} stopped: {last}", f.sandbox);
        }
        if self.watch.has_queued() && self.modal.is_none() && self.logview.is_none() && self.job.is_none() {
            self.show_prompt();
        }
    }

    fn show_prompt(&mut self) {
        let Some(p) = self.watch.next() else { return };
        self.prompt_at = Some(Instant::now());
        let here = self.session().map(|s| s.sandbox.clone());
        match p {
            Prompt::Blocked { sandbox, gateway, b, count } => {
                let prog = b.program().to_string();
                let mut title = format!("{prog} was blocked from reaching {}", b.target());
                if here.as_deref() != Some(sandbox.as_str()) {
                    title.push_str(&format!(" in {sandbox}"));
                }
                if count > 1 {
                    title.push_str(&format!(" ({count}x)"));
                }
                let mut items = Vec::new();
                // Read-only means HTTP method rules, so only HTTP ports get it.
                if matches!(b.port, 80 | 443 | 8080 | 8443) {
                    items.push(format!("Allow read-only (GET/HEAD/OPTIONS) for {prog}"));
                }
                items.push(format!("Allow all methods for {prog}"));
                items.push("Ignore this time".into());
                items.push("Ignore until the session ends".into());
                self.select(&title, items, false, SelKind::Blocked { sandbox, gateway, b });
            }
            Prompt::Notice { sandbox, what, reason } => {
                self.modal = Some(Modal::Info(notice_text(&sandbox, &what, &reason)));
            }
        }
    }

    /// `p`: the denials seen in a sandbox while its shell was open.
    fn review(&mut self, sandbox: String) {
        let seen = self.watch.for_sandbox(&sandbox);
        if seen.is_empty() {
            let how = if self.has_session(&sandbox) { "" } else { " The watcher runs while its shell is open (Enter)." };
            self.modal = Some(Modal::Info(format!("No blocked requests seen in {sandbox}.{how}")));
            return;
        }
        let items = seen.iter().map(|s| review_label(s)).collect();
        let denials = seen.iter().map(|s| s.denial.clone()).collect();
        self.select(
            &format!("Blocked in {sandbox}; Enter asks again"),
            items,
            false,
            SelKind::Review { sandbox, denials },
        );
    }

    pub fn on_paste(&mut self, text: String) {
        if self.modal.is_none() {
            if let Some(s) = self.session_mut() {
                s.on_paste(text);
            }
        }
    }

    pub fn on_focus(&mut self, gained: bool) {
        if let Some(s) = self.session_mut() {
            s.on_focus(gained);
        }
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
                sync: None,
            });
            let _ = tx.send(d);
        });
    }

    fn bg_msg<F>(&self, f: F)
    where
        F: FnOnce() -> Result<String, String> + Send + 'static,
    {
        self.bg(move || f().map(|msg| Done { msg, next: None, sync: None }));
    }

    pub fn on_done(&mut self, d: Done) {
        self.status = d.msg;
        if self.status.starts_with("Error") {
            self.temp_access = None;
        }
        self.refresh();
        if let Some(stage) = d.sync {
            match stage {
                SyncStage::Manifest { sandbox, agent, manifest } => self.pick_sync_items(sandbox, agent, manifest),
                SyncStage::Check { sync, check, offered } => self.confirm_sync(sync, check, offered),
                SyncStage::Attach { sync, profile } => self.attach_and_check(sync, vec![profile], vec![], 2),
                SyncStage::Generated { sync, name, yaml } => {
                    self.modal = Some(Modal::Confirm {
                        text: format!(
                            "Import a profile named `{name}`, create the provider, attach it to {} and run the installs? You can remove it afterwards.\n\n{}",
                            sync.sandbox,
                            profile_summary(&yaml)
                        ) + "\n\nv: view the full profile",
                        kind: ConfKind::SyncGen { sync, name, yaml },
                    });
                }
            }
            return;
        }
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
            if cli::is_github_key(&key) && cli::gh_token().is_some() {
                let items = vec![
                    "Use the token from `gh auth token` (read on this host; never shown or put on a command line)".to_string(),
                    "Paste a token (hidden)".to_string(),
                ];
                self.select(
                    "GitHub token",
                    items,
                    false,
                    SelKind::GhToken(InKind::Cred { provider, keys, values, update, next }),
                );
                return;
            }
            let mask = key != "FORGEJO_HOST";
            let hint = if cli::is_github_key(&key) {
                ". Use a fine-grained, read-only token: github.com/settings/personal-access-tokens/new"
            } else {
                ""
            };
            let title = format!("{provider}: {key}{}{hint}", if mask { " (hidden)" } else { "" });
            self.input(&title, mask, InKind::Cred { provider, keys, values, update, next });
            return;
        }
        let dir = self.dir.clone();
        let attach = if update { None } else { self.sync_secret.take() };
        self.status = format!("Working on provider {provider}...");
        self.bg(move || {
            let msg = if update {
                cli::update_credentials(&provider, &values)?
            } else {
                cli::create_provider(&dir, &provider, &values)?
            };
            let sync = attach.map(|sync| SyncStage::Attach { sync, profile: provider.clone() });
            Ok(Done { msg, next, sync })
        });
    }

    /// Run `claude setup-token` with the terminal handed over; `finish_job`
    /// then asks for the printed token (the command has no machine-readable output).
    fn generate_token(&mut self, flow: InKind) {
        self.job = Some(vec![Step { prog: "claude".into(), args: vec!["setup-token".into()] }]);
        self.token_flow = Some(flow);
    }

    /// Called after a terminal hand-over: resume a token flow with the paste prompt.
    /// Remove the staging directory of the "add skills" flow, if one exists.
    pub fn cleanup_sync_stage(&mut self) {
        self.sync_secret = None;
        if let Some(dir) = self.sync_stage.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn pick_sync_items(&mut self, sandbox: String, agent: String, manifest: cli::SyncManifest) {
        self.cleanup_sync_stage();
        self.sync_stage = Some(manifest.stage_dir.clone());
        let labels: Vec<String> = manifest.items.iter().map(sync_label).collect();
        let secrets = match manifest.findings {
            Some(0) => "secret scan clean".to_string(),
            Some(n) => format!("secret scan: {n} finding(s)"),
            None => "secret scan skipped (gitleaks missing)".to_string(),
        };
        let title = format!("Add to {sandbox} ({agent}); Space marks, {secrets}");
        let defaults: Vec<bool> = manifest.items.iter().map(|i| i.default).collect();
        let kind = SelKind::SyncItems { sandbox, agent, manifest, labels: labels.clone() };
        self.select(&title, labels, true, kind);
        if let Some(Modal::Select { marked, .. }) = &mut self.modal {
            *marked = defaults;
        }
    }

    fn confirm_sync(&mut self, sync: SyncPlan, check: cli::SyncCheck, offered: u8) {
        if !check.ok {
            let mut text = format!("The policy of {} blocks these installs:\n", sync.sandbox);
            for id in &check.blocked {
                text.push_str(&format!("  {id}\n"));
            }
            text.push_str("\nMissing network access (host, binary):\n");
            for (host, bin) in &check.missing {
                text.push_str(&format!("  {host}  {bin}\n"));
            }
            if offered == 0 {
                let mut hosts: Vec<&str> = check.missing.iter().map(|(h, _)| h.as_str()).collect();
                // One host can be missing for several programs.
                hosts.sort();
                hosts.dedup();
                let mut items = Vec::new();
                if !check.fixes.is_empty() {
                    items.push(format!("Add the built-in profile(s): {}", check.fixes.join(", ")));
                }
                items.push("Generate a custom profile for exactly this (review it first)".to_string());
                items.push("Cancel".to_string());
                let title = format!("{} blocks installs; missing: {}", sync.sandbox, hosts.join(", "));
                self.select(&title, items, false, SelKind::SyncAccess { sync, check });
                return;
            }
            if let (Some((profile, keys)), true) = (check.secret_fixes.first(), offered < 2) {
                let exists = self.snap.providers.iter().any(|g| &g.name == profile);
                text.push_str(&format!(
                    "\nThe `{profile}` provider grants the rest but needs a secret ({}). ",
                    keys.join(" or ")
                ));
                text.push_str(if exists {
                    "It exists on this gateway: attach it to the sandbox?"
                } else if keys.iter().any(|k| cli::is_github_key(k)) && cli::gh_token().is_some() {
                    "Create it from your `gh` login (the token stays on this host) and attach it?"
                } else {
                    "Create it now (you will be asked for the secret, hidden) and attach it?"
                });
                self.modal = Some(Modal::Confirm {
                    text,
                    kind: ConfKind::SyncSecret { sync, profile: profile.clone() },
                });
                return;
            }
            if !check.unfixed.is_empty() {
                text.push_str("\nNo built-in profile covers:");
                for (host, bin) in &check.unfixed {
                    text.push_str(&format!(" {host} ({bin})"));
                }
                text.push('\n');
            }
            text.push_str("\nAttach a provider profile that grants it (a: attach provider), or edit the policy, then retry.");
            self.cleanup_sync_stage();
            self.modal = Some(Modal::Info(text));
            return;
        }
        let steps_list = sync_steps(&sync);
        let uploads = steps_list.iter().any(|s| s.args.iter().any(|a| a == "upload"));
        if uploads && sync.manifest.findings.unwrap_or(0) > 0 {
            let n = sync.manifest.findings.unwrap_or(0);
            self.cleanup_sync_stage();
            self.modal = Some(Modal::Info(format!(
                "The secret scan found {n} finding(s) in the files that would be uploaded. Run `sync-agents --json` and review the staged files, or deselect the uploads (own-skills, context)."
            )));
            return;
        }
        let steps = steps_list.len();
        self.modal = Some(Modal::Confirm {
            text: format!(
                "Run {steps} step(s) in sandbox '{}' for {}? Policy allows the network access they need.",
                sync.sandbox, sync.agent
            ),
            kind: ConfKind::Sync(sync),
        });
    }

    /// Create (the ones in `create`) and attach `profiles`, then check the policy again.
    /// `round` is stored in the result so the same offer is not made twice.
    fn attach_and_check(&mut self, sync: SyncPlan, profiles: Vec<String>, create: Vec<String>, round: u8) {
        let dir = self.dir.clone();
        self.status = format!("Adding {} to {}...", profiles.join(", "), sync.sandbox);
        self.bg(move || {
            for p in &profiles {
                if create.contains(p) {
                    cli::create_provider(&dir, p, &[])?;
                }
                cli::os(&["sandbox", "provider", "attach", &sync.sandbox, p, "--wait", "--timeout", "30"])
                    .map_err(|e| {
                        if e.contains("not in a state required") {
                            format!(
                                "{p} overlaps the endpoints of a provider already on {} (for example pypi and github both define api.github.com). Detach one (x) and retry. {e}",
                                sync.sandbox
                            )
                        } else {
                            e
                        }
                    })?;
            }
            let check = cli::sync_check(&dir, &sync.sandbox, &sync.agent, &sync.ids)?;
            Ok(Done {
                msg: format!("Added {}", profiles.join(", ")),
                next: None,
                sync: Some(SyncStage::Check { sync, check, offered: round }),
            })
        });
    }

    /// Ask whether to remove the generated install access, if any is still attached;
    /// `before` is shown above the question.
    fn offer_drop(&mut self, before: &str) -> bool {
        let Some((sandbox, name)) = self.temp_access.take() else { return false };
        self.modal = Some(Modal::Confirm {
            text: format!("{before}Detach {name} from {sandbox} and delete it and its profile? Its network access is only needed while installing."),
            kind: ConfKind::DropAccess { sandbox, name },
        });
        true
    }

    /// After a terminal job. A sync that stopped says at which step; when the output reads
    /// like GitHub refusing access (a private repository), it offers the `github` provider
    /// and a retry from that step.
    pub fn finish_job(&mut self, failed: Option<Failed>) {
        self.capture = false;
        if let Some(f) = &failed {
            self.status = format!("Step failed: {} ({})", f.label, f.status);
        }
        if let (Some(f), Some(mut sync)) = (failed, self.running_sync.take()) {
            let output = if f.tail.is_empty() { String::new() } else { format!("\n\nLast output:\n{}", f.tail.join("\n")) };
            if auth_failure(&f.tail) {
                sync.skip += f.index;
                self.modal = Some(Modal::Confirm {
                    text: format!(
                        "This step failed, and its output looks like GitHub refusing access (a private repository?):\n  {}{output}\n\nCreate or attach the `github` provider (the token stays on this host), then run the installs again from this step?",
                        f.label
                    ),
                    kind: ConfKind::SyncSecret { sync, profile: "github".into() },
                });
                return;
            }
            self.cleanup_sync_stage();
            let text = format!("The installs stopped at a failed step ({}):\n  {}{output}", f.status, f.label);
            if !self.offer_drop(&format!("{text}\n\n")) {
                self.modal = Some(Modal::Info(text));
            }
            return;
        }
        self.running_sync = None;
        self.cleanup_sync_stage();
        self.offer_drop("");
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
                        providers: if p.2.is_empty() { vec![] } else { vec![p.2.into()] },
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
            SelKind::SyncAccess { sync, check } => {
                if first.starts_with("Add the built-in") {
                    let create: Vec<String> = check
                        .fixes
                        .iter()
                        .filter(|p| !self.snap.providers.iter().any(|g| &g.name == *p))
                        .cloned()
                        .collect();
                    self.attach_and_check(sync, check.fixes, create, 1);
                } else if first.starts_with("Generate") {
                    self.input(
                        &format!("Name for the generated provider (empty = install-{})", sync.sandbox),
                        false,
                        InKind::GenName(sync),
                    );
                } else {
                    self.cleanup_sync_stage();
                }
            }
            SelKind::GhToken(flow) => {
                let InKind::Cred { provider, keys, mut values, update, next } = flow else { return };
                let key = keys[values.len()].clone();
                match cli::gh_token().filter(|_| first.starts_with("Use")) {
                    Some(token) => {
                        values.push((key, token));
                        self.cred_step(InKind::Cred { provider, keys, values, update, next });
                    }
                    None => {
                        let title = format!(
                            "{provider}: {key} (hidden). Use a fine-grained, read-only token: github.com/settings/personal-access-tokens/new"
                        );
                        self.input(&title, true, InKind::Cred { provider, keys, values, update, next });
                    }
                }
            }
            SelKind::SyncAgents(sandbox) => {
                let agent = if first == "both" { "both".to_string() } else { first };
                let dir = self.dir.clone();
                self.status = "Reading the host's skills and plugins...".into();
                self.bg(move || {
                    let manifest = cli::sync_manifest(&dir, &sandbox, &agent)?;
                    Ok(Done {
                        msg: format!("Found {} item(s)", manifest.items.len()),
                        next: None,
                        sync: Some(SyncStage::Manifest { sandbox, agent, manifest }),
                    })
                });
            }
            SelKind::SyncItems { sandbox, agent, manifest, labels } => {
                let ids: Vec<String> = labels
                    .iter()
                    .zip(&manifest.items)
                    .filter(|(l, _)| picks.contains(l))
                    .map(|(_, i)| i.id.clone())
                    .collect();
                if ids.is_empty() {
                    self.cleanup_sync_stage();
                    self.status = "Nothing selected".into();
                    return;
                }
                let dir = self.dir.clone();
                self.status = "Checking the sandbox policy...".into();
                self.bg(move || {
                    let check = cli::sync_check(&dir, &sandbox, &agent, &ids)?;
                    Ok(Done {
                        msg: "Policy checked".into(),
                        next: None,
                        sync: Some(SyncStage::Check { sync: SyncPlan { sandbox, agent, manifest, ids, skip: 0 }, check, offered: 0 }),
                    })
                });
            }
            SelKind::Blocked { sandbox, gateway, b } => {
                if let Some(rest) = first.strip_prefix("Allow ") {
                    let all = rest.starts_with("all");
                    self.watch.decide(&sandbox, &b, State::Allowed(all));
                    self.status = format!("Allowing {} to reach {} in {sandbox}...", b.program(), b.target());
                    self.bg_msg(move || watch::allow(&gateway, &sandbox, &b, all));
                } else {
                    let until_end = first.contains("session ends");
                    let state = if until_end { State::Ignored } else { State::IgnoredOnce };
                    self.watch.decide(&sandbox, &b, state);
                    let how = if until_end { "until the session ends" } else { "this time" };
                    self.status = format!("Ignored {} -> {} {how}", b.program(), b.target());
                }
            }
            SelKind::Review { sandbox, denials } => {
                let Some(i) = self.watch.for_sandbox(&sandbox).iter().position(|s| review_label(s) == first) else { return };
                match denials.get(i).cloned() {
                    Some(Denial::Blocked(b)) => {
                        let gateway = self.snap.gateway.clone();
                        self.watch.reprompt(&sandbox, &gateway, b);
                        self.show_prompt();
                    }
                    Some(Denial::Notice { what, reason }) => {
                        self.modal = Some(Modal::Info(notice_text(&sandbox, &what, &reason)));
                    }
                    None => {}
                }
            }
            SelKind::SessionMenu => {
                if first.starts_with("Dashboard") {
                    self.active = None;
                    self.status = "Shell kept running; Enter on its sandbox returns to it".into();
                    self.refresh();
                } else if first.starts_with("Close") {
                    if let Some(i) = self.active {
                        let s = self.close_session(i);
                        self.status = format!("Closed the shell in {}", s.sandbox);
                        self.refresh();
                    }
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
            InKind::GenName(sync) => {
                let raw = if text.trim().is_empty() { format!("install-{}", sync.sandbox) } else { text };
                let name: String = raw
                    .trim()
                    .to_lowercase()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                if self.snap.providers.iter().any(|p| p.name == name) {
                    self.cleanup_sync_stage();
                    self.modal = Some(Modal::Info(format!("A provider named {name} already exists. Pick another name.")));
                    return;
                }
                let dir = self.dir.clone();
                self.status = "Generating the profile...".into();
                self.bg(move || {
                    let yaml = cli::sync_gen_profile(&dir, &sync.sandbox, &sync.agent, &sync.ids, &name)?;
                    Ok(Done {
                        msg: "Profile generated".into(),
                        next: None,
                        sync: Some(SyncStage::Generated { sync, name, yaml }),
                    })
                });
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
            ConfKind::Sync(plan) => {
                self.job = Some(sync_steps(&plan));
                self.capture = true;
                self.running_sync = Some(plan);
            }
            ConfKind::SyncGen { sync, name, yaml } => {
                self.temp_access = Some((sync.sandbox.clone(), name.clone()));
                self.status = format!("Importing {name} and attaching it to {}...", sync.sandbox);
                let dir = self.dir.clone();
                self.bg(move || {
                    cli::import_generated(&yaml, &name, &sync.sandbox)?;
                    let check = cli::sync_check(&dir, &sync.sandbox, &sync.agent, &sync.ids)?;
                    Ok(Done {
                        msg: format!("Attached {name}"),
                        next: None,
                        sync: Some(SyncStage::Check { sync, check, offered: 1 }),
                    })
                });
            }
            ConfKind::DropAccess { sandbox, name } => self.bg_msg(move || cli::drop_generated(&sandbox, &name)),
            ConfKind::SyncSecret { sync, profile } => {
                if self.snap.providers.iter().any(|g| g.name == profile) {
                    self.attach_and_check(sync, vec![profile], vec![], 2);
                } else {
                    self.sync_secret = Some(sync);
                    self.start_creds(profile, false, None);
                }
            }
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
        if self.active.is_some() {
            self.session_key(key);
            return;
        }
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

    /// Keys while a shell has focus: the chord, then an open modal, then the shell.
    fn session_key(&mut self, key: KeyEvent) {
        let menu = matches!(self.modal, Some(Modal::Select { kind: SelKind::SessionMenu, .. }));
        if term::is_chord(&key) {
            if menu {
                self.modal = None;
            } else if self.modal.is_none() {
                let name = self.session().map(|s| s.sandbox.clone()).unwrap_or_default();
                let items = vec![
                    "Back to the session".into(),
                    "Dashboard (keep the session running)".into(),
                    "Close the session".into(),
                ];
                self.select(&format!("Shell in {name}"), items, false, SelKind::SessionMenu);
            }
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.modal_key(modal, key);
            return;
        }
        if let Some(s) = self.session_mut() {
            s.on_key(&key);
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
        let Some(name) = self.selected_sandbox().map(|s| s.name.clone()) else {
            if matches!(code, KeyCode::Char('K' | 'l' | 'd' | 's' | 'S' | 'a' | 'x' | 'p') | KeyCode::Enter) {
                self.modal = Some(Modal::Info(
                    "No sandbox selected. This gateway has none: press n to create one (g switches gateway).".into(),
                ));
            }
            return;
        };
        match code {
            KeyCode::Char('p') => self.review(name),
            KeyCode::Char('l') => match LogView::open(name) {
                Ok(lv) => self.logview = Some(lv),
                Err(e) => self.status = format!("Error: {e}"),
            },
            KeyCode::Enter => self.open_session(name),
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
            KeyCode::Char('K') => {
                let items = vec!["claude-code".into(), "pi".into(), "both".into()];
                self.select("Add skills and plugins for", items, false, SelKind::SyncAgents(name));
            }
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

    fn open_viewer(&mut self, title: &str, text: String, back: Modal) {
        self.modal = Some(Modal::Viewer { title: title.into(), text, scroll: 0, back: Box::new(back) });
    }

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) {
        // A prompt can open mid-typing: Enter meant for the shell must not pick an answer.
        let fresh = self.prompt_at.is_some_and(|t| t.elapsed() < PROMPT_GRACE);
        if fresh && matches!(modal, Modal::Info(_) | Modal::Select { kind: SelKind::Blocked { .. }, .. }) {
            self.modal = Some(modal);
            return;
        }
        match modal {
            Modal::Info(text) => {
                if key.code == KeyCode::Char('v') {
                    self.open_viewer("Details", text.clone(), Modal::Info(text));
                }
            }
            Modal::Viewer { title, text, mut scroll, back } => {
                // The viewer fills the screen minus its border and margin; stop when the last line is at the bottom.
                let rows = ratatui::crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24).saturating_sub(4);
                let last = (text.lines().count() as u16).saturating_sub(rows);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                        self.modal = Some(*back);
                        return;
                    }
                    KeyCode::Char('j') | KeyCode::Down => scroll = (scroll + 1).min(last),
                    KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => scroll = (scroll + 10).min(last),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::Char('g') | KeyCode::Home => scroll = 0,
                    KeyCode::Char('G') | KeyCode::End => scroll = last,
                    _ => {}
                }
                self.modal = Some(Modal::Viewer { title, text, scroll, back });
            }
            Modal::Confirm { text, kind } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.on_confirm(kind),
                KeyCode::Char('v') => {
                    let full = match &kind {
                        ConfKind::SyncGen { yaml, .. } => yaml.clone(),
                        _ => text.clone(),
                    };
                    self.open_viewer("Details", full, Modal::Confirm { text, kind });
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.cleanup_sync_stage();
                    if matches!(kind, ConfKind::Sync(_) | ConfKind::SyncSecret { .. }) {
                        self.offer_drop("");
                    }
                }
                _ => self.modal = Some(Modal::Confirm { text, kind }),
            },
            Modal::Input { title, mut buf, mask, kind } => match key.code {
                KeyCode::Esc => self.cleanup_sync_stage(),
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
                KeyCode::Esc if matches!(kind, SelKind::SessionMenu | SelKind::Review { .. }) => {}
                KeyCode::Esc if matches!(kind, SelKind::Blocked { .. }) => self.on_select(kind, vec!["Ignore this time".into()]),
                KeyCode::Esc => self.cleanup_sync_stage(),
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
