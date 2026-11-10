//! Application state, modal flows and key handling.
use crate::cli::{self, Snapshot};
use crate::clip;
use crate::ghrepo::{self, GhRepo};
use crate::logview::LogView;
use crate::ports::{self, Ports, Scan};
use crate::preload;
use crate::term::{self, Session};
use crate::watch::{self, Blocked, Denial, Follow, Prompt, State, Watch};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc, Mutex,
    },
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
/// How long an alert stays top-right.
pub const TOAST_FOR: Duration = Duration::from_secs(5);

/// Why a remote gets no repo access although its host has a provider: osh supports GitHub on
/// github.com only, Bitbucket Data Center https remotes need `/scm/` without a context path.
fn unfit(remote: &ghrepo::Remote, forge: Option<ghrepo::Forge>) -> String {
    match forge {
        None => format!("No repo access for {}", remote.host),
        Some(ghrepo::Forge::GitHub) => format!(
            "No repo access: osh supports GitHub on github.com only, not {}",
            remote.host
        ),
        Some(f) => format!(
            "No repo access: {}/{} is not a {} repository path osh understands",
            remote.host,
            remote.path.join("/"),
            f.label()
        ),
    }
}

/// Last line of the new-sandbox toolchain picker: writes the marks back to `toolchains.toml`.
const SAVE_TOOLCHAINS_ITEM: &str = "Save this choice as the default for new sandboxes";
/// Toolchain picker line that opens `toolchains.toml` in the editor (Enter on it).
pub const EDIT_TOOLCHAINS_ITEM: &str = "Edit toolchains.toml (add hosts or groups)...";
const TOOLCHAINS_FILE: &str = "toolchains.toml";
const GH_REPO_ITEM: &str = "Git repo, read/write (GitHub, GitLab, Bitbucket or Forgejo token)...";

/// Data files list entries that start a new file.
const NEW_PROFILE_ITEM: &str = "New provider profile...";
const NEW_DOCKERFILE_ITEM: &str = "New Dockerfile...";
/// Actions of one data file.
const FA_EDIT: &str = "Edit (your editor, else osh's)";
const FA_CREATE: &str = "Create it (your editor, else osh's)";
const FA_VIEW: &str = "View";
const FA_DIFF: &str = "Compare with the shipped default";
const FA_RESTORE: &str = "Restore the shipped default";
const FA_DELETE: &str = "Delete this file";
const FA_GW_DIFF: &str = "Compare with the gateway's copy";
const FA_GW_SAVE: &str = "Save the gateway's copy as this file";
const FA_GW_DELETE: &str = "Delete the profile from this gateway";
const FA_BACK: &str = "Back to the data files";

/// The question after a profile update reached running sandboxes.
fn profile_restart_text(profile: &str, sandboxes: &[String]) -> String {
    let it = if sandboxes.len() == 1 { "it" } else { "them" };
    format!(
        "Profile {profile} is updated. {} {} a provider made from it and may keep the old rules until restarted.\n\nRestart {it} now? Running programs stop and /tmp is cleared; files in /sandbox are kept.",
        sandboxes.join(", "),
        if sandboxes.len() == 1 { "uses" } else { "use" }
    )
}

/// What a restored file takes effect with.
fn restore_hint(rel: &str) -> &'static str {
    match rel {
        "sandbox-policy.yaml" | "toolchains.toml" => "; new sandboxes use it",
        r if r.starts_with("gateway") => {
            "; it applies the next time osh installs the gateway config (Upgrade OpenShell in the command palette)"
        }
        r if r.starts_with("images/") => "; build the image again to use it (b on the Images tab)",
        r if r.starts_with("providers/") => "; import it again to use it (u on the Profiles tab)",
        _ => "",
    }
}

/// A new provider profile: the narrowest grant, to widen on purpose.
fn profile_template(id: &str) -> String {
    format!(
        "# {id}: what this profile allows, and what it does not. Grant the narrowest host, port,\n\
         # method and binary needed. Save, then import it on the Profiles tab (u).\n\
         id: {id}\n\
         display_name: {id}\n\
         description: Read-only access to example.com\n\
         category: data\n\
         endpoints:\n  - host: example.com\n    port: 443\n    protocol: rest\n    access: read-only\n    enforcement: enforce\n\
         binaries: [/usr/bin/curl]\n"
    )
}

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
    /// Read/write access to the shared folder's repo (GitHub, GitLab, Bitbucket or Forgejo);
    /// the provider is created on confirm.
    pub gh: Option<GhGrant>,
    /// Where the wizard continues once a provider the plan needs exists.
    pub resume: Resume,
    /// Build the image again before creating, because its Dockerfile changed since the last build.
    pub rebuild: bool,
    /// Toolchain groups (`toolchains.toml`) switched on for this sandbox; `None` = the file's
    /// own `enabled` flags (not asked yet, or the list is unusable).
    pub toolchains: Option<Vec<String>>,
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
    /// Which toolchain host groups this new sandbox may read from.
    Toolchains(Plan),
    /// Which toolchain host groups to open in a running sandbox (sandbox, gateway).
    ToolchainsRunning(String, String),
    /// `toolchains.toml` cannot be used (broken TOML, no groups): edit it, or go on without it.
    ToolchainsFix(EditThen),
    /// Whether to rebuild an outdated image before the sandbox is created from it.
    Rebuild(Plan),
    /// Which of several sandboxes (gateway, name) sharing the project folder to open.
    OpenShared(Vec<(String, String)>),
    /// No folder, the folder `osh` was started in, or one typed in.
    ShareChoice(Plan),
    /// Read-only or read-write access to the shared host folder.
    ShareAccess(Plan),
    AttachProvider(String),
    DetachProvider(String),
    /// Which sandbox receives the state of this one.
    TransferTo(String),
    NewProvider,
    /// Repo access for the repo of the shared folder, once its provider is known.
    GhAccess(Plan, GhRepo),
    /// Which git provider hosts the remote; the answer is remembered per host. The plan is the
    /// wizard's; `None` is the Providers tab's shared token.
    GitProvider(Option<Plan>, ghrepo::Remote),
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
    /// The shell menu's port list: stop a forward, forward a listener, or type a port.
    Forwards {
        sandbox: String,
        gateway: String,
    },
    /// A port started listening in a sandbox: forward it to the host?
    Forward {
        sandbox: String,
        gateway: String,
        port: u16,
    },
    /// A rejected request-target holds an encoded `/`: allow it through a generated provider.
    Slashes {
        sandbox: String,
        host: String,
        port: u16,
        binary: String,
    },
    /// Denials seen in a sandbox (`p`, or the shell menu); refreshed as new ones arrive.
    /// Choosing a blocked one asks again.
    Review {
        sandbox: String,
    },
    /// Every data file (paths relative to the data dir, in list order), plus "new" entries.
    DataFiles(Vec<String>),
    /// What to do with one data file (relative path).
    DataFile(String),
}

/// Where to go once the editor closes: back to the picker that opened it, with the fresh file.
#[derive(Clone)]
pub enum EditThen {
    Nothing,
    /// Back to the actions of this data file.
    DataFile(String),
    /// A gateway's config was edited: offer the restart that applies it.
    Gateway(String),
    NewSandbox(Box<Plan>),
    Running(String, String),
}

#[derive(Clone)]
pub enum InKind {
    OtherImage,
    /// A port to forward from this sandbox.
    ForwardPort {
        sandbox: String,
        gateway: String,
    },
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
    /// Name of a new data file: a provider profile (true) or a Dockerfile (false).
    NewFile(bool),
    /// vCPUs, then memory, for every sandbox of a VM gateway.
    VmCpus(String),
    VmMemory(String, u32),
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
    /// Copy the home directory of `from` into `to`.
    Transfer {
        from: String,
        to: String,
    },
    /// Open GitHub's token list; the carried names are the tokens to delete there.
    OpenTokenPage(Vec<String>),
    DeleteProvider(String),
    ResetGateway(String),
    /// Remove a gateway osh created (typed confirmation was the modal).
    RemoveGateway(String),
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
    /// Put the shipped default of a data file back (the user's copy is kept as `.osh-bak`).
    RestoreFile(String),
    /// Delete a data file.
    DeleteFile(String),
    /// Delete a profile from the current gateway.
    DeleteProfile(String),
    /// Run `osh ARGS` with the terminal handed over: the subcommand's own code and step list,
    /// after this confirmation (`--confirmed` in ARGS skips its per-step questions).
    OshJob(Vec<String>),
    /// The VM size of every sandbox on a VM gateway: (gateway, vcpus, mem_mib).
    VmSize(String, u32, u32),
}

/// Live state of a background job, shown in a progress popup while it runs. `total` 0 means the
/// job cannot say how far it is: the popup then shows a moving bar and the elapsed time.
pub struct Task {
    pub title: String,
    pub total: usize,
    pub done: usize,
    pub current: String,
    pub started: Instant,
}

impl Task {
    pub fn new(title: impl Into<String>, total: usize) -> Self {
        Task {
            title: title.into(),
            total,
            done: 0,
            current: String::new(),
            started: Instant::now(),
        }
    }
}

pub enum Modal {
    /// The key reference for the current tab; scrollable, read-only.
    Help {
        scroll: u16,
    },
    /// Type to filter every action, Enter runs the chosen one.
    Palette {
        query: String,
        cursor: usize,
    },
    /// A running job; Esc hides the popup, the job goes on.
    Task(std::sync::Arc<std::sync::Mutex<Task>>),
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
    /// The built-in editor over the whole screen; `then` runs once it closes.
    Editor {
        ed: Box<crate::editor::Editor>,
        then: EditThen,
    },
    /// The user's own editor in osh's terminal over the whole screen; `then` runs once it is done.
    ExtEdit {
        run: Box<crate::exted::Run>,
        then: EditThen,
    },
    /// Read-only scrollable text; `back` is shown again when it closes.
    Viewer {
        title: String,
        text: String,
        scroll: u16,
        back: Option<Box<Modal>>,
    },
}

/// Rows a scrolling modal can show: the terminal minus border and margin. Fixed under test so a unit test
/// does not depend on the height of the terminal that runs `cargo test`.
fn modal_rows() -> u16 {
    modal_screen().1.saturating_sub(4)
}

/// Terminal size, fixed under test.
fn modal_screen() -> (u16, u16) {
    if cfg!(test) {
        return (100, 24);
    }
    ratatui::crossterm::terminal::size().unwrap_or((80, 24))
}

