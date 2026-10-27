//! Application state, modal flows and key handling.
use crate::cli::{self, Snapshot};
use crate::clip;
use crate::ghrepo::{self, GhRepo};
use crate::logview::LogView;
use crate::term::{self, Session};
use crate::watch::{self, Blocked, Denial, Follow, Prompt, State, Watch};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{atomic::Ordering, mpsc::Sender, Arc, Mutex},
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

/// Entry in the Providers tab's create list that starts the repo token flow.
const GH_REPO_ITEM: &str = "GitHub repo, read/write (fine-grained token)...";

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
    /// Absolute host folder mounted at `/sandbox/<folder name>`; empty = none.
    pub share: String,
    pub share_rw: bool,
    /// Read/write access to the shared folder's GitHub repo; the provider is created on confirm.
    pub gh: Option<GhGrant>,
    /// Where the wizard continues once a provider the plan needs exists.
    pub resume: Resume,
}

/// A repo token pasted for a new sandbox; kept in memory only until the provider is created.
#[derive(Clone)]
pub struct GhGrant {
    pub repo: GhRepo,
    pub token: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Resume {
    #[default]
    Name,
    Command,
    Create,
}

/// What a repo token is for.
#[derive(Clone)]
pub enum Goal {
    /// A token only this sandbox uses: `github-<sandbox>-<repo>`.
    Sandbox(Plan),
    /// A token shared by sandboxes, created for this plan: `github-<repo>`.
    SharedFor(Plan),
    /// A shared token created from the Providers tab.
    Shared,
    /// A new token for an existing provider.
    Renew(String),
}

#[derive(Clone)]
pub struct RepoFlow {
    pub repo: GhRepo,
    pub goal: Goal,
}

#[derive(Clone)]
pub enum SelKind {
    Preset,
    Image,
    Providers(Plan),
    /// Which of several sandboxes (gateway, name) sharing the project folder to open.
    OpenShared(Vec<(String, String)>),
    /// No folder, the folder `osh` was started in, or one typed in.
    ShareChoice(Plan),
    /// Read-only or read-write access to the shared host folder.
    ShareAccess(Plan),
    AttachProvider(String),
    DetachProvider(String),
    NewProvider,
    /// GitHub access for the repo of the shared folder.
    GhAccess(Plan, GhRepo),
    /// Which tracked repo token of a sandbox to renew.
    RenewPick(Vec<ghrepo::Tracked>),
    Gateway,
    /// How to obtain the Claude Code setup token; carries the credential flow to resume.
    ClaudeToken(InKind),
    /// Whether to take a GitHub token from `gh auth token` or paste one; carries the flow to resume.
    GhToken(InKind),
    /// Which attached provider to detach so that `profile` can be attached.
    Conflict {
        sync: SyncPlan,
        profile: String,
        round: u8,
    },
    /// How to grant the network access a blocked sync is missing.
    SyncAccess {
        sync: SyncPlan,
        check: cli::SyncCheck,
    },
    /// Which agents to add skills for, in this sandbox.
    SyncAgents(String),
    /// Items to add; `labels` are the list entries in manifest order.
    SyncItems {
        sandbox: String,
        agent: String,
        manifest: cli::SyncManifest,
        labels: Vec<String>,
    },
    /// Ctrl-] menu over the focused shell.
    SessionMenu,
    /// What to do about a program the sandbox policy blocked.
    Blocked {
        sandbox: String,
        gateway: String,
        b: Blocked,
    },
    /// A rejected request-target holds an encoded `/`: allow it through a generated provider.
    Slashes {
        sandbox: String,
        host: String,
        port: u16,
        binary: String,
    },
    /// Denials seen in a sandbox (`p`); choosing a blocked one asks again.
    Review {
        sandbox: String,
        denials: Vec<Denial>,
    },
}

#[derive(Clone)]
pub enum InKind {
    OtherImage,
    Name(Plan),
    Cpu(Plan),
    Memory(Plan),
    Share(Plan),
    Command(Plan),
    /// The hidden token for a repo; checked against GitHub before it is used.
    RepoToken(RepoFlow),
    /// `OWNER/REPO` for a shared repo provider.
    RepoName,
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
    /// Show the token steps, open the page, then ask for the token.
    RepoSteps(RepoFlow),
    /// Stop and start these sandboxes so they pick up a renewed token.
    Restart(Vec<String>),
    DeleteSandbox(String),
    DeleteProvider(String),
    ResetGateway(String),
    Sync(SyncPlan),
    /// Set up a provider that needs a secret, then attach it and check again.
    SyncSecret {
        sync: SyncPlan,
        profile: String,
    },
    /// Import, create and attach a generated profile, then check again.
    SyncGen {
        sync: SyncPlan,
        name: String,
        yaml: String,
    },
    /// Detach and delete the install-access provider once the installs are done.
    DropAccess {
        sandbox: String,
        name: String,
    },
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
    /// What the providers attached to a sandbox allow; read-only, scrollable.
    Providers {
        sandbox: String,
        perms: Vec<cli::ProviderPerm>,
        scroll: u16,
    },
    /// Read-only scrollable text; `back` is shown again when it closes.
    Viewer {
        title: String,
        text: String,
        scroll: u16,
        back: Box<Modal>,
    },
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
            Some(c) if self.args.iter().any(|a| a == "-c") => {
                c.strip_prefix(SANDBOX_PATH).unwrap_or(c).to_string()
            }
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
    pub gh: Option<GhStage>,
}

/// Result of checking a pasted repo token against GitHub.
pub enum GhStage {
    /// A repo token was swapped; these running sandboxes still use the old one.
    Renewed {
        provider: String,
        sandboxes: Vec<String>,
    },
    Checked {
        flow: Box<RepoFlow>,
        token: String,
        problem: Option<String>,
    },
}

/// Result of a background step of the "add skills to a sandbox" flow.
pub enum SyncStage {
    Manifest {
        sandbox: String,
        agent: String,
        manifest: cli::SyncManifest,
    },
    /// `offered`: fix rounds already done (0 none, 1 anonymous profiles, 2 secret profiles).
    Check {
        sync: SyncPlan,
        check: cli::SyncCheck,
        offered: u8,
    },
    /// A provider that needs a secret now exists: attach it and check again.
    Attach { sync: SyncPlan, profile: String },
    /// A generated profile for the user to review.
    Generated {
        sync: SyncPlan,
        name: String,
        yaml: String,
    },
    /// A conflicting provider was detached: attach `profile` again.
    Retry {
        sync: SyncPlan,
        profile: String,
        round: u8,
    },
    /// The gateway would not attach `profile` next to the providers already `attached`.
    Conflict {
        sync: SyncPlan,
        profile: String,
        attached: Vec<String>,
        round: u8,
    },
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
    /// Installed gateway.toml differs from the one osh ships (`osh setup` installs it).
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
    /// Start of the run, the clock the spinner frames are read from.
    born: Instant,
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
    /// Repo tokens osh created, with their expiry; shown on the Providers tab.
    pub tracked: Vec<ghrepo::Tracked>,
    selected: Arc<Mutex<Option<String>>>,
    /// Sandbox (gateway, name) to open once that gateway's snapshot has loaded.
    pending_open: Option<(String, String)>,
    wake: Sender<()>,
    done_tx: Sender<Done>,
}

pub struct ProfileRow {
    pub id: String,
    pub in_repo: bool,
    pub gateway_version: Option<u64>,
}

const PRESETS: [(&str, &str, &str); 3] = [
    (
        "Claude Code (subscription)",
        "claude-code:latest",
        "claude-code",
    ),
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
    let tag = if i.kind == "host-only" {
        " (host-only)"
    } else {
        ""
    };
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
    for item in plan
        .manifest
        .items
        .iter()
        .filter(|i| plan.ids.contains(&i.id))
    {
        for (_, spec) in item
            .agents
            .iter()
            .filter(|(a, _)| wanted.contains(&a.as_str()))
        {
            if let Some(cmds) = spec.as_array() {
                steps.extend(
                    cmds.iter()
                        .filter_map(|c| c.as_str())
                        .map(|c| in_sandbox(&plan.sandbox, c)),
                );
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

/// Choices when the gateway refuses to attach `profile`: detach one of the others (only
/// needed while installing, and can be attached again afterwards), or stop. The usual
/// cause is a credentialed endpoint (github) on a host another profile allows uninspected.
pub fn conflict_items(attached: &[String], profile: &str) -> Vec<String> {
    let mut items: Vec<String> = attached
        .iter()
        .filter(|a| *a != profile)
        .map(|a| format!("Detach {a}, then attach {profile} and check again"))
        .collect();
    items.push("Cancel".into());
    items
}

/// The provider a `conflict_items` choice detaches.
pub fn conflict_pick(item: &str) -> Option<&str> {
    item.strip_prefix("Detach ")?.split(',').next()
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

fn prog_of(binary: &str) -> &str {
    binary.rsplit('/').next().unwrap_or(binary)
}

fn review_label(s: &watch::Seen) -> String {
    let state = match s.state {
        State::Pending => "pending",
        State::Allowed(false) => "allowed (read-only)",
        State::Allowed(true) => "allowed (all)",
        State::IgnoredOnce => "ignored once",
        State::Ignored => "ignored",
        State::Never => "never",
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

fn hit_label((gw, name): &(String, String)) -> String {
    format!("{name} on {gw}")
}

/// True when the installed `gateway.toml` is missing or differs from the one this host should run.
fn config_stale() -> bool {
    let os = if cfg!(target_os = "macos") {
        crate::platform::Os::MacOs
    } else {
        crate::platform::Os::Linux
    };
    let name = crate::gateway::config_name_for(os, cli::runtime());
    crate::gateway::stale(
        name,
        &crate::platform::RealHost,
        &crate::gateway::Paths::detect(),
    )
}

impl App {
    pub fn new(
        dir: PathBuf,
        selected: Arc<Mutex<Option<String>>>,
        wake: Sender<()>,
        done_tx: Sender<Done>,
    ) -> Self {
        App {
            config_stale: config_stale(),
            dir,
            tab: Tab::Sandboxes,
            snap: Snapshot::default(),
            cursor: [0; 5],
            modal: None,
            logview: None,
            status: "Loading...".into(),
            status_at: None,
            born: Instant::now(),
            quit: false,
            job: None,
            capture: false,
            running_sync: None,
            sessions: Vec::new(),
            active: None,
            watch: Watch::with_never_file(watch::never_path()),
            follows: Vec::new(),
            prompt_at: None,
            token_flow: None,
            sync_stage: None,
            sync_secret: None,
            tracked: ghrepo::load_tracked(ghrepo::tracked_path().as_deref()),
            temp_access: None,
            selected,
            pending_open: None,
            wake,
            done_tx,
        }
    }

    /// Open one of `hits` (gateway, sandbox) from `cli::find_shared`: straight away when there
    /// is one, after a pick when there are several.
    pub fn jump_to(&mut self, hits: Vec<(String, String)>) {
        match hits.len() {
            0 => self.status = "No ready sandbox shares this folder or its git root".into(),
            1 => self.go_to(hits[0].clone()),
            _ => {
                let items = hits.iter().map(hit_label).collect();
                self.select(
                    "Sandboxes sharing this project",
                    items,
                    false,
                    SelKind::OpenShared(hits),
                );
            }
        }
    }

    /// Switch to the sandbox's gateway if needed; the session opens once that gateway has loaded.
    fn go_to(&mut self, (gw, name): (String, String)) {
        if gw != cli::gateway() {
            cli::set_gateway(&gw);
            self.snap = Snapshot::default();
            self.snap.gateway = gw.clone();
            self.cursor = [0; 5];
            self.status = format!("Switching to {gw}...");
            self.sync_selection();
            self.refresh();
        }
        self.pending_open = Some((gw, name));
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
                gateway_version: self
                    .snap
                    .profiles
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.version),
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
        if self.snap.gateway_ok
            && self.snap.error.is_none()
            && self
                .pending_open
                .as_ref()
                .is_some_and(|(gw, _)| *gw == self.snap.gateway)
        {
            if let Some((_, name)) = self.pending_open.take() {
                self.open_session(name);
            }
        }
        self.config_stale = config_stale();
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
        if self
            .status_at
            .as_ref()
            .is_none_or(|(s, _)| *s != self.status)
        {
            self.status_at = Some((self.status.clone(), Instant::now()));
        }
    }

    /// Whether the status names work still in progress (`Detaching x...`).
    pub fn busy(&self) -> bool {
        self.status.ends_with("...")
    }

    /// The spinner frame for now, while busy.
    pub fn spinner(&self) -> Option<&'static str> {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        self.busy()
            .then(|| FRAMES[(self.born.elapsed().as_millis() / 80) as usize % FRAMES.len()])
    }

    /// The status, while it is new enough to show over a shell (always while busy).
    pub fn flash(&self) -> Option<&str> {
        self.status_at
            .as_ref()
            .filter(|(s, t)| {
                *s == self.status && (self.busy() || t.elapsed() < Duration::from_secs(6))
            })
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
        self.sessions
            .iter()
            .any(|s| s.sandbox == sandbox && s.gateway == self.snap.gateway)
    }

    /// Focus the shell of `sandbox`, starting one sized to the screen if none runs.
    fn open_session(&mut self, sandbox: String) {
        let gw = cli::gateway();
        if let Some(i) = self
            .sessions
            .iter()
            .position(|s| s.sandbox == sandbox && s.gateway == gw)
        {
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
                    Err(e) => {
                        self.status = format!("Blocked-request watcher off for {sandbox}: {e}")
                    }
                }
            }
            Err(e) => self.status = format!("Error: {e}"),
        }
    }

    fn close_session(&mut self, i: usize) -> Session {
        let s = self.sessions.remove(i);
        self.follows
            .retain(|f| !(f.sandbox == s.sandbox && f.gateway == s.gateway));
        if !self.sessions.iter().any(|o| o.sandbox == s.sandbox) {
            if s.pasted.load(Ordering::Relaxed) {
                let (gateway, sandbox) = (s.gateway.clone(), s.sandbox.clone());
                thread::spawn(move || clip::clean(&gateway, &sandbox));
            }
            self.watch.end_session(&s.sandbox);
            if matches!(&self.modal, Some(Modal::Select { kind: SelKind::Blocked { sandbox, .. }, .. }) if *sandbox == s.sandbox)
            {
                self.modal = None;
            }
        }
        self.active = match self.active {
            Some(a) if a == i => {
                if matches!(
                    self.modal,
                    Some(Modal::Select {
                        kind: SelKind::SessionMenu,
                        ..
                    })
                ) {
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
        let mut copy = None;
        for (i, s) in self.sessions.iter_mut().enumerate() {
            s.pump();
            // Only the shell in front may write to the host clipboard; others are dropped.
            if let Some(t) = s.vt.take_clipboard() {
                if self.active == Some(i) {
                    copy = Some(t);
                }
            }
        }
        if let Some(t) = copy {
            self.copy(&t, "sandbox copied");
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
                self.copy(&text, "copied");
            }
        }
    }

    /// Host clipboard: OSC 52 through the terminal, plus `pbcopy` on macOS.
    fn copy(&mut self, text: &str, what: &str) {
        let mut out = std::io::stdout();
        let _ = out
            .write_all(term::osc52(text).as_bytes())
            .and_then(|_| out.flush());
        if cfg!(target_os = "macos") {
            if let Ok(mut c) = Command::new("pbcopy")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                if let Some(mut i) = c.stdin.take() {
                    let _ = i.write_all(text.as_bytes());
                }
                thread::spawn(move || c.wait());
            }
        }
        self.status = format!("{what} {} chars", text.chars().count());
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
        if self.watch.has_queued()
            && self.modal.is_none()
            && self.logview.is_none()
            && self.job.is_none()
        {
            self.show_prompt();
        }
    }

    fn show_prompt(&mut self) {
        let Some(p) = self.watch.next() else { return };
        self.prompt_at = Some(Instant::now());
        let here = self.session().map(|s| s.sandbox.clone());
        match p {
            Prompt::Blocked {
                sandbox,
                gateway,
                b,
                count,
            } => {
                let prog = b.program().to_string();
                let mut title = format!("{prog} was blocked from reaching {}", b.target());
                if here.as_deref() != Some(sandbox.as_str()) {
                    title.push_str(&format!(" in {sandbox}"));
                }
                if count > 1 {
                    title.push_str(&format!(" ({count}x)"));
                }
                let mut items = Vec::new();
                // A profile for the host knows its details (encoded slashes, git POSTs).
                let attached = cli::attached_providers(&sandbox);
                for id in cli::profiles_for_host(&self.dir, &b.host)
                    .into_iter()
                    .filter(|id| !attached.contains(id))
                {
                    items.push(format!("Attach the {id} provider (recommended)"));
                }
                // Read-only means HTTP method rules, so only HTTP ports get it.
                if matches!(b.port, 80 | 443 | 8080 | 8443) {
                    items.push(format!("Allow read-only (GET/HEAD/OPTIONS) for {prog}"));
                }
                items.push(format!("Allow all methods for {prog}"));
                items.push("Ignore this time".into());
                items.push("Ignore until the session ends".into());
                items.push(format!("Never allow {} in {sandbox}", b.target()));
                self.select(
                    &title,
                    items,
                    false,
                    SelKind::Blocked {
                        sandbox,
                        gateway,
                        b,
                    },
                );
            }
            Prompt::Notice {
                sandbox,
                what,
                reason,
            } => {
                if let Some((host, port, binary)) = self.slash_fix(&sandbox, &what, &reason) {
                    let title = format!(
                        "{host}:{port} rejected a request path with an encoded '/' in {sandbox}"
                    );
                    let items = vec![
                        format!("Allow encoded slashes for {host} ({})", prog_of(&binary)),
                        "Dismiss".into(),
                    ];
                    self.select(
                        &title,
                        items,
                        false,
                        SelKind::Slashes {
                            sandbox,
                            host,
                            port,
                            binary,
                        },
                    );
                    return;
                }
                self.modal = Some(Modal::Info(notice_text(&sandbox, &what, &reason)));
            }
        }
    }

    /// The host, port and program to fix an encoded-slash rejection for, when the program
    /// is known from an earlier blocked request to the same host.
    fn slash_fix(&self, sandbox: &str, what: &str, reason: &str) -> Option<(String, u16, String)> {
        if !reason.contains("allow_encoded_slash") {
            return None;
        }
        let (host, port) = what.split_whitespace().next()?.rsplit_once(':')?;
        let port: u16 = port.parse().ok()?;
        let binary = self
            .watch
            .for_sandbox(sandbox)
            .into_iter()
            .find_map(|s| match &s.denial {
                Denial::Blocked(b) if b.host == host && b.port == port => Some(b.binary.clone()),
                _ => None,
            })?;
        Some((host.to_string(), port, binary))
    }

    /// `p`: the denials seen in a sandbox while its shell was open.
    fn review(&mut self, sandbox: String) {
        let seen = self.watch.for_sandbox(&sandbox);
        if seen.is_empty() {
            let how = if self.has_session(&sandbox) {
                ""
            } else {
                " The watcher runs while its shell is open (Enter)."
            };
            self.modal = Some(Modal::Info(format!(
                "No blocked requests seen in {sandbox}.{how}"
            )));
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
            // A terminal that holds only an image on the clipboard reports an empty paste.
            if text.is_empty() && self.paste_image() {
                return;
            }
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
                gh: None,
            });
            let _ = tx.send(d);
        });
    }

    fn bg_msg<F>(&self, f: F)
    where
        F: FnOnce() -> Result<String, String> + Send + 'static,
    {
        self.bg(move || {
            f().map(|msg| Done {
                msg,
                next: None,
                sync: None,
                gh: None,
            })
        });
    }

    pub fn on_done(&mut self, d: Done) {
        self.status = d.msg;
        self.tracked = ghrepo::load_tracked(ghrepo::tracked_path().as_deref());
        if self.status.starts_with("Error") {
            self.temp_access = None;
        }
        self.refresh();
        if let Some(stage) = d.sync {
            match stage {
                SyncStage::Manifest {
                    sandbox,
                    agent,
                    manifest,
                } => self.pick_sync_items(sandbox, agent, manifest),
                SyncStage::Check {
                    sync,
                    check,
                    offered,
                } => self.confirm_sync(sync, check, offered),
                SyncStage::Attach { sync, profile } => {
                    self.attach_and_check(sync, vec![profile], vec![], 2)
                }
                SyncStage::Retry {
                    sync,
                    profile,
                    round,
                } => self.attach_and_check(sync, vec![profile], vec![], round),
                SyncStage::Conflict {
                    sync,
                    profile,
                    attached,
                    round,
                } => {
                    let title = format!(
                        "{profile} cannot be attached next to what {} has",
                        sync.sandbox
                    );
                    let items = conflict_items(&attached, &profile);
                    self.select(
                        &title,
                        items,
                        false,
                        SelKind::Conflict {
                            sync,
                            profile,
                            round,
                        },
                    );
                }
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
        match d.gh {
            Some(GhStage::Checked {
                flow,
                token,
                problem,
            }) => return self.repo_checked(*flow, token, problem),
            Some(GhStage::Renewed {
                provider,
                sandboxes,
            }) => {
                if !sandboxes.is_empty() && !self.status.starts_with("Error") {
                    self.modal = Some(Modal::Confirm {
                        text: format!(
                            "{provider} has a new token, but programs already running in {} keep the old one.\n\nRestart {}? Running programs in {} stop and /tmp is cleared; files in /sandbox are kept.",
                            sandboxes.join(", "),
                            if sandboxes.len() == 1 { "it" } else { "them" },
                            if sandboxes.len() == 1 { "it" } else { "them" },
                        ),
                        kind: ConfKind::Restart(sandboxes),
                    });
                }
                return;
            }
            None => {}
        }
        if let Some((plan, mut missing)) = d.next {
            if self.status.starts_with("Error") {
                return;
            }
            if missing.is_empty() {
                match plan.resume {
                    Resume::Name => self.ask_name(plan),
                    Resume::Command => self.ask_command(plan),
                    Resume::Create => self.job = Some(self.sandbox_steps(&plan)),
                }
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

    fn start_creds(&mut self, provider: String, update: bool, next: Option<(Plan, Vec<String>)>) {
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
        self.cred_step(InKind::Cred {
            provider,
            keys,
            values: vec![],
            update,
            next,
        });
    }

    /// Ask for the next key, or run the create/update once all are collected.
    fn cred_step(&mut self, kind: InKind) {
        let InKind::Cred {
            provider,
            keys,
            mut values,
            update,
            next,
        } = kind
        else {
            return;
        };
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
                    SelKind::ClaudeToken(InKind::Cred {
                        provider,
                        keys,
                        values,
                        update,
                        next,
                    }),
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
                    SelKind::GhToken(InKind::Cred {
                        provider,
                        keys,
                        values,
                        update,
                        next,
                    }),
                );
                return;
            }
            let mask = key != "FORGEJO_HOST";
            let hint = if cli::is_github_key(&key) {
                ". Use a fine-grained, read-only token: github.com/settings/personal-access-tokens/new"
            } else {
                ""
            };
            let title = format!(
                "{provider}: {key}{}{hint}",
                if mask { " (hidden)" } else { "" }
            );
            self.input(
                &title,
                mask,
                InKind::Cred {
                    provider,
                    keys,
                    values,
                    update,
                    next,
                },
            );
            return;
        }
        let dir = self.dir.clone();
        let attach = if update {
            None
        } else {
            self.sync_secret.take()
        };
        self.status = format!("Working on provider {provider}...");
        self.bg(move || {
            let msg = if update {
                cli::update_credentials(&provider, &values)?
            } else {
                cli::create_provider(&dir, &provider, &values)?
            };
            let sync = attach.map(|sync| SyncStage::Attach {
                sync,
                profile: provider.clone(),
            });
            Ok(Done {
                msg,
                next,
                sync,
                gh: None,
            })
        });
    }

    /// Run `claude setup-token` with the terminal handed over; `finish_job`
    /// then asks for the printed token (the command has no machine-readable output).
    fn generate_token(&mut self, flow: InKind) {
        self.job = Some(vec![Step {
            prog: "claude".into(),
            args: vec!["setup-token".into()],
        }]);
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
        let kind = SelKind::SyncItems {
            sandbox,
            agent,
            manifest,
            labels: labels.clone(),
        };
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
                    items.push(format!(
                        "Add the built-in profile(s): {}",
                        check.fixes.join(", ")
                    ));
                }
                items.push(
                    "Generate a custom profile for exactly this (review it first)".to_string(),
                );
                items.push("Cancel".to_string());
                let title = format!(
                    "{} blocks installs; missing: {}",
                    sync.sandbox,
                    hosts.join(", ")
                );
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
                    kind: ConfKind::SyncSecret {
                        sync,
                        profile: profile.clone(),
                    },
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
        let uploads = steps_list
            .iter()
            .any(|s| s.args.iter().any(|a| a == "upload"));
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
    fn attach_and_check(
        &mut self,
        sync: SyncPlan,
        profiles: Vec<String>,
        create: Vec<String>,
        round: u8,
    ) {
        let dir = self.dir.clone();
        self.status = format!("Adding {} to {}...", profiles.join(", "), sync.sandbox);
        self.bg(move || {
            for p in &profiles {
                if create.contains(p) {
                    cli::create_provider(&dir, p, &[])?;
                }
                if let Err(e) = cli::os(&[
                    "sandbox",
                    "provider",
                    "attach",
                    &sync.sandbox,
                    p,
                    "--wait",
                    "--timeout",
                    "30",
                ]) {
                    if !e.contains(cli::ATTACH_CONFLICT) {
                        return Err(e);
                    }
                    // The gateway refused to merge it with what is attached: offer to detach one.
                    let attached = cli::attached_providers(&sync.sandbox);
                    return Ok(Done {
                        msg: format!("Could not attach {p} to {}", sync.sandbox),
                        next: None,
                        sync: Some(SyncStage::Conflict {
                            sync,
                            profile: p.clone(),
                            attached,
                            round,
                        }),
                        gh: None,
                    });
                }
            }
            let check = cli::sync_check(&dir, &sync.sandbox, &sync.agent, &sync.ids)?;
            Ok(Done {
                msg: format!("Added {}", profiles.join(", ")),
                next: None,
                sync: Some(SyncStage::Check {
                    sync,
                    check,
                    offered: round,
                }),
                gh: None,
            })
        });
    }

    /// Ask whether to remove the generated install access, if any is still attached;
    /// `before` is shown above the question.
    fn offer_drop(&mut self, before: &str) -> bool {
        let Some((sandbox, name)) = self.temp_access.take() else {
            return false;
        };
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
            let output = if f.tail.is_empty() {
                String::new()
            } else {
                format!("\n\nLast output:\n{}", f.tail.join("\n"))
            };
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
            let text = format!(
                "The installs stopped at a failed step ({}):\n  {}{output}",
                f.status, f.label
            );
            if !self.offer_drop(&format!("{text}\n\n")) {
                self.modal = Some(Modal::Info(text));
            }
            return;
        }
        self.running_sync = None;
        self.cleanup_sync_stage();
        self.offer_drop("");
        let Some(flow) = self.token_flow.take() else {
            return;
        };
        let InKind::Cred {
            provider,
            keys,
            values,
            ..
        } = &flow
        else {
            return;
        };
        let title = format!(
            "Paste the token claude printed: {provider}: {} (hidden)",
            keys[values.len()]
        );
        self.input(&title, true, flow);
    }

    fn ask_name(&mut self, plan: Plan) {
        self.input(
            "Sandbox name (empty = generated)",
            false,
            InKind::Name(plan),
        );
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
        if let Some(img) = self
            .snap
            .images
            .iter()
            .find(|i| i.tag == plan.image && !i.built)
        {
            steps.push(Step {
                prog: cli::runtime_bin().into(),
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
        if let Some(json) = self.share_config(plan) {
            args.extend(["--driver-config-json".into(), json]);
            args.extend([
                "--label".into(),
                format!("{}={}", cli::SHARE_LABEL, cli::share_key(&plan.share)),
            ]);
        }
        for p in &plan.providers {
            args.extend(["--provider".into(), p.clone()]);
        }
        if let Some(g) = &plan.gh {
            args.extend([
                "--provider".into(),
                ghrepo::provider_name(&plan.name, &g.repo),
            ]);
        }
        if plan.command.is_empty() {
            // The detached shell stays the main process; sessions come from `exec`.
            args.push("--detach".into());
        } else {
            args.extend([
                "--no-keep".into(),
                "--".into(),
                "sh".into(),
                "-c".into(),
                plan.command.clone(),
            ]);
        }
        steps.push(Step {
            prog: "openshell".into(),
            args: cli::with_gateway(args),
        });
        steps
    }

    fn on_select(&mut self, kind: SelKind, picks: Vec<String>) {
        let first = picks.first().cloned().unwrap_or_default();
        match kind {
            SelKind::Preset => {
                let Some(p) = PRESETS.iter().find(|p| p.0 == first) else {
                    return;
                };
                if p.1.is_empty() {
                    let mut items: Vec<String> =
                        self.snap.images.iter().map(|i| i.tag.clone()).collect();
                    items.push("Other image reference...".into());
                    items.push("Gateway default image".into());
                    self.select("Image", items, false, SelKind::Image);
                } else {
                    let plan = Plan {
                        image: p.1.into(),
                        providers: if p.2.is_empty() {
                            vec![]
                        } else {
                            vec![p.2.into()]
                        },
                        ..Plan::default()
                    };
                    self.proceed(plan);
                }
            }
            SelKind::Image => {
                let plan = Plan::default();
                match first.as_str() {
                    "Other image reference..." => {
                        self.input("Image reference", false, InKind::OtherImage)
                    }
                    "Gateway default image" => self.pick_providers(plan),
                    _ => self.pick_providers(Plan {
                        image: first,
                        ..plan
                    }),
                }
            }
            SelKind::Providers(mut plan) => {
                plan.providers = picks;
                self.proceed(plan);
            }
            SelKind::AttachProvider(sb) => {
                self.bg_msg(move || {
                    cli::os(&[
                        "sandbox",
                        "provider",
                        "attach",
                        &sb,
                        &first,
                        "--wait",
                        "--timeout",
                        "30",
                    ])
                    .map(|_| format!("Attached {first} to {sb}"))
                });
            }
            SelKind::DetachProvider(sb) => {
                self.bg_msg(move || {
                    cli::os(&[
                        "sandbox",
                        "provider",
                        "detach",
                        &sb,
                        &first,
                        "--wait",
                        "--timeout",
                        "30",
                    ])
                    .map(|_| format!("Detached {first} from {sb}"))
                });
            }
            SelKind::OpenShared(hits) => {
                if let Some(hit) = hits.into_iter().find(|h| hit_label(h) == first) {
                    self.go_to(hit);
                }
            }
            SelKind::ShareChoice(mut plan) => {
                if first == "No folder" {
                    self.ask_command(plan);
                } else if let Some(dir) = first
                    .strip_prefix("Current folder: ")
                    .or_else(|| first.strip_prefix("Git repo root: "))
                {
                    plan.share = dir.into();
                    self.ask_access(plan);
                } else {
                    self.input(
                        "Host folder to share, absolute path (empty = none)",
                        false,
                        InKind::Share(plan),
                    );
                }
            }
            SelKind::ShareAccess(mut plan) => {
                plan.share_rw = first == "Read-write";
                self.ask_github(plan);
            }
            SelKind::GhAccess(mut plan, repo) => {
                if first.starts_with("Read-only") {
                    if !plan.providers.iter().any(|p| p == "github") {
                        plan.providers.push("github".into());
                    }
                    if self.missing(&["github".to_string()]).is_empty() {
                        self.ask_command(plan);
                    } else {
                        plan.resume = Resume::Command;
                        self.start_creds("github".into(), false, Some((plan, vec![])));
                    }
                } else if let Some(name) = first.strip_prefix("Use existing provider ") {
                    plan.providers.push(name.into());
                    self.ask_command(plan);
                } else if first.starts_with("Read and write, a token for this sandbox") {
                    if plan.name.is_empty() {
                        plan.name = self.free_name(&ghrepo::slug(&repo.repo));
                        self.status = format!("Sandbox name set to {}", plan.name);
                    }
                    self.repo_steps(RepoFlow {
                        repo,
                        goal: Goal::Sandbox(plan),
                    });
                } else if first.starts_with("Read and write, a token shared") {
                    self.repo_steps(RepoFlow {
                        repo,
                        goal: Goal::SharedFor(plan),
                    });
                } else {
                    self.ask_command(plan);
                }
            }
            SelKind::RenewPick(tracked) => {
                let label = first.split_whitespace().next().unwrap_or_default();
                if let Some(t) = tracked.iter().find(|t| t.provider == label) {
                    self.renew(t);
                }
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
                    let InKind::Cred {
                        provider,
                        keys,
                        values,
                        ..
                    } = &flow
                    else {
                        return;
                    };
                    let key = &keys[values.len()];
                    let title = format!("{provider}: {key} (hidden)");
                    self.input(&title, true, flow);
                } else {
                    self.generate_token(flow);
                }
            }
            SelKind::Conflict {
                sync,
                profile,
                round,
            } => {
                let Some(other) = conflict_pick(&first).map(String::from) else {
                    self.cleanup_sync_stage();
                    self.offer_drop("");
                    return;
                };
                self.status = format!("Detaching {other} from {}...", sync.sandbox);
                let sandbox = sync.sandbox.clone();
                let tx = self.done_tx.clone();
                thread::spawn(move || {
                    let r = cli::os(&[
                        "sandbox",
                        "provider",
                        "detach",
                        &sandbox,
                        &other,
                        "--wait",
                        "--timeout",
                        "30",
                    ]);
                    let d = match r {
                        Ok(_) => Done {
                            msg: format!("Detached {other}"),
                            next: None,
                            sync: Some(SyncStage::Retry {
                                sync,
                                profile,
                                round,
                            }),
                            gh: None,
                        },
                        Err(e) => Done {
                            msg: format!("Error: {e}"),
                            next: None,
                            sync: None,
                            gh: None,
                        },
                    };
                    let _ = tx.send(d);
                });
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
                        &format!(
                            "Name for the generated provider (empty = install-{})",
                            sync.sandbox
                        ),
                        false,
                        InKind::GenName(sync),
                    );
                } else {
                    self.cleanup_sync_stage();
                }
            }
            SelKind::GhToken(flow) => {
                let InKind::Cred {
                    provider,
                    keys,
                    mut values,
                    update,
                    next,
                } = flow
                else {
                    return;
                };
                let key = keys[values.len()].clone();
                match cli::gh_token().filter(|_| first.starts_with("Use")) {
                    Some(token) => {
                        values.push((key, token));
                        self.cred_step(InKind::Cred {
                            provider,
                            keys,
                            values,
                            update,
                            next,
                        });
                    }
                    None => {
                        let title = format!(
                            "{provider}: {key} (hidden). Use a fine-grained, read-only token: github.com/settings/personal-access-tokens/new"
                        );
                        self.input(
                            &title,
                            true,
                            InKind::Cred {
                                provider,
                                keys,
                                values,
                                update,
                                next,
                            },
                        );
                    }
                }
            }
            SelKind::SyncAgents(sandbox) => {
                let agent = if first == "both" {
                    "both".to_string()
                } else {
                    first
                };
                let dir = self.dir.clone();
                self.status = "Reading the host's skills and plugins...".into();
                self.bg(move || {
                    let manifest = cli::sync_manifest(&dir, &sandbox, &agent)?;
                    Ok(Done {
                        msg: format!("Found {} item(s)", manifest.items.len()),
                        next: None,
                        sync: Some(SyncStage::Manifest {
                            sandbox,
                            agent,
                            manifest,
                        }),
                        gh: None,
                    })
                });
            }
            SelKind::SyncItems {
                sandbox,
                agent,
                manifest,
                labels,
            } => {
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
                        sync: Some(SyncStage::Check {
                            sync: SyncPlan {
                                sandbox,
                                agent,
                                manifest,
                                ids,
                                skip: 0,
                            },
                            check,
                            offered: 0,
                        }),
                        gh: None,
                    })
                });
            }
            SelKind::Blocked {
                sandbox,
                gateway,
                b,
            } => {
                if let Some(id) = first
                    .strip_prefix("Attach the ")
                    .and_then(|r| r.split_whitespace().next())
                {
                    let id = id.to_string();
                    let dir = self.dir.clone();
                    self.watch.decide(&sandbox, &b, State::Allowed(false));
                    self.status = format!("Attaching {id} to {sandbox}...");
                    self.bg_msg(move || cli::attach_profile(&dir, &sandbox, &id));
                } else if first.starts_with("Never allow ") {
                    self.watch.decide(&sandbox, &b, State::Never);
                    self.status = format!("Never asking about {} in {sandbox}", b.target());
                } else if let Some(rest) = first.strip_prefix("Allow ") {
                    let all = rest.starts_with("all");
                    self.watch.decide(&sandbox, &b, State::Allowed(all));
                    self.status = format!(
                        "Allowing {} to reach {} in {sandbox}...",
                        b.program(),
                        b.target()
                    );
                    self.bg_msg(move || watch::allow(&gateway, &sandbox, &b, all));
                } else {
                    let until_end = first.contains("session ends");
                    let state = if until_end {
                        State::Ignored
                    } else {
                        State::IgnoredOnce
                    };
                    self.watch.decide(&sandbox, &b, state);
                    let how = if until_end {
                        "until the session ends"
                    } else {
                        "this time"
                    };
                    self.status = format!("Ignored {} -> {} {how}", b.program(), b.target());
                }
            }
            SelKind::Slashes {
                sandbox,
                host,
                port,
                binary,
            } => {
                if first.starts_with("Allow ") {
                    self.status = format!("Allowing encoded slashes for {host} in {sandbox}...");
                    self.bg_msg(move || cli::allow_slashes(&sandbox, &host, port, &binary));
                }
            }
            SelKind::Review { sandbox, denials } => {
                let Some(i) = self
                    .watch
                    .for_sandbox(&sandbox)
                    .iter()
                    .position(|s| review_label(s) == first)
                else {
                    return;
                };
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
                if first.starts_with("Providers") {
                    if let Some(name) = self.session().map(|s| s.sandbox.clone()) {
                        let perms = cli::provider_permissions(&name, &self.snap.providers);
                        self.modal = Some(Modal::Providers {
                            sandbox: name,
                            perms,
                            scroll: 0,
                        });
                    }
                } else if first.starts_with("Dashboard") {
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
                if first == GH_REPO_ITEM {
                    let hint = cli::project_dirs()
                        .iter()
                        .find_map(|d| cli::repo_origin(d))
                        .map(|r| r.full())
                        .unwrap_or_default();
                    let title = if hint.is_empty() {
                        "Repository, OWNER/REPO".to_string()
                    } else {
                        format!("Repository, OWNER/REPO (empty = {hint})")
                    };
                    self.input(&title, false, InKind::RepoName);
                } else if self.snap.providers.iter().any(|p| p.name == first) {
                    self.modal = Some(Modal::Info(format!(
                        "Provider {first} exists. Use u on the Providers tab to update credentials."
                    )));
                } else {
                    self.start_creds(first, false, None);
                }
            }
        }
    }

    /// After the share is set: offer GitHub access when the shared folder is a GitHub repo.
    fn ask_github(&mut self, plan: Plan) {
        let Some(repo) = cli::repo_origin(&plan.share) else {
            return self.ask_command(plan);
        };
        let mut items = vec![
            "No GitHub access".to_string(),
            "Read-only (the github provider)".into(),
        ];
        let id = ghrepo::profile_id(&repo);
        let now = ghrepo::now_secs();
        for p in self.snap.providers.iter().filter(|p| p.ty == id) {
            let note = self
                .tracked_for(&p.name)
                .map(|t| format!(" ({})", ghrepo::expiry_note(now, t.expiry_ms)))
                .unwrap_or_default();
            items.push(format!("Use existing provider {}{note}", p.name));
        }
        items.push("Read and write, a token for this sandbox".into());
        items.push(format!(
            "Read and write, a token shared by sandboxes ({})",
            ghrepo::shared_provider_name(&repo)
        ));
        let title = format!("GitHub access to {}", repo.full());
        self.select(&title, items, false, SelKind::GhAccess(plan, repo));
    }

    /// `base`, or `base-2`, `base-3`... when a sandbox of that name exists.
    fn free_name(&self, base: &str) -> String {
        let taken = |n: &str| self.snap.sandboxes.iter().any(|s| s.name == n);
        let mut name = base.to_string();
        let mut i = 2;
        while taken(&name) {
            name = format!("{base}-{i}");
            i += 1;
        }
        name
    }