/// Rows the scrolling popups produce for `rows` wrapped at the current width, minus the rows
/// that fit on screen: the furthest the popup can scroll.
fn modal_last(rows: usize) -> u16 {
    (rows as u16).saturating_sub(modal_rows())
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
    /// A profile was updated; these running sandboxes use a provider made from it.
    ProfileUpdated {
        profile: String,
        sandboxes: Vec<String>,
    },
    /// A sandbox was deleted with its token providers; the tokens still live where they were
    /// made: GitHub token names, and (provider, token name, page) for the other providers.
    Leftover {
        github: Vec<String>,
        other: Vec<(String, String, String)>,
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
    /// The status the latest snapshot produced. While `status` still equals it, the poller owns
    /// the line and keeps it true; any other message is an action's result and stays.
    snap_status: String,
    /// No snapshot of the current gateway has arrived yet (first start, or just switched), so
    /// whether it is reachable is not known.
    pub awaiting_snapshot: bool,
    /// When `status` last changed; a shell's status bar shows it for a few seconds.
    status_at: Option<(String, Instant)>,
    /// Everything raised since osh started, newest last, and the one shown top-right for 5 s.
    pub alerts: Vec<crate::alerts::Alert>,
    toast: Option<(crate::alerts::Alert, Instant)>,
    /// Start of the run, the clock the spinner frames are read from.
    born: Instant,
    pub quit: bool,
    pub job: Option<Vec<Step>>,
    /// Set by `a` on the Gateways tab; the main loop opens the wizard and clears it.
    pub wizard_request: Option<crate::wizard::Mode>,
    /// The gateways osh created (`gateways.json`); reloaded on refresh.
    pub registry: crate::gwmodel::Registry,
    /// What the legacy `openshell` gateway's installed `gateway.toml` drives.
    pub legacy_kind: Option<crate::gwmodel::GwKind>,
    /// Record the job's output, so a failed step can be explained (only sync jobs:
    /// `claude setup-token` prints a secret).
    pub capture: bool,
    /// The sync plan the running job installs.
    running_sync: Option<SyncPlan>,
    /// A detached sandbox being created: its name (empty = generated) and the names that existed
    /// before, so the host tmux config can be copied in once the job ends.
    tmux_after_create: Option<(String, Vec<String>)>,
    /// Embedded sandbox shells; they keep running while the dashboard is shown.
    pub sessions: Vec<Session>,
    /// Last status sent to the host tmux window (`workmux set-window-status`).
    host_status: Option<crate::term::AgentStatus>,
    /// Session that has the screen and the keyboard.
    pub active: Option<usize>,
    /// Policy denials seen in sandboxes with a shell open, and the prompts still to show.
    pub watch: Watch,
    /// One log follower per open shell, feeding `watch`.
    follows: Vec<Follow>,
    /// Listening ports of the sandboxes with a shell open, and the forward offers still to show.
    pub ports: Ports,
    /// One listener poller per open shell, feeding `ports`.
    scans: Vec<Scan>,
    /// When the last watcher prompt opened; keys typed just before it are not answers.
    pub prompt_at: Option<Instant>,
    /// An automatic-allow batch is running: `policy update` calls must not overlap.
    auto_running: Arc<AtomicBool>,
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
    /// Where "Which git provider hosts HOST?" answers are kept; `None` under test.
    pub hosts_file: Option<PathBuf>,
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

/// Stop the forward osh started for `port`, off the UI thread; a forward already gone is fine.
fn stop_forward(sandbox: &str, gateway: &str, port: u16) {
    let args = ports::forward_args("stop", sandbox, gateway, port);
    thread::spawn(move || {
        let _ = cli::run("openshell", &args, &[]);
    });
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

/// The banner rule: the built-in gateway config only counts as "out of date" when osh did not
/// make the main gateway (a wizard-made main has its own generated config).
fn config_stale_with(reg: &crate::gwmodel::Registry, differs: bool) -> bool {
    differs && reg.main().is_none()
}

/// True when the installed `gateway.toml` is missing or differs from the one this host should run.
/// Unit tests never look: the answer would come from the real HOME and a real `docker info`, and
/// `Paths::detect` exits the test binary when HOME is unset.
fn config_stale(reg: &crate::gwmodel::Registry) -> bool {
    if cfg!(test) {
        return false;
    }
    let os = if cfg!(target_os = "macos") {
        crate::platform::Os::MacOs
    } else {
        crate::platform::Os::Linux
    };
    let name = crate::gateway::config_name_for(os, cli::runtime());
    let paths = crate::gateway::Paths::detect();
    // The stub osh writes for the Homebrew service is deliberately not the built-in config: the
    // main gateway is one osh runs itself. Without this, a run whose data dir has no registry
    // (the dev launcher points it at the checkout) would call the stub out of date.
    let is_stub = std::fs::read_to_string(paths.config_home.join("gateway.toml"))
        .is_ok_and(|t| t == crate::gwops::BREW_SERVICE_TOML);
    let differs = !is_stub && crate::gateway::stale(name, &crate::platform::RealHost, &paths);
    config_stale_with(reg, differs)
}

/// The kind of gateway the installed `gateway.toml` runs, from its `compute_driver`. `None` in
/// unit tests, for the reason `config_stale` gives.
fn installed_driver() -> Option<crate::gwmodel::GwKind> {
    use crate::platform::Host as _;
    if cfg!(test) {
        return None;
    }
    let p = crate::gateway::Paths::detect();
    let text = crate::platform::RealHost.read(&p.config_home.join("gateway.toml"))?;
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("compute_driver"))?;
    crate::gwmodel::GwKind::from_driver(line.split('"').nth(1)?)
}

/// What the Gateways table shows about one gateway.
pub struct GwInfo {
    pub kind: String,
    pub port: String,
    pub main: bool,
    /// osh created it, so osh may remove it.
    pub managed: bool,
}

impl App {
    /// Kind, port and whether it is the main gateway, from the registry, the legacy names, or `?`.
    pub fn gateway_info(&self, name: &str, endpoint: &str) -> GwInfo {
        use crate::gwmodel::GwKind;
        let port = endpoint
            .rsplit(':')
            .next()
            .map(|p| p.trim_end_matches('/'))
            .filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
            .unwrap_or("")
            .to_string();
        if let Some(g) = self.registry.get(name) {
            return GwInfo {
                kind: g.kind.label().into(),
                port,
                main: g.main,
                managed: true,
            };
        }
        let legacy_main = name == "openshell" && self.registry.main().is_none();
        let kind = if name == "openshell" {
            self.legacy_kind
        } else {
            GwKind::parse(name)
        };
        GwInfo {
            kind: kind.map_or("?", GwKind::label).into(),
            port,
            main: legacy_main,
            managed: false,
        }
    }

    /// Reload what the tab shows after the wizard or a removal changed things.
    pub fn reload_gateways(&mut self) {
        self.registry = crate::gwmodel::Registry::load(&self.dir);
        self.legacy_kind = installed_driver();
    }

    pub fn new(
        dir: PathBuf,
        selected: Arc<Mutex<Option<String>>>,
        wake: Sender<()>,
        done_tx: Sender<Done>,
    ) -> Self {
        let registry = crate::gwmodel::Registry::load(&dir);
        App {
            config_stale: config_stale(&registry),
            dir,
            tab: Tab::Sandboxes,
            snap: Snapshot::default(),
            cursor: [0; 5],
            modal: None,
            logview: None,
            status: "Loading...".into(),
            snap_status: String::new(),
            awaiting_snapshot: true,
            status_at: None,
            alerts: Vec::new(),
            toast: None,
            born: Instant::now(),
            quit: false,
            job: None,
            capture: false,
            running_sync: None,
            tmux_after_create: None,
            sessions: Vec::new(),
            host_status: None,
            active: None,
            watch: Watch::with_never_file(watch::never_path())
                .with_auto_any_file(watch::auto_any_path()),
            follows: Vec::new(),
            ports: Ports::default(),
            scans: Vec::new(),
            prompt_at: None,
            auto_running: Arc::new(AtomicBool::new(false)),
            token_flow: None,
            sync_stage: None,
            sync_secret: None,
            wizard_request: None,
            registry,
            legacy_kind: installed_driver(),
            tracked: ghrepo::load_tracked(ghrepo::tracked_path().as_deref()),
            hosts_file: ghrepo::hosts_path(),
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

    /// Aims every `openshell` call at `name` and shows it as loading until its first snapshot.
    /// The gateway list and its cursor stay, so the Gateways tab does not empty or jump.
    fn change_gateway(&mut self, name: &str) {
        cli::set_gateway(name);
        let gateways = std::mem::take(&mut self.snap.gateways);
        self.snap = Snapshot {
            gateway: name.to_string(),
            gateways,
            ..Default::default()
        };
        let on_gateways = self.cursor[4];
        self.cursor = [0; 5];
        self.cursor[4] = on_gateways;
        self.awaiting_snapshot = true;
        self.status = format!("Switching to {name}...");
        self.sync_selection();
        self.refresh();
    }

    /// Switch to the sandbox's gateway if needed; the session opens once that gateway has loaded.
    fn go_to(&mut self, (gw, name): (String, String)) {
        if gw != cli::gateway() {
            self.change_gateway(&gw);
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

    /// What a snapshot says about its gateway, as a status line.
    fn snapshot_status(s: &Snapshot) -> String {
        if let Some(e) = &s.error {
            e.clone()
        } else if s.gateway_ok {
            "Ready".into()
        } else {
            "Gateway unreachable: on the Gateways tab (5), R starts it and l shows its log".into()
        }
    }

    /// Whether the status is still the poller's to keep current: a load in progress (first
    /// start, `r`, a gateway switch) or what an earlier snapshot said.
    fn status_is_snapshot_owned(&self) -> bool {
        matches!(self.status.as_str(), "Loading..." | "Refreshing...")
            || (self.status.starts_with("Switching to ") && self.status.ends_with("..."))
            || (!self.snap_status.is_empty() && self.status == self.snap_status)
    }

    pub fn on_snapshot(&mut self, snap: Snapshot) {
        if self.status_is_snapshot_owned() {
            self.snap_status = Self::snapshot_status(&snap);
            self.status = self.snap_status.clone();
        }
        self.snap = snap;
        self.awaiting_snapshot = false;
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
        self.config_stale = config_stale(&self.registry);
        for i in 0..5 {
            let len = [
                self.snap.sandboxes.len(),
                self.snap.providers.len(),
                self.profile_rows().len(),
                self.snap.images.len(),
                self.snap.gateways.len(),
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

    /// Keep `a` in the log and show it top-right until it times out or is dismissed.
    pub fn push_alert(&mut self, a: crate::alerts::Alert) {
        self.toast = Some((a.clone(), Instant::now()));
        self.alerts.push(a);
    }

    /// The alert still on screen: shown for `TOAST_FOR`, or until dismissed.
    pub fn toast(&self) -> Option<&crate::alerts::Alert> {
        self.toast
            .as_ref()
            .filter(|(a, t)| t.elapsed() < TOAST_FOR && self.alert_visible(a))
            .map(|(a, _)| a)
    }

    /// Inside a shell only that sandbox's alerts show; the dashboard shows all of them.
    fn alert_visible(&self, a: &crate::alerts::Alert) -> bool {
        match self.session() {
            Some(s) => a.sandbox.as_deref() == Some(s.sandbox.as_str()),
            None => true,
        }
    }

    fn visible_alerts(&self) -> Vec<&crate::alerts::Alert> {
        self.alerts
            .iter()
            .filter(|a| self.alert_visible(a))
            .collect()
    }

    pub fn dismiss_toast(&mut self) -> bool {
        self.toast
            .take()
            .is_some_and(|(_, t)| t.elapsed() < TOAST_FOR)
    }

    /// Every alert as text, newest first, for the viewer behind `!`.
    fn alerts_text(&self) -> String {
        let shown = self.visible_alerts();
        if shown.is_empty() {
            return "Nothing suspicious seen since osh started.".into();
        }
        let on_dashboard = self.session().is_none();
        shown
            .into_iter()
            .rev()
            .map(|a| {
                let tag = if a.sev == crate::alerts::Severity::High {
                    "HIGH"
                } else {
                    "warn"
                };
                let who = match (&a.sandbox, on_dashboard) {
                    (Some(s), true) => format!(" ({s})"),
                    _ => String::new(),
                };
                format!("[{tag}]{who} {}\n      {}", a.title, a.detail)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn show_alerts(&mut self, back: Option<Modal>) {
        let n = self.visible_alerts().len();
        let text = self.alerts_text();
        self.toast = None;
        self.open_viewer(&format!("Alerts ({n})"), text, back);
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
    /// "Allowed ..." stays longer: it is the cue to retry the request that was blocked.
    pub fn flash(&self) -> Option<&str> {
        let keep = if self.status.starts_with("Allowed ") {
            Duration::from_secs(20)
        } else {
            Duration::from_secs(6)
        };
        self.status_at
            .as_ref()
            .filter(|(s, t)| *s == self.status && (self.busy() || t.elapsed() < keep))
            .map(|(s, _)| s.as_str())
    }

    /// How long the current status has been shown, once it is long enough to be worth a count.
    pub fn busy_for(&self) -> Option<Duration> {
        let (s, t) = self.status_at.as_ref()?;
        (*s == self.status && self.busy())
            .then(|| t.elapsed())
            .filter(|d| *d >= Duration::from_secs(2))
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
                self.scans.push(Scan::spawn(sandbox.clone(), gw.clone()));
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
        self.scans
            .retain(|f| !(f.sandbox == s.sandbox && f.gateway == s.gateway));
        if !self.sessions.iter().any(|o| o.sandbox == s.sandbox) {
            if s.pasted.load(Ordering::Relaxed) {
                let (gateway, sandbox) = (s.gateway.clone(), s.sandbox.clone());
                thread::spawn(move || clip::clean(&gateway, &sandbox));
            }
            self.watch.end_session(&s.sandbox);
            for port in self.ports.end_session(&s.sandbox) {
                stop_forward(&s.sandbox, &s.gateway, port);
            }
            if matches!(&self.modal, Some(Modal::Select { kind: SelKind::Forward { sandbox, .. }, .. }) if *sandbox == s.sandbox)
            {
                self.modal = None;
            }
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

    /// Argument for `workmux set-window-status` when the most urgent agent status over all
    /// shells differs from the last one sent; the host window is one, so the shells share it.
    fn host_status_change(&mut self) -> Option<&'static str> {
        let now = self
            .sessions
            .iter()
            .filter_map(|s| crate::term::agent_status(&s.vt.title()))
            .max();
        if now == self.host_status {
            return None;
        }
        self.host_status = now;
        Some(crate::term::AgentStatus::word(now))
    }

    /// Show what the agents in the shells are doing on osh's own tmux window, the way workmux
    /// does for agents running on the host. Does nothing outside tmux or without workmux.
    fn mirror_host_status(&mut self) {
        let Some(word) = self.host_status_change() else {
            return;
        };
        if std::env::var_os("TMUX_PANE").is_none() {
            return;
        }
        std::thread::spawn(move || {
            let _ = std::process::Command::new("workmux")
                .args(["set-window-status", word])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        });
    }

    /// Feed every shell's output to its terminal, focused or not, and drop ended ones.
    pub fn pump_sessions(&mut self) {
        self.pump_ext_editor();
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
        self.mirror_host_status();
    }

    /// Size every shell, focused or not, to the pane it gets on a `w`x`h` screen.
    pub fn fit_sessions(&mut self, w: u16, h: u16) {
        let (cols, rows) = pane(w, h);
        for s in &mut self.sessions {
            s.resize(cols, rows);
        }
        if let Some(s) = self.ext_session() {
            s.resize(cols, rows);
        }
    }

    pub fn on_mouse(&mut self, me: MouseEvent) {
        if let Some(text) = self.ext_session().and_then(|s| s.on_mouse(&me)) {
            self.copy(&text, "copied");
        }
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
        for sc in &mut self.scans {
            if let Some(now) = sc.latest() {
                self.ports.observe(&sc.sandbox, &sc.gateway, &now);
            }
        }
        for (sandbox, gateway, port) in self.ports.take_gone() {
            stop_forward(&sandbox, &gateway, port);
        }
        self.refresh_review();
        self.apply_auto_allows();
        let here = self.session().map(|s| s.sandbox.clone());
        let idle = self.modal.is_none() && self.logview.is_none() && self.job.is_none();
        if self.watch.has_queued(here.as_deref()) && idle {
            self.show_prompt();
        } else if self.ports.has_queued(here.as_deref()) && idle {
            self.show_forward_offer();
        }
    }

    /// The shell menu's list of forwards and forwardable listeners.
    fn show_forwards(&mut self) {
        let Some((sandbox, gateway)) = self
            .session()
            .map(|s| (s.sandbox.clone(), s.gateway.clone()))
        else {
            return;
        };
        let mut items: Vec<String> = self
            .ports
            .forwards(&sandbox)
            .into_iter()
            .map(|p| format!("Stop forwarding localhost:{p}"))
            .collect();
        items.extend(
            self.ports
                .unforwarded(&sandbox)
                .into_iter()
                .map(|p| format!("Forward port {p} (listening in the sandbox)")),
        );
        items.push("Forward another port...".into());
        self.select(
            &format!("Port forwards of {sandbox}"),
            items,
            false,
            SelKind::Forwards { sandbox, gateway },
        );
    }

    /// Ask whether the next listening port should be forwarded to the host.
    fn show_forward_offer(&mut self) {
        let here = self.session().map(|s| s.sandbox.clone());
        let Some(o) = self.ports.next(here.as_deref()) else {
            return;
        };
        self.prompt_at = Some(Instant::now());
        let mut title = format!("Port {} started listening", o.port);
        if here.as_deref() != Some(o.sandbox.as_str()) {
            title.push_str(&format!(" in {}", o.sandbox));
        }
        title.push_str(": forward it to this computer?");
        let items = vec![
            format!("Forward to localhost:{}", o.port),
            format!("Forward to localhost:{} and open the browser", o.port),
            "Not now".into(),
            "Ignore this port until the session ends".into(),
        ];
        self.select(
            &title,
            items,
            false,
            SelKind::Forward {
                sandbox: o.sandbox,
                gateway: o.gateway,
                port: o.port,
            },
        );
    }

    fn answer_forward(&mut self, sandbox: String, gateway: String, port: u16, first: &str) {
        if first.starts_with("Ignore") {
            self.ports.ignore(&sandbox, port);
            self.status = format!("Not asking about port {port} in {sandbox} again this session");
        } else if first.starts_with("Forward") {
            if !ports::host_port_free(port) {
                self.status = format!(
                    "Error: port {port} is already in use on this computer; stop what uses it and start the server again"
                );
                return;
            }
            let open = first.ends_with("browser");
            self.ports.forwarded(&sandbox, port);
            self.status = format!("Forwarding localhost:{port} to {sandbox}...");
            self.bg_msg(move || {
                cli::run(
                    "openshell",
                    &ports::forward_args("start", &sandbox, &gateway, port),
                    &[],
                )?;
                let url = format!("http://localhost:{port}");
                if open && !cli::open_url(&url) {
                    return Ok(format!("Forwarding {url}; no browser could be started"));
                }
                Ok(format!("Forwarding {url} to {sandbox}"))
            });
        }
    }

    /// Apply the allows decided without a prompt, one `policy update` at a time.
    fn apply_auto_allows(&mut self) {
        if self.auto_running.load(Ordering::Relaxed) {
            return;
        }
        let batch = self.watch.take_auto();
        if batch.is_empty() {
            return;
        }
        self.auto_running.store(true, Ordering::Relaxed);
        let running = self.auto_running.clone();
        self.status = format!("Allowing {} web read(s)...", batch.len());
        let any: std::collections::HashSet<String> = batch
            .iter()
            .filter(|(sb, _, _)| self.watch.auto_any_on(sb))
            .map(|(sb, _, _)| sb.clone())
            .collect();
        self.bg_msg(move || {
            let mut done = Vec::new();
            let mut errs = Vec::new();
            for (sandbox, gateway, b) in &batch {
                // In "any program" mode the whole registrable domain opens at once; a refused
                // update (overlap with a provider's endpoint) falls back to the exact host.
                let res = if any.contains(sandbox) {
                    watch::allow_domain(gateway, sandbox, b)
                        .or_else(|_| watch::allow(gateway, sandbox, b, false))
                } else {
                    watch::allow(gateway, sandbox, b, false)
                };
                match res {
                    Ok(_) => done.push(b.target()),
                    Err(e) => errs.push(format!("{}: {e}", b.target())),
                }
            }
            running.store(false, Ordering::Relaxed);
            if errs.is_empty() {
                Ok(format!(
                    "Allowed web reads: {}; they are live, ask again",
                    done.join(", ")
                ))
            } else {
                Err(format!("Error: could not allow {}", errs.join("; ")))
            }
        });
    }

    fn show_prompt(&mut self) {
        let here = self.session().map(|s| s.sandbox.clone());
        let Some(p) = self.watch.next(here.as_deref()) else {
            return;
        };
        self.prompt_at = Some(Instant::now());
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
                    items.push(format!(
                        "Allow read-only (GET/HEAD/OPTIONS) for {prog} to {} only",
                        b.target()
                    ));
                }
                items.push(format!(
                    "Allow all methods for {prog} to {} only",
                    b.target()
                ));
                if watch::is_web_read(&b) && !self.watch.auto_web_on(&sandbox) {
                    items.push(format!(
                        "Allow web reads (GET/HEAD/OPTIONS) by curl and Claude Code in {sandbox} until the session ends, no more prompts"
                    ));
                }
                if watch::is_any_read(&b) {
                    items.push(if self.watch.auto_any_on(&sandbox) {
                        format!("Stop auto-allowing web reads by any program in {sandbox}")
                    } else {
                        format!(
                            "Allow web reads (GET/HEAD/OPTIONS) by any program in {sandbox}, always, no more prompts"
                        )
                    });
                }
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

    /// Shown in the review list while nothing was blocked yet; choosing it does nothing.
    const REVIEW_EMPTY: &'static str = "Nothing blocked yet; new requests appear here";

    /// The review list for `sandbox` as items: one per denial seen, or the placeholder.
    fn review_items(&self, sandbox: &str) -> Vec<String> {
        let items: Vec<String> = self
            .watch
            .for_sandbox(sandbox)
            .iter()
            .map(|s| review_label(s))
            .collect();
        if items.is_empty() {
            vec![Self::REVIEW_EMPTY.into()]
        } else {
            items
        }
    }

    /// `p` or the shell menu: the denials seen in a sandbox while its shell was open.
    /// The list stays open and fills in as the watcher sees more (`pump_watch`).
    fn review(&mut self, sandbox: String) {
        let items = self.review_items(&sandbox);
        let title = if self.has_session(&sandbox) {
            format!("Blocked in {sandbox}; Enter asks again")
        } else {
            format!(
                "Blocked in {sandbox}; Enter asks again (the watcher runs while its shell is open)"
            )
        };
        self.select(&title, items, false, SelKind::Review { sandbox });
    }

    /// Rebuild an open review list from what the watcher has seen, keeping the cursor on the
    /// same denial (by its position; entries are only ever appended).
    fn refresh_review(&mut self) {
        let Some(Modal::Select {
            kind: SelKind::Review { sandbox },
            ..
        }) = &self.modal
        else {
            return;
        };
        let new = self.review_items(&sandbox.clone());
        if let Some(Modal::Select {
            items,
            cursor,
            marked,
            ..
        }) = &mut self.modal
        {
            if *items != new {
                *cursor = (*cursor).min(new.len() - 1);
                *marked = vec![false; new.len()];
                *items = new;
            }
        }
    }

    pub fn on_paste(&mut self, text: String) {
        if let Some(Modal::Editor { ed, .. }) = &mut self.modal {
            ed.paste(&text);
            return;
        }
        if let Some(s) = self.ext_session() {
            s.on_paste(text);
            return;
        }
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

    fn bg<F>(&mut self, f: F)
    where
        F: FnOnce() -> Result<Done, String> + Send + 'static,
    {
        // Work the user started from the dashboard shows a progress popup, named by the status
        // ("Creating provider x"). A shell is never covered, and a popup that is already open
        // (a job with real progress opens its own) stays.
        if self.active.is_none() && self.modal.is_none() && self.busy() {
            let title = self.status.trim_end_matches("...").to_string();
            self.modal = Some(Modal::Task(std::sync::Arc::new(std::sync::Mutex::new(
                Task::new(title, 0),
            ))));
        }
        let tx = self.done_tx.clone();
        thread::spawn(move || {
            // A panic must still end the spinner the status shows while this runs.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .unwrap_or_else(|_| Err("the background task failed unexpectedly".into()));
            let d = r.unwrap_or_else(|e| Done {
                msg: format!("Error: {e}"),
                next: None,
                sync: None,
                gh: None,
            });
            let _ = tx.send(d);
        });
    }

    fn bg_msg<F>(&mut self, f: F)
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
        if matches!(self.modal, Some(Modal::Task(_))) {
            self.modal = None;
        }
        // A multi-line result is too long for the status line: show it whole in a popup.
        if d.msg.contains('\n') && self.modal.is_none() {
            self.modal = Some(Modal::Info(d.msg.clone()));
        }
        self.status = d.msg.lines().next().unwrap_or_default().to_string();
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
            Some(GhStage::ProfileUpdated { profile, sandboxes }) => {
                self.modal = Some(Modal::Confirm {
                    text: profile_restart_text(&profile, &sandboxes),
                    kind: ConfKind::Restart(sandboxes),
                });
                return;
            }
            Some(GhStage::Leftover { github, other }) => {
                let mut text = String::new();
                if !github.is_empty() {
                    text = format!(
                        "The GitHub token{} only this sandbox used {} still valid on github.com: {}.\n\nGitHub has no way to delete a token from here. Open your token list now so you can delete {}?",
                        if github.len() == 1 { "" } else { "s" },
                        if github.len() == 1 { "is" } else { "are" },
                        github.join(", "),
                        if github.len() == 1 { "it" } else { "them" },
                    );
                }
                for (label, token, page) in &other {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&format!(
                        "The {label} token {token} stays valid until you delete it at {page}."
                    ));
                }
                self.modal = Some(if github.is_empty() {
                    Modal::Info(text)
                } else {
                    Modal::Confirm {
                        text,
                        kind: ConfKind::OpenTokenPage(github),
                    }
                });
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
                    Resume::Create => self.start_create(&plan),
                }
            } else {
                let p = missing.remove(0);
                self.start_creds(p, false, Some((plan, missing)));
            }
        }
    }

    // ---- flows -----------------------------------------------------------

    fn select(&mut self, title: &str, items: Vec<String>, multi: bool, kind: SelKind) {
        let marked = vec![false; items.len()];
        self.select_marked(title, items, marked, multi, kind);
    }

    fn select_marked(
        &mut self,
        title: &str,
        items: Vec<String>,
        marked: Vec<bool>,
        multi: bool,
        kind: SelKind,
    ) {
        if items.is_empty() {
            self.modal = Some(Modal::Info(format!("Nothing to choose from for: {title}")));
            return;
        }
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

    /// Whether the job about to run prints a secret on the terminal (`claude setup-token`).
    pub fn job_prints_secret(&self) -> bool {
        self.token_flow.is_some()
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
                "The secret scan found {n} finding(s) in the files that would be uploaded. Run `osh sync-agents --json` and review the staged files, or deselect the uploads (own-skills, context)."
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
        if let Some((name, before)) = self.tmux_after_create.take() {
            if failed.is_none() {
                let dir = self.dir.clone();
                self.status = "Copying the host tmux and git config...".into();
                self.bg_msg(move || cli::install_sandbox_confs(&dir, &name, &before));
            }
        }
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
            self.pick_toolchains(plan);
        } else {
            let first = missing.remove(0);
            self.start_creds(first, false, Some((plan, missing)));
        }
    }

    /// A sandbox keeps the image it was created from, so offer the fresh build before creating
    /// from an image whose Dockerfile changed since it was built.
    fn ask_rebuild(&mut self, plan: Plan) {
        let outdated = self
            .snap
            .images
            .iter()
            .any(|i| i.tag == plan.image && i.built && i.freshness == cli::Freshness::Outdated);
        if !outdated {
            return self.confirm_create(plan);
        }
        let title = format!(
            "{} is outdated (its Dockerfile changed since it was built). A sandbox keeps the image it is created from",
            plan.image
        );
        let items = vec![
            "Rebuild the image first".to_string(),
            "Use the current image".to_string(),
        ];
        self.select(&title, items, false, SelKind::Rebuild(plan));
    }

    fn confirm_create(&mut self, plan: Plan) {
        let steps = self.sandbox_steps(&plan);
        let summary = steps
            .iter()
            .map(|s| format!("{} {}", s.prog, s.args.join(" ")))
            .collect::<Vec<_>>()
            .join("\n");
        let extra = plan.gh.as_ref().map_or(String::new(), |g| {
            // Forgejo tokens never expire, so they get no expiry line.
            let expiry = if g.repo.forge.expires() {
                format!(" (expires in {} days)", ghrepo::TOKEN_DAYS)
            } else {
                String::new()
            };
            format!(
                "First creates provider {} for {} from the pasted token{expiry}.\n\n",
                ghrepo::provider_name(&plan.name, &g.repo),
                g.repo.shown(),
            )
        });
        let mut note = self
            .creation_policy(&plan)
            .1
            .map_or(String::new(), |n| format!("Note: {n}.\n\n"));
        if !self.limits_supported() {
            note.push_str(&format!("{}.\n\n", self.no_limits_note()));
        }
        self.modal = Some(Modal::Confirm {
            text: format!("{extra}{note}Run:\n{summary}\n\nContinue?"),
            kind: ConfKind::CreateSandbox(plan),
        });
    }

    /// Queue the create job; a detached sandbox then gets the host's tmux config (`finish_job`).
    fn start_create(&mut self, plan: &Plan) {
        if plan.command.is_empty() {
            let before = self.snap.sandboxes.iter().map(|s| s.name.clone()).collect();
            self.tmux_after_create = Some((plan.name.clone(), before));
        }
        self.job = Some(self.sandbox_steps(plan));
    }

    /// The policy file for a new sandbox: `sandbox-policy.yaml` plus the toolchain hosts of
    /// `toolchains.toml` (see `preload`), and what the user should know about it: a missing base
    /// policy is replaced by the shipped one (never a `--policy` path that does not exist), and an
    /// unusable list or block leaves the hosts out. The sandbox still starts either way.
    fn creation_policy(&self, plan: &Plan) -> (PathBuf, Option<String>) {
        let base = self.dir.join("sandbox-policy.yaml");
        let generated = self.dir.join(".sandbox-policy.generated.yaml");
        let mut note = None;
        let text = std::fs::read_to_string(&base).unwrap_or_else(|_| {
            note = Some(
                "sandbox-policy.yaml is missing, so the shipped default is used (Data files in the command palette restores the file)".to_string(),
            );
            crate::assets::embedded("sandbox-policy.yaml")
                .unwrap_or_default()
                .to_string()
        });
        let groups = match std::fs::read_to_string(self.dir.join(TOOLCHAINS_FILE)) {
            Ok(list) => preload::parse(&list).unwrap_or_else(|e| {
                note = Some(format!(
                    "toolchains.toml is not valid TOML ({}), so no toolchain hosts are added",
                    crate::editor::error_summary(&e)
                ));
                Vec::new()
            }),
            Err(_) => {
                note = note.clone().or(Some(
                    "toolchains.toml is missing, so no toolchain hosts are added".to_string(),
                ));
                Vec::new()
            }
        };
        let groups = match &plan.toolchains {
            Some(picks) => preload::select(&groups, picks),
            None => groups,
        };
        let policy = preload::creation_policy(&text, &groups).unwrap_or_else(|e| {
            note = Some(e);
            preload::creation_policy(&text, &[]).unwrap_or_else(|_| text.clone())
        });
        match std::fs::write(&generated, policy) {
            Ok(()) => (generated, note),
            Err(e) if base.is_file() => (
                base,
                Some(format!(
                    "Could not write the generated policy ({e}), so the plain sandbox-policy.yaml is used without toolchain hosts"
                )),
            ),
            Err(e) => (
                generated,
                Some(format!("Could not write the sandbox policy: {e}")),
            ),
        }
    }

    fn sandbox_steps(&self, plan: &Plan) -> Vec<Step> {
        let mut steps = Vec::new();
        if let Some(img) = self
            .snap
            .images
            .iter()
            .find(|i| i.tag == plan.image && (!i.built || plan.rebuild))
        {
            steps.push(Step {
                prog: cli::runtime_bin().into(),
                args: cli::build_args(&self.dir, &img.file, &img.tag),
            });
        }
        let mut args: Vec<String> = vec!["sandbox".into(), "create".into()];
        // The default policy denies opening a PTY, which tmux needs in the shell.
        args.extend([
            "--policy".into(),
            self.creation_policy(plan).0.to_string_lossy().into(),
        ]);
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
            SelKind::Toolchains(plan) if picks.iter().any(|p| p == EDIT_TOOLCHAINS_ITEM) => {
                self.edit_toolchains(EditThen::NewSandbox(Box::new(plan)))
            }
            SelKind::ToolchainsRunning(name, gateway)
                if picks.iter().any(|p| p == EDIT_TOOLCHAINS_ITEM) =>
            {
                self.edit_toolchains(EditThen::Running(name, gateway))
            }
            SelKind::ToolchainsFix(then) => {
                if first == EDIT_TOOLCHAINS_ITEM {
                    self.edit_toolchains(then);
                } else if let EditThen::NewSandbox(plan) = then {
                    self.ask_name(*plan);
                }
            }
            SelKind::Toolchains(mut plan) => {
                let save = picks.iter().any(|p| p == SAVE_TOOLCHAINS_ITEM);
                let names: Vec<String> = picks
                    .iter()
                    .filter(|p| *p != SAVE_TOOLCHAINS_ITEM)
                    .map(|p| preload::group_of(p).to_string())
                    .collect();
                if save {
                    self.save_toolchain_defaults(&names);
                }
                plan.toolchains = Some(names);
                self.ask_name(plan);
            }
            SelKind::ToolchainsRunning(name, gateway) => {
                let names: Vec<String> = picks
                    .iter()
                    .map(|p| preload::group_of(p).to_string())
                    .collect();
                self.open_toolchains(name, gateway, &names);
            }
            SelKind::TransferTo(from) => {
                self.modal = Some(Modal::Confirm {
                    text: format!(
                        "Copy the home directory of '{from}' into '{first}'? Files with the same path in '{first}' are overwritten. This can take a while."
                    ),
                    kind: ConfKind::Transfer { from, to: first },
                });
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
                    // Only offered where a read-only profile ships (GitHub, Forgejo).
                    let read = repo.forge.read_profile().unwrap_or("github");
                    if !plan.providers.iter().any(|p| p == read) {
                        plan.providers.push(read.into());
                    }
                    if self.missing(&[read.to_string()]).is_empty() {
                        self.ask_command(plan);
                    } else {
                        plan.resume = Resume::Command;
                        if repo.is_github() {
                            self.start_creds("github".into(), false, Some((plan, vec![])));
                        } else {
                            self.start_forgejo_creds(&repo.host, plan);
                        }
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
            SelKind::GitProvider(plan, remote) => {
                let forge = ghrepo::chosen_forge(&first, &remote.host);
                ghrepo::remember_host(
                    self.hosts_file.as_deref(),
                    &remote.host,
                    forge,
                    remote.ssh_port,
                );
                match plan {
                    Some(plan) => self.with_forge(plan, &remote, forge),
                    None => match forge.and_then(|f| remote.repo(f)) {
                        Some(repo) => self.repo_steps(RepoFlow {
                            repo,
                            goal: Goal::Shared,
                        }),
                        None => self.status = unfit(&remote, forge),
                    },
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
                self.change_gateway(name);
            }
            SelKind::Rebuild(mut plan) => {
                plan.rebuild = first.starts_with("Rebuild");
                self.confirm_create(plan);
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
                self.bg(move || {
                    cli::os(&[
                        "sandbox",
                        "provider",
                        "detach",
                        &sandbox,
                        &other,
                        "--wait",
                        "--timeout",
                        "30",
                    ])?;
                    Ok(Done {
                        msg: format!("Detached {other}"),
                        next: None,
                        sync: Some(SyncStage::Retry {
                            sync,
                            profile,
                            round,
                        }),
                        gh: None,
                    })
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
                } else if first.starts_with("Stop auto-allowing") {
                    self.watch.disable_auto_any(&sandbox);
                    self.watch.decide(&sandbox, &b, State::IgnoredOnce);
                    self.status =
                        format!("Web reads by any program in {sandbox} ask again from now on");
                } else if first.starts_with("Allow web reads") && first.contains("by any program") {
                    self.watch.enable_auto_any(&sandbox);
                    self.watch.decide(&sandbox, &b, State::Allowed(false));
                    self.status = format!(
                        "Web reads by any program are allowed in {sandbox}, also next time; allowing {}...",
                        b.target()
                    );
                    self.watch.auto_allow(&sandbox, &gateway, b);
                } else if first.starts_with("Allow web reads") {
                    self.watch.enable_auto_web(&sandbox);
                    self.watch.decide(&sandbox, &b, State::Allowed(false));
                    self.status = format!(
                        "Web reads by curl and Claude Code are allowed in {sandbox} until the session ends; allowing {}...",
                        b.target()
                    );
                    self.watch.auto_allow(&sandbox, &gateway, b);
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
            SelKind::Forwards { sandbox, gateway } => {
                let port = |s: &str| {
                    s.rsplit(|c: char| !c.is_ascii_digit())
                        .find(|t| !t.is_empty())
                        .and_then(|t| t.parse::<u16>().ok())
                };
                if let (true, Some(p)) = (first.starts_with("Stop "), port(&first)) {
                    self.ports.unforward(&sandbox, p);
                    stop_forward(&sandbox, &gateway, p);
                    self.status = format!("Stopped forwarding localhost:{p}");
                } else if first.starts_with("Forward another") {
                    self.input(
                        "Port in the sandbox to forward (the host uses the same number)",
                        false,
                        InKind::ForwardPort { sandbox, gateway },
                    );
                } else if let Some(p) = port(&first) {
                    self.answer_forward(sandbox, gateway, p, &format!("Forward to localhost:{p}"));
                }
            }
            SelKind::Forward {
                sandbox,
                gateway,
                port,
            } => self.answer_forward(sandbox, gateway, port, &first),
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
            SelKind::DataFiles(rels) => match first.as_str() {
                NEW_PROFILE_ITEM => self.new_file(true),
                NEW_DOCKERFILE_ITEM => self.new_file(false),
                _ => {
                    if let Some(rel) = rels.into_iter().find(|r| {
                        first
                            .strip_prefix(r.as_str())
                            .is_some_and(|rest| rest.starts_with("  "))
                    }) {
                        self.file_menu(rel);
                    }
                }
            },
            SelKind::DataFile(rel) => self.on_file_action(rel, &first),
            SelKind::Review { sandbox } => {
                let seen: Vec<Denial> = self
                    .watch
                    .for_sandbox(&sandbox)
                    .iter()
                    .filter(|s| review_label(s) == first)
                    .map(|s| s.denial.clone())
                    .collect();
                match seen.into_iter().next() {
                    Some(Denial::Blocked(b)) => {
                        let gateway = self.snap.gateway.clone();
                        self.watch.reprompt(&sandbox, &gateway, b);
                        self.show_prompt();
                    }
                    Some(Denial::Notice { what, reason }) => {
                        self.modal = Some(Modal::Info(notice_text(&sandbox, &what, &reason)));
                    }
                    None => {
                        // The list changed under the cursor (or the placeholder): keep it open.
                        self.review(sandbox);
                    }
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
                } else if first.starts_with("Blocked") {
                    if let Some(name) = self.session().map(|s| s.sandbox.clone()) {
                        self.review(name);
                    }
                } else if first.starts_with("Auto-allow web reads") {
                    if let Some(name) = self.session().map(|s| s.sandbox.clone()) {
                        if self.watch.auto_any_on(&name) {
                            self.watch.disable_auto_any(&name);
                            self.status =
                                format!("Web reads by any program in {name} ask again from now on");
                        } else {
                            self.watch.enable_auto_any(&name);
                            self.status = format!(
                                "Web reads by any program are allowed in {name}, also next time"
                            );
                        }
                    }
                } else if first.starts_with("Open the toolchain hosts") {
                    if let Some((name, gateway)) = self
                        .session()
                        .map(|s| (s.sandbox.clone(), s.gateway.clone()))
                    {
                        self.pick_toolchains_running(name, gateway);
                    }
                } else if first.starts_with("Port forwards") {
                    self.show_forwards();
                } else if first.starts_with("Alerts") {
                    self.show_alerts(None);
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
                    let hint = self.project_repo().map(|r| r.shown()).unwrap_or_default();
                    let title = if hint.is_empty() {
                        "Repository, OWNER/REPO on github.com, or HOST/PATH as in its clone URL"
                            .to_string()
                    } else {
                        format!("Repository, OWNER/REPO or HOST/PATH (empty = {hint})")
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

    /// The repo of the first project folder on GitHub or on a host with a remembered provider.
    fn project_repo(&self) -> Option<GhRepo> {
        cli::project_dirs()
            .iter()
            .filter_map(|d| cli::origin_remote(d))
            .find_map(|r| {
                let forge = if r.host == ghrepo::GITHUB_HOST {
                    Some(ghrepo::Forge::GitHub)
                } else {
                    ghrepo::host_entry(self.hosts_file.as_deref(), &r.host)?.forge
                };
                r.repo(forge?)
            })
    }

    /// The provider of `host`: GitHub for github.com, else the remembered answer (`Some(None)`
    /// for "none / other"), else `None` when the user has not been asked.
    fn known_forge(&self, host: &str) -> Option<Option<ghrepo::Forge>> {
        if host == ghrepo::GITHUB_HOST {
            return Some(Some(ghrepo::Forge::GitHub));
        }
        ghrepo::host_entry(self.hosts_file.as_deref(), host).map(|e| e.forge)
    }

    fn ask_provider(&mut self, plan: Option<Plan>, remote: ghrepo::Remote) {
        let title = format!("Which git provider hosts {}?", remote.host);
        let items = ghrepo::provider_choices(&remote.host);
        self.select(&title, items, false, SelKind::GitProvider(plan, remote));
    }

    /// After the share is set: offer repo access when the shared folder's `origin` is on
    /// github.com, or on a host whose provider the user names (asked once per host, then
    /// remembered).
    fn ask_github(&mut self, plan: Plan) {
        let Some(remote) = cli::origin_remote(&plan.share) else {
            return self.ask_command(plan);
        };
        match self.known_forge(&remote.host) {
            Some(forge) => self.with_forge(plan, &remote, forge),
            None => self.ask_provider(Some(plan), remote),
        }
    }

    /// The access question for `remote` on `forge`; straight on to the command for "none", or
    /// with a status line when the remote does not fit that provider.
    fn with_forge(&mut self, plan: Plan, remote: &ghrepo::Remote, forge: Option<ghrepo::Forge>) {
        match forge.and_then(|f| remote.repo(f)) {
            Some(repo) => self.ask_repo_access(plan, repo),
            None => {
                if forge.is_some() {
                    self.status = unfit(remote, forge);
                }
                self.ask_command(plan);
            }
        }
    }

    /// A shared token from the Providers tab for a repo given as HOST/PATH.
    fn shared_on(&mut self, remote: ghrepo::Remote) {
        match self.known_forge(&remote.host) {
            Some(forge) => match forge.and_then(|f| remote.repo(f)) {
                Some(repo) => self.repo_steps(RepoFlow {
                    repo,
                    goal: Goal::Shared,
                }),
                None => self.status = unfit(&remote, forge),
            },
            None => self.ask_provider(None, remote),
        }
    }

    fn ask_repo_access(&mut self, plan: Plan, repo: GhRepo) {
        let label = repo.forge.label();
        let mut items = vec![format!("No {label} access")];
        if let Some(read) = repo.forge.read_profile() {
            items.push(format!("Read-only (the {read} provider)"));
        }
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
        let title = format!("{label} access to {}", repo.shown());
        self.select(&title, items, false, SelKind::GhAccess(plan, repo));
    }

    /// Create the read-only `forgejo` provider with its host already filled in.
    fn start_forgejo_creds(&mut self, host: &str, plan: Plan) {
        let keys = cli::profile_env_keys(&self.dir, "forgejo");
        let values = if keys.first().map(String::as_str) == Some("FORGEJO_HOST") {
            vec![("FORGEJO_HOST".to_string(), host.to_string())]
        } else {
            vec![]
        };
        self.cred_step(InKind::Cred {
            provider: "forgejo".into(),
            keys,
            values,
            update: false,
            next: Some((plan, vec![])),
        });
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
        let label = self.repo_label(&flow);
        let url = ghrepo::token_url(&flow.repo, &label);
        self.modal = Some(Modal::Confirm {
            text: format!(
                "{}\n\nOpen the page now and paste the token next?",
                ghrepo::steps_for(&flow.repo, &url, &label)
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
        match GhRepo::from_spec(&t.repo) {
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

    /// The toolchain groups of `toolchains.toml`, or None (with a status line when the file is
    /// broken) so the caller can carry on without the list.
    fn toolchain_groups(&mut self) -> Option<Vec<preload::Group>> {
        let Ok(list) = std::fs::read_to_string(self.dir.join(TOOLCHAINS_FILE)) else {
            self.status = "toolchains.toml is missing, so no toolchain hosts are offered (Data files in the command palette restores it)".into();
            return None;
        };
        match preload::parse(&list) {
            Ok(g) if !g.is_empty() => {
                let skipped = preload::skipped_hosts(&list);
                if !skipped.is_empty() {
                    self.status = format!(
                        "toolchains.toml: {} skipped (only exact host names work)",
                        skipped.join(", ")
                    );
                }
                Some(g)
            }
            Ok(_) => None,
            Err(e) => {
                self.status = format!(
                    "toolchains.toml is not valid TOML: {}",
                    e.lines().next().unwrap_or_default()
                );
                None
            }
        }
    }

    /// New-sandbox step: which toolchain host groups it may read from. Marks start at the file's
    /// defaults; the last line saves the marks as the new defaults. Skipped when the list is
    /// missing or broken (the sandbox then starts without it, as before).
    fn pick_toolchains(&mut self, plan: Plan) {
        let Some(groups) = self.toolchain_groups() else {
            if let Some(why) = self.toolchains_broken() {
                return self.offer_toolchain_fix(&why, EditThen::NewSandbox(Box::new(plan)));
            }
            return self.ask_name(plan);
        };
        let mut items: Vec<String> = groups.iter().map(preload::label).collect();
        let mut marked: Vec<bool> = groups.iter().map(|g| g.on).collect();
        items.push(SAVE_TOOLCHAINS_ITEM.into());
        items.push(EDIT_TOOLCHAINS_ITEM.into());
        marked.extend([false, false]);
        self.select_marked(
            "Toolchain hosts this sandbox may read from (space to mark, Enter to confirm)",
            items,
            marked,
            true,
            SelKind::Toolchains(plan),
        );
    }

    /// Writes the marked groups into `toolchains.toml` as the defaults, keeping its comments.
    fn save_toolchain_defaults(&mut self, on: &[String]) {
        let path = self.dir.join(TOOLCHAINS_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            self.status = "Defaults not saved: toolchains.toml is missing".into();
            return;
        };
        let Ok(groups) = preload::parse(&text) else {
            self.status = "Defaults not saved: toolchains.toml is no longer valid TOML".into();
            return;
        };
        let want: Vec<(String, bool)> = groups
            .iter()
            .map(|g| (g.name.clone(), on.contains(&g.name)))
            .collect();
        match std::fs::write(&path, preload::set_enabled(&text, &want)) {
            Ok(()) => self.status = "Toolchain defaults saved".into(),
            Err(e) => self.status = format!("Could not save toolchains.toml: {e}"),
        }
    }

    /// Ctrl-] entry: choose which groups to open in a running sandbox (marks start at the defaults).
    fn pick_toolchains_running(&mut self, name: String, gateway: String) {
        let Some(groups) = self.toolchain_groups() else {
            let why = self
                .toolchains_broken()
                .unwrap_or_else(|| "toolchains.toml has no usable groups".into());
            return self.offer_toolchain_fix(&why, EditThen::Running(name, gateway));
        };
        let mut items: Vec<String> = groups.iter().map(preload::label).collect();
        let mut marked: Vec<bool> = groups.iter().map(|g| g.on).collect();
        items.push(EDIT_TOOLCHAINS_ITEM.into());
        marked.push(false);
        self.select_marked(
            &format!("Toolchain hosts to open in {name} (space to mark, Enter to confirm)"),
            items,
            marked,
            true,
            SelKind::ToolchainsRunning(name, gateway),
        );
    }

    /// Why `toolchains.toml` cannot be used, when it exists but is not valid TOML or has no group.
    fn toolchains_broken(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(TOOLCHAINS_FILE)).ok()?;
        match preload::parse(&text) {
            Ok(g) if g.is_empty() => Some("toolchains.toml has no groups".into()),
            Ok(_) => None,
            Err(e) => Some(format!(
                "toolchains.toml is not valid TOML: {}",
                crate::editor::error_summary(&e)
            )),
        }
    }

    /// The fix path for a broken list: edit it here, or (new sandbox) go on without it.
    fn offer_toolchain_fix(&mut self, why: &str, then: EditThen) {
        let other = match then {
            EditThen::NewSandbox(_) => "Continue without the toolchain hosts",
            _ => "Cancel",
        };
        self.select(
            why,
            vec![EDIT_TOOLCHAINS_ITEM.into(), other.into()],
            false,
            SelKind::ToolchainsFix(then),
        );
    }

    /// Open `toolchains.toml` in the editor; saving checks it parses.
    fn edit_toolchains(&mut self, then: EditThen) {
        let check: crate::editor::Check = preload::check;
        let path = self.dir.join(TOOLCHAINS_FILE);
        self.open_editor(TOOLCHAINS_FILE, path, None, Some(check), then);
    }

    /// A palette command that has no key of its own.
    fn run_command(&mut self, cmd: crate::palette::Command) {
        use crate::palette::Command as C;
        match cmd {
            C::EditToolchains => self.edit_toolchains(EditThen::Nothing),
            C::EditorMode => {
                use crate::exted::Pref;
                let next = crate::exted::load_pref(&self.dir).next();
                self.status = match crate::exted::save_pref(&self.dir, next) {
                    Ok(()) => match next {
                        Pref::Auto => "Editor: auto (yours from $VISUAL, $EDITOR or PATH; else the built-in one)",
                        Pref::External => "Editor: always yours ($VISUAL, $EDITOR or PATH), never the built-in one",
                        Pref::Builtin => "Editor: always osh's built-in one",
                    }
                    .into(),
                    Err(e) => format!("Error: could not save the setting: {e}"),
                }
            }
            C::DataFiles => self.data_files(),
            C::EditPolicy => self.file_menu("sandbox-policy.yaml".into()),
        }
    }

    /// Something to tell the user about the data files at start-up (kept edits, an earlier
    /// install's data): an alert, so it survives the alternate screen and stays under `!`.
    pub fn data_notice(&mut self, text: String) {
        let title = text.lines().next().unwrap_or_default().to_string();
        self.push_alert(crate::alerts::Alert {
            sev: crate::alerts::Severity::Warn,
            title,
            detail: text,
            sandbox: None,
        });
    }

    /// The Data files list: every file osh ships or the user added, with how it stands against
    /// the shipped default, and entries that start a new profile or Dockerfile.
    fn data_files(&mut self) {
        let files = crate::assets::files(&self.dir);
        let width = files.iter().map(|(r, _)| r.len()).max().unwrap_or(0);
        let mut items: Vec<String> = files
            .iter()
            .map(|(r, s)| format!("{r:<width$}  {}", s.label()))
            .collect();
        items.push(NEW_PROFILE_ITEM.into());
        items.push(NEW_DOCKERFILE_ITEM.into());
        let rels = files.into_iter().map(|(r, _)| r).collect();
        self.select(
            &format!("Data files in {}", self.dir.display()),
            items,
            false,
            SelKind::DataFiles(rels),
        );
    }

    /// The profile id a data file stands for (`providers/ID.yaml` or `.yml`).
    fn profile_of(rel: &str) -> Option<&str> {
        let name = rel.strip_prefix("providers/")?;
        name.strip_suffix(".yaml")
            .or_else(|| name.strip_suffix(".yml"))
    }

    /// What can be done with one data file: only the actions that change something are listed.
    fn file_menu(&mut self, rel: String) {
        use crate::assets::{self, FileState};
        let state = assets::state(&self.dir, &rel);
        let shipped = assets::embedded(&rel).is_some();
        let exists = self.dir.join(&rel).is_file();
        let mut items: Vec<&str> = Vec::new();
        if exists {
            items.extend([FA_EDIT, FA_VIEW]);
        } else if !shipped {
            items.push(FA_CREATE);
        }
        if shipped && exists && state != FileState::Shipped {
            items.push(FA_DIFF);
        }
        if shipped && state.restorable() {
            items.push(FA_RESTORE);
        }
        if exists && !assets::required(&rel) {
            items.push(FA_DELETE);
        }
        let on_gateway =
            Self::profile_of(&rel).is_some_and(|id| self.snap.profiles.iter().any(|p| p.id == id));
        if on_gateway {
            items.push(if exists { FA_GW_DIFF } else { FA_GW_SAVE });
            items.push(FA_GW_DELETE);
        }
        items.push(FA_BACK);
        let what = if !exists && !shipped {
            if on_gateway {
                "no file; the gateway has this profile".to_string()
            } else {
                "no file yet".to_string()
            }
        } else {
            state.label().to_string()
        };
        self.select(
            &format!("{rel} ({what})"),
            items.into_iter().map(String::from).collect(),
            false,
            SelKind::DataFile(rel),
        );
    }

    /// The file menu as a modal, to come back to after a viewer.
    fn file_menu_modal(&mut self, rel: &str) -> Option<Modal> {
        self.file_menu(rel.to_string());
        self.modal.take()
    }

    fn on_file_action(&mut self, rel: String, action: &str) {
        let path = self.dir.join(&rel);
        match action {
            FA_EDIT | FA_CREATE => self.edit_file(rel),
            FA_VIEW => {
                let text = std::fs::read_to_string(&path).unwrap_or_else(|e| format!("{e}"));
                let back = self.file_menu_modal(&rel);
                self.open_viewer(&rel, text, back);
            }
            FA_DIFF => {
                let mine = std::fs::read_to_string(&path).unwrap_or_default();
                let text =
                    crate::assets::diff(crate::assets::embedded(&rel).unwrap_or_default(), &mine);
                let back = self.file_menu_modal(&rel);
                self.open_viewer(&format!("{rel}: shipped default vs your copy"), text, back);
            }
            FA_RESTORE => {
                let keep = if path.is_file() {
                    format!(" Your copy is kept as {rel}.osh-bak.")
                } else {
                    String::new()
                };
                self.modal = Some(Modal::Confirm {
                    text: format!("Put the shipped default of {rel} back?{keep}"),
                    kind: ConfKind::RestoreFile(rel),
                });
            }
            FA_DELETE => {
                let back = if crate::assets::embedded(&rel).is_some() {
                    " The shipped default stays available under Data files (Restore)."
                } else {
                    " It cannot be brought back."
                };
                self.modal = Some(Modal::Confirm {
                    text: format!("Delete {rel}?{back}"),
                    kind: ConfKind::DeleteFile(rel),
                });
            }
            FA_GW_DIFF => {
                let Some(id) = Self::profile_of(&rel).map(String::from) else {
                    return;
                };
                self.status = format!("Comparing {id} with the gateway's copy...");
                self.bg_msg(move || {
                    let theirs = cli::export_profile(&id)?;
                    let mine = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                    Ok(format!(
                        "{id}: the gateway's copy (-) against {rel} (+)\n\n{}",
                        crate::assets::diff(&theirs, &mine)
                    ))
                });
            }
            FA_GW_SAVE => {
                let Some(id) = Self::profile_of(&rel).map(String::from) else {
                    return;
                };
                self.status = format!("Saving the gateway's {id} to {rel}...");
                self.bg_msg(move || {
                    let text = cli::export_profile(&id)?;
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(&path, text).map_err(|e| e.to_string())?;
                    Ok(format!("Saved the gateway's {id} as {rel}"))
                });
            }
            FA_GW_DELETE => {
                let Some(id) = Self::profile_of(&rel).map(String::from) else {
                    return;
                };
                self.modal = Some(Modal::Confirm {
                    text: format!(
                        "Delete the profile {id} from gateway {}? Providers made from it stop working until it is imported again. The file stays.",
                        cli::gateway()
                    ),
                    kind: ConfKind::DeleteProfile(id),
                });
            }
            _ => self.data_files(),
        }
    }

    /// Open a data file in the editor with the check that fits it; back to its menu after.
    fn edit_file(&mut self, rel: String) {
        let check = crate::assets::check_for(&rel);
        let path = self.dir.join(&rel);
        self.open_editor(&rel.clone(), path, None, check, EditThen::DataFile(rel));
    }

    /// Ask for the name of a new provider profile (`profile`) or Dockerfile.
    fn new_file(&mut self, profile: bool) {
        let title = if profile {
            "Name of the new provider profile (letters, digits, - and _)"
        } else {
            "Name of the new image (letters, digits, - and _); its Dockerfile starts as a copy of pi's"
        };
        self.input(title, false, InKind::NewFile(profile));
    }

    /// Start the new file in the editor from a template; it is written on the first save.
    fn create_file(&mut self, profile: bool, name: &str) {
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            self.modal = Some(Modal::Info(format!(
                "{name:?} cannot be a file name here: use letters, digits, - and _ only."
            )));
            return;
        }
        let rel = if profile {
            format!("providers/{name}.yaml")
        } else {
            format!("images/Dockerfile-{name}")
        };
        if self.dir.join(&rel).exists() || (profile && cli::profile_path(&self.dir, name).exists())
        {
            return self.file_menu(rel);
        }
        let template = if profile {
            profile_template(name)
        } else {
            format!(
                "# The {name} image, started as a copy of Dockerfile-pi. Build it on the Images tab (b).\n{}",
                crate::assets::embedded("images/Dockerfile-pi").unwrap_or("FROM debian:stable-slim\n")
            )
        };
        let (path, check) = (self.dir.join(&rel), crate::assets::check_for(&rel));
        self.open_editor(
            &rel.clone(),
            path,
            Some(template),
            check,
            EditThen::DataFile(rel),
        );
    }

    /// The editor is open (it takes every key and paste).
    pub fn editing(&self) -> bool {
        matches!(
            self.modal,
            Some(Modal::Editor { .. } | Modal::ExtEdit { .. })
        )
    }

    /// Act on what the editor asked for after a key: copy to the host clipboard, save, close.
    fn editor_done(
        &mut self,
        mut ed: Box<crate::editor::Editor>,
        then: EditThen,
        out: crate::editor::Out,
    ) {
        use crate::editor::Out;
        if let Some(text) = ed.take_copied() {
            if !cfg!(test) {
                self.copy(&text, "Copied");
            }
        }
        let close = match out {
            Out::Stay => false,
            Out::Save => {
                if ed.save().is_ok() {
                    self.status = format!("Saved {}", ed.name);
                }
                false
            }
            Out::SaveQuit => match ed.save() {
                Ok(()) => {
                    self.status = format!("Saved {}", ed.name);
                    true
                }
                Err(_) => false,
            },
            Out::Quit => true,
        };
        if !close {
            self.modal = Some(Modal::Editor { ed, then });
            return;
        }
        self.edit_closed(then, ed.saves > 0);
    }

    /// After an edit, in either editor: back to where it was opened from, with what comes next.
    fn edit_closed(&mut self, then: EditThen, saved: bool) {
        match then {
            EditThen::Nothing => {}
            EditThen::DataFile(rel) => self.file_menu(rel),
            EditThen::Gateway(name) => {
                if saved {
                    self.modal = Some(Modal::Confirm {
                        text: format!(
                            "Restart {name} now so it uses the saved config? Sandboxes on it lose their connections for a moment."
                        ),
                        kind: ConfKind::OshJob(vec![
                            "gateway".into(),
                            "restart".into(),
                            name,
                            "--confirmed".into(),
                        ]),
                    });
                }
            }
            EditThen::NewSandbox(plan) => self.pick_toolchains(*plan),
            EditThen::Running(name, gateway) => self.pick_toolchains_running(name, gateway),
        }
    }

    /// The one way osh opens a file for editing: the user's editor ($VISUAL, $EDITOR, or one on
    /// PATH) in osh's terminal, else the built-in editor, as the palette's editor setting says.
    /// `template` starts a file that does not exist yet; `check` must pass before it is written.
    fn open_editor(
        &mut self,
        name: &str,
        path: PathBuf,
        template: Option<String>,
        check: Option<crate::editor::Check>,
        then: EditThen,
    ) {
        use crate::exted::{Choice, Pref};
        let text = match &template {
            Some(t) => t.clone(),
            None => match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    self.modal = Some(Modal::Info(format!("Cannot open {name}: {e}")));
                    return;
                }
            },
        };
        let pref = crate::exted::load_pref(&self.dir);
        let why = match crate::exted::choose(pref, crate::exted::host_editor()) {
            Choice::External(r) => {
                let (prog, from) = (r.program().to_string(), r.from.clone());
                let (w, h) = modal_screen();
                match crate::exted::Run::start(r, name, path.clone(), &text, check, pane(w, h)) {
                    Ok(run) => {
                        self.status = format!(
                            "Editing {name} in {prog} ({from}); osh checks and saves it when {prog} exits"
                        );
                        self.modal = Some(Modal::ExtEdit {
                            run: Box::new(run),
                            then,
                        });
                        return;
                    }
                    Err(e) if pref == Pref::External => {
                        self.modal = Some(Modal::Info(format!(
                            "{prog} ({from}) did not start: {e}. The editor setting (palette) is external, so osh did not open its built-in editor."
                        )));
                        return;
                    }
                    Err(e) => format!("built-in editor: {prog} ({from}) did not start: {e}"),
                }
            }
            Choice::Builtin(why) => why,
            Choice::Refuse(why) => {
                self.modal = Some(Modal::Info(format!(
                    "No editor of your own to open {name} in: {why}. Set $VISUAL or $EDITOR, or set the editor (palette) back to auto or built-in."
                )));
                return;
            }
        };
        let ed = match template {
            Some(t) => Ok(crate::editor::Editor::create(name, path, &t, check)),
            None => crate::editor::Editor::open(name, path, check),
        };
        match ed {
            Ok(mut ed) => {
                ed.hint(&why);
                self.status = why;
                self.modal = Some(Modal::Editor {
                    ed: Box::new(ed),
                    then,
                })
            }
            Err(e) => self.modal = Some(Modal::Info(e)),
        }
    }

    fn ext_session(&mut self) -> Option<&mut Session> {
        match &mut self.modal {
            Some(Modal::ExtEdit { run, .. }) => run.session.as_mut(),
            _ => None,
        }
    }

    /// The user's editor is running in front (it wants every key, the mouse and frames).
    pub fn ext_editing(&self) -> bool {
        matches!(&self.modal, Some(Modal::ExtEdit { run, .. }) if run.session.is_some())
    }

    /// Feed the editor's output to its terminal; once it exits, check and save what it wrote.
    fn pump_ext_editor(&mut self) {
        let Some(s) = self.ext_session() else {
            return;
        };
        s.pump();
        let (copy, exited) = (s.vt.take_clipboard(), s.exited);
        if let Some(t) = copy {
            if !cfg!(test) {
                self.copy(&t, "copied");
            }
        }
        if !exited {
            return;
        }
        let Some(Modal::ExtEdit { mut run, then }) = self.modal.take() else {
            return;
        };
        let ok = run.session.as_mut().and_then(|s| s.wait_exit());
        self.ext_finished(run, then, ok);
    }

    fn ext_finished(&mut self, mut run: Box<crate::exted::Run>, then: EditThen, ok: Option<bool>) {
        use crate::exted::Outcome;
        let (name, prog) = (run.name.clone(), run.editor.program().to_string());
        match run.finish(ok) {
            Outcome::Saved => {
                self.status = format!("Saved {name} (edited in {prog})");
                self.edit_closed(then, true);
            }
            Outcome::Unchanged => {
                self.status = format!("{name} unchanged: nothing written");
                self.edit_closed(then, false);
            }
            Outcome::Cancelled(why) => {
                self.status = format!("Cancelled: {why}");
                self.edit_closed(then, false);
            }
            // The edits stay in the temporary copy; the screen asks to reopen it or discard.
            Outcome::Invalid(e) => {
                self.status = format!("Not saved: {e}");
                self.modal = Some(Modal::ExtEdit { run, then });
            }
        }
    }

    /// Keys go to the running editor; after a failed check, Enter (or r) reopens it on the same
    /// copy and d discards the edits.
    fn ext_editor_key(&mut self, mut run: Box<crate::exted::Run>, then: EditThen, key: KeyEvent) {
        if let Some(s) = run.session.as_mut() {
            s.on_key(&key);
            self.modal = Some(Modal::ExtEdit { run, then });
            return;
        }
        let (name, prog) = (run.name.clone(), run.editor.program().to_string());
        match key.code {
            KeyCode::Enter | KeyCode::Char('r') => {
                let (w, h) = modal_screen();
                match run.spawn(pane(w, h)) {
                    Ok(()) => {
                        self.status = format!("Editing {name} again in {prog}");
                        self.modal = Some(Modal::ExtEdit { run, then });
                    }
                    Err(e) => {
                        self.status = format!(
                            "Discarded: {prog} did not start again ({e}); {name} not changed"
                        );
                        self.edit_closed(then, false);
                    }
                }
            }
            KeyCode::Char('d') => {
                self.status = format!("Discarded the edits: {name} not changed");
                self.edit_closed(then, false);
            }
            _ => self.modal = Some(Modal::ExtEdit { run, then }),
        }
    }

    /// Open the hosts of the named groups in a running sandbox, one `policy update` per host.
    fn open_toolchains(&mut self, name: String, gateway: String, names: &[String]) {
        let Some(groups) = self.toolchain_groups() else {
            return;
        };
        let groups = preload::select(&groups, names);
        let cmds = preload::update_commands(&gateway, &name, &groups);
        if cmds.is_empty() {
            self.status = "No toolchain group was marked".into();
            return;
        }
        self.status = format!("Opening the toolchain hosts in {name}...");
        let task = std::sync::Arc::new(std::sync::Mutex::new(Task::new(
            format!("Opening toolchain hosts in {name}"),
            cmds.len(),
        )));
        self.modal = Some(Modal::Task(task.clone()));
        self.bg_msg(move || {
            // One refused host (a provider's rule already covers it, say) must not stop the
            // rest; the report names what happened to each.
            let mut results = Vec::new();
            for args in &cmds {
                let host = preload::command_host(args);
                if let Ok(mut t) = task.lock() {
                    t.current = host.clone();
                }
                results.push((host, cli::run("openshell", args, &[]).map(|_| ())));
                if let Ok(mut t) = task.lock() {
                    t.done += 1;
                }
            }
            Ok(preload::report(&name, &results))
        });
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
            InKind::NewFile(profile) => self.create_file(profile, &text),
            InKind::VmCpus(name) => match text.trim().parse::<u32>() {
                Ok(n) if (1..=64).contains(&n) => {
                    let (_, mem) = self.vm_size_now();
                    self.input(
                        &format!("Memory in MiB for every sandbox on {name} (now {mem})"),
                        false,
                        InKind::VmMemory(name, n),
                    );
                }
                _ => self.status = "Not changed: give a number of vCPUs from 1 to 64".into(),
            },
            InKind::VmMemory(name, vcpus) => match text.trim().parse::<u32>() {
                Ok(mem) if (512..=262_144).contains(&mem) => {
                    self.modal = Some(Modal::Confirm {
                        text: format!(
                            "Give every sandbox on {name} {vcpus} vCPUs and {mem} MiB?\n\nosh writes this into the gateway's config and restarts the gateway: running sandboxes on it stop, and new ones start at this size."
                        ),
                        kind: ConfKind::VmSize(name, vcpus, mem),
                    })
                }
                _ => self.status = "Not changed: give the memory in MiB, 512 to 262144".into(),
            },
            InKind::OtherImage => {
                if !text.trim().is_empty() {
                    self.pick_providers(Plan {
                        image: text.trim().into(),
                        ..Plan::default()
                    });
                }
            }
            InKind::ForwardPort { sandbox, gateway } => match text.trim().parse::<u16>() {
                Ok(p) if p > 0 => {
                    self.answer_forward(sandbox, gateway, p, &format!("Forward to localhost:{p}"))
                }
                _ => self.status = "Error: a port is a number from 1 to 65535".into(),
            },
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
                    self.status = self.no_limits_note();
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
                self.ask_rebuild(plan);
            }
            InKind::RepoName => {
                let text = text.trim();
                let text = text.strip_prefix("https://").unwrap_or(text);
                let repo = if text.is_empty() {
                    self.project_repo()
                } else if text.split('/').count() >= 3 {
                    // HOST/PATH: the host's provider, asked once like in the wizard.
                    match ghrepo::parse_url(&format!("https://{text}")) {
                        Some(remote) => return self.shared_on(remote),
                        None => None,
                    }
                } else {
                    ghrepo::parse_remote(&format!("https://github.com/{text}"))
                };
                match repo {
                    Some(repo) => self.repo_steps(RepoFlow {
                        repo,
                        goal: Goal::Shared,
                    }),
                    None => {
                        self.status =
                            "Error: expected OWNER/REPO on github.com, or HOST/PATH".into()
                    }
                }
            }
            InKind::RepoToken(flow) => {
                let token = text.trim().to_string();
                if token.is_empty() {
                    self.status = "Cancelled: empty value".into();
                    return;
                }
                self.status = format!("Checking the token against {}...", flow.repo.shown());
                self.bg(move || {
                    let problem = match cli::repo_status(&flow.repo, &token) {
                        Ok(s) => cli::repo_status_problem(s, &flow.repo),
                        Err(e) => Some(format!(
                            "Could not reach {}: {e}",
                            cli::host_name(&flow.repo)
                        )),
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

    /// Why the create flow did not ask for CPU and memory limits.
    fn no_limits_note(&self) -> String {
        if self.snap.driver == "vm" {
            let (cpus, mem) = self.vm_size_now();
            format!(
                "No CPU or memory question: every VM sandbox gets the gateway's size, {cpus} vCPUs and {mem} MiB (V on the Gateways tab changes it)"
            )
        } else {
            "No CPU or memory question: the gateway has not said which driver it runs yet, so limits are left to its defaults".into()
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
        let home = std::env::var("HOME").unwrap_or_default();
        let title = match crate::alerts::risky_share(&plan.share, &home) {
            Some(why) => {
                self.push_alert(crate::alerts::Alert {
                    sev: crate::alerts::Severity::High,
                    title: format!("Sharing {} is risky", plan.share),
                    detail: format!("{why}. Share the project folder instead."),
                    sandbox: Some(plan.name.clone()).filter(|n| !n.is_empty()),
                });
                format!("Access to the shared folder (RISKY: {why})")
            }
            None => "Access to the shared folder".to_string(),
        };
        self.select(
            &title,
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
            ConfKind::CreateSandbox(mut plan) => {
                if plan.share_rw && !plan.share.is_empty() {
                    let _ = crate::alerts::register_rw_share(&self.dir, &plan.share, &plan.name);
                }
                match plan.gh.take() {
                    None => self.start_create(&plan),
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
                }
            }
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
                let title = format!("Token for {} (hidden)", flow.repo.shown());
                self.input(&title, true, InKind::RepoToken(flow));
            }
            ConfKind::OpenTokenPage(names) => {
                self.status = if cli::open_url(ghrepo::TOKENS_PAGE) {
                    format!("Delete {} on GitHub", names.join(", "))
                } else {
                    format!("Could not open a browser; go to {}", ghrepo::TOKENS_PAGE)
                };
            }
            ConfKind::Transfer { from, to } => {
                self.status = format!("Transferring {from} to {to}...");
                self.bg_msg(move || cli::transfer_state(&from, &to));
            }
            ConfKind::DeleteSandbox(name) => {
                let providers = self.snap.providers.clone();
                self.bg(move || {
                    cli::os(&["sandbox", "delete", &name])?;
                    let gone = cli::drop_repo_providers(&name, &providers);
                    let msg = if gone.is_empty() {
                        format!("Deleted sandbox {name}")
                    } else {
                        let names: Vec<&str> = gone.iter().map(|d| d.provider.as_str()).collect();
                        format!("Deleted sandbox {name} and {}", names.join(", "))
                    };
                    let mut github = Vec::new();
                    let mut other = Vec::new();
                    for d in gone {
                        match d.page {
                            Some((label, page)) => other.push((label, d.token, page)),
                            None => github.push(d.token),
                        }
                    }
                    let any = !github.is_empty() || !other.is_empty();
                    Ok(Done {
                        msg,
                        next: None,
                        sync: None,
                        gh: any.then_some(GhStage::Leftover { github, other }),
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
            ConfKind::RestoreFile(rel) => {
                let path = self.dir.join(&rel);
                let backup = path.with_file_name(format!(
                    "{}.osh-bak",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
                let kept = match std::fs::copy(&path, &backup) {
                    Ok(_) => format!(" (your copy: {rel}.osh-bak)"),
                    Err(_) => String::new(),
                };
                self.status = match crate::assets::restore(&self.dir, &rel) {
                    Ok(()) => format!("Restored the shipped {rel}{kept}{}", restore_hint(&rel)),
                    Err(e) => format!("Error: could not restore {rel}: {e}"),
                };
            }
            ConfKind::DeleteFile(rel) => {
                self.status = match crate::assets::remove(&self.dir, &rel) {
                    Ok(()) => format!("Deleted {rel}"),
                    Err(e) => format!("Error: could not delete {rel}: {e}"),
                };
            }
            ConfKind::OshJob(args) => self.osh_job(args),
            ConfKind::VmSize(name, vcpus, mem) => self.osh_job(vec![
                "gateway".into(),
                "vm-size".into(),
                name,
                vcpus.to_string(),
                mem.to_string(),
                "--confirmed".into(),
            ]),
            ConfKind::DeleteProfile(id) => {
                self.status = format!("Deleting profile {id}...");
                self.bg_msg(move || {
                    cli::os(&["profile", "delete", &id]).map(|_| format!("Deleted profile {id}"))
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
            ConfKind::RemoveGateway(name) => {
                let exe = std::env::current_exe().unwrap_or_else(|_| "osh".into());
                self.job = Some(vec![Step {
                    prog: exe.to_string_lossy().into(),
                    args: vec![
                        "gateway".into(),
                        "remove".into(),
                        name,
                        "--confirmed".into(),
                    ],
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
        // The editor takes every key, Ctrl-C (copy) included.
        if self.editing() {
            if let Some(m) = self.modal.take() {
                self.modal_key(m, key);
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('p')
            && self.logview.is_none()
            && self.modal.is_none()
        {
            self.modal = Some(Modal::Palette {
                query: String::new(),
                cursor: 0,
            });
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
            KeyCode::Esc if self.toast().is_some() => {
                self.dismiss_toast();
            }
            KeyCode::Char('!') => self.show_alerts(None),
            KeyCode::Char('?') => self.modal = Some(Modal::Help { scroll: 0 }),
            KeyCode::Char(':') => {
                self.modal = Some(Modal::Palette {
                    query: String::new(),
                    cursor: 0,
                })
            }
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
        if self.editing() {
            if let Some(m) = self.modal.take() {
                self.modal_key(m, key);
            }
            return;
        }
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
                self.toast = None;
                let items = vec![
                    "Back to the session".into(),
                    "Providers and their permissions".into(),
                    "Blocked requests".into(),
                    if self.watch.auto_any_on(&name) {
                        "Auto-allow web reads by any program: on (Enter turns it off)".into()
                    } else {
                        "Auto-allow web reads by any program: off (Enter turns it on)".into()
                    },
                    "Open the toolchain hosts in this sandbox (choose groups)".into(),
                    format!("Port forwards ({})", self.ports.forwards(&name).len()),
                    format!("Alerts ({})", self.visible_alerts().len()),
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
                if code == KeyCode::Char('n') {
                    return self.new_file(false);
                }
                if code == KeyCode::Char('e') {
                    if let Some(img) = self.snap.images.get(self.current()) {
                        let rel = img
                            .file
                            .strip_prefix(&self.dir)
                            .unwrap_or(&img.file)
                            .to_string_lossy()
                            .into_owned();
                        self.file_menu(rel);
                    }
                    return;
                }
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
                KeyCode::Char('K' | 'l' | 'd' | 's' | 'S' | 'a' | 'x' | 'p' | 't' | 'T')
                    | KeyCode::Enter
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
                            "{name} has no repo token created by osh."
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
                                    t.shown().1,
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
            KeyCode::Char('T') => {
                let others = self
                    .snap
                    .sandboxes
                    .iter()
                    .map(|s| s.name.clone())
                    .filter(|n| *n != name)
                    .collect();
                self.select(
                    &format!("Transfer the state of {name} to"),
                    others,
                    false,
                    SelKind::TransferTo(name),
                );
            }
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
        if code == KeyCode::Char('a') {
            self.wizard_request = Some(crate::wizard::Mode::Add);
            return;
        }
        let Some(g) = self.snap.gateways.get(self.current()).cloned() else {
            return;
        };
        let info = self.gateway_info(&g.name, &g.endpoint);
        match code {
            KeyCode::Char('d') => {
                let wipeable =
                    info.managed || matches!(g.name.as_str(), "openshell" | "openshell-docker");
                if !wipeable {
                    self.modal = Some(Modal::Info(format!(
                        "{} is not managed by osh; only gateways osh set up can be wiped.",
                        g.name
                    )));
                    return;
                }
                let what = if info.kind == "VM" {
                    "microVMs"
                } else {
                    "containers and volumes"
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
            KeyCode::Char('x') => {
                if !info.managed {
                    self.modal = Some(Modal::Info(format!(
                        "{} was not created by osh, so osh will not remove it.",
                        g.name
                    )));
                    return;
                }
                let extras: Vec<String> = self
                    .registry
                    .gateways
                    .iter()
                    .filter(|x| !x.main)
                    .map(|x| x.name.clone())
                    .collect();
                if info.main && !extras.is_empty() {
                    self.modal = Some(Modal::Info(format!(
                        "{} is the main gateway. Remove {} first.",
                        g.name,
                        extras.join(", ")
                    )));
                    return;
                }
                self.modal = Some(Modal::Confirm {
                    text: format!(
                        "Remove gateway {name}?\n\nIts service is stopped and disabled and it is\n\
                         forgotten by osh and the OpenShell CLI.\n\
                         Its sandboxes' data stays on disk (osh keeps the database).\n\
                         y removes it and keeps the data; D removes it and DELETES the data too.",
                        name = g.name
                    ),
                    kind: ConfKind::RemoveGateway(g.name.clone()),
                });
            }
            KeyCode::Char('R') => {
                let up = g.name == self.snap.gateway && self.snap.gateway_ok;
                self.modal = Some(Modal::Confirm {
                    text: format!(
                        "{} gateway {}?\n\nosh installs its config if that changed, restarts its service and waits for it to answer.{}",
                        if up { "Restart" } else { "Start" },
                        g.name,
                        if up {
                            " Sandboxes on it lose their connections for a moment."
                        } else {
                            ""
                        }
                    ),
                    kind: ConfKind::OshJob(vec![
                        "gateway".into(),
                        "restart".into(),
                        g.name.clone(),
                        "--confirmed".into(),
                    ]),
                });
            }
            KeyCode::Char('l') => self.gateway_log(&g.name),
            KeyCode::Char('e') => self.edit_gateway_config(&g.name),
            KeyCode::Char('V') => {
                if info.kind != "VM" {
                    self.modal = Some(Modal::Info(format!(
                        "{} is not a VM gateway. Its sandboxes take CPU and memory limits one by one, asked when a sandbox is created (n on the Sandboxes tab).",
                        g.name
                    )));
                    return;
                }
                let (cpus, _) = self.vm_size_now();
                self.input(
                    &format!(
                        "vCPUs for every sandbox on {} (now {cpus}; the VM driver has no per-sandbox size)",
                        g.name
                    ),
                    false,
                    InKind::VmCpus(g.name.clone()),
                );
            }
            KeyCode::Enter => {
                let name = g.name.clone();
                self.change_gateway(&name);
            }
            KeyCode::Char('s') => {
                if g.active {
                    self.status = format!("{} is already the default gateway", g.name);
                    return;
                }
                let name = g.name.clone();
                self.status = format!("Making {name} the default gateway...");
                self.bg_msg(move || cli::select_default_gateway(&name));
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
        if code == KeyCode::Char('n') {
            return self.new_file(true);
        }
        if !matches!(code, KeyCode::Char('u' | 'e') | KeyCode::Enter) {
            return;
        }
        let rows = self.profile_rows();
        let Some(row) = rows.get(self.current()) else {
            return;
        };
        let rel = self.profile_rel(&row.id);
        if code == KeyCode::Char('e') {
            return self.file_menu(rel);
        }
        if !row.in_repo {
            self.modal = Some(Modal::Info(format!(
                "There is no {rel} to import from: the gateway has this profile but osh has no file for it.\nPress e to save the gateway's copy as that file, or to delete the profile from the gateway."
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
        // Providers made from this profile, to find the running sandboxes an update reaches.
        let providers: Vec<String> = self
            .snap
            .providers
            .iter()
            .filter(|p| p.ty == id)
            .map(|p| p.name.clone())
            .collect();
        let sandboxes = self.snap.sandboxes.clone();
        self.bg(move || {
            let msg = cli::sync_profile(&dir, &id, None)?;
            let mut running: Vec<String> = Vec::new();
            if msg.starts_with("Updated") {
                for p in &providers {
                    for s in cli::running_with(p, &sandboxes) {
                        if !running.contains(&s) {
                            running.push(s);
                        }
                    }
                }
            }
            Ok(Done {
                msg,
                next: None,
                sync: None,
                gh: (!running.is_empty()).then_some(GhStage::ProfileUpdated {
                    profile: id,
                    sandboxes: running,
                }),
            })
        });
    }

    /// Run `osh ARGS` with the terminal handed over (see `ConfKind::OshJob`).
    fn osh_job(&mut self, args: Vec<String>) {
        let exe = std::env::current_exe().unwrap_or_else(|_| "osh".into());
        self.job = Some(vec![Step {
            prog: exe.to_string_lossy().into(),
            args,
        }]);
    }

    /// The VM size the gateways start microVMs with: the one set here, else the driver's default.
    fn vm_size_now(&self) -> (u32, u32) {
        if cfg!(test) {
            return crate::gwops::VM_DEFAULT_SIZE;
        }
        let p = crate::gateway::Paths::detect();
        crate::gwops::vm_size(&crate::platform::RealHost, &p)
            .unwrap_or(crate::gwops::VM_DEFAULT_SIZE)
    }

    /// Follow a gateway's own service log (journal or launchd/Homebrew log files) in the log view,
    /// which works while the gateway is down, when there is no sandbox to read logs from.
    fn gateway_log(&mut self, name: &str) {
        use crate::platform::Host as _;
        let h = crate::platform::RealHost;
        let p = crate::gateway::Paths::detect();
        let os = if cfg!(target_os = "macos") {
            crate::platform::Os::MacOs
        } else {
            crate::platform::Os::Linux
        };
        let driver = h
            .read(&p.config_home.join("gateway.toml"))
            .and_then(|t| crate::gateway::driver_runtime(&t));
        let spec = match self.registry.get(name) {
            Some(s) => Ok(s.clone()),
            None => crate::gateway::legacy_spec(os, name, driver.or(cli::runtime())),
        };
        match spec {
            Ok(spec) => {
                let (prog, args) = crate::gwops::log_command(os, &spec, &p);
                match LogView::open_command(format!("{name} service"), &prog, args) {
                    Ok(lv) => self.logview = Some(lv),
                    Err(e) => self.status = format!("Error: {e}"),
                }
            }
            Err(_) => {
                self.modal = Some(Modal::Info(format!(
                    "osh does not know how {name} runs (osh did not create it), so it cannot show its service log."
                )))
            }
        }
    }

    /// Open the config osh keeps for a gateway in the editor; on close, offer the restart that
    /// applies it.
    fn edit_gateway_config(&mut self, name: &str) {
        let os = if cfg!(target_os = "macos") {
            crate::platform::Os::MacOs
        } else {
            crate::platform::Os::Linux
        };
        let rel = match self.registry.get(name) {
            Some(spec) if crate::gwops::packaged(spec, os) => {
                self.modal = Some(Modal::Info(format!(
                    "osh writes the config of {name} itself (OpenShell's own gateway.toml) and puts it back when it differs, so an edit would not last. What osh lets you change: the VM size of a VM gateway (V)."
                )));
                return;
            }
            Some(_) => format!("gateways/{name}.toml"),
            None if name == "openshell" => {
                crate::gateway::config_name_for(os, cli::runtime()).to_string()
            }
            None if name == "openshell-docker" => "gateway-docker.toml".to_string(),
            None => {
                self.modal = Some(Modal::Info(format!(
                    "osh did not create {name}, so it has no config of it to edit."
                )));
                return;
            }
        };
        let (path, check) = (self.dir.join(&rel), crate::assets::check_for(&rel));
        self.open_editor(&rel, path, None, check, EditThen::Gateway(name.to_string()));
    }

    /// The data-dir path of a profile's file, relative: the one that exists, else `ID.yaml`.
    fn profile_rel(&self, id: &str) -> String {
        let p = cli::profile_path(&self.dir, id);
        format!(
            "providers/{}",
            p.file_name().unwrap_or_default().to_string_lossy()
        )
    }

    fn open_viewer(&mut self, title: &str, text: String, back: Option<Modal>) {
        self.modal = Some(Modal::Viewer {
            title: title.into(),
            text,
            scroll: 0,
            back: back.map(Box::new),
        });
    }

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) {
        // A prompt can open mid-typing: Enter or Esc meant for the shell must not pick an answer.
        // Moving the cursor is harmless, so it is never swallowed (it made `Down` need two presses).
        let fresh = self.prompt_at.is_some_and(|t| t.elapsed() < PROMPT_GRACE);
        if fresh
            && matches!(key.code, KeyCode::Enter | KeyCode::Esc)
            && matches!(
                modal,
                Modal::Info(_)
                    | Modal::Select {
                        kind: SelKind::Blocked { .. } | SelKind::Forward { .. },
                        ..
                    }
            )
        {
            self.modal = Some(modal);
            return;
        }
        match modal {
            Modal::Help { mut scroll } => {
                let cols = crate::ui::tall_cols(modal_screen().0);
                let last =
                    modal_last(crate::ui::wrap_lines(&crate::ui::help_lines(self.tab), cols).len());
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Enter => return,
                    KeyCode::Char('j') | KeyCode::Down => scroll = (scroll + 1).min(last),
                    KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => scroll = (scroll + 10).min(last),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::Char('g') | KeyCode::Home => scroll = 0,
                    KeyCode::Char('G') | KeyCode::End => scroll = last,
                    _ => {}
                }
                self.modal = Some(Modal::Help { scroll });
            }
            Modal::Palette {
                mut query,
                mut cursor,
            } => {
                let hits = crate::palette::filter(self.tab, &query);
                match key.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        if let Some((action, _)) = hits.get(cursor) {
                            match action.cmd {
                                Some(cmd) => self.run_command(cmd),
                                None => self.on_key(KeyEvent::from(action.code)),
                            }
                        }
                        return;
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        cursor = (cursor + 1).min(hits.len().saturating_sub(1))
                    }
                    KeyCode::Up | KeyCode::BackTab => cursor = cursor.saturating_sub(1),
                    KeyCode::Backspace => {
                        query.pop();
                        cursor = 0;
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(c);
                        cursor = 0;
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Palette { query, cursor });
            }
            Modal::Task(t) => {
                if key.code != KeyCode::Esc {
                    self.modal = Some(Modal::Task(t));
                }
            }
            Modal::Info(text) => {
                if key.code == KeyCode::Char('v') {
                    self.open_viewer("Details", text.clone(), Some(Modal::Info(text)));
                }
            }
            Modal::Providers {
                sandbox,
                perms,
                mut scroll,
            } => {
                let cols = crate::ui::tall_cols(modal_screen().0);
                let last =
                    modal_last(crate::ui::wrap_lines(&crate::ui::perm_lines(&perms), cols).len());
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
            Modal::Editor { mut ed, then } => {
                let out = ed.on_key(key);
                self.editor_done(ed, then, out);
            }
            Modal::ExtEdit { run, then } => self.ext_editor_key(run, then, key),
            Modal::Viewer {
                title,
                text,
                mut scroll,
                back,
            } => {
                // The viewer fills the screen minus its border and margin; stop when the last line is at the bottom.
                let cols = crate::ui::tall_cols(modal_screen().0);
                let last = modal_last(crate::ui::wrap_text(&text, cols).len());
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                        self.modal = back.map(|b| *b);
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
                KeyCode::Char('D') if matches!(kind, ConfKind::RemoveGateway(_)) => {
                    let ConfKind::RemoveGateway(name) = kind else {
                        return;
                    };
                    self.modal = Some(Modal::Confirm {
                        text: format!(
                            "Remove gateway {name} AND DELETE its data?\n\nEvery sandbox on it, with its files, and its stored provider credentials are deleted for good. This cannot be undone."
                        ),
                        kind: ConfKind::OshJob(vec![
                            "gateway".into(),
                            "remove".into(),
                            name,
                            "--delete-data".into(),
                            "--confirmed".into(),
                        ]),
                    });
                }
                KeyCode::Char('v') => {
                    let full = match &kind {
                        ConfKind::SyncGen { yaml, .. } => yaml.clone(),
                        _ => text.clone(),
                    };
                    self.open_viewer("Details", full, Some(Modal::Confirm { text, kind }));
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
                KeyCode::Esc if matches!(kind, SelKind::Forward { .. }) => {
                    self.on_select(kind, vec!["Not now".into()])
                }
                KeyCode::Esc => self.cleanup_sync_stage(),
                KeyCode::Enter if items[cursor] == EDIT_TOOLCHAINS_ITEM => {
                    self.on_select(kind, vec![EDIT_TOOLCHAINS_ITEM.into()])
                }
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

    use crate::gwmodel::{GatewaySpec, GwKind};

    #[test]
    fn a_pending_allow_counts_seconds_and_the_result_stays_visible_longer() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut a = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        let ago = |s: u64| Instant::now() - Duration::from_secs(s);
        a.status = "Allowing curl to reach example.com:443 in sb...".into();
        a.status_at = Some((a.status.clone(), ago(1)));
        assert_eq!(a.busy_for(), None);
        a.status_at = Some((a.status.clone(), ago(12)));
        assert!(a.busy_for().unwrap() >= Duration::from_secs(12));
        a.status =
            "Allowed curl to reach example.com:443 (all methods) in sb; it is live, ask again"
                .into();
        a.status_at = Some((a.status.clone(), ago(10)));
        assert!(a.flash().is_some());
        a.status = "Ignored curl -> example.com:443 this time".into();
        a.status_at = Some((a.status.clone(), ago(10)));
        assert!(a.flash().is_none());
    }

    fn gw(name: &str, port: u16) -> cli::Gateway {
        cli::Gateway {
            name: name.into(),
            endpoint: format!("https://localhost:{port}"),
            ..Default::default()
        }
    }

    fn on_gateways(app: &mut App, gws: Vec<cli::Gateway>) {
        app.tab = Tab::Gateways;
        app.snap.gateways = gws;
        app.cursor[4] = 0;
    }

    fn spec(kind: GwKind, port: u16, main: bool) -> GatewaySpec {
        GatewaySpec {
            name: kind.name().into(),
            kind,
            port,
            main,
        }
    }

    #[test]
    fn the_out_of_date_banner_is_not_shown_when_osh_made_the_main() {
        let mut reg = crate::gwmodel::Registry::default();
        assert!(
            config_stale_with(&reg, true),
            "nothing registered: the older rule applies"
        );
        assert!(!config_stale_with(&reg, false));
        reg.add(spec(GwKind::Podman, 17670, true));
        assert!(
            !config_stale_with(&reg, true),
            "the main's config is osh's own, not the built-in one"
        );
    }

    #[test]
    fn wizard_request_is_set_by_a_on_the_gateways_tab() {
        let mut app = app();
        on_gateways(&mut app, vec![gw("openshell", 17670)]);
        app.gateway_key(KeyCode::Char('a'));
        assert_eq!(app.wizard_request, Some(crate::wizard::Mode::Add));
        // Even with no gateway at all, adding must work.
        let mut app = self::app();
        on_gateways(&mut app, vec![]);
        app.gateway_key(KeyCode::Char('a'));
        assert_eq!(app.wizard_request, Some(crate::wizard::Mode::Add));
    }

    #[test]
    fn x_on_a_legacy_gateway_explains_it_is_not_managed() {
        let mut app = app();
        on_gateways(&mut app, vec![gw("openshell", 17670)]);
        app.gateway_key(KeyCode::Char('x'));
        match &app.modal {
            Some(Modal::Info(t)) => assert!(t.contains("not created by osh"), "{t}"),
            _ => panic!("expected an explanation"),
        }
    }

    #[test]
    fn x_on_an_extra_asks_for_confirmation_before_removing() {
        let mut app = app();
        app.registry.add(spec(GwKind::Docker, 17670, true));
        app.registry.add(spec(GwKind::Podman, 17671, false));
        on_gateways(
            &mut app,
            vec![gw("openshell-docker", 17670), gw("openshell-podman", 17671)],
        );
        app.cursor[4] = 1;
        app.gateway_key(KeyCode::Char('x'));
        match &app.modal {
            Some(Modal::Confirm {
                text,
                kind: ConfKind::RemoveGateway(n),
            }) => {
                assert_eq!(n, "openshell-podman");
                assert!(
                    text.contains("data stays") || text.contains("keeps"),
                    "{text}"
                );
            }
            _ => panic!("expected a confirmation"),
        }
    }

    #[test]
    fn x_on_the_main_gateway_with_extras_says_to_remove_them_first() {
        let mut app = app();
        app.registry.add(spec(GwKind::Docker, 17670, true));
        app.registry.add(spec(GwKind::Podman, 17671, false));
        on_gateways(
            &mut app,
            vec![gw("openshell-docker", 17670), gw("openshell-podman", 17671)],
        );
        app.gateway_key(KeyCode::Char('x'));
        match &app.modal {
            Some(Modal::Info(t)) => {
                assert!(t.contains("openshell-podman") && t.contains("first"), "{t}")
            }
            _ => panic!("expected an explanation"),
        }
    }

    #[test]
    fn reset_is_offered_for_registry_and_legacy_gateways_only() {
        for name in ["openshell", "openshell-docker", "openshell-podman"] {
            let mut app = app();
            app.registry.add(spec(GwKind::Podman, 17671, false));
            on_gateways(&mut app, vec![gw(name, 17670)]);
            app.gateway_key(KeyCode::Char('d'));
            assert!(
                matches!(&app.modal, Some(Modal::Confirm { kind: ConfKind::ResetGateway(n), .. }) if n == name),
                "{name}"
            );
        }
        let mut app = app();
        on_gateways(&mut app, vec![gw("somebody-elses", 9000)]);
        app.gateway_key(KeyCode::Char('d'));
        assert!(matches!(&app.modal, Some(Modal::Info(t)) if t.contains("not managed")));
    }

    #[test]
    fn s_on_the_default_gateway_says_so_without_running_anything() {
        let mut app = app();
        app.snap.gateways = vec![cli::Gateway {
            name: "openshell-docker".into(),
            endpoint: "e".into(),
            active: true,
        }];
        app.gateway_key(KeyCode::Char('s'));
        assert_eq!(
            app.status,
            "openshell-docker is already the default gateway"
        );
    }

    #[test]
    fn gateway_rows_show_kind_port_and_main() {
        let mut app = app();
        app.registry.add(spec(GwKind::Docker, 17670, true));
        app.registry.add(spec(GwKind::Podman, 17671, false));
        let docker = app.gateway_info("openshell-docker", "https://localhost:17670");
        assert_eq!(
            (docker.kind.as_str(), docker.port.as_str(), docker.main),
            ("Docker", "17670", true)
        );
        let podman = app.gateway_info("openshell-podman", "https://localhost:17671");
        assert_eq!(
            (podman.kind.as_str(), podman.port.as_str(), podman.main),
            ("Podman", "17671", false)
        );
        let mut legacy = self::app();
        legacy.legacy_kind = Some(GwKind::Vm);
        let vm = legacy.gateway_info("openshell", "https://127.0.0.1:17670");
        assert_eq!(
            (vm.kind.as_str(), vm.port.as_str(), vm.main),
            ("VM", "17670", true)
        );
        let other = legacy.gateway_info("custom", "https://example.com");
        assert_eq!(other.kind, "?");
    }

    #[test]
    fn footer_keys_match_the_readme_table() {
        let footer = crate::ui::footer_help(Tab::Gateways);
        let readme = include_str!("../../README.md");
        let row = readme
            .lines()
            .find(|l| l.starts_with("| Gateways"))
            .expect("README Gateways row");
        for key in ["a", "x", "d"] {
            assert!(
                footer.contains(&format!("{key} ")),
                "footer lacks {key}: {footer}"
            );
            assert!(
                row.contains(&format!("(`{key}`)")),
                "README Gateways row lacks ({key}): {row}"
            );
        }
    }

    /// The first word of each footer entry (`Enter`, `a/x`, `Tab/1-5`), split at `/`.
    fn footer_keys(help: &str) -> Vec<String> {
        help.split("  ")
            .filter_map(|e| e.split_whitespace().next())
            .flat_map(|k| k.split('/'))
            .filter(|k| !k.is_empty())
            .map(String::from)
            .collect()
    }

    /// Every key in the footer must be in tests/e2e/actions.txt with a scenario that exists (or a
    /// stated reason it cannot run), so a new key cannot ship without an end-to-end check.
    #[test]
    fn every_footer_key_has_an_end_to_end_scenario() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/e2e");
        let manifest = std::fs::read_to_string(root.join("actions.txt")).expect("actions.txt");
        let scenarios = std::fs::read_to_string(root.join("scenarios.py")).expect("scenarios.py");
        let rows: Vec<Vec<&str>> = manifest
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| l.split('|').map(str::trim).collect())
            .collect();
        for r in &rows {
            assert_eq!(r.len(), 3, "surface | key | scenario: {r:?}");
            if let Some(why) = r[2].strip_prefix("SKIP:") {
                assert!(why.trim().len() > 15, "give a real reason: {r:?}");
            } else {
                assert!(
                    scenarios.contains(&format!("@scenario(\"{}\")", r[2])),
                    "no scenario {} in scenarios.py for {r:?}",
                    r[2]
                );
            }
        }
        let has = |surface: &str, key: &str| rows.iter().any(|r| r[0] == surface && r[1] == key);
        for (tab, name) in TABS {
            for k in footer_keys(crate::ui::footer_help(tab)) {
                assert!(
                    has(name, &k),
                    "{name} footer key {k:?} is not in actions.txt"
                );
            }
        }
        for k in footer_keys(crate::ui::GLOBAL_HELP) {
            assert!(has("global", &k), "global key {k:?} is not in actions.txt");
        }
    }

    /// A dashboard whose data dir is an empty temporary folder, so nothing a test does writes
    /// into the checkout.
    fn app() -> App {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let dir = std::env::temp_dir().join(format!("osh-app-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        App::new(dir, Arc::new(Mutex::new(None)), wake, done)
    }

    fn alert(title: &str) -> crate::alerts::Alert {
        crate::alerts::Alert {
            sev: crate::alerts::Severity::High,
            title: title.into(),
            detail: "look at it".into(),
            sandbox: None,
        }
    }

    #[test]
    fn an_alert_shows_for_five_seconds_and_stays_in_the_log() {
        let mut a = app();
        assert!(a.toast().is_none());
        a.push_alert(alert(".envrc appeared in app"));
        assert_eq!(a.toast().unwrap().title, ".envrc appeared in app");
        a.toast.as_mut().unwrap().1 = Instant::now() - TOAST_FOR - Duration::from_millis(10);
        assert!(a.toast().is_none(), "gone after 5 s");
        assert_eq!(a.alerts.len(), 1, "but still listed");
    }

    #[test]
    fn esc_dismisses_the_alert_and_bang_lists_them() {
        let mut a = app();
        a.push_alert(alert("first"));
        a.push_alert(alert("second"));
        assert_eq!(a.toast().unwrap().title, "second", "the newest shows");
        a.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(a.toast().is_none());
        a.on_key(KeyEvent::from(KeyCode::Char('!')));
        let Some(Modal::Viewer { title, text, .. }) = &a.modal else {
            panic!("no list");
        };
        assert!(title.contains("Alerts (2)"), "{title}");
        assert!(
            text.find("second").unwrap() < text.find("first").unwrap(),
            "newest first"
        );
        assert!(text.contains("[HIGH]") && text.contains("look at it"));
    }

    #[test]
    fn closing_the_alert_list_leaves_no_box_behind() {
        let mut a = app();
        a.push_alert(alert("x"));
        a.on_key(KeyEvent::from(KeyCode::Char('!')));
        assert!(matches!(a.modal, Some(Modal::Viewer { .. })));
        a.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(
            a.modal.is_none(),
            "found by the end-to-end suite: an empty Info box stayed"
        );
    }

    #[test]
    fn the_alert_list_says_so_when_empty() {
        let mut a = app();
        a.on_key(KeyEvent::from(KeyCode::Char('!')));
        let Some(Modal::Viewer { text, .. }) = &a.modal else {
            panic!("no list");
        };
        assert!(text.contains("Nothing suspicious"));
    }

    #[test]
    fn sharing_the_home_folder_raises_a_high_alert_and_warns_in_the_question() {
        let mut a = app();
        let home = std::env::var("HOME").unwrap();
        a.ask_access(Plan {
            share: home.clone(),
            ..Plan::default()
        });
        let t = a.toast().expect("alert");
        assert!(t.title.contains("risky") && t.sev == crate::alerts::Severity::High);
        let Some(Modal::Select { title, .. }) = &a.modal else {
            panic!("no question");
        };
        assert!(title.contains("RISKY"), "{title}");
        let mut b = app();
        b.ask_access(Plan {
            share: format!("{home}/code/app"),
            ..Plan::default()
        });
        assert!(
            b.toast().is_none() && b.alerts.is_empty(),
            "a project folder is fine"
        );
    }

    #[test]
    fn a_prompt_moves_on_the_first_down_but_ignores_enter_while_fresh() {
        let mut a = app();
        let d = watch::parse(
            "[1] [sandbox] [OCSF ] [ocsf] NET:OPEN [MED] DENIED /usr/bin/curl(0) -> example.com:443 [reason:transparent_tcp_policy_denied]",
        )
        .unwrap();
        a.watch.push("sb", "gw", d);
        a.pump_watch();
        assert!(a.prompt_at.is_some());
        a.on_key(KeyEvent::from(KeyCode::Down));
        let Some(Modal::Select { cursor, .. }) = &a.modal else {
            panic!("no prompt");
        };
        assert_eq!(*cursor, 1, "one Down, one step");
        a.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(a.modal, Some(Modal::Select { .. })),
            "Enter inside the grace period is the shell's, not an answer"
        );
    }

    #[test]
    fn a_new_listening_port_is_offered_for_forwarding_and_esc_declines() {
        let none = std::collections::BTreeSet::new();
        let mut a = app();
        a.ports.observe("sb", "gw", &none);
        a.ports
            .observe("sb", "gw", &[8000u16].into_iter().collect());
        a.pump_watch();
        let Some(Modal::Select { title, items, .. }) = &a.modal else {
            panic!("no offer");
        };
        assert!(title.contains("Port 8000") && title.contains("in sb"));
        assert_eq!(items[0], "Forward to localhost:8000");
        a.prompt_at = None;
        a.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(a.modal.is_none() && !a.ports.has_queued(None));
    }

    #[test]
    fn the_port_forwards_list_shows_forwards_and_listeners() {
        use crate::term::Session;
        let mut a = app();
        a.ports.observe("sb", "gw", &Default::default());
        a.ports
            .observe("sb", "gw", &[8000u16, 9000].into_iter().collect());
        a.ports.forwarded("sb", 8000);
        let s = Session::spawn("sb".into(), "gw".into(), "cat", &[], 20, 5).unwrap();
        a.sessions.push(s);
        a.active = Some(0);
        a.show_forwards();
        let Some(Modal::Select { items, .. }) = &a.modal else {
            panic!("no list");
        };
        assert_eq!(
            items,
            &[
                "Stop forwarding localhost:8000",
                "Forward port 9000 (listening in the sandbox)",
                "Forward another port..."
            ]
        );
    }

    #[test]
    fn ignoring_a_port_for_the_session_stops_the_offers() {
        let mut a = app();
        a.answer_forward(
            "sb".into(),
            "gw".into(),
            8000,
            "Ignore this port until the session ends",
        );
        a.ports.observe("sb", "gw", &Default::default());
        a.ports
            .observe("sb", "gw", &[8000u16].into_iter().collect());
        assert!(!a.ports.has_queued(None));
    }

    #[test]
    fn a_shell_sees_only_its_own_sandbox_alerts_and_prompts() {
        use crate::term::Session;
        let mut a = app();
        let scoped = |sb: Option<&str>, t: &str| crate::alerts::Alert {
            sandbox: sb.map(str::to_string),
            ..alert(t)
        };
        a.push_alert(scoped(Some("a"), "in a"));
        a.push_alert(scoped(Some("b"), "in b"));
        a.push_alert(scoped(None, "unknown"));
        assert_eq!(a.visible_alerts().len(), 3, "dashboard sees all");
        let s = Session::spawn("a".into(), String::new(), "cat", &[], 20, 5).unwrap();
        a.sessions.push(s);
        a.active = Some(0);
        let v = a.visible_alerts();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].title, "in a");
        a.toast = Some((scoped(Some("b"), "in b"), Instant::now()));
        assert!(a.toast().is_none(), "b's toast stays out of a's shell");
        let text = a.alerts_text();
        assert!(text.contains("in a") && !text.contains("in b") && !text.contains("unknown"));
        // A denial in b waits while a's shell has focus, then shows on the dashboard.
        let d = watch::parse(
            "[1] [sandbox] [OCSF ] [ocsf] NET:OPEN [MED] DENIED /usr/bin/curl(0) -> example.com:443 [reason:transparent_tcp_policy_denied]",
        )
        .unwrap();
        a.watch.push("b", "gw", d);
        a.pump_watch();
        assert!(a.modal.is_none(), "no popup from b inside a");
        a.active = None;
        a.pump_watch();
        assert!(matches!(a.modal, Some(Modal::Select { .. })));
    }

    #[test]
    fn host_status_follows_the_most_urgent_shell_and_is_sent_once() {
        use crate::term::Session;
        let mut a = app();
        let mut s1 = Session::spawn("a".into(), String::new(), "cat", &[], 20, 5).unwrap();
        let mut s2 = Session::spawn("b".into(), String::new(), "cat", &[], 20, 5).unwrap();
        assert_eq!(a.host_status_change(), None);
        s1.vt.feed("\x1b]0;osh-status:🤖\x07".as_bytes());
        a.sessions.push(s1);
        assert_eq!(a.host_status_change(), Some("working"));
        assert_eq!(a.host_status_change(), None);
        s2.vt.feed("\x1b]0;osh-status:💬\x07".as_bytes());
        a.sessions.push(s2);
        assert_eq!(a.host_status_change(), Some("waiting"));
        a.sessions.remove(1);
        assert_eq!(a.host_status_change(), Some("working"));
        a.sessions.clear();
        assert_eq!(a.host_status_change(), Some("clear"));
    }

    #[test]
    fn transfer_offers_the_other_sandboxes_and_asks_before_copying() {
        let mut a = app();
        a.snap.sandboxes = ["one", "two", "three"]
            .iter()
            .map(|n| cli::Sandbox {
                name: n.to_string(),
                ..cli::Sandbox::default()
            })
            .collect();
        a.cursor[0] = 1;
        a.sandbox_key(KeyCode::Char('T'));
        match &a.modal {
            Some(Modal::Select {
                items,
                kind: SelKind::TransferTo(from),
                ..
            }) => {
                assert_eq!(from, "two");
                assert_eq!(items, &["one".to_string(), "three".to_string()]);
            }
            _ => panic!("expected the target list"),
        }
        a.on_select(SelKind::TransferTo("two".into()), vec!["three".into()]);
        match &a.modal {
            Some(Modal::Confirm {
                text,
                kind: ConfKind::Transfer { from, to },
            }) => {
                assert_eq!((from.as_str(), to.as_str()), ("two", "three"));
                assert!(text.contains("overwritten"), "{text}");
            }
            _ => panic!("expected a confirmation"),
        }
        a.snap.sandboxes.truncate(1);
        a.cursor[0] = 0;
        a.sandbox_key(KeyCode::Char('T'));
        assert!(matches!(&a.modal, Some(Modal::Info(t)) if t.contains("Nothing to choose")));
    }

    #[test]
    fn only_a_detached_create_copies_the_host_tmux_config_afterwards() {
        let mut a = app();
        a.snap.sandboxes = vec![cli::Sandbox {
            name: "old".into(),
            ..cli::Sandbox::default()
        }];
        a.start_create(&Plan::default());
        assert_eq!(
            a.tmux_after_create,
            Some((String::new(), vec!["old".to_string()]))
        );
        a.tmux_after_create = None;
        a.start_create(&Plan {
            command: "true".into(),
            ..Plan::default()
        });
        assert_eq!(a.tmux_after_create, None);
    }

    #[test]
    fn a_new_sandbox_asks_for_toolchain_groups_and_can_save_them_as_defaults() {
        let dir = std::env::temp_dir().join(format!("osh-tc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("toolchains.toml"),
            "# keep\n[a]\nenabled = true\nhosts = [\"a.example\"]\n[b]\nenabled = false\nhosts = [\"b.example\"]\n",
        )
        .unwrap();
        let mut app = app();
        app.dir = dir.clone();
        app.proceed(Plan::default());
        let Some(Modal::Select {
            kind: SelKind::Toolchains(plan),
            items,
            marked,
            ..
        }) = app.modal.take()
        else {
            panic!("expected the toolchain picker");
        };
        assert_eq!(
            marked,
            vec![true, false, false, false],
            "marks start at the file"
        );
        assert_eq!(items[items.len() - 2], SAVE_TOOLCHAINS_ITEM);
        assert_eq!(items.last().map(String::as_str), Some(EDIT_TOOLCHAINS_ITEM));
        app.on_select(
            SelKind::Toolchains(plan),
            vec![items[1].clone(), SAVE_TOOLCHAINS_ITEM.into()],
        );
        let saved = std::fs::read_to_string(dir.join("toolchains.toml")).unwrap();
        assert!(saved.contains("# keep"));
        let groups = preload::parse(&saved).unwrap();
        assert!(!groups[0].on && groups[1].on, "b is the new default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn toolchain_dir(tag: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("osh-tce-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("toolchains.toml"), text).unwrap();
        dir
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    fn ctrl_key(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            press(
                app,
                if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                },
            );
        }
    }

    /// Move a select list's cursor to `item` and press Enter.
    fn enter_on(app: &mut App, item: &str) {
        let Some(Modal::Select { items, .. }) = &app.modal else {
            panic!("expected a list");
        };
        let at = items
            .iter()
            .position(|i| i == item)
            .expect("item in the list");
        for _ in 0..at {
            press(app, KeyCode::Down);
        }
        press(app, KeyCode::Enter);
    }

    #[test]
    fn the_new_sandbox_picker_edits_toolchains_toml_and_comes_back_with_the_new_group() {
        let dir = toolchain_dir("new", "[a]\nenabled = true\nhosts = [\"a.example\"]\n");
        let mut app = app();
        app.dir = dir.clone();
        app.proceed(Plan {
            name: "keep-me".into(),
            ..Plan::default()
        });
        enter_on(&mut app, EDIT_TOOLCHAINS_ITEM);
        assert!(
            matches!(
                &app.modal,
                Some(Modal::Editor { then: EditThen::NewSandbox(p), .. }) if p.name == "keep-me"
            ),
            "Enter on the line opens the editor, even with nothing marked"
        );
        ctrl_key(&mut app, 'e');
        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        type_text(
            &mut app,
            "\n[java]\nenabled = false\nhosts = [\"repo.maven.apache.org\"]",
        );
        ctrl_key(&mut app, 's');
        assert_eq!(app.status, "Saved toolchains.toml");
        assert!(app.editing(), "saving keeps the editor open");
        press(&mut app, KeyCode::Esc);
        let items = select_items(&app).1;
        assert!(
            select_items(&app)
                .1
                .iter()
                .any(|i| preload::group_of(i) == "java"),
            "the picker is back with the new group: {items:?}"
        );
        let Some(Modal::Select {
            kind: SelKind::Toolchains(plan),
            ..
        }) = &app.modal
        else {
            panic!("the new-sandbox picker");
        };
        assert_eq!(plan.name, "keep-me", "the plan survives the edit");
        let saved = std::fs::read_to_string(dir.join("toolchains.toml")).unwrap();
        assert!(
            saved.ends_with("hosts = [\"repo.maven.apache.org\"]\n"),
            "{saved}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_editor_refuses_to_save_broken_toml_and_asks_before_discarding() {
        let original = "[a]\nenabled = true\n";
        let dir = toolchain_dir("bad", original);
        let mut app = app();
        app.dir = dir.clone();
        app.pick_toolchains_running("sb".into(), "gw".into());
        enter_on(&mut app, EDIT_TOOLCHAINS_ITEM);
        type_text(&mut app, "[broken");
        ctrl_key(&mut app, 's');
        let Some(Modal::Editor { ed, .. }) = &app.modal else {
            panic!("the editor stays open");
        };
        let (note, err) = ed.note().unwrap();
        assert!(
            *err && note.starts_with("Not saved: TOML parse error"),
            "{note}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("toolchains.toml")).unwrap(),
            original
        );
        // Ctrl-C copies in the editor; it does not quit osh.
        ctrl_key(&mut app, 'c');
        assert!(!app.quit && app.editing());
        // Closing asks; s tries to save again and stays because the text is still broken.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('s'));
        assert!(app.editing());
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('d'));
        assert!(!app.editing(), "d discards");
        assert!(matches!(
            &app.modal,
            Some(Modal::Select { kind: SelKind::ToolchainsRunning(n, g), .. }) if n == "sb" && g == "gw"
        ));
        assert_eq!(
            std::fs::read_to_string(dir.join("toolchains.toml")).unwrap(),
            original
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The user's editor for the rest of this test thread (`None`: none can be found).
    fn fake_editor(argv: Option<&[&str]>) {
        crate::exted::FAKE
            .with(|f| *f.borrow_mut() = argv.map(|a| a.iter().map(|s| s.to_string()).collect()));
    }

    /// A shell script as the user's editor; the file to edit is `$1`.
    fn sh_editor(script: &str) {
        fake_editor(Some(&["sh", "-c", script, "fake-editor"]));
    }

    /// Pump until the user's editor has exited and osh has acted on what it wrote.
    fn wait_editor(app: &mut App) {
        let end = Instant::now() + Duration::from_secs(10);
        while app.ext_editing() {
            assert!(Instant::now() < end, "the editor never exited");
            app.pump_sessions();
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn ext_run(app: &App) -> &crate::exted::Run {
        match &app.modal {
            Some(Modal::ExtEdit { run, .. }) => run,
            _ => panic!("expected the user's editor"),
        }
    }

    #[test]
    fn the_users_editor_edits_toolchains_and_the_picker_follows() {
        let dir = toolchain_dir("ext", "[a]\nenabled = true\nhosts = [\"a.example\"]\n");
        let mut app = app();
        app.dir = dir.clone();
        sh_editor(r#"printf '[b]\nenabled = true\nhosts = ["b.example"]\n' > "$1""#);
        app.pick_toolchains_running("sb".into(), "gw".into());
        enter_on(&mut app, EDIT_TOOLCHAINS_ITEM);
        assert!(app.ext_editing() && app.editing());
        let copy = ext_run(&app).scratch.file.clone();
        assert_eq!(copy.file_name().unwrap(), "toolchains.toml");
        assert!(app.status.contains("in sh (test)"), "{}", app.status);
        wait_editor(&mut app);
        assert_eq!(
            std::fs::read_to_string(dir.join("toolchains.toml")).unwrap(),
            "[b]\nenabled = true\nhosts = [\"b.example\"]\n"
        );
        assert_eq!(app.status, "Saved toolchains.toml (edited in sh)");
        let Some(Modal::Select {
            items,
            kind: SelKind::ToolchainsRunning(..),
            ..
        }) = &app.modal
        else {
            panic!("back to the picker");
        };
        assert!(items.iter().any(|i| i.starts_with("b")), "{items:?}");
        assert!(
            !copy.parent().unwrap().exists(),
            "the temporary copy is gone"
        );
        fake_editor(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_edit_reopens_the_editor_with_it_or_is_discarded() {
        let original = "[a]\nenabled = true\n";
        let dir = toolchain_dir("extbad", original);
        let mut app = app();
        app.dir = dir.clone();
        // Breaks the file; run again on its own broken text, fixes it.
        sh_editor(
            r#"if grep -q broken "$1"; then printf '[c]\nhosts = ["c.example"]\n' > "$1"; else printf '[broken\n' > "$1"; fi"#,
        );
        app.edit_toolchains(EditThen::Nothing);
        let copy = ext_run(&app).scratch.file.clone();
        wait_editor(&mut app);
        assert!(app.editing() && !app.ext_editing(), "osh asks what to do");
        assert!(
            ext_run(&app)
                .error
                .as_deref()
                .unwrap()
                .starts_with("TOML parse error"),
            "{:?}",
            ext_run(&app).error
        );
        assert!(
            app.status.starts_with("Not saved: TOML parse error"),
            "{}",
            app.status
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("toolchains.toml")).unwrap(),
            original
        );
        // Other keys do nothing; r reopens the same copy, which still holds the edit.
        press(&mut app, KeyCode::Char('x'));
        assert!(app.editing());
        press(&mut app, KeyCode::Char('r'));
        assert!(app.ext_editing());
        assert_eq!(ext_run(&app).scratch.file, copy, "the same copy");
        wait_editor(&mut app);
        assert_eq!(
            std::fs::read_to_string(dir.join("toolchains.toml")).unwrap(),
            "[c]\nhosts = [\"c.example\"]\n"
        );
        assert!(app.modal.is_none());
        // Broken again, and this time discarded.
        sh_editor(r#"printf '[broken\n' > "$1""#);
        app.edit_toolchains(EditThen::Nothing);
        let copy = ext_run(&app).scratch.file.clone();
        wait_editor(&mut app);
        press(&mut app, KeyCode::Char('d'));
        assert!(!app.editing());
        assert_eq!(
            app.status,
            "Discarded the edits: toolchains.toml not changed"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("toolchains.toml")).unwrap(),
            "[c]\nhosts = [\"c.example\"]\n"
        );
        assert!(
            !copy.parent().unwrap().exists(),
            "the temporary copy is gone"
        );
        fake_editor(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quitting_without_a_change_or_a_failing_editor_writes_nothing() {
        let dir = toolchain_dir("extcancel", "[a]\n");
        let mut app = app();
        app.dir = dir.clone();
        sh_editor("true");
        app.edit_toolchains(EditThen::Nothing);
        wait_editor(&mut app);
        assert_eq!(app.status, "toolchains.toml unchanged: nothing written");
        sh_editor(r#"printf '[z]\n' > "$1"; exit 1"#);
        app.edit_toolchains(EditThen::Nothing);
        wait_editor(&mut app);
        assert!(
            app.status.starts_with("Cancelled: sh exited with an error"),
            "{}",
            app.status
        );
        // Ctrl-C goes to the editor (here it stops it), not to osh.
        sh_editor("sleep 5");
        app.edit_toolchains(EditThen::Nothing);
        ctrl_key(&mut app, 'c');
        assert!(!app.quit);
        wait_editor(&mut app);
        assert!(
            app.status.starts_with("Cancelled: sh exited with an error"),
            "{}",
            app.status
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("toolchains.toml")).unwrap(),
            "[a]\n"
        );
        fake_editor(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_built_in_editor_is_the_fallback_and_can_be_forced_either_way() {
        let dir = toolchain_dir("extpick", "[a]\n");
        let mut app = app();
        app.dir = dir.clone();
        let note = |app: &App| match &app.modal {
            Some(Modal::Editor { ed, .. }) => ed.note().unwrap().0.clone(),
            _ => panic!("expected the built-in editor"),
        };
        fake_editor(None);
        app.edit_toolchains(EditThen::Nothing);
        assert_eq!(note(&app), "built-in editor: no editor in unit tests");
        assert_eq!(app.status, note(&app));
        fake_editor(Some(&["/nonexistent/osh-editor"]));
        app.modal = None;
        app.edit_toolchains(EditThen::Nothing);
        assert!(
            note(&app).starts_with("built-in editor: osh-editor (test) did not start"),
            "{}",
            note(&app)
        );
        // The palette cycles auto, external, built-in.
        app.modal = None;
        app.run_command(crate::palette::Command::EditorMode);
        assert_eq!(crate::exted::load_pref(&dir), crate::exted::Pref::External);
        assert!(
            app.status.starts_with("Editor: always yours"),
            "{}",
            app.status
        );
        app.edit_toolchains(EditThen::Nothing);
        assert!(
            matches!(&app.modal, Some(Modal::Info(t)) if t.starts_with("osh-editor (test) did not start")),
            "forced external does not fall back"
        );
        fake_editor(None);
        app.modal = None;
        app.edit_toolchains(EditThen::Nothing);
        assert!(
            matches!(&app.modal, Some(Modal::Info(t)) if t.starts_with("No editor of your own"))
        );
        app.modal = None;
        app.run_command(crate::palette::Command::EditorMode);
        assert_eq!(crate::exted::load_pref(&dir), crate::exted::Pref::Builtin);
        sh_editor("true");
        app.edit_toolchains(EditThen::Nothing);
        assert_eq!(note(&app), "built-in editor (chosen in the palette)");
        app.modal = None;
        app.run_command(crate::palette::Command::EditorMode);
        assert_eq!(crate::exted::load_pref(&dir), crate::exted::Pref::Auto);
        fake_editor(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_users_editor_fills_the_screen_and_a_failed_check_is_explained() {
        use ratatui::{backend::TestBackend, Terminal};
        let dir = toolchain_dir("extdraw", "[a]\n");
        let mut app = app();
        app.dir = dir.clone();
        let screen = |app: &App| {
            let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
            term.draw(|f| crate::ui::draw(f, app)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..24)
                .map(|y| {
                    (0..100)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        sh_editor(r#"echo HELLO-EDITOR; read x; printf '[broken\n' > "$1""#);
        app.edit_toolchains(EditThen::Nothing);
        let end = Instant::now() + Duration::from_secs(10);
        while !screen(&app)[0].contains("HELLO-EDITOR") {
            assert!(Instant::now() < end, "{:?}", screen(&app));
            app.pump_sessions();
            thread::sleep(Duration::from_millis(10));
        }
        let rows = screen(&app);
        assert!(
            rows[23].contains(" toolchains.toml ") && rows[23].contains("sh (test)"),
            "{}",
            rows[23]
        );
        press(&mut app, KeyCode::Enter);
        wait_editor(&mut app);
        let rows = screen(&app).join("\n");
        assert!(
            rows.contains("toolchains.toml is not valid: not saved"),
            "{rows}"
        );
        assert!(rows.contains("TOML parse error"), "{rows}");
        assert!(rows.contains("Enter/r reopen"), "{rows}");
        press(&mut app, KeyCode::Char('d'));
        fake_editor(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_edit_action_goes_through_the_one_route() {
        // In the source: only `open_editor` starts either editor.
        let src = include_str!("app.rs");
        let code = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
        let start = code.find("    fn open_editor(").unwrap();
        let end = start + 1 + code[start + 1..].find("\n    fn ").unwrap();
        for needle in [
            "editor::Editor::open(",
            "editor::Editor::create(",
            "editor::Editor::new(",
            "exted::Run::start(",
            "Modal::Editor {\n",
            "Modal::ExtEdit {\n",
        ] {
            for (at, _) in code.match_indices(needle) {
                assert!(
                    (start..end).contains(&at),
                    "{needle} outside open_editor at byte {at}"
                );
            }
        }
        // In behaviour: each action opens the user's editor on its file.
        let dir = toolchain_dir("extall", "[a]\n");
        std::fs::write(
            dir.join("gateway-docker.toml"),
            crate::assets::embedded("gateway-docker.toml").unwrap(),
        )
        .unwrap();
        let mut app = app();
        app.dir = dir.clone();
        sh_editor("true");
        let mut opened = Vec::new();
        let mut check = |app: &mut App| {
            opened.push(
                ext_run(app)
                    .scratch
                    .file
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            wait_editor(app);
            app.modal = None;
        };
        app.edit_toolchains(EditThen::Nothing);
        check(&mut app);
        app.edit_file("sandbox-policy.yaml".into());
        check(&mut app);
        app.create_file(true, "e2e-prof");
        check(&mut app);
        app.create_file(false, "foo");
        check(&mut app);
        app.edit_gateway_config("openshell-docker");
        check(&mut app);
        assert_eq!(
            opened,
            [
                "toolchains.toml",
                "sandbox-policy.yaml",
                "e2e-prof.yaml",
                "Dockerfile-foo",
                "gateway-docker.toml"
            ]
        );
        assert!(
            !dir.join("providers/e2e-prof.yaml").exists(),
            "an untouched template is not written"
        );
        // A saved gateway config offers the restart, as the built-in editor's save does.
        sh_editor(r#"printf '\n# edited\n' >> "$1""#);
        app.edit_gateway_config("openshell-docker");
        wait_editor(&mut app);
        assert!(
            matches!(&app.modal, Some(Modal::Confirm { text, .. }) if text.starts_with("Restart openshell-docker")),
            "{}",
            app.status
        );
        fake_editor(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_list_offers_the_editor_or_going_on_without_it() {
        let dir = toolchain_dir("fix", "[a\n");
        let mut app = app();
        app.dir = dir.clone();
        app.proceed(Plan::default());
        let Some(Modal::Select { title, items, .. }) = &app.modal else {
            panic!("the fix offer");
        };
        assert!(
            title.starts_with("toolchains.toml is not valid TOML: TOML parse error at line 1"),
            "{title}"
        );
        assert_eq!(items[0], EDIT_TOOLCHAINS_ITEM);
        enter_on(&mut app, "Continue without the toolchain hosts");
        assert!(matches!(
            &app.modal,
            Some(Modal::Input {
                kind: InKind::Name(_),
                ..
            })
        ));
        // Fix it in the editor: the picker follows.
        app.proceed(Plan::default());
        press(&mut app, KeyCode::Enter);
        assert!(app.editing());
        press(&mut app, KeyCode::End);
        type_text(&mut app, "]\nenabled = true\nhosts = [\"a.example\"]");
        ctrl_key(&mut app, 's');
        press(&mut app, KeyCode::Esc);
        assert!(matches!(
            &app.modal,
            Some(Modal::Select {
                kind: SelKind::Toolchains(_),
                ..
            })
        ));
        // A running sandbox with an empty list gets the same offer, with Cancel.
        std::fs::write(dir.join("toolchains.toml"), "# nothing yet\n").unwrap();
        app.modal = None;
        app.pick_toolchains_running("sb".into(), String::new());
        assert_eq!(
            select_items(&app).1,
            vec![EDIT_TOOLCHAINS_ITEM.to_string(), "Cancel".into()]
        );
        enter_on(&mut app, "Cancel");
        assert!(app.modal.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_palette_opens_the_editor_without_a_key_and_pastes_go_into_it() {
        let dir = toolchain_dir("palette", "[a]\nenabled = true\n");
        let mut app = app();
        app.dir = dir.clone();
        press(&mut app, KeyCode::Char(':'));
        type_text(&mut app, "edit toolchains");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            &app.modal,
            Some(Modal::Editor {
                then: EditThen::Nothing,
                ..
            })
        ));
        app.on_paste("# pasted\n".into());
        let Some(Modal::Editor { ed, .. }) = &app.modal else {
            panic!("editor");
        };
        assert!(ed.text().starts_with("# pasted\n[a]"), "{}", ed.text());
        ctrl_key(&mut app, 's');
        press(&mut app, KeyCode::Esc);
        assert!(
            app.modal.is_none(),
            "closing from the palette returns to the dashboard"
        );
        assert!(std::fs::read_to_string(dir.join("toolchains.toml"))
            .unwrap()
            .starts_with("# pasted"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_editor_is_drawn_over_the_whole_screen() {
        use ratatui::{backend::TestBackend, Terminal};
        let dir = toolchain_dir("draw", "[a]\nenabled = true\n");
        let mut app = app();
        app.dir = dir.clone();
        app.edit_toolchains(EditThen::Nothing);
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let row = |y: u16| {
            (0..100)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert!(row(0).contains("Edit toolchains.toml"), "{}", row(0));
        assert!(row(1).contains(" 1 [a]"), "{}", row(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_creation_policy_follows_the_sandboxs_own_toolchain_choice() {
        let dir = std::env::temp_dir().join(format!("osh-tp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sandbox-policy.yaml"), "version: 1\n").unwrap();
        std::fs::write(
            dir.join("toolchains.toml"),
            "[a]\nenabled = true\nhosts = [\"a.example\"]\n[b]\nenabled = false\nhosts = [\"b.example\"]\n",
        )
        .unwrap();
        let mut app = app();
        app.dir = dir.clone();
        let plan = Plan {
            toolchains: Some(vec!["b".into()]),
            ..Plan::default()
        };
        let text = std::fs::read_to_string(app.creation_policy(&plan).0).unwrap();
        assert!(text.contains("b.example") && !text.contains("a.example"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("osh-df-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::assets::materialize(&dir).unwrap();
        dir
    }

    fn items(app: &App) -> Vec<String> {
        match &app.modal {
            Some(Modal::Select { items, .. }) => items.clone(),
            _ => panic!("a list is open"),
        }
    }

    #[test]
    fn data_files_compare_and_restore_an_edited_policy_inside_osh() {
        let dir = data_dir("policy");
        let policy = dir.join("sandbox-policy.yaml");
        let shipped = std::fs::read_to_string(&policy).unwrap();
        std::fs::write(&policy, format!("{shipped}# mine\n")).unwrap();
        let mut a = app();
        a.dir = dir.clone();
        a.run_command(crate::palette::Command::DataFiles);
        let list = items(&a);
        let line = list
            .iter()
            .find(|l| l.starts_with("sandbox-policy.yaml "))
            .unwrap();
        assert!(line.ends_with("edited by you"), "{line}");
        assert!(list.contains(&NEW_PROFILE_ITEM.to_string()));
        a.modal = None;
        a.on_select(
            SelKind::DataFiles(vec!["sandbox-policy.yaml".into()]),
            vec![line.clone()],
        );
        let menu = items(&a);
        assert!(menu.contains(&FA_DIFF.into()) && menu.contains(&FA_RESTORE.into()));
        assert!(
            !menu.contains(&FA_DELETE.into()),
            "a file osh needs is not deleted"
        );
        a.modal = None;
        a.on_select(
            SelKind::DataFile("sandbox-policy.yaml".into()),
            vec![FA_DIFF.into()],
        );
        let Some(Modal::Viewer { text, back, .. }) = &a.modal else {
            panic!("the diff is shown");
        };
        assert!(text.contains("+ # mine"), "{text}");
        assert!(back.is_some(), "closing it goes back to the file's menu");
        a.modal = None;
        a.on_select(
            SelKind::DataFile("sandbox-policy.yaml".into()),
            vec![FA_RESTORE.into()],
        );
        assert!(matches!(
            a.modal,
            Some(Modal::Confirm {
                kind: ConfKind::RestoreFile(_),
                ..
            })
        ));
        press(&mut a, KeyCode::Char('y'));
        assert_eq!(std::fs::read_to_string(&policy).unwrap(), shipped);
        assert!(
            std::fs::read_to_string(dir.join("sandbox-policy.yaml.osh-bak"))
                .unwrap()
                .ends_with("# mine\n")
        );
        assert!(a
            .status
            .starts_with("Restored the shipped sandbox-policy.yaml"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_profile_starts_from_a_template_in_the_editor_and_bad_names_are_refused() {
        let dir = data_dir("newprof");
        let mut a = app();
        a.dir = dir.clone();
        a.tab = Tab::Profiles;
        press(&mut a, KeyCode::Char('n'));
        assert!(matches!(
            a.modal,
            Some(Modal::Input {
                kind: InKind::NewFile(true),
                ..
            })
        ));
        a.modal = None;
        a.on_input(InKind::NewFile(true), "a/b".into());
        assert!(matches!(&a.modal, Some(Modal::Info(t)) if t.contains("letters, digits")));
        a.on_input(InKind::NewFile(true), "mine".into());
        let Some(Modal::Editor { ed, .. }) = &mut a.modal else {
            panic!("the editor opens");
        };
        assert!(ed.text().contains("id: mine") && ed.modified());
        assert!(!dir.join("providers/mine.yaml").exists());
        ed.save().unwrap();
        assert!(dir.join("providers/mine.yaml").is_file());
        // Closing it shows the new file's own actions.
        press(&mut a, KeyCode::Esc);
        assert!(items(&a).contains(&FA_DELETE.to_string()));
        a.modal = None;
        a.on_select(
            SelKind::DataFile("providers/mine.yaml".into()),
            vec![FA_DELETE.into()],
        );
        press(&mut a, KeyCode::Char('y'));
        assert!(!dir.join("providers/mine.yaml").exists());
        // A Dockerfile starts from pi's and is checked for a FROM line.
        a.on_input(InKind::NewFile(false), "x".into());
        let Some(Modal::Editor { ed, .. }) = &a.modal else {
            panic!("the editor opens");
        };
        assert!(ed.text().contains("FROM ") && ed.path.ends_with("images/Dockerfile-x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_profile_only_on_the_gateway_can_be_saved_as_a_file_or_deleted_from_it() {
        let dir = data_dir("gwonly");
        let mut a = app();
        a.dir = dir.clone();
        a.tab = Tab::Profiles;
        a.snap.profiles = vec![cli::Profile {
            id: "zz-only".into(),
            version: 1,
        }];
        a.cursor[2] = a
            .profile_rows()
            .iter()
            .position(|r| r.id == "zz-only")
            .unwrap();
        press(&mut a, KeyCode::Char('u'));
        assert!(matches!(&a.modal, Some(Modal::Info(t)) if t.contains("Press e")));
        a.modal = None;
        press(&mut a, KeyCode::Char('e'));
        let menu = items(&a);
        assert!(menu.contains(&FA_GW_SAVE.into()) && menu.contains(&FA_GW_DELETE.into()));
        assert!(!menu.contains(&FA_VIEW.into()), "no file to view yet");
        a.modal = None;
        a.on_select(
            SelKind::DataFile("providers/zz-only.yaml".into()),
            vec![FA_GW_DELETE.into()],
        );
        assert!(matches!(
            a.modal,
            Some(Modal::Confirm {
                kind: ConfKind::DeleteProfile(_),
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_policy_falls_back_to_the_shipped_one_and_a_bad_block_is_explained() {
        let dir = std::env::temp_dir().join(format!("osh-tpm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("toolchains.toml"),
            "[a]\nenabled = true\nhosts = [\"a.example\"]\n",
        )
        .unwrap();
        let mut a = app();
        a.dir = dir.clone();
        let (path, note) = a.creation_policy(&Plan::default());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("/dev") && text.contains("a.example"),
            "{text}"
        );
        assert!(note.unwrap().contains("sandbox-policy.yaml is missing"));
        std::fs::write(
            dir.join("sandbox-policy.yaml"),
            "version: 1\nnetwork_policies: { mine: {} }\n",
        )
        .unwrap();
        let (path, note) = a.creation_policy(&Plan::default());
        assert!(!std::fs::read_to_string(path).unwrap().contains("a.example"));
        assert!(note.unwrap().contains("block"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gateways_start_restart_and_resize_through_osh_with_a_confirmation() {
        let mut a = app();
        on_gateways(&mut a, vec![gw("openshell-docker", 17671)]);
        press(&mut a, KeyCode::Char('R'));
        let Some(Modal::Confirm {
            kind: ConfKind::OshJob(args),
            text,
        }) = &a.modal
        else {
            panic!("R asks first");
        };
        assert!(
            text.starts_with("Start gateway openshell-docker?"),
            "{text}"
        );
        assert_eq!(
            args,
            &["gateway", "restart", "openshell-docker", "--confirmed"]
        );
        press(&mut a, KeyCode::Char('y'));
        let job = a.job.take().expect("runs osh itself");
        assert_eq!(job[0].args[..2], ["gateway".to_string(), "restart".into()]);
        press(&mut a, KeyCode::Char('V'));
        assert!(matches!(&a.modal, Some(Modal::Info(t)) if t.contains("is not a VM gateway")));
        a.modal = None;
        on_gateways(&mut a, vec![gw("openshell-vm", 17672)]);
        press(&mut a, KeyCode::Char('V'));
        assert!(matches!(
            a.modal,
            Some(Modal::Input {
                kind: InKind::VmCpus(_),
                ..
            })
        ));
        a.modal = None;
        a.on_input(InKind::VmCpus("openshell-vm".into()), "99".into());
        assert!(a.status.starts_with("Not changed"));
        a.on_input(InKind::VmCpus("openshell-vm".into()), "4".into());
        a.modal = None;
        a.on_input(InKind::VmMemory("openshell-vm".into(), 4), "4096".into());
        press(&mut a, KeyCode::Char('y'));
        let job = a.job.take().unwrap();
        assert_eq!(
            job[0].args,
            [
                "gateway",
                "vm-size",
                "openshell-vm",
                "4",
                "4096",
                "--confirmed"
            ]
        );
    }

    #[test]
    fn removing_a_gateway_can_delete_its_data_from_the_same_question() {
        let mut a = app();
        a.registry.add(crate::gwmodel::GatewaySpec {
            name: "openshell-podman".into(),
            kind: crate::gwmodel::GwKind::Podman,
            port: 17671,
            main: false,
        });
        on_gateways(&mut a, vec![gw("openshell-podman", 17671)]);
        press(&mut a, KeyCode::Char('x'));
        assert!(
            matches!(&a.modal, Some(Modal::Confirm { text, .. }) if text.contains("D removes it and DELETES"))
        );
        press(&mut a, KeyCode::Char('D'));
        let Some(Modal::Confirm {
            kind: ConfKind::OshJob(args),
            ..
        }) = &a.modal
        else {
            panic!("a second, stronger question");
        };
        assert!(args.contains(&"--delete-data".to_string()));
    }

    #[test]
    fn start_up_notices_become_alerts_and_an_unreachable_gateway_names_keys() {
        let mut a = app();
        a.data_notice("osh kept your edited copy of: x.\nmore".into());
        assert_eq!(a.alerts[0].title, "osh kept your edited copy of: x.");
        assert!(a.alerts[0].detail.contains("more"));
        let s = App::snapshot_status(&Snapshot::default());
        assert!(s.contains("R starts it") && !s.contains("brew") && !s.contains("systemctl"));
    }

    #[test]
    fn a_profile_update_that_reaches_running_sandboxes_offers_their_restart() {
        let mut a = app();
        a.on_done(Done {
            msg: "Updated profile npm".into(),
            next: None,
            sync: None,
            gh: Some(GhStage::ProfileUpdated {
                profile: "npm".into(),
                sandboxes: vec!["box".into()],
            }),
        });
        let Some(Modal::Confirm {
            text,
            kind: ConfKind::Restart(names),
        }) = &a.modal
        else {
            panic!("the restart is offered");
        };
        assert!(text.starts_with("Profile npm is updated. box uses a provider"));
        assert_eq!(names, &["box"]);
    }

    #[test]
    fn a_vm_gateway_says_why_the_create_flow_asks_no_limits() {
        let mut a = app();
        a.snap.driver = "vm".into();
        a.on_input(InKind::Name(Plan::default()), "x".into());
        assert!(
            a.status
                .contains("every VM sandbox gets the gateway's size"),
            "{}",
            a.status
        );
        assert!(a.status.contains("V on the Gateways tab"));
    }

    #[test]
    fn an_outdated_image_is_offered_a_rebuild_before_the_sandbox_is_created() {
        let mut a = app();
        a.snap.images = vec![cli::Image {
            tag: "osh-pi".into(),
            built: true,
            freshness: cli::Freshness::Outdated,
            ..cli::Image::default()
        }];
        let plan = Plan {
            image: "osh-pi".into(),
            ..Plan::default()
        };
        a.ask_rebuild(plan);
        let Some(Modal::Select {
            kind: SelKind::Rebuild(plan),
            items,
            ..
        }) = a.modal.take()
        else {
            panic!("no rebuild question");
        };
        assert!(items[0].starts_with("Rebuild"));
        a.on_select(SelKind::Rebuild(plan.clone()), vec![items[0].clone()]);
        let Some(Modal::Confirm { text, .. }) = a.modal.take() else {
            panic!("no confirmation");
        };
        assert!(
            text.contains(" build ") && text.contains("sandbox create"),
            "{text}"
        );
        assert!(
            text.contains("--policy ") && text.contains("sandbox-policy.yaml"),
            "tmux needs the PTY devices the default policy denies: {text}"
        );

        a.on_select(SelKind::Rebuild(plan), vec![items[1].clone()]);
        let Some(Modal::Confirm { text, .. }) = a.modal.take() else {
            panic!("no confirmation");
        };
        assert!(!text.contains(" build "), "{text}");
    }

    #[test]
    fn an_up_to_date_image_goes_straight_to_the_confirmation() {
        let mut a = app();
        a.snap.images = vec![cli::Image {
            tag: "osh-pi".into(),
            built: true,
            ..cli::Image::default()
        }];
        a.ask_rebuild(Plan {
            image: "osh-pi".into(),
            ..Plan::default()
        });
        assert!(matches!(a.modal, Some(Modal::Confirm { .. })));
    }

    #[test]
    fn only_a_setup_token_job_is_marked_as_printing_a_secret() {
        let mut a = app();
        assert!(!a.job_prints_secret());
        a.generate_token(InKind::Cred {
            provider: "claude".into(),
            keys: vec!["CLAUDE_CODE_OAUTH_TOKEN".into()],
            values: vec![],
            update: false,
            next: None,
        });
        assert!(a.job_prints_secret());
        a.finish_job(None);
        assert!(!a.job_prints_secret());
    }

    fn snap(gateway: &str, ok: bool) -> Snapshot {
        Snapshot {
            gateway: gateway.into(),
            gateway_ok: ok,
            ..Default::default()
        }
    }

    #[test]
    fn a_switch_is_loading_until_the_new_gateways_first_snapshot_then_ready() {
        let mut a = app();
        a.on_snapshot(snap("openshell-docker", true));
        assert_eq!(a.status, "Ready");
        a.snap.gateways = vec![
            cli::Gateway {
                name: "openshell-docker".into(),
                endpoint: "e1".into(),
                ..Default::default()
            },
            cli::Gateway {
                name: "openshell-vm".into(),
                endpoint: "e2".into(),
                ..Default::default()
            },
        ];
        a.cursor[4] = 1;
        a.change_gateway("openshell-vm");
        assert_eq!(a.status, "Switching to openshell-vm...");
        assert!(a.busy() && a.awaiting_snapshot);
        assert_eq!(a.snap.gateways.len(), 2, "the Gateways tab keeps its rows");
        assert_eq!(a.cursor[4], 1, "and its cursor");
        a.on_snapshot(snap("openshell-vm", true));
        assert_eq!(a.status, "Ready");
        assert!(!a.busy() && !a.awaiting_snapshot);
        cli::set_gateway("");
    }

    #[test]
    fn the_status_follows_the_gateway_but_keeps_an_actions_result() {
        let mut a = app();
        a.on_snapshot(snap("g", true));
        assert_eq!(a.status, "Ready");
        // The gateway goes down and comes back: "Ready" must not outlive either.
        a.on_snapshot(snap("g", false));
        assert!(a.status.starts_with("Gateway unreachable"), "{}", a.status);
        a.on_snapshot(snap("g", true));
        assert_eq!(a.status, "Ready");
        let mut bad = snap("g", true);
        bad.error = Some("Error: sandbox list failed".into());
        a.on_snapshot(bad);
        assert_eq!(a.status, "Error: sandbox list failed");
        a.on_snapshot(snap("g", true));
        assert_eq!(
            a.status, "Ready",
            "an error clears when the gateway recovers"
        );
        // What an action reported, or is still doing, is not overwritten by the poller.
        a.status = "Creating provider x...".into();
        a.on_snapshot(snap("g", false));
        assert_eq!(a.status, "Creating provider x...");
        a.status = "Detached y".into();
        a.on_snapshot(snap("g", true));
        assert_eq!(a.status, "Detached y");
    }

    #[test]
    fn a_panicking_background_task_still_ends_the_status() {
        let (wake, _w) = mpsc::channel();
        let (done, rx) = mpsc::channel();
        let mut a = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        a.bg(|| -> Result<Done, String> { panic!("boom") });
        let d = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(d.msg.starts_with("Error"), "{}", d.msg);
    }

    #[test]
    fn the_gateways_cursor_stays_inside_the_list() {
        let mut a = app();
        a.cursor[4] = 3;
        let mut s = snap("g", true);
        s.gateways = vec![cli::Gateway {
            name: "g".into(),
            endpoint: "e".into(),
            ..Default::default()
        }];
        a.on_snapshot(s);
        assert_eq!(a.cursor[4], 0);
    }

    fn repo() -> GhRepo {
        GhRepo {
            forge: ghrepo::Forge::GitHub,
            host: "github.com".into(),
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
        let hosts = std::env::temp_dir().join(format!("osh-app-hosts-{}", std::process::id()));
        let _ = std::fs::remove_file(&hosts);
        a.hosts_file = Some(hosts.clone());
        ghrepo::remember_host(Some(&hosts), "example.com", None, None);
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
        let _ = std::fs::remove_file(hosts);
        let _ = std::fs::remove_dir_all(d);
    }

    fn forgejo_repo() -> GhRepo {
        GhRepo {
            forge: ghrepo::Forge::Forgejo,
            host: "example.org".into(),
            owner: "acme".into(),
            repo: "app".into(),
        }
    }

    /// The remote `ask_github` would read from `url`.
    fn remote(url: &str) -> ghrepo::Remote {
        ghrepo::parse_url(url).unwrap()
    }

    fn asks_for_command(a: &App) -> bool {
        matches!(
            &a.modal,
            Some(Modal::Input {
                kind: InKind::Command(_),
                ..
            })
        )
    }

    #[test]
    fn an_unknown_host_is_asked_about_once_and_the_answer_is_remembered() {
        let hosts = std::env::temp_dir().join(format!("osh-app-ask-{}", std::process::id()));
        let _ = std::fs::remove_file(&hosts);
        let url = "git@example.org:acme/app.git";
        let d = git_dir("fjask", Some(url));
        let plan = Plan {
            share: d.clone(),
            ..Plan::default()
        };
        let mut a = app();
        a.hosts_file = Some(hosts.clone());
        a.ask_github(plan.clone());
        let (title, items) = select_items(&a);
        assert_eq!(title, "Which git provider hosts example.org?");
        assert_eq!(
            items,
            [
                "GitHub",
                "GitLab",
                "Bitbucket",
                "Forgejo",
                ghrepo::NO_PROVIDER_ITEM
            ]
        );
        a.on_select(
            SelKind::GitProvider(Some(plan.clone()), remote(url)),
            vec!["Forgejo".into()],
        );
        let (title, items) = select_items(&a);
        assert_eq!(title, "Forgejo access to example.org/acme/app");
        assert_eq!(items[0], "No Forgejo access");
        assert_eq!(items[1], "Read-only (the forgejo provider)");
        assert!(items
            .iter()
            .any(|i| i.contains("shared by sandboxes (forgejo-app)")));
        // Remembered: no question the second time.
        a.ask_github(plan.clone());
        assert_eq!(select_items(&a).0, "Forgejo access to example.org/acme/app");
        // "None / other" is remembered too and never asked again.
        let mut b = app();
        b.hosts_file = Some(hosts.clone());
        b.on_select(
            SelKind::GitProvider(Some(plan.clone()), remote(url)),
            vec![ghrepo::NO_PROVIDER_ITEM.into()],
        );
        assert!(asks_for_command(&b));
        b.modal = None;
        b.ask_github(plan);
        assert!(asks_for_command(&b));
        let _ = std::fs::remove_file(hosts);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_provider_question_puts_the_likely_one_first() {
        let hosts = std::env::temp_dir().join(format!("osh-app-likely-{}", std::process::id()));
        let _ = std::fs::remove_file(&hosts);
        let d = git_dir("likely", Some("https://gitlab.com/grp/sub/app.git"));
        let mut a = app();
        a.hosts_file = Some(hosts.clone());
        a.ask_github(Plan {
            share: d.clone(),
            ..Plan::default()
        });
        let (title, items) = select_items(&a);
        assert_eq!(title, "Which git provider hosts gitlab.com?");
        assert_eq!(items[0], "GitLab");
        assert_eq!(items.last().unwrap(), ghrepo::NO_PROVIDER_ITEM);
        let _ = std::fs::remove_file(hosts);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn gitlab_access_has_no_read_only_item_and_shows_gitlab_steps() {
        let hosts = std::env::temp_dir().join(format!("osh-app-gl-{}", std::process::id()));
        let _ = std::fs::remove_file(&hosts);
        let url = "https://gitlab.com/grp/sub/app.git";
        let mut a = app();
        a.hosts_file = Some(hosts.clone());
        a.on_select(
            SelKind::GitProvider(Some(Plan::default()), remote(url)),
            vec!["GitLab".into()],
        );
        let (title, items) = select_items(&a);
        assert_eq!(title, "GitLab access to gitlab.com/grp/sub/app");
        assert_eq!(items[0], "No GitLab access");
        assert!(!items.iter().any(|i| i.starts_with("Read-only")));
        assert!(items
            .iter()
            .any(|i| i.contains("shared by sandboxes (gitlab-app)")));
        assert_eq!(
            ghrepo::host_entry(Some(&hosts), "gitlab.com")
                .unwrap()
                .forge,
            Some(ghrepo::Forge::GitLab)
        );
        let Some(Modal::Select { kind, .. }) = a.modal.take() else {
            panic!("no select")
        };
        a.on_select(
            kind,
            vec!["Read and write, a token for this sandbox".into()],
        );
        match &a.modal {
            Some(Modal::Confirm { text, .. }) => {
                assert!(text.contains("project access token for grp/sub/app on gitlab.com"));
                assert!(text.contains("read_api and write_repository"));
                assert!(text.contains("/grp/sub/app/-/settings/access_tokens"));
            }
            _ => panic!("no steps"),
        }
        let _ = std::fs::remove_file(hosts);
    }

    #[test]
    fn a_remote_that_does_not_fit_the_provider_gets_no_access() {
        let hosts = std::env::temp_dir().join(format!("osh-app-unfit-{}", std::process::id()));
        let _ = std::fs::remove_file(&hosts);
        let mut a = app();
        a.hosts_file = Some(hosts.clone());
        // An https Data Center remote must be /scm/PROJECT/REPO.
        a.on_select(
            SelKind::GitProvider(
                Some(Plan::default()),
                remote("https://example.net/bitbucket/scm/P/app.git"),
            ),
            vec!["Bitbucket".into()],
        );
        assert!(asks_for_command(&a));
        assert!(a.status.contains("Bitbucket Data Center"), "{}", a.status);
        // GitHub is github.com only.
        a.on_select(
            SelKind::GitProvider(Some(Plan::default()), remote("git@example.invalid:a/b.git")),
            vec!["GitHub".into()],
        );
        assert!(asks_for_command(&a));
        assert!(a.status.contains("github.com only"), "{}", a.status);
        let _ = std::fs::remove_file(hosts);
    }

    #[test]
    fn the_providers_tab_asks_the_provider_of_a_new_host_then_shows_its_steps() {
        let hosts = std::env::temp_dir().join(format!("osh-app-tab-{}", std::process::id()));
        let _ = std::fs::remove_file(&hosts);
        let mut a = app();
        a.hosts_file = Some(hosts.clone());
        a.on_input(InKind::RepoName, "example.net/scm/PROJ/app.git".into());
        let (title, _) = select_items(&a);
        assert_eq!(title, "Which git provider hosts example.net?");
        let Some(Modal::Select { kind, .. }) = a.modal.take() else {
            panic!("no select")
        };
        a.on_select(kind, vec!["Bitbucket".into()]);
        match &a.modal {
            Some(Modal::Confirm { text, kind }) => {
                assert!(text.contains("HTTP access tokens > Create token"));
                assert!(matches!(
                    kind,
                    ConfKind::RepoSteps(RepoFlow {
                        goal: Goal::Shared,
                        ..
                    })
                ));
            }
            _ => panic!("no steps"),
        }
        // Remembered: straight to the steps.
        a.modal = None;
        a.on_input(InKind::RepoName, "example.net/scm/PROJ/lib".into());
        assert!(matches!(&a.modal, Some(Modal::Confirm { .. })));
        let _ = std::fs::remove_file(hosts);
    }

    #[test]
    fn a_deleted_sandbox_with_a_gitlab_token_names_its_page() {
        let mut a = app();
        a.on_done(Done {
            msg: "Deleted sandbox dev and gitlab-dev-app".into(),
            next: None,
            sync: None,
            gh: Some(GhStage::Leftover {
                github: vec![],
                other: vec![(
                    "GitLab".into(),
                    "osh-dev-app".into(),
                    "https://gitlab.com/a/app/-/settings/access_tokens".into(),
                )],
            }),
        });
        match &a.modal {
            Some(Modal::Info(text)) => {
                assert!(text.contains("The GitLab token osh-dev-app"));
                assert!(text.contains("https://gitlab.com/a/app/-/settings/access_tokens"));
            }
            _ => panic!("no gitlab note"),
        }
    }

    #[test]
    fn forgejo_read_write_shows_steps_without_any_expiry() {
        let mut a = app();
        a.on_select(
            SelKind::GhAccess(Plan::default(), forgejo_repo()),
            vec!["Read and write, a token shared by sandboxes (forgejo-app)".into()],
        );
        match &a.modal {
            Some(Modal::Confirm { text, kind }) => {
                assert!(text.contains("https://example.org/user/settings/applications/tokens/new"));
                assert!(text.contains("write:repository") && text.contains("osh-app"));
                assert!(!text.contains("30") && !text.contains("expires in"));
                assert!(matches!(
                    kind,
                    ConfKind::RepoSteps(RepoFlow {
                        goal: Goal::SharedFor(_),
                        ..
                    })
                ));
            }
            _ => panic!("no steps"),
        }
    }

    #[test]
    fn the_create_confirmation_has_an_expiry_line_for_github_only() {
        let plan = |repo: GhRepo| Plan {
            name: "dev".into(),
            gh: Some(GhGrant {
                repo,
                token: "t".into(),
            }),
            ..Plan::default()
        };
        let mut a = app();
        a.confirm_create(plan(repo()));
        let Some(Modal::Confirm { text, .. }) = &a.modal else {
            panic!("no confirm")
        };
        assert!(text.contains("github-dev-app") && text.contains("expires in 30 days"));
        a.confirm_create(plan(forgejo_repo()));
        let Some(Modal::Confirm { text, .. }) = &a.modal else {
            panic!("no confirm")
        };
        assert!(
            text.contains("forgejo-dev-app") && !text.contains("expires"),
            "{text}"
        );
        let steps = a.sandbox_steps(&plan(forgejo_repo()));
        assert!(steps
            .last()
            .unwrap()
            .args
            .join(" ")
            .contains("--provider forgejo-dev-app"));
    }

    #[test]
    fn a_deleted_sandbox_with_a_forgejo_token_names_the_host_page() {
        let mut a = app();
        a.on_done(Done {
            msg: "Deleted sandbox dev and forgejo-dev-app".into(),
            next: None,
            sync: None,
            gh: Some(GhStage::Leftover {
                github: vec![],
                other: vec![(
                    "Forgejo".into(),
                    "osh-dev-app".into(),
                    ghrepo::forgejo_tokens_page("example.org"),
                )],
            }),
        });
        match &a.modal {
            Some(Modal::Info(text)) => {
                assert!(text.contains("osh-dev-app") && text.contains("example.org"));
                assert!(text.contains("https://example.org/user/settings/applications"));
            }
            _ => panic!("no forgejo note"),
        }
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
    fn deleting_a_sandbox_with_its_own_token_offers_to_open_the_github_token_list() {
        let mut a = app();
        a.on_done(Done {
            msg: "Deleted sandbox dev and github-dev-app".into(),
            next: None,
            sync: None,
            gh: Some(GhStage::Leftover {
                github: vec!["osh-dev-app".into()],
                other: vec![],
            }),
        });
        match &a.modal {
            Some(Modal::Confirm {
                text,
                kind: ConfKind::OpenTokenPage(names),
            }) => {
                assert_eq!(names, &["osh-dev-app".to_string()]);
                assert!(text.contains("osh-dev-app") && text.contains("delete it"));
            }
            _ => panic!("no token offer"),
        }
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

    #[test]
    fn background_work_from_the_dashboard_shows_a_progress_popup_until_it_is_done() {
        let mut a = app();
        a.status = "Creating provider x...".into();
        a.bg_msg(|| Ok("Created provider x".into()));
        let Some(Modal::Task(t)) = &a.modal else {
            panic!("no progress popup")
        };
        assert_eq!(t.lock().unwrap().title, "Creating provider x");
        // Esc hides it, any other key is ignored while it shows.
        a.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert!(matches!(a.modal, Some(Modal::Task(_))));
        a.on_done(Done {
            msg: "Created provider x".into(),
            next: None,
            sync: None,
            gh: None,
        });
        assert!(a.modal.is_none());
        // Work that sets no "..." status (a quick toggle) opens nothing.
        a.status = "Ready".into();
        a.bg_msg(|| Ok("ok".into()));
        assert!(a.modal.is_none());
    }
}