    /// Label for the token name GitHub shows: the sandbox, or just the repo.
    fn repo_label(&self, flow: &RepoFlow) -> String {
        match &flow.goal {
            Goal::Sandbox(plan) => format!("{}-{}", plan.name, flow.repo.repo),
            _ => flow.repo.repo.clone(),
        }
    }

    fn repo_steps(&mut self, flow: RepoFlow) {
        let url = ghrepo::token_url(&flow.repo, &self.repo_label(&flow));
        self.modal = Some(Modal::Confirm {
            text: format!(
                "{}\n\nOpen the page now and paste the token next?",
                ghrepo::steps(&flow.repo, &url)
            ),
            kind: ConfKind::RepoSteps(flow),
        });
    }

    /// Banner text when a repo token of this gateway expired or expires within a week.
    pub fn expiry_banner(&self) -> Option<String> {
        let names: Vec<String> = self.snap.providers.iter().map(|p| p.name.clone()).collect();
        let now = ghrepo::now_secs();
        let list = ghrepo::expiring(&self.tracked, &cli::gateway(), &names, now);
        (!list.is_empty()).then(|| ghrepo::banner(&list, now))
    }

    fn tracked_for(&self, provider: &str) -> Option<ghrepo::Tracked> {
        let gw = cli::gateway();
        self.tracked
            .iter()
            .find(|t| t.provider == provider && t.gateway == gw)
            .cloned()
    }

    fn renew(&mut self, t: &ghrepo::Tracked) {
        match ghrepo::parse_remote(&format!("https://github.com/{}", t.repo)) {
            Some(repo) => self.repo_steps(RepoFlow {
                repo,
                goal: Goal::Renew(t.provider.clone()),
            }),
            None => self.status = format!("Error: cannot read the repo of {}", t.provider),
        }
    }

    /// The pasted token was checked: continue the flow, or ask again with the problem.
    fn repo_checked(&mut self, flow: RepoFlow, token: String, problem: Option<String>) {
        if let Some(p) = problem {
            let title = format!("{p}. Paste again (hidden), Esc cancels");
            return self.input(&title, true, InKind::RepoToken(flow));
        }
        let repo = flow.repo.clone();
        match flow.goal {
            Goal::Sandbox(mut plan) => {
                plan.gh = Some(GhGrant { repo, token });
                self.ask_command(plan);
            }
            Goal::SharedFor(mut plan) => {
                let provider = ghrepo::shared_provider_name(&repo);
                plan.providers.push(provider.clone());
                plan.resume = Resume::Command;
                self.status = format!("Creating provider {provider}...");
                self.bg(move || {
                    cli::create_repo_provider(&repo, &provider, &token)?;
                    Ok(Done {
                        msg: format!("Created provider {provider}"),
                        next: Some((plan, vec![])),
                        sync: None,
                        gh: None,
                    })
                });
            }
            Goal::Shared => {
                let provider = ghrepo::shared_provider_name(&repo);
                self.status = format!("Creating provider {provider}...");
                self.bg_msg(move || cli::create_repo_provider(&repo, &provider, &token));
            }
            Goal::Renew(provider) => {
                self.status = format!("Renewing the token of {provider}...");
                let sandboxes = self.snap.sandboxes.clone();
                self.bg(move || {
                    let msg = cli::renew_repo_token(&repo, &provider, &token)?;
                    let running = cli::running_with(&provider, &sandboxes);
                    Ok(Done {
                        msg,
                        next: None,
                        sync: None,
                        gh: Some(GhStage::Renewed {
                            provider,
                            sandboxes: running,
                        }),
                    })
                });
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
        self.select(
            "Providers (space to mark, Enter to confirm)",
            names,
            true,
            SelKind::Providers(plan),
        );
    }

    fn on_input(&mut self, kind: InKind, text: String) {
        match kind {
            InKind::OtherImage => {
                if !text.trim().is_empty() {
                    self.pick_providers(Plan {
                        image: text.trim().into(),
                        ..Plan::default()
                    });
                }
            }
            InKind::GenName(sync) => {
                let raw = if text.trim().is_empty() {
                    format!("install-{}", sync.sandbox)
                } else {
                    text
                };
                let name: String = raw
                    .trim()
                    .to_lowercase()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                if self.snap.providers.iter().any(|p| p.name == name) {
                    self.cleanup_sync_stage();
                    self.modal = Some(Modal::Info(format!(
                        "A provider named {name} already exists. Pick another name."
                    )));
                    return;
                }
                let dir = self.dir.clone();
                self.status = "Generating the profile...".into();
                self.bg(move || {
                    let yaml =
                        cli::sync_gen_profile(&dir, &sync.sandbox, &sync.agent, &sync.ids, &name)?;
                    Ok(Done {
                        msg: "Profile generated".into(),
                        next: None,
                        sync: Some(SyncStage::Generated { sync, name, yaml }),
                        gh: None,
                    })
                });
            }
            InKind::Name(mut plan) => {
                plan.name = text.trim().into();
                if self.limits_supported() {
                    self.input(
                        "CPU limit, e.g. 500m, 2 (empty = unlimited)",
                        false,
                        InKind::Cpu(plan),
                    );
                } else {
                    self.ask_share(plan);
                }
            }
            InKind::Cpu(mut plan) => {
                let Some(v) = limit(&text) else {
                    return self.bad_limit("CPU");
                };
                plan.cpu = v;
                self.input(
                    "Memory limit, e.g. 512Mi, 4Gi (empty = unlimited)",
                    false,
                    InKind::Memory(plan),
                );
            }
            InKind::Memory(mut plan) => {
                let Some(v) = limit(&text) else {
                    return self.bad_limit("Memory");
                };
                plan.memory = v;
                self.ask_share(plan);
            }
            InKind::Share(mut plan) => {
                let text = text.trim();
                if text.is_empty() {
                    return self.ask_command(plan);
                }
                let path = match text.strip_prefix("~/") {
                    Some(rest) => std::env::var("HOME")
                        .map(|h| format!("{h}/{rest}"))
                        .unwrap_or_else(|_| text.into()),
                    None => text.into(),
                };
                let path = path.trim_end_matches('/').to_string();
                if !std::path::Path::new(&path).is_absolute()
                    || !std::path::Path::new(&path).is_dir()
                {
                    self.status = format!("Not an existing absolute folder: {path}");
                    return self.ask_share(plan);
                }
                plan.share = path;
                self.ask_access(plan);
            }
            InKind::Command(mut plan) => {
                plan.command = text.trim().into();
                let steps = self.sandbox_steps(&plan);
                let summary = steps
                    .iter()
                    .map(|s| format!("{} {}", s.prog, s.args.join(" ")))
                    .collect::<Vec<_>>()
                    .join("\n");
                let extra = plan.gh.as_ref().map_or(String::new(), |g| {
                    format!(
                        "First creates provider {} for {} from the pasted token (expires in {} days).\n\n",
                        ghrepo::provider_name(&plan.name, &g.repo),
                        g.repo.full(),
                        ghrepo::TOKEN_DAYS
                    )
                });
                self.modal = Some(Modal::Confirm {
                    text: format!("{extra}Run:\n{summary}\n\nContinue?"),
                    kind: ConfKind::CreateSandbox(plan),
                });
            }
            InKind::RepoName => {
                let text = text.trim();
                let repo = if text.is_empty() {
                    cli::project_dirs().iter().find_map(|d| cli::repo_origin(d))
                } else {
                    ghrepo::parse_remote(&format!("https://github.com/{text}"))
                };
                match repo {
                    Some(repo) => self.repo_steps(RepoFlow {
                        repo,
                        goal: Goal::Shared,
                    }),
                    None => self.status = "Error: expected OWNER/REPO on github.com".into(),
                }
            }
            InKind::RepoToken(flow) => {
                let token = text.trim().to_string();
                if token.is_empty() {
                    self.status = "Cancelled: empty value".into();
                    return;
                }
                self.status = format!("Checking the token against {}...", flow.repo.full());
                self.bg(move || {
                    let problem = match cli::repo_status(&flow.repo, &token) {
                        Ok(s) => cli::repo_status_problem(s, &flow.repo),
                        Err(e) => Some(format!("Could not reach GitHub: {e}")),
                    };
                    Ok(Done {
                        msg: "Checked the token".into(),
                        next: None,
                        sync: None,
                        gh: Some(GhStage::Checked {
                            flow: Box::new(flow),
                            token,
                            problem,
                        }),
                    })
                });
            }
            InKind::Cred {
                provider,
                keys,
                mut values,
                update,
                next,
            } => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    self.status = "Cancelled: empty value".into();
                    return;
                }
                let key = keys[values.len()].clone();
                values.push((key, text));
                self.cred_step(InKind::Cred {
                    provider,
                    keys,
                    values,
                    update,
                    next,
                });
            }
        }
    }

    /// The vm driver ignores per-sandbox limits; its size is fixed gateway-wide.
    fn limits_supported(&self) -> bool {
        !self.snap.driver.is_empty() && self.snap.driver != "vm"
    }

    /// Only the vm, docker and podman gateways in this repo allow host mounts.
    fn shares_supported(&self) -> bool {
        matches!(self.snap.driver.as_str(), "vm" | "docker" | "podman")
    }

    fn ask_share(&mut self, plan: Plan) {
        if self.shares_supported() {
            let mut items = vec!["No folder".to_string()];
            for (i, dir) in cli::project_dirs().iter().enumerate() {
                let what = if i == 0 {
                    "Current folder"
                } else {
                    "Git repo root"
                };
                items.push(format!("{what}: {dir}"));
            }
            items.push("Other folder...".into());
            self.select(
                "Share a host folder?",
                items,
                false,
                SelKind::ShareChoice(plan),
            );
        } else {
            self.ask_command(plan);
        }
    }

    fn ask_access(&mut self, plan: Plan) {
        self.select(
            "Access to the shared folder",
            vec!["Read-only".into(), "Read-write".into()],
            false,
            SelKind::ShareAccess(plan),
        );
    }

    /// `--driver-config-json` value mounting the plan's host folder, keyed by the active driver.
    fn share_config(&self, plan: &Plan) -> Option<String> {
        if plan.share.is_empty() {
            return None;
        }
        let name = std::path::Path::new(&plan.share).file_name()?.to_str()?;
        let mount = serde_json::json!({
            "type": "bind",
            "source": plan.share,
            "target": format!("/sandbox/{name}"),
            "read_only": !plan.share_rw,
        });
        Some(serde_json::json!({ self.snap.driver.as_str(): { "mounts": [mount] } }).to_string())
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
                let cur = if g.name == self.snap.gateway {
                    "  (current)"
                } else {
                    ""
                };
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
            ConfKind::CreateSandbox(mut plan) => match plan.gh.take() {
                None => self.job = Some(self.sandbox_steps(&plan)),
                Some(g) => {
                    let provider = ghrepo::provider_name(&plan.name, &g.repo);
                    plan.gh = Some(GhGrant {
                        repo: g.repo.clone(),
                        token: String::new(),
                    });
                    plan.resume = Resume::Create;
                    self.status = format!("Creating provider {provider}...");
                    self.bg(move || {
                        cli::create_repo_provider(&g.repo, &provider, &g.token)?;
                        Ok(Done {
                            msg: format!("Created provider {provider}"),
                            next: Some((plan, vec![])),
                            sync: None,
                            gh: None,
                        })
                    });
                }
            },
            ConfKind::Restart(names) => {
                self.status = format!("Restarting {}...", names.join(", "));
                self.bg_msg(move || {
                    for n in &names {
                        cli::restart_sandbox(n).map_err(|e| format!("{n}: {e}"))?;
                    }
                    Ok(format!("Restarted {}", names.join(", ")))
                });
            }
            ConfKind::RepoSteps(flow) => {
                let url = ghrepo::token_url(&flow.repo, &self.repo_label(&flow));
                if !cli::open_url(&url) {
                    self.status = "Could not open a browser; use the URL shown".into();
                }
                let title = format!("Token for {} (hidden)", flow.repo.full());
                self.input(&title, true, InKind::RepoToken(flow));
            }
            ConfKind::DeleteSandbox(name) => {
                let providers = self.snap.providers.clone();
                self.bg_msg(move || {
                    cli::os(&["sandbox", "delete", &name])?;
                    let gone = cli::drop_repo_providers(&name, &providers);
                    Ok(if gone.is_empty() {
                        format!("Deleted sandbox {name}")
                    } else {
                        format!("Deleted sandbox {name} and {}", gone.join(", "))
                    })
                })
            }
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
                        sync: Some(SyncStage::Check {
                            sync,
                            check,
                            offered: 1,
                        }),
                        gh: None,
                    })
                });
            }
            ConfKind::DropAccess { sandbox, name } => {
                self.bg_msg(move || cli::drop_generated(&sandbox, &name))
            }
            ConfKind::SyncSecret { sync, profile } => {
                if self.snap.providers.iter().any(|g| g.name == profile) {
                    self.attach_and_check(sync, vec![profile], vec![], 2);
                } else {
                    self.sync_secret = Some(sync);
                    self.start_creds(profile, false, None);
                }
            }
            ConfKind::ResetGateway(name) => {
                let exe = std::env::current_exe().unwrap_or_else(|_| "osh".into());
                self.job = Some(vec![Step {
                    prog: exe.to_string_lossy().into(),
                    args: vec!["gateway-reset".into(), name, "--confirmed".into()],
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
        let menu = matches!(
            self.modal,
            Some(Modal::Select {
                kind: SelKind::SessionMenu,
                ..
            })
        );
        if term::is_chord(&key) {
            if menu {
                self.modal = None;
            } else if self.modal.is_none() {
                let name = self
                    .session()
                    .map(|s| s.sandbox.clone())
                    .unwrap_or_default();
                let items = vec![
                    "Back to the session".into(),
                    "Providers and their permissions".into(),
                    "Dashboard (keep the session running)".into(),
                    "Close the session".into(),
                ];
                self.select(
                    &format!("Shell in {name}"),
                    items,
                    false,
                    SelKind::SessionMenu,
                );
            }
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.modal_key(modal, key);
            return;
        }
        // Ctrl-V with an image on the host clipboard; otherwise the shell gets the key.
        if key.modifiers == KeyModifiers::CONTROL
            && key.code == KeyCode::Char('v')
            && self.paste_image()
        {
            return;
        }
        if let Some(s) = self.session_mut() {
            s.on_key(&key);
        }
    }

    /// Upload the host clipboard's image to the focused shell's sandbox and type its path
    /// there. False when the clipboard holds no image.
    fn paste_image(&mut self) -> bool {
        if self.session().is_none() {
            return false;
        }
        let Some(png) = clip::host_image() else {
            if let Some(hint) = clip::missing_tool_hint() {
                self.status = hint;
            }
            return false;
        };
        let Some(s) = self.session() else {
            return false;
        };
        let (gateway, sandbox) = (s.gateway.clone(), s.sandbox.clone());
        let (input, bracketed, pasted) = (s.input(), s.vt.bracketed_paste(), s.pasted.clone());
        self.status = format!("Uploading image ({} KB) to {sandbox}...", png.len() / 1024);
        self.bg_msg(move || {
            let path = clip::upload(&gateway, &sandbox, &png)?;
            pasted.store(true, Ordering::Relaxed);
            let _ = input.send(term::paste_bytes(path.clone(), bracketed));
            Ok(format!("Pasted image ({} KB): {path}", png.len() / 1024))
        });
        true
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
                    if cli::runtime().is_none() {
                        self.status = "No running container runtime: start Docker or Podman, or run `osh setup --runtime docker|podman`".into();
                        return;
                    }
                    if let Some(img) = self.snap.images.get(self.current()) {
                        self.job = Some(vec![Step {
                            prog: cli::runtime_bin().into(),
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
            if matches!(
                code,
                KeyCode::Char('K' | 'l' | 'd' | 's' | 'S' | 'a' | 'x' | 'p' | 't') | KeyCode::Enter
            ) {
                self.modal = Some(Modal::Info(
                    "No sandbox selected. This gateway has none: press n to create one (g switches gateway).".into(),
                ));
            }
            return;
        };
        match code {
            KeyCode::Char('p') => self.review(name),
            KeyCode::Char('t') => {
                let attached = cli::attached_providers(&name);
                let mine: Vec<ghrepo::Tracked> = self
                    .tracked
                    .iter()
                    .filter(|t| t.gateway == cli::gateway() && attached.contains(&t.provider))
                    .cloned()
                    .collect();
                match mine.len() {
                    0 => {
                        self.modal = Some(Modal::Info(format!(
                            "{name} has no GitHub repo token created by osh."
                        )))
                    }
                    1 => self.renew(&mine[0]),
                    _ => {
                        let now = ghrepo::now_secs();
                        let items = mine
                            .iter()
                            .map(|t| {
                                format!(
                                    "{} ({}, {})",
                                    t.provider,
                                    t.repo,
                                    ghrepo::expiry_note(now, t.expiry_ms)
                                )
                            })
                            .collect();
                        self.select("Renew which token?", items, false, SelKind::RenewPick(mine));
                    }
                }
            }
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
                self.select(
                    "Add skills and plugins for",
                    items,
                    false,
                    SelKind::SyncAgents(name),
                );
            }
            KeyCode::Char('a') => {
                let items = self.snap.providers.iter().map(|p| p.name.clone()).collect();
                self.select(
                    "Attach provider",
                    items,
                    false,
                    SelKind::AttachProvider(name),
                );
            }
            KeyCode::Char('x') => {
                let attached = cli::attached_providers(&name);
                self.select(
                    "Detach provider",
                    attached,
                    false,
                    SelKind::DetachProvider(name),
                );
            }
            _ => {}
        }
    }

    fn gateway_key(&mut self, code: KeyCode) {
        let Some(g) = self.snap.gateways.get(self.current()) else {
            return;
        };
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
                let mut ids = cli::profile_ids(&self.dir);
                ids.push(GH_REPO_ITEM.into());
                self.select(
                    "Create provider from profile",
                    ids,
                    false,
                    SelKind::NewProvider,
                );
            }
            KeyCode::Char('u') => {
                if let Some(p) = self.snap.providers.get(self.current()).cloned() {
                    if let Some(t) = self.tracked_for(&p.name) {
                        return self.renew(&t);
                    }
                    if p.keys.is_empty() {
                        self.modal =
                            Some(Modal::Info(format!("{} stores no credentials.", p.name)));
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
        let Some(row) = rows.get(self.current()) else {
            return;
        };
        if !row.in_repo {
            self.modal = Some(Modal::Info(format!(
                "No {}.yaml in providers/ to sync from.",
                row.id
            )));
            return;
        }
        if cli::profile_env_keys(&self.dir, &row.id)
            .first()
            .map(String::as_str)
            == Some("FORGEJO_HOST")
        {
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
        self.modal = Some(Modal::Viewer {
            title: title.into(),
            text,
            scroll: 0,
            back: Box::new(back),
        });
    }

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) {
        // A prompt can open mid-typing: Enter meant for the shell must not pick an answer.
        let fresh = self.prompt_at.is_some_and(|t| t.elapsed() < PROMPT_GRACE);
        if fresh
            && matches!(
                modal,
                Modal::Info(_)
                    | Modal::Select {
                        kind: SelKind::Blocked { .. },
                        ..
                    }
            )
        {
            self.modal = Some(modal);
            return;
        }
        match modal {
            Modal::Info(text) => {
                if key.code == KeyCode::Char('v') {
                    self.open_viewer("Details", text.clone(), Modal::Info(text));
                }
            }
            Modal::Providers {
                sandbox,
                perms,
                mut scroll,
            } => {
                let rows = ratatui::crossterm::terminal::size()
                    .map(|(_, h)| h)
                    .unwrap_or(24)
                    .saturating_sub(4);
                let last = (crate::ui::perm_lines(&perms).len() as u16).saturating_sub(rows);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return,
                    KeyCode::Char('j') | KeyCode::Down => scroll = (scroll + 1).min(last),
                    KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => scroll = (scroll + 10).min(last),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::Char('g') | KeyCode::Home => scroll = 0,
                    KeyCode::Char('G') | KeyCode::End => scroll = last,
                    _ => {}
                }
                self.modal = Some(Modal::Providers {
                    sandbox,
                    perms,
                    scroll,
                });
            }
            Modal::Viewer {
                title,
                text,
                mut scroll,
                back,
            } => {
                // The viewer fills the screen minus its border and margin; stop when the last line is at the bottom.
                let rows = ratatui::crossterm::terminal::size()
                    .map(|(_, h)| h)
                    .unwrap_or(24)
                    .saturating_sub(4);
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
                self.modal = Some(Modal::Viewer {
                    title,
                    text,
                    scroll,
                    back,
                });
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
            Modal::Input {
                title,
                mut buf,
                mask,
                kind,
            } => match key.code {
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
                    self.modal = Some(Modal::Input {
                        title,
                        buf,
                        mask,
                        kind,
                    });
                }
            },
            Modal::Select {
                title,
                items,
                mut cursor,
                mut marked,
                multi,
                kind,
            } => match key.code {
                KeyCode::Esc if matches!(kind, SelKind::SessionMenu | SelKind::Review { .. }) => {}
                KeyCode::Esc if matches!(kind, SelKind::Blocked { .. }) => {
                    self.on_select(kind, vec!["Ignore this time".into()])
                }
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
                        KeyCode::Char('j') | KeyCode::Down => {
                            cursor = (cursor + 1).min(items.len() - 1)
                        }
                        KeyCode::Char('k') | KeyCode::Up => cursor = cursor.saturating_sub(1),
                        KeyCode::Char(' ') if multi => marked[cursor] = !marked[cursor],
                        _ => {}
                    }
                    self.modal = Some(Modal::Select {
                        title,
                        items,
                        cursor,
                        marked,
                        multi,
                        kind,
                    });
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn app() -> App {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done)
    }

    fn repo() -> GhRepo {
        GhRepo {
            owner: "acme".into(),
            repo: "app".into(),
        }
    }

    /// A temporary git folder whose `origin` is `url`.
    fn git_dir(tag: &str, url: Option<&str>) -> String {
        let dir = std::env::temp_dir().join(format!("osh-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().to_string();
        cli::run(
            "git",
            &["-C".into(), d.clone(), "init".into(), "-q".into()],
            &[],
        )
        .unwrap();
        if let Some(u) = url {
            let args = ["-C", &d, "remote", "add", "origin", u].map(String::from);
            cli::run("git", &args, &[]).unwrap();
        }
        d
    }

    fn select_items(app: &App) -> (String, Vec<String>) {
        match &app.modal {
            Some(Modal::Select { title, items, .. }) => (title.clone(), items.clone()),
            _ => panic!("no select"),
        }
    }

    #[test]
    fn github_access_is_offered_for_a_shared_github_repo_only() {
        let mut a = app();
        let d = git_dir("gh", Some("git@github.com:acme/app.git"));
        a.ask_github(Plan {
            share: d.clone(),
            ..Plan::default()
        });
        let (title, items) = select_items(&a);
        assert_eq!(title, "GitHub access to acme/app");
        assert_eq!(items[0], "No GitHub access");
        assert!(items[1].starts_with("Read-only"));
        assert!(items
            .iter()
            .any(|i| i.starts_with("Read and write, a token for this sandbox")));
        assert!(items
            .iter()
            .any(|i| i.contains("shared by sandboxes (github-app)")));
        let _ = std::fs::remove_dir_all(d);

        let mut a = app();
        let d = git_dir("other", Some("git@example.com:acme/app.git"));
        a.ask_github(Plan {
            share: d.clone(),
            ..Plan::default()
        });
        assert!(matches!(
            &a.modal,
            Some(Modal::Input {
                kind: InKind::Command(_),
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn existing_repo_providers_are_listed_with_their_expiry() {
        let mut a = app();
        let d = git_dir("exist", Some("https://github.com/acme/app"));
        a.snap.providers.push(cli::Provider {
            name: "github-app".into(),
            ty: "gh-acme-app".into(),
            keys: vec!["GITHUB_TOKEN".into()],
        });
        a.snap.providers.push(cli::Provider {
            name: "github".into(),
            ty: "github".into(),
            keys: vec![],
        });
        a.ask_github(Plan {
            share: d.clone(),
            ..Plan::default()
        });
        let (_, items) = select_items(&a);
        assert!(items.contains(&"Use existing provider github-app".to_string()));
        assert!(!items.iter().any(|i| i.contains("provider github ")));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn read_write_names_the_sandbox_after_the_repo_and_shows_the_token_steps() {
        let mut a = app();
        a.snap.sandboxes.push(cli::Sandbox {
            name: "app".into(),
            ..cli::Sandbox::default()
        });
        a.on_select(
            SelKind::GhAccess(Plan::default(), repo()),
            vec!["Read and write, a token for this sandbox".into()],
        );
        match &a.modal {
            Some(Modal::Confirm { text, kind }) => {
                assert!(text.contains("Only select repositories, then acme/app"));
                assert!(text.contains("target_name=acme"));
                assert!(text.contains("name=osh-app-2-app"));
                match kind {
                    ConfKind::RepoSteps(RepoFlow {
                        goal: Goal::Sandbox(p),
                        ..
                    }) => assert_eq!(p.name, "app-2"),
                    _ => panic!("wrong confirm"),
                }
            }
            _ => panic!("no steps"),
        }
    }

    #[test]
    fn a_bad_token_is_asked_for_again_a_good_one_continues_the_wizard() {
        let mut a = app();
        let flow = RepoFlow {
            repo: repo(),
            goal: Goal::Sandbox(Plan {
                name: "dev".into(),
                ..Plan::default()
            }),
        };
        a.repo_checked(
            flow.clone(),
            "t".into(),
            Some("The token cannot see acme/app (404)".into()),
        );
        match &a.modal {
            Some(Modal::Input { title, mask, .. }) => {
                assert!(*mask);
                assert!(title.contains("404") && title.contains("Paste again"));
            }
            _ => panic!("no re-ask"),
        }
        a.repo_checked(flow, "secret".into(), None);
        match &a.modal {
            Some(Modal::Input {
                kind: InKind::Command(p),
                ..
            }) => {
                let g = p.gh.as_ref().unwrap();
                assert_eq!(
                    (g.repo.full(), g.token.as_str()),
                    ("acme/app".into(), "secret")
                );
            }
            _ => panic!("wizard did not continue"),
        }
    }

    #[test]
    fn the_sandbox_create_command_names_the_repo_provider_and_never_holds_the_token() {
        let a = app();
        let steps = a.sandbox_steps(&Plan {
            name: "dev".into(),
            gh: Some(GhGrant {
                repo: repo(),
                token: "ghp_secret".into(),
            }),
            ..Plan::default()
        });
        let args = steps.last().unwrap().args.join(" ");
        assert!(args.contains("--provider github-dev-app"), "{args}");
        assert!(!args.contains("ghp_secret"));
    }

    #[test]
    fn a_renewed_token_offers_to_restart_the_sandboxes_still_using_the_old_one() {
        let mut a = app();
        a.on_done(Done {
            msg: "Renewed the token of github-dev-app".into(),
            next: None,
            sync: None,
            gh: Some(GhStage::Renewed {
                provider: "github-dev-app".into(),
                sandboxes: vec!["dev".into()],
            }),
        });
        match &a.modal {
            Some(Modal::Confirm {
                text,
                kind: ConfKind::Restart(names),
            }) => {
                assert_eq!(names, &["dev".to_string()]);
                assert!(text.contains("Restart it?"));
                assert!(text.contains("/tmp is cleared") && text.contains("/sandbox are kept"));
            }
            _ => panic!("no restart offer"),
        }
        let mut a = app();
        a.on_done(Done {
            msg: "Renewed".into(),
            next: None,
            sync: None,
            gh: Some(GhStage::Renewed {
                provider: "p".into(),
                sandboxes: vec![],
            }),
        });
        assert!(a.modal.is_none());
    }

    #[test]
    fn the_banner_names_an_expiring_token_of_a_provider_that_exists() {
        let mut a = app();
        assert_eq!(a.expiry_banner(), None);
        let soon = ghrepo::expiry_ms(ghrepo::now_secs(), 3);
        a.tracked.push(ghrepo::Tracked {
            gateway: cli::gateway(),
            provider: "github-dev-app".into(),
            expiry_ms: soon,
            repo: "acme/app".into(),
        });
        assert_eq!(a.expiry_banner(), None, "provider is not on the gateway");
        a.snap.providers.push(cli::Provider {
            name: "github-dev-app".into(),
            ty: "gh-acme-app".into(),
            keys: vec![],
        });
        let b = a.expiry_banner().unwrap();
        assert!(
            b.contains("github-dev-app") && b.contains("expires in 3d"),
            "{b}"
        );
    }

    #[test]
    fn free_names_skip_existing_sandboxes() {
        let mut a = app();
        assert_eq!(a.free_name("app"), "app");
        for n in ["app", "app-2"] {
            a.snap.sandboxes.push(cli::Sandbox {
                name: n.into(),
                ..cli::Sandbox::default()
            });
        }
        assert_eq!(a.free_name("app"), "app-3");
    }
}
