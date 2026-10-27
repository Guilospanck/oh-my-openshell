//! Turns facts about the host into an ordered list of install steps and runs them with consent.
//! Planning is pure (Facts in, steps out); only `execute` touches the host, through `Host`.
use crate::gwmodel::GwKind;
use crate::platform::{gather, Facts, Host, Kvm, Os, PkgMgr, PodmanMachine, RealHost, Runtime};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

/// Downloads OpenShell's installer to a file and runs it, so a failed or cut-off download fails
/// the step (a `curl | sh` pipe reports the status of `sh`, which runs an empty script and
/// succeeds), then checks that `openshell` can be found.
pub const OPENSHELL_INSTALL: &str = concat!(
    "set -e\n",
    "t=\"$(mktemp)\"\n",
    "trap 'rm -f \"$t\"' EXIT\n",
    "curl -fLsS https://raw.githubusercontent.com/NVIDIA/OpenShell/main/install.sh -o \"$t\"\n",
    "sh \"$t\"\n",
    "command -v openshell >/dev/null || { echo \"openshell is not on PATH after the installer ran\" >&2; exit 1; }"
);

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub yes: bool,
    pub dry_run: bool,
    /// Which runtime to install when none exists; `None` means the user has not picked yet.
    pub runtime: Option<Runtime>,
    /// The gateway types to set up; the first is the main one. Empty keeps the older behaviour
    /// of choosing from `runtime` alone.
    pub gateways: Vec<GwKind>,
    /// Run steps with their output streamed to `Event::Line` instead of sharing the terminal.
    pub capture: bool,
    /// Run `sudo` as `sudo -n`: it fails instead of prompting (for a full-screen view).
    pub sudo_noninteractive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// Run a program directly, with these arguments (never through a shell, bar fixed scripts).
    Cmd { prog: String, args: Vec<String> },
    /// Write a file in-process (config files), replacing any symlink there.
    Write { path: PathBuf, content: String },
    /// Something osh cannot or must not do itself: the text tells the user what to do.
    Manual(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub id: &'static str,
    pub title: String,
    pub kind: StepKind,
    /// Needed for osh itself to work (OpenShell, a runtime); the rest only enables a feature.
    pub optional: bool,
    /// Id of a step that must have succeeded first.
    pub after: Option<&'static str>,
}

impl Step {
    /// `command_line` for the screen: a multi-line `sh -c` script shows its first line only.
    pub fn display(&self) -> String {
        match &self.kind {
            StepKind::Cmd { prog, args }
                if prog == "sh" && args.first().is_some_and(|a| a == "-c") =>
            {
                let script = args.get(1).map(String::as_str).unwrap_or("");
                // Short scripts are shown whole (lines joined by `; `), long ones by their first
                // line and `...`; either way quoted so a short one can be pasted into a shell.
                if script.lines().count() <= 6 {
                    format!("sh -c {}", sh_quote(&one_line(script)))
                } else {
                    format!(
                        "sh -c {}",
                        sh_quote(&format!("{} ...", script.lines().next().unwrap_or("")))
                    )
                }
            }
            _ => self.command_line(),
        }
    }

    pub fn command_line(&self) -> String {
        match &self.kind {
            StepKind::Cmd { prog, args } => format!("{prog} {}", args.join(" ")),
            StepKind::Write { path, .. } => format!("write {}", path.display()),
            StepKind::Manual(t) => t.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    Yes,
    No,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Done,
    DryRun,
    Declined,
    /// A step it depended on did not succeed.
    Blocked,
    Failed(String),
    /// The user has to do it.
    Manual,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StepResult {
    pub id: &'static str,
    pub outcome: Outcome,
}

/// Package name of a tool for a package manager, where it differs.
fn pkg_name(tool: &'static str, pm: PkgMgr) -> &'static str {
    match (tool, pm) {
        ("gh", PkgMgr::Pacman) => "github-cli",
        ("docker", PkgMgr::Apt) => "docker.io",
        ("docker", PkgMgr::Dnf) => "moby-engine",
        _ => tool,
    }
}

/// Package manager install command, up to the package names.
fn install_prefix(pm: PkgMgr) -> Option<Vec<&'static str>> {
    Some(match pm {
        PkgMgr::Brew => vec!["brew", "install"],
        PkgMgr::Apt => vec!["apt-get", "install", "-y"],
        PkgMgr::Dnf => vec!["dnf", "install", "-y"],
        PkgMgr::Pacman => vec!["pacman", "-S", "--noconfirm", "--needed"],
        PkgMgr::Zypper => vec!["zypper", "install", "-y"],
        PkgMgr::None => return None,
    })
}

struct Planner {
    f: Facts,
    steps: Vec<Step>,
    apt_updated: bool,
    /// Suffix for the ids of a second (third...) gateway type's prerequisite steps, so two
    /// runtimes in one plan do not share `runtime`, `runtime-service`...
    sfx: String,
    kinds_planned: usize,
}

impl Planner {
    fn new(f: &Facts) -> Planner {
        Planner {
            f: f.clone(),
            steps: Vec::new(),
            apt_updated: false,
            sfx: String::new(),
            kinds_planned: 0,
        }
    }

    /// `base` with this gateway type's suffix, as a `&'static str` (a plan is built once per run).
    fn rid(&self, base: &'static str) -> &'static str {
        if self.sfx.is_empty() {
            base
        } else {
            Box::leak(format!("{base}{}", self.sfx).into_boxed_str())
        }
    }

    fn push(&mut self, id: &'static str, title: &str, kind: StepKind, optional: bool) {
        let id = self.rid(id);
        self.steps.push(Step {
            id,
            title: title.into(),
            kind,
            optional,
            after: None,
        });
    }

    /// A command that needs root: run as is for root, behind sudo otherwise, or left to the
    /// user when there is no sudo.
    fn privileged(&self, argv: Vec<String>) -> StepKind {
        if self.f.is_root {
            StepKind::Cmd {
                prog: argv[0].clone(),
                args: argv[1..].to_vec(),
            }
        } else if self.f.has_sudo {
            StepKind::Cmd {
                prog: "sudo".into(),
                args: argv,
            }
        } else {
            StepKind::Manual(format!("as root, run: {}", argv.join(" ")))
        }
    }

    fn pkg(&mut self, id: &'static str, title: &str, tools: &[&'static str], optional: bool) {
        let pm = self.f.pkg;
        let Some(prefix) = install_prefix(pm) else {
            let names: Vec<_> = tools.iter().map(|t| pkg_name(t, pm)).collect();
            let text = format!("install with your package manager: {}", names.join(" "));
            return self.push(id, title, StepKind::Manual(text), optional);
        };
        if pm == PkgMgr::Apt && !self.apt_updated {
            self.apt_updated = true;
            let kind = self.privileged(vec!["apt-get".into(), "update".into()]);
            self.push(
                "apt-update",
                "Refresh the apt package index",
                kind,
                optional,
            );
        }
        let mut argv: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
        argv.extend(tools.iter().map(|t| pkg_name(t, pm).to_string()));
        let kind = if pm == PkgMgr::Brew {
            StepKind::Cmd {
                prog: argv[0].clone(),
                args: argv[1..].to_vec(),
            }
        } else {
            self.privileged(argv)
        };
        self.push(id, title, kind, optional);
    }

    fn manual_runtime_started(&self, rt: Runtime) -> String {
        match (self.f.os, rt) {
            (Os::Linux, Runtime::Docker) => {
                "docker is installed but not answering: sudo systemctl start docker".into()
            }
            (Os::Linux, Runtime::Podman) => {
                "podman is installed but not answering: systemctl --user start podman.socket".into()
            }
            (Os::MacOs, Runtime::Docker) => {
                "docker is installed but not answering: colima start (or start Docker Desktop)"
                    .into()
            }
            (Os::MacOs, Runtime::Podman) => {
                "podman is installed but not answering: podman machine start".into()
            }
        }
    }

    fn runtime(&mut self, o: &Options) {
        let f = self.f.clone();
        if !f.runtimes_ready.is_empty() {
            return;
        }
        if f.os == Os::Linux && f.podman_needs_socket {
            // Podman answers but its API socket is off; the gateway needs it. No sudo: a user unit.
            let kind = StepKind::Cmd {
                prog: "systemctl".into(),
                args: ["--user", "enable", "--now", "podman.socket"]
                    .map(String::from)
                    .to_vec(),
            };
            self.push("runtime", "Start the Podman API socket", kind, false);
            return;
        }
        if f.os == Os::Linux && f.docker_permission_denied && !f.is_root && !f.user.is_empty() {
            // The daemon is up; this user just cannot reach it yet.
            let kind = self.privileged(
                ["usermod", "-aG", "docker", f.user.as_str()]
                    .map(String::from)
                    .to_vec(),
            );
            self.push("runtime-group", "Let your user run docker", kind, false);
            self.push(
                "runtime-relogin",
                "Apply the new group",
                StepKind::Manual(
                    "log out and back in (or run `newgrp docker`), then run osh again".into(),
                ),
                false,
            );
            return;
        }
        if let Some(rt) = f.runtimes_installed.first().copied() {
            // The default macOS vm gateway needs no runtime, so a stopped one is not worth a step.
            if f.os == Os::Linux {
                let text = self.manual_runtime_started(rt);
                self.push(
                    "runtime",
                    "Start the container runtime",
                    StepKind::Manual(text),
                    false,
                );
            }
            return;
        }
        let Some(rt) = o.runtime else {
            if f.os == Os::Linux {
                self.push(
                    "runtime",
                    "Install a container runtime",
                    StepKind::Manual(
                        "no container runtime found: rerun with --gateways docker or --gateways podman"
                            .into(),
                    ),
                    false,
                );
            }
            return;
        };
        match (f.os, rt) {
            (Os::MacOs, Runtime::Docker) => self.push(
                "runtime",
                "Install Docker",
                StepKind::Manual("brew install colima docker, then colima start".into()),
                true,
            ),
            (Os::MacOs, Runtime::Podman) => {
                self.pkg("runtime", "Install Podman", &["podman"], true);
                self.push(
                    "runtime-machine",
                    "Start the Podman machine",
                    StepKind::Manual("podman machine init && podman machine start".into()),
                    true,
                );
            }
            (Os::Linux, _) => {
                let title = format!("Install {}", rt.bin());
                self.pkg("runtime", &title, &[rt.bin()], false);
                if matches!(
                    self.steps.last().map(|s| &s.kind),
                    Some(StepKind::Manual(_))
                ) {
                    return;
                }
                let (id, title, kind) = match rt {
                    Runtime::Docker => (
                        "runtime-service",
                        "Start the Docker daemon at boot",
                        self.privileged(vec![
                            "systemctl".into(),
                            "enable".into(),
                            "--now".into(),
                            "docker".into(),
                        ]),
                    ),
                    Runtime::Podman => (
                        "runtime-service",
                        "Start the Podman API socket",
                        StepKind::Cmd {
                            prog: "systemctl".into(),
                            args: ["--user", "enable", "--now", "podman.socket"]
                                .map(String::from)
                                .to_vec(),
                        },
                    ),
                };
                self.push(id, title, kind, false);
                let base = self.rid("runtime");
                self.steps.last_mut().unwrap().after = Some(base);
                if rt == Runtime::Docker && !f.is_root && !f.user.is_empty() {
                    let kind = self.privileged(
                        ["usermod", "-aG", "docker", f.user.as_str()]
                            .map(String::from)
                            .to_vec(),
                    );
                    self.push("runtime-group", "Let your user run docker", kind, false);
                    let base = self.rid("runtime");
                    self.steps.last_mut().unwrap().after = Some(base);
                    // The gateway runs as a user service, which keeps the groups it started with:
                    // OpenShell's installer would wait for a connection that cannot happen yet.
                    self.push(
                        "runtime-relogin",
                        "Apply the new group",
                        StepKind::Manual(
                            "log out and back in (or reboot) so the docker group reaches your user services, then run osh setup again"
                                .into(),
                        ),
                        false,
                    );
                    let group = self.rid("runtime-group");
                    self.steps.last_mut().unwrap().after = Some(group);
                }
            }
        }
    }
}

/// Steps one gateway type needs before its gateway can start. Returns the id the gateway should
/// wait for. `first_after` is a dependency of the first step added (curl, say).
impl Planner {
    fn kind_prereqs(
        &mut self,
        kind: GwKind,
        first_after: Option<&'static str>,
    ) -> Option<&'static str> {
        self.sfx = if self.kinds_planned == 0 {
            String::new()
        } else {
            format!("-{}", self.kinds_planned + 1)
        };
        let before = self.steps.len();
        let f = self.f.clone();
        match kind {
            GwKind::Docker | GwKind::Podman => {
                let rt = if kind == GwKind::Docker {
                    Runtime::Docker
                } else {
                    Runtime::Podman
                };
                if f.runtimes_ready.contains(&rt) {
                    // Nothing to do.
                } else if f.os == Os::MacOs {
                    self.mac_runtime(rt);
                } else {
                    // The existing single-runtime logic, on a view of the facts that only knows `rt`.
                    let mut view = f.clone();
                    view.runtimes_ready.retain(|r| *r == rt);
                    view.runtimes_installed.retain(|r| *r == rt);
                    view.podman_needs_socket &= rt == Runtime::Podman;
                    view.docker_permission_denied &= rt == Runtime::Docker;
                    let saved = std::mem::replace(&mut self.f, view);
                    self.runtime(&Options {
                        runtime: Some(rt),
                        ..Default::default()
                    });
                    self.f = saved;
                }
            }
            GwKind::Vm => self.vm_prereqs(),
        }
        let added = &mut self.steps[before..];
        if let Some(first) = added.first_mut() {
            first.after = first.after.or(first_after);
        }
        let sfx = self.sfx.clone();
        let find = |base: &str| {
            let want = format!("{base}{sfx}");
            added.iter().find(|s| s.id == want).map(|s| s.id)
        };
        let wait = [
            "kvm-relogin",
            "runtime-relogin",
            "colima-start",
            "docker-desktop",
            "pm-start",
            "runtime-service",
            "runtime",
            "kvm",
        ]
        .into_iter()
        .find_map(find);
        self.kinds_planned += 1;
        self.sfx = String::new();
        wait
    }

    fn mac_runtime(&mut self, rt: Runtime) {
        let f = self.f.clone();
        match rt {
            Runtime::Docker if f.docker_desktop => self.push(
                "docker-desktop",
                "Start Docker Desktop",
                StepKind::Manual(
                    "start Docker Desktop and wait until it says it is running, then run osh again"
                        .into(),
                ),
                false,
            ),
            Runtime::Docker => {
                if !f.colima {
                    self.pkg(
                        "colima",
                        "Install Colima and Docker",
                        &["colima", "docker"],
                        false,
                    );
                }
                let after = (!f.colima).then(|| self.rid("colima"));
                self.push(
                    "colima-start",
                    "Start Colima",
                    StepKind::Cmd {
                        prog: "colima".into(),
                        args: vec!["start".into()],
                    },
                    false,
                );
                self.steps.last_mut().unwrap().after = after;
            }
            Runtime::Podman => {
                let installed = f.runtimes_installed.contains(&Runtime::Podman);
                if !installed {
                    self.pkg("runtime", "Install Podman", &["podman"], false);
                }
                let mut after = (!installed).then(|| self.rid("runtime"));
                if f.podman_machine == PodmanMachine::None {
                    self.push(
                        "pm-init",
                        "Create the Podman machine",
                        StepKind::Cmd {
                            prog: "podman".into(),
                            args: vec!["machine".into(), "init".into()],
                        },
                        false,
                    );
                    self.steps.last_mut().unwrap().after = after;
                    after = Some(self.rid("pm-init"));
                }
                self.push(
                    "pm-start",
                    "Start the Podman machine",
                    StepKind::Cmd {
                        prog: "podman".into(),
                        args: vec!["machine".into(), "start".into()],
                    },
                    false,
                );
                self.steps.last_mut().unwrap().after = after;
            }
        }
    }

    fn vm_prereqs(&mut self) {
        let f = self.f.clone();
        if f.os == Os::Linux {
            match f.kvm {
                Kvm::Absent => self.push(
                    "kvm",
                    "VM gateway needs /dev/kvm",
                    StepKind::Manual(
                        "the VM gateway needs /dev/kvm, which is missing: virtualization is off or unsupported on this machine".into(),
                    ),
                    false,
                ),
                Kvm::NoAccess if !f.is_root && !f.user.is_empty() => {
                    let kind = self.privileged(["usermod", "-aG", "kvm", f.user.as_str()].map(String::from).to_vec());
                    self.push("kvm-group", "Let your user use KVM", kind, false);
                    self.push(
                        "kvm-relogin",
                        "Apply the new group",
                        StepKind::Manual("log out and back in (or reboot) so the kvm group applies, then run osh setup again".into()),
                        false,
                    );
                    let group = self.rid("kvm-group");
                    self.steps.last_mut().unwrap().after = Some(group);
                }
                _ => {}
            }
        }
        if !f.has_e2fsprogs && !self.steps.iter().any(|s| s.id == "e2fsprogs") {
            self.pkg(
                "e2fsprogs",
                "Install e2fsprogs (the VM driver builds sandbox disks with it)",
                &["e2fsprogs"],
                false,
            );
        }
    }
}

/// The plan when the wanted gateway types are known: each type's prerequisites (the main's before
/// OpenShell on Linux, where OpenShell's installer waits for a connected gateway; the others'
/// after it), OpenShell, and the optional tools. The gateways themselves are added by
/// `plan_gateways`.
fn plan_for_gateways(f: &Facts, o: &Options) -> (Vec<Step>, Vec<Option<&'static str>>) {
    let mut p = Planner::new(f);
    let mac_brew = f.os == Os::MacOs && f.pkg == PkgMgr::Brew;
    if f.os == Os::MacOs && f.pkg == PkgMgr::None {
        p.push(
            "homebrew",
            "Install Homebrew",
            StepKind::Manual("install Homebrew from https://brew.sh, then run osh again".into()),
            false,
        );
    }
    let main = o.gateways[0];
    let mut after = None;
    if !f.has_openshell {
        if !f.has_curl {
            p.pkg("curl", "Install curl", &["curl"], false);
            after = Some("curl");
        }
        if mac_brew && !f.has_e2fsprogs {
            p.pkg(
                "e2fsprogs",
                "Install e2fsprogs (OpenShell needs 1.43+)",
                &["e2fsprogs"],
                false,
            );
        }
    }
    let mut openshell_after = after;
    // The step each gateway type's own steps must wait for, in the order of `o.gateways`.
    let mut waits: Vec<Option<&'static str>> = Vec::new();
    if f.os == Os::Linux {
        let wait = p.kind_prereqs(main, after);
        openshell_after = wait.or(after);
        waits.push(wait);
    }
    if !f.has_openshell {
        p.push(
            "openshell",
            "Install OpenShell",
            StepKind::Cmd {
                prog: "sh".into(),
                args: vec!["-c".into(), OPENSHELL_INSTALL.into()],
            },
            false,
        );
        p.steps.last_mut().unwrap().after = openshell_after;
    }
    if f.os == Os::MacOs {
        let wait = p.kind_prereqs(main, (!f.has_openshell).then_some("openshell"));
        waits.push(wait);
    }
    for k in &o.gateways[1..] {
        let wait = p.kind_prereqs(*k, None);
        waits.push(wait);
    }
    p.optional_tools();
    (p.steps, waits)
}

impl Planner {
    fn optional_tools(&mut self) {
        let f = self.f.clone();
        let mac_brew = f.os == Os::MacOs && f.pkg == PkgMgr::Brew;
        if f.os == Os::MacOs && !f.has_clipboard && mac_brew {
            self.pkg(
                "pngpaste",
                "Install pngpaste (paste host images with Ctrl-V)",
                &["pngpaste"],
                true,
            );
        }
        if f.os == Os::Linux && !f.has_clipboard {
            let tool = if f.wayland { "wl-clipboard" } else { "xclip" };
            self.pkg(
                "clipboard",
                "Install a clipboard tool (paste host images with Ctrl-V)",
                &[tool],
                true,
            );
        }
        if !f.has_tmux {
            self.pkg("tmux", "Install tmux", &["tmux"], true);
        }
        if !f.has_gh {
            self.pkg("gh", "Install the GitHub CLI", &["gh"], true);
        }
    }
}

pub fn plan(f: &Facts, o: &Options) -> Vec<Step> {
    if !o.gateways.is_empty() {
        return plan_for_gateways(f, o).0;
    }
    let mut p = Planner::new(f);
    let mac_brew = f.os == Os::MacOs && f.pkg == PkgMgr::Brew;
    if f.os == Os::MacOs && f.pkg == PkgMgr::None {
        p.push(
            "homebrew",
            "Install Homebrew",
            StepKind::Manual("install Homebrew from https://brew.sh, then run osh again".into()),
            false,
        );
    }
    // OpenShell's installer waits for `openshell status` to report connected, and on Linux the
    // gateway cannot connect before a container runtime works, so the runtime goes first there.
    // (The macOS vm gateway needs none.)
    let mut runtime_planned = false;
    if !f.has_openshell {
        let mut after = None;
        if !f.has_curl {
            p.pkg("curl", "Install curl", &["curl"], false);
            after = Some("curl");
        }
        if mac_brew {
            p.pkg(
                "e2fsprogs",
                "Install e2fsprogs (OpenShell needs 1.43+)",
                &["e2fsprogs"],
                false,
            );
        }
        if f.os == Os::Linux {
            let before = p.steps.len();
            p.runtime(o);
            runtime_planned = true;
            let added = &mut p.steps[before..];
            if let Some(first) = added.first_mut() {
                first.after = first.after.or(after);
            }
            // What OpenShell waits for: the user's action, else the service, else the install.
            let wait = ["runtime-relogin", "runtime-service", "runtime"]
                .into_iter()
                .find(|id| added.iter().any(|s| s.id == *id));
            after = wait.or(after);
        }
        p.push(
            "openshell",
            "Install OpenShell",
            StepKind::Cmd {
                prog: "sh".into(),
                args: vec!["-c".into(), OPENSHELL_INSTALL.into()],
            },
            false,
        );
        p.steps.last_mut().unwrap().after = after;
    }
    if !runtime_planned {
        p.runtime(o);
    }
    if f.os == Os::MacOs && !f.has_clipboard && mac_brew {
        p.pkg(
            "pngpaste",
            "Install pngpaste (paste host images with Ctrl-V)",
            &["pngpaste"],
            true,
        );
    }
    if f.os == Os::Linux && !f.has_clipboard {
        let tool = if f.wayland { "wl-clipboard" } else { "xclip" };
        p.pkg(
            "clipboard",
            "Install a clipboard tool (paste host images with Ctrl-V)",
            &[tool],
            true,
        );
    }
    if !f.has_tmux {
        p.pkg("tmux", "Install tmux", &["tmux"], true);
    }
    if !f.has_gh {
        p.pkg("gh", "Install the GitHub CLI", &["gh"], true);
    }
    p.steps
}

/// `docker,podman` as a list of gateway types, first = main. Also accepts `openshell-docker` etc.
pub fn parse_gateways(text: &str) -> Result<Vec<GwKind>, String> {
    let mut kinds: Vec<GwKind> = Vec::new();
    for word in text.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        let k = GwKind::parse(word)
            .ok_or_else(|| format!("{word} is not a gateway type; use vm, docker or podman"))?;
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    }
    if kinds.is_empty() {
        return Err("name at least one gateway type: vm, docker or podman".into());
    }
    Ok(kinds)
}

/// The gateways the OpenShell CLI already knows, as (name, port), from `openshell gateway list`.
pub fn existing_gateways(h: &dyn Host) -> Vec<(String, u16)> {
    let Ok(text) = h.run("openshell", &["gateway", "list", "-o", "json"]) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let name = g["name"].as_str()?.to_string();
            let port = g["endpoint"]
                .as_str()
                .and_then(|e| e.rsplit(':').next())
                .and_then(|p| p.trim_end_matches('/').parse().ok())
                .unwrap_or(0);
            Some((name, port))
        })
        .collect()
}

/// The whole plan for the wanted gateway types: prerequisites, OpenShell, then each gateway's
/// config, service and registration. Types already registered by osh or known to the CLI are
/// skipped. The first wanted type becomes the main gateway only when no gateway exists yet;
/// otherwise every one is an extra. `Err` when a type cannot work on this machine.
pub fn plan_gateways(
    f: &Facts,
    o: &Options,
    h: &dyn Host,
    p: &crate::gateway::Paths,
    reg: &crate::gwmodel::Registry,
    existing: &[(String, u16)],
    is_bound: &dyn Fn(u16) -> bool,
) -> Result<Vec<Step>, String> {
    use crate::gwmodel::{availability, next_port, GatewaySpec, State};
    for k in &o.gateways {
        if let State::Unavailable(why) = availability(f, *k).state {
            return Err(format!("{}: {why}", k.label()));
        }
    }
    let wanted: Vec<GwKind> = o
        .gateways
        .iter()
        .copied()
        .filter(|k| !reg.has_kind(*k) && !existing.iter().any(|(n, _)| n == k.name()))
        .collect();
    // Gateways osh already made that were asked for again: make sure they are running (a re-run
    // resumes a stopped one), and leave the healthy ones alone.
    let mut resume: Vec<Step> = Vec::new();
    for k in &o.gateways {
        let Some(spec) = reg.gateways.iter().find(|g| g.kind == *k) else {
            continue;
        };
        if spec.main {
            resume.extend(crate::gwops::main_gateway_steps(f, p, h, spec, false));
        } else if !crate::gwops::extra_running(f.os, h, spec) {
            resume.extend(crate::gwops::add_gateway_steps(f, p, h, reg, spec));
        }
    }
    if wanted.is_empty() {
        return Ok(resume);
    }
    let mut o2 = o.clone();
    o2.gateways = wanted.clone();
    let (mut steps, waits) = plan_for_gateways(f, &o2);
    let no_main = reg.main().is_none() && existing.is_empty();
    let mut taken: Vec<u16> = reg
        .gateways
        .iter()
        .map(|g| g.port)
        .chain(existing.iter().map(|(_, p)| *p))
        .collect();
    let mut reg_now = reg.clone();
    let mut prev_last: Option<&'static str> = None;
    for (i, kind) in wanted.iter().enumerate() {
        let is_main = i == 0 && no_main;
        let port = if is_main {
            17670
        } else {
            next_port(&taken, is_bound)
        };
        taken.push(port);
        let spec = GatewaySpec {
            name: kind.name().into(),
            kind: *kind,
            port,
            main: is_main,
        };
        let mut g = if is_main {
            let installing = !f.has_openshell || existing.is_empty();
            crate::gwops::main_gateway_steps(f, p, h, &spec, installing)
        } else {
            crate::gwops::add_gateway_steps(f, p, h, &reg_now, &spec)
        };
        let dep = if is_main && !f.has_openshell {
            Some("openshell")
        } else {
            waits.get(i).copied().flatten().or(prev_last)
        };
        if let Some(first) = g.first_mut() {
            first.after = first.after.or(dep);
        }
        prev_last = g.last().map(|s| s.id);
        reg_now.add(spec);
        steps.extend(g);
    }
    resume.extend(steps);
    Ok(resume)
}

/// Plans and runs the wanted gateway types on this machine (the `--gateways` and `gateway add`
/// path). Returns the process exit code.
pub fn run_gateways(o: &Options) -> i32 {
    let h = RealHost;
    let f = gather(&h);
    let p = crate::gateway::Paths::detect();
    let reg = crate::gwmodel::Registry::load(&p.data);
    let existing = existing_gateways(&h);
    let bound = |port: u16| std::net::TcpListener::bind(("127.0.0.1", port)).is_err();
    match plan_gateways(&f, o, &h, &p, &reg, &existing, &bound) {
        Err(e) => {
            eprintln!("osh: {e}");
            2
        }
        Ok(steps) if steps.is_empty() => {
            println!("Nothing to do: those gateways are already set up.");
            0
        }
        Ok(steps) => i32::from(!run_steps(
            &h,
            steps,
            o,
            !o.yes && io::stdin().is_terminal(),
        )),
    }
}

/// What happened while `execute_events` ran, so a screen can follow along.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    Start(usize),
    Line(usize, String),
    Done(usize, Outcome),
}

pub fn execute(
    steps: &[Step],
    h: &dyn Host,
    o: &Options,
    ask: &mut dyn FnMut(&Step) -> Answer,
) -> Vec<StepResult> {
    execute_events(steps, h, o, ask, &mut |_| {})
}

/// Runs the steps in order, reporting each step's start, its output lines (with `o.capture`) and
/// its outcome to `on_event`.
pub fn execute_events(
    steps: &[Step],
    h: &dyn Host,
    o: &Options,
    ask: &mut dyn FnMut(&Step) -> Answer,
    on_event: &mut dyn FnMut(Event),
) -> Vec<StepResult> {
    let mut results = Vec::new();
    let mut succeeded: Vec<&'static str> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        on_event(Event::Start(i));
        let mut outcome = Outcome::Done;
        if step.after.is_some_and(|a| !succeeded.contains(&a)) {
            outcome = Outcome::Blocked;
        } else if let StepKind::Manual(_) = step.kind {
            outcome = Outcome::Manual;
        } else if o.dry_run {
            outcome = Outcome::DryRun;
        } else if ask(step) == Answer::No {
            outcome = Outcome::Declined;
        } else {
            let done = match &step.kind {
                StepKind::Cmd { prog, args } => {
                    let mut argv: Vec<&str> = args.iter().map(String::as_str).collect();
                    let prog = prog.as_str();
                    if o.sudo_noninteractive && prog == "sudo" {
                        argv.insert(0, "-n");
                    }
                    if o.capture {
                        let mut sink = |l: &str| on_event(Event::Line(i, l.to_string()));
                        h.run_streaming(prog, &argv, &mut sink)
                    } else {
                        h.run_inherit(prog, &argv)
                    }
                }
                StepKind::Write { path, content } => h.write(path, content),
                StepKind::Manual(_) => Ok(()),
            };
            if let Err(e) = done {
                outcome = Outcome::Failed(e);
            }
        }
        if matches!(outcome, Outcome::Done | Outcome::DryRun) {
            succeeded.push(step.id);
        }
        on_event(Event::Done(i, outcome.clone()));
        results.push(StepResult {
            id: step.id,
            outcome,
        });
    }
    results
}

/// Read-only report: `(text, ok)`, `ok` false when a hard requirement is missing.
pub fn doctor(f: &Facts) -> (String, bool) {
    let mut out = String::new();
    let mut ok = true;
    let mut line = |state: &str, name: &str, note: &str| {
        let note = if note.is_empty() {
            String::new()
        } else {
            format!("  ({note})")
        };
        out.push_str(&format!("{state:<8} {name}{note}\n"));
    };
    if f.has_openshell {
        line("ok", "openshell", "");
    } else {
        ok = false;
        line("MISSING", "openshell", "osh setup installs it");
    }
    let runtime_needed = f.os == Os::Linux;
    if let Some(rt) = f.runtime() {
        line("ok", &format!("container runtime ({})", rt.bin()), "");
    } else {
        let note = if f.runtimes_installed.is_empty() {
            "osh setup --gateways docker|podman|vm sets one up".to_string()
        } else if f.docker_permission_denied {
            "docker is running but this user is not allowed to use docker yet; log out and back in"
                .to_string()
        } else {
            "installed but not answering; start it".to_string()
        };
        if runtime_needed {
            ok = false;
            line("MISSING", "container runtime", &note);
        } else {
            line(
                "optional",
                "container runtime",
                &format!("image builds and the Docker gateway; {note}"),
            );
        }
    }
    for (have, name, why) in [
        (f.has_tmux, "tmux", "sandbox sessions in tmux"),
        (f.has_gh, "gh", "GitHub repo picker"),
        (
            f.has_clipboard,
            "clipboard tool",
            "paste host images with Ctrl-V",
        ),
    ] {
        if have {
            line("ok", name, "");
        } else {
            line("optional", name, why);
        }
    }
    (out, ok)
}

/// A short script on one line: lines are joined with `; `, except after a word that expects the
/// next line to continue the command (`then`, `else`, `do`, `{`, `&&`, `||`, `|`).
fn one_line(script: &str) -> String {
    let mut out = String::new();
    for line in script.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !out.is_empty() {
            let open = ["then", "else", "do", "{", "&&", "||", "|"]
                .iter()
                .any(|w| out.ends_with(w));
            out.push_str(if open { " " } else { "; " });
        }
        out.push_str(line);
    }
    out
}

/// `text` as one shell word: single quotes, with each embedded `'` written as `'\''`.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// "tool X is missing" with the exact command that installs it, for features that degrade.
/// `tool` is a command name, or `clipboard` for the image-paste tool.
pub fn hint(tool: &'static str, os: Os, pkg: PkgMgr, wayland: bool) -> String {
    let (label, tool) = match (tool, os) {
        ("clipboard", Os::MacOs) => ("a clipboard tool", "pngpaste"),
        ("clipboard", Os::Linux) if wayland => ("a clipboard tool", "wl-clipboard"),
        ("clipboard", Os::Linux) => ("a clipboard tool", "xclip"),
        (t, _) => (t, t),
    };
    let name = pkg_name(tool, pkg);
    match install_prefix(pkg) {
        None => format!("{label} is not installed: install {name} with your package manager"),
        Some(prefix) => {
            let sudo = if pkg == PkgMgr::Brew { "" } else { "sudo " };
            format!(
                "{label} is not installed: {sudo}{} {name}",
                prefix.join(" ")
            )
        }
    }
}

/// What happened, for the end of a run: failures and the things left to the user.
pub fn summary(steps: &[Step], results: &[StepResult]) -> String {
    let mut out = String::new();
    for (s, r) in steps.iter().zip(results) {
        match &r.outcome {
            Outcome::Failed(e) => out.push_str(&format!("failed: {} ({e})\n", s.title)),
            Outcome::Manual => {
                out.push_str(&format!("do yourself: {}: {}\n", s.title, s.command_line()))
            }
            Outcome::Blocked => out.push_str(&format!(
                "skipped (an earlier step did not finish): {}\n",
                s.title
            )),
            Outcome::Declined => out.push_str(&format!("declined: {}\n", s.title)),
            Outcome::Done | Outcome::DryRun => {}
        }
    }
    out
}

fn prompt(text: &str) -> String {
    print!("{text}");
    let _ = io::stdout().flush();
    let mut a = String::new();
    let _ = io::stdin().lock().read_line(&mut a);
    a.trim().to_lowercase()
}

/// Plans, shows, and (with consent per step) runs setup on this host. With `required_only`,
/// optional tools are left out. Returns true when no required step failed or was left undone.
pub fn run(o: &Options, required_only: bool) -> bool {
    let h = RealHost;
    let f = gather(&h);
    let mut o = o.clone();
    let interactive = !o.yes && io::stdin().is_terminal();
    if f.runtimes_installed.is_empty() && o.runtime.is_none() && interactive {
        let needed = f.os == Os::Linux;
        let ask = if needed {
            "No container runtime found. Install [d]ocker or [p]odman? [d/p/skip] "
        } else {
            ""
        };
        if needed {
            o.runtime = match prompt(ask).as_str() {
                "d" | "docker" => Some(Runtime::Docker),
                "p" | "podman" => Some(Runtime::Podman),
                _ => None,
            };
        }
    }
    let mut steps = plan(&f, &o);
    if required_only {
        steps.retain(|s| !s.optional);
    }
    let paths = crate::gateway::Paths::detect();
    steps.extend(crate::gateway::plan(&f, &o, &h, &paths, false));
    run_steps(&h, steps, &o, interactive)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Consent {
    /// Ask before each step.
    Ask,
    /// Run every step: `--yes`, or a dry run that changes nothing.
    All,
    /// Not a terminal and no `--yes`: nobody can answer, so nothing runs.
    Refuse,
}

pub fn consent(o: &Options, interactive: bool) -> Consent {
    if o.yes || o.dry_run {
        Consent::All
    } else if interactive {
        Consent::Ask
    } else {
        Consent::Refuse
    }
}

/// Shows `steps`, runs them with consent per step, prints what is left. True when no required
/// step failed or was left undone.
pub fn run_steps(h: &dyn Host, steps: Vec<Step>, o: &Options, interactive: bool) -> bool {
    if steps.is_empty() {
        println!("Nothing to install: everything osh needs is present.");
        return true;
    }
    println!("osh will run {} steps:", steps.len());
    for (i, s) in steps.iter().enumerate() {
        println!(
            "  {}. {}{}\n       $ {}",
            i + 1,
            s.title,
            if s.optional { " (optional)" } else { "" },
            s.display()
        );
    }
    let mode = consent(o, interactive);
    if mode == Consent::Refuse {
        eprintln!(
            "\nnot a terminal, so nobody can approve these steps: rerun with --yes to run them"
        );
        return false;
    }
    let total = steps.len();
    let mut n = 0;
    let results = execute(&steps, h, o, &mut |s| {
        n += 1;
        println!("\n[{n}/{total}] {}\n    $ {}", s.title, s.display());
        if mode == Consent::All {
            return Answer::Yes;
        }
        match prompt("    Run it? [Y/n] ").as_str() {
            "n" | "no" => Answer::No,
            _ => Answer::Yes,
        }
    });
    let text = summary(&steps, &results);
    if !text.is_empty() {
        println!("\n{text}");
    }
    run_ok(&steps, &results, o)
}

/// True when no required step failed or was left undone. A dry run changes nothing, so listing
/// steps only the user can do is not a failure there.
pub fn run_ok(steps: &[Step], results: &[StepResult], o: &Options) -> bool {
    o.dry_run
        || steps
            .iter()
            .zip(results)
            .all(|(s, r)| s.optional || matches!(r.outcome, Outcome::Done | Outcome::DryRun))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gwmodel::GwKind;
    use crate::platform::testing::FakeHost;
    use crate::platform::{gather_for, Kvm, PkgMgr, PodmanMachine};

    fn facts(os: Os, pkg: PkgMgr) -> Facts {
        Facts {
            os,
            pkg,
            is_root: false,
            has_sudo: true,
            user: "ana".into(),
            wayland: false,
            has_openshell: false,
            has_curl: true,
            has_tmux: false,
            has_gh: false,
            has_clipboard: false,
            runtimes_installed: vec![],
            runtimes_ready: vec![],
            podman_needs_socket: false,
            docker_permission_denied: false,
            kvm: Kvm::Absent,
            colima: false,
            docker_desktop: false,
            podman_machine: PodmanMachine::None,
            has_e2fsprogs: false,
        }
    }

    fn lines(steps: &[Step]) -> Vec<String> {
        steps.iter().map(Step::command_line).collect()
    }

    fn has(steps: &[Step], needle: &str) -> bool {
        lines(steps).iter().any(|l| l.contains(needle))
    }

    fn ids(steps: &[Step]) -> Vec<&'static str> {
        steps.iter().map(|s| s.id).collect()
    }

    #[test]
    fn plan_macos_fresh_has_brew_and_openshell() {
        let steps = plan(&facts(Os::MacOs, PkgMgr::Brew), &Options::default());
        assert!(has(&steps, "brew install e2fsprogs"));
        assert!(has(&steps, "brew install pngpaste"));
        assert!(has(&steps, OPENSHELL_INSTALL));
        assert!(!has(&steps, "docker"), "vm gateway needs no runtime");
    }

    #[test]
    fn plan_macos_without_brew_asks_for_homebrew() {
        let steps = plan(&facts(Os::MacOs, PkgMgr::None), &Options::default());
        assert!(matches!(steps[0].kind, StepKind::Manual(ref t) if t.contains("brew.sh")));
        assert!(!ids(&steps).contains(&"e2fsprogs"));
    }

    #[test]
    fn plan_ubuntu_no_runtime_installs_chosen_runtime() {
        let o = Options {
            runtime: Some(Runtime::Podman),
            ..Default::default()
        };
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &o);
        assert!(has(&steps, "sudo apt-get install -y podman"));
        assert!(has(&steps, "systemctl --user enable --now podman.socket"));
        assert!(!has(&steps, "docker.io"));
        let docker = Options {
            runtime: Some(Runtime::Docker),
            ..Default::default()
        };
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &docker);
        assert!(has(&steps, "sudo apt-get install -y docker.io"));
        assert!(has(&steps, "sudo systemctl enable --now docker"));
        assert!(has(&steps, "sudo usermod -aG docker ana"));
    }

    #[test]
    fn plan_linux_without_a_pick_asks_for_one() {
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &Options::default());
        assert!(steps.iter().any(|s| s.id == "runtime"
            && matches!(s.kind, StepKind::Manual(ref t) if t.contains("--gateways"))));
    }

    #[test]
    fn plan_skips_runtime_when_podman_ready() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.runtimes_installed = vec![Runtime::Podman];
        f.runtimes_ready = vec![Runtime::Podman];
        let o = Options {
            runtime: Some(Runtime::Docker),
            ..Default::default()
        };
        let steps = plan(&f, &o);
        assert!(!has(&steps, "docker"));
        assert!(!ids(&steps).contains(&"runtime"));
    }

    #[test]
    fn plan_installed_but_stopped_docker_is_manual_hint() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.runtimes_installed = vec![Runtime::Docker];
        let steps = plan(&f, &Options::default());
        let hint = steps.iter().find(|s| s.id == "runtime").unwrap();
        assert!(matches!(&hint.kind, StepKind::Manual(t) if t.contains("systemctl start docker")));
        assert!(!has(&steps, "apt-get install -y docker"));
    }

    #[test]
    fn plan_unknown_distro_is_all_manual() {
        let o = Options {
            runtime: Some(Runtime::Docker),
            ..Default::default()
        };
        let steps = plan(&facts(Os::Linux, PkgMgr::None), &o);
        for s in steps.iter().filter(|s| s.id != "openshell") {
            assert!(
                matches!(s.kind, StepKind::Manual(_)),
                "{} should be manual",
                s.id
            );
        }
    }

    #[test]
    fn plan_without_sudo_marks_pkg_steps_manual() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_sudo = false;
        let o = Options {
            runtime: Some(Runtime::Podman),
            ..Default::default()
        };
        let steps = plan(&f, &o);
        let pkg = steps.iter().find(|s| s.id == "runtime").unwrap();
        assert!(
            matches!(&pkg.kind, StepKind::Manual(t) if t.contains("apt-get install -y podman"))
        );
    }

    #[test]
    fn plan_as_root_runs_package_manager_without_sudo() {
        let mut f = facts(Os::Linux, PkgMgr::Dnf);
        f.is_root = true;
        f.has_sudo = false;
        let o = Options {
            runtime: Some(Runtime::Podman),
            ..Default::default()
        };
        let steps = plan(&f, &o);
        assert!(has(&steps, "dnf install -y podman"));
        assert!(!has(&steps, "sudo"));
    }

    #[test]
    fn plan_installs_missing_optional_tools_and_marks_them_optional() {
        let steps = plan(&facts(Os::Linux, PkgMgr::Pacman), &Options::default());
        let gh = steps.iter().find(|s| s.id == "gh").unwrap();
        assert!(gh.optional && gh.command_line().contains("github-cli"));
        assert!(has(&steps, "xclip"));
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.wayland = true;
        assert!(has(&plan(&f, &Options::default()), "wl-clipboard"));
    }

    #[test]
    fn plan_apt_updates_the_index_before_installing() {
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &Options::default());
        let ids = ids(&steps);
        let update = ids.iter().position(|i| *i == "apt-update").unwrap();
        let tmux = ids.iter().position(|i| *i == "tmux").unwrap();
        assert!(update < tmux);
    }

    #[test]
    fn plan_is_empty_when_everything_present() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        f.has_tmux = true;
        f.has_gh = true;
        f.has_clipboard = true;
        f.runtimes_installed = vec![Runtime::Docker];
        f.runtimes_ready = vec![Runtime::Docker];
        assert!(plan(&f, &Options::default()).is_empty());
    }

    #[test]
    fn plan_installs_curl_before_openshell_when_missing() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_curl = false;
        let steps = plan(&f, &Options::default());
        let ids = ids(&steps);
        assert!(
            ids.iter().position(|i| *i == "curl").unwrap()
                < ids.iter().position(|i| *i == "openshell").unwrap()
        );
        let openshell = steps.iter().find(|s| s.id == "openshell").unwrap();
        // No runtime was picked either, and that step sits between curl and OpenShell.
        assert_eq!(openshell.after, Some("runtime"));
        assert_eq!(
            steps.iter().find(|s| s.id == "runtime").unwrap().after,
            Some("curl")
        );
    }

    #[test]
    fn linux_installs_the_runtime_before_openshell_whose_installer_waits_for_the_gateway() {
        let o = Options {
            runtime: Some(Runtime::Podman),
            ..Default::default()
        };
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &o);
        let ids = ids(&steps);
        let pos = |id: &str| ids.iter().position(|i| *i == id).unwrap();
        assert!(pos("runtime") < pos("openshell"));
        assert!(pos("runtime-service") < pos("openshell"));
        let openshell = steps.iter().find(|s| s.id == "openshell").unwrap();
        assert_eq!(openshell.after, Some("runtime-service"));
    }

    #[test]
    fn a_fresh_docker_install_stops_for_a_new_login_before_openshell() {
        let o = Options {
            runtime: Some(Runtime::Docker),
            ..Default::default()
        };
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &o);
        let relogin = steps.iter().find(|s| s.id == "runtime-relogin").unwrap();
        assert!(matches!(&relogin.kind, StepKind::Manual(t) if t.contains("log out")));
        let openshell = steps.iter().find(|s| s.id == "openshell").unwrap();
        assert_eq!(openshell.after, Some("runtime-relogin"));
        // Root has no group to join, so nothing to wait for.
        let mut root = facts(Os::Linux, PkgMgr::Apt);
        root.is_root = true;
        let steps = plan(&root, &o);
        assert!(steps.iter().all(|s| s.id != "runtime-relogin"));
        // Podman's socket is a user unit started right away: no new login.
        let podman = Options {
            runtime: Some(Runtime::Podman),
            ..Default::default()
        };
        assert!(plan(&facts(Os::Linux, PkgMgr::Apt), &podman)
            .iter()
            .all(|s| s.id != "runtime-relogin"));
    }

    #[test]
    fn macos_keeps_openshell_first_since_the_vm_gateway_needs_no_runtime() {
        let o = Options {
            runtime: Some(Runtime::Podman),
            ..Default::default()
        };
        let steps = plan(&facts(Os::MacOs, PkgMgr::Brew), &o);
        let ids = ids(&steps);
        assert!(
            ids.iter().position(|i| *i == "openshell") < ids.iter().position(|i| *i == "runtime")
        );
    }

    #[test]
    fn openshell_waits_for_a_runtime_that_needs_the_user_to_act() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.runtimes_installed = vec![Runtime::Docker];
        f.docker_permission_denied = true;
        let steps = plan(&f, &Options::default());
        let openshell = steps.iter().find(|s| s.id == "openshell").unwrap();
        assert_eq!(openshell.after, Some("runtime-relogin"));
    }

    fn gw_ids(steps: &[Step]) -> Vec<&'static str> {
        steps.iter().map(|s| s.id).collect()
    }

    fn with_gateways(kinds: &[GwKind]) -> Options {
        Options {
            gateways: kinds.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn linux_main_prereqs_come_before_openshell_and_extras_after() {
        let steps = plan(
            &facts(Os::Linux, PkgMgr::Apt),
            &with_gateways(&[GwKind::Docker, GwKind::Podman]),
        );
        let ids = gw_ids(&steps);
        let pos = |id: &str| {
            ids.iter()
                .position(|i| *i == id)
                .unwrap_or_else(|| panic!("{id} in {ids:?}"))
        };
        assert!(
            pos("runtime") < pos("openshell"),
            "the main's runtime first: {ids:?}"
        );
        assert!(
            pos("openshell") < pos("runtime-2"),
            "the extra's runtime after OpenShell: {ids:?}"
        );
        let docker = steps
            .iter()
            .find(|s| s.id == "runtime")
            .unwrap()
            .command_line();
        assert!(docker.contains("docker.io"), "{docker}");
        let podman = steps
            .iter()
            .find(|s| s.id == "runtime-2")
            .unwrap()
            .command_line();
        assert!(podman.contains("podman"), "{podman}");
    }

    #[test]
    fn every_step_id_is_unique_when_two_runtimes_are_planned() {
        let steps = plan(
            &facts(Os::Linux, PkgMgr::Apt),
            &with_gateways(&[GwKind::Docker, GwKind::Podman]),
        );
        let mut ids = gw_ids(&steps);
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate ids: {ids:?}");
        for s in &steps {
            if let Some(a) = s.after {
                assert!(
                    steps.iter().any(|x| x.id == a),
                    "{} waits for missing {a}",
                    s.id
                );
            }
        }
    }

    #[test]
    fn a_ready_type_adds_no_prerequisite_steps() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.runtimes_installed = vec![Runtime::Docker];
        f.runtimes_ready = vec![Runtime::Docker];
        let steps = plan(&f, &with_gateways(&[GwKind::Docker]));
        assert!(!has(&steps, "docker"), "{:?}", lines(&steps));
        assert!(steps.iter().all(|s| !s.id.starts_with("runtime")));
    }

    #[test]
    fn vm_on_linux_needs_the_kvm_group_then_a_new_login() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        f.kvm = Kvm::NoAccess;
        let steps = plan(&f, &with_gateways(&[GwKind::Vm]));
        assert!(has(&steps, "sudo usermod -aG kvm ana"));
        assert!(has(&steps, "apt-get install -y e2fsprogs"));
        let relogin = steps.iter().find(|s| s.id == "kvm-relogin").unwrap();
        assert!(matches!(&relogin.kind, StepKind::Manual(t) if t.contains("log out")));
        assert_eq!(relogin.after, Some("kvm-group"));
    }

    #[test]
    fn vm_without_kvm_is_a_manual_dead_end_with_the_reason() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        let steps = plan(&f, &with_gateways(&[GwKind::Vm]));
        let s = steps.iter().find(|s| s.id == "kvm").unwrap();
        assert!(matches!(&s.kind, StepKind::Manual(t) if t.contains("/dev/kvm")));
    }

    #[test]
    fn plan_installs_colima_and_starts_it_for_docker_on_macos() {
        let mut f = facts(Os::MacOs, PkgMgr::Brew);
        f.has_openshell = true;
        let steps = plan(&f, &with_gateways(&[GwKind::Docker]));
        assert!(has(&steps, "brew install colima docker"));
        let start = steps.iter().find(|s| s.id == "colima-start").unwrap();
        assert_eq!(start.command_line(), "colima start");
        assert_eq!(start.after, Some("colima"));
    }

    #[test]
    fn docker_desktop_on_macos_asks_the_user_to_start_it() {
        let mut f = facts(Os::MacOs, PkgMgr::Brew);
        f.has_openshell = true;
        f.docker_desktop = true;
        let steps = plan(&f, &with_gateways(&[GwKind::Docker]));
        let s = steps.iter().find(|s| s.id == "docker-desktop").unwrap();
        assert!(matches!(&s.kind, StepKind::Manual(t) if t.contains("Docker Desktop")));
        assert!(!has(&steps, "colima"));
    }

    #[test]
    fn plan_inits_and_starts_the_podman_machine_on_macos() {
        let mut f = facts(Os::MacOs, PkgMgr::Brew);
        f.has_openshell = true;
        f.runtimes_installed = vec![Runtime::Podman];
        let steps = plan(&f, &with_gateways(&[GwKind::Podman]));
        assert!(has(&steps, "podman machine init"));
        assert!(has(&steps, "podman machine start"));
        f.podman_machine = PodmanMachine::Stopped;
        let steps = plan(&f, &with_gateways(&[GwKind::Podman]));
        assert!(!has(&steps, "podman machine init") && has(&steps, "podman machine start"));
    }

    #[test]
    fn vm_on_macos_installs_e2fsprogs_once_even_with_a_fresh_openshell() {
        let steps = plan(
            &facts(Os::MacOs, PkgMgr::Brew),
            &with_gateways(&[GwKind::Vm]),
        );
        let n = steps.iter().filter(|s| s.id == "e2fsprogs").count();
        assert_eq!(n, 1, "{:?}", lines(&steps));
    }

    #[test]
    fn gateway_plans_still_offer_the_optional_tools() {
        let steps = plan(
            &facts(Os::Linux, PkgMgr::Apt),
            &with_gateways(&[GwKind::Podman]),
        );
        assert!(steps.iter().any(|s| s.id == "tmux" && s.optional));
    }

    fn gw_paths() -> crate::gateway::Paths {
        crate::gateway::Paths {
            home: "/h".into(),
            config_home: "/h/.config/openshell".into(),
            state: "/h/.local/state/openshell".into(),
            share: "/h/.local/share/openshell".into(),
            data: "/h/.local/share/osh".into(),
            exe: "/usr/local/bin/osh".into(),
        }
    }

    fn gw_plan(
        f: &Facts,
        kinds: &[GwKind],
        reg: &crate::gwmodel::Registry,
        existing: &[(String, u16)],
    ) -> Result<Vec<Step>, String> {
        plan_gateways(
            f,
            &with_gateways(kinds),
            &FakeHost::default(),
            &gw_paths(),
            reg,
            existing,
            &|_| false,
        )
    }

    fn written_json(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .filter_map(|s| match &s.kind {
                StepKind::Write { path, content } if path.ends_with("gateways.json") => {
                    Some(content.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn gateways_flag_orders_main_first_and_names_gateways_after_their_type() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        f.runtimes_installed = vec![Runtime::Docker, Runtime::Podman];
        f.runtimes_ready = vec![Runtime::Docker, Runtime::Podman];
        let steps = gw_plan(
            &f,
            &[GwKind::Podman, GwKind::Docker],
            &crate::gwmodel::Registry::default(),
            &[],
        )
        .unwrap();
        let main_cfg = steps
            .iter()
            .find_map(|s| match &s.kind {
                StepKind::Write { path, content } if path.ends_with("gateway.toml") => {
                    Some(content.clone())
                }
                _ => None,
            })
            .unwrap();
        assert!(
            main_cfg.contains("name = \"openshell-podman\""),
            "the first is main: {main_cfg}"
        );
        let t = lines(&steps).join("\n");
        assert!(t.contains("write /h/.local/share/osh/gateways/openshell-docker.toml"));
        let regs = written_json(&steps);
        assert!(
            regs.last().unwrap().contains("\"openshell-podman\"")
                && regs.last().unwrap().contains("17671")
        );
    }

    #[test]
    fn vm_without_kvm_is_refused_with_the_reason() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        let e = gw_plan(&f, &[GwKind::Vm], &crate::gwmodel::Registry::default(), &[]).unwrap_err();
        assert!(e.contains("VM") && e.contains("/dev/kvm"), "{e}");
    }

    #[test]
    fn an_existing_main_makes_every_ticked_type_an_extra_on_free_ports() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        f.kvm = Kvm::Usable;
        f.has_e2fsprogs = true;
        f.runtimes_installed = vec![Runtime::Podman];
        f.runtimes_ready = vec![Runtime::Podman];
        let mut reg = crate::gwmodel::Registry::default();
        reg.add(crate::gwmodel::GatewaySpec {
            name: "openshell-docker".into(),
            kind: GwKind::Docker,
            port: 17670,
            main: true,
        });
        let steps = plan_gateways(
            &f,
            &with_gateways(&[GwKind::Podman, GwKind::Vm]),
            &FakeHost::default(),
            &gw_paths(),
            &reg,
            &[],
            &|p| p == 17671,
        )
        .unwrap();
        let t = lines(&steps).join("\n");
        assert!(
            !t.contains("write /h/.config/openshell/gateway.toml"),
            "no main steps: {t}"
        );
        let last = written_json(&steps).pop().unwrap();
        assert!(last.contains("\"openshell-docker\"") && last.contains("\"openshell-podman\""));
        assert!(
            last.contains("17672") && last.contains("17673"),
            "17671 is bound: {last}"
        );
    }

    #[test]
    fn kinds_already_registered_or_known_to_the_cli_are_skipped() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        let mut reg = crate::gwmodel::Registry::default();
        reg.add(crate::gwmodel::GatewaySpec {
            name: "openshell-podman".into(),
            kind: GwKind::Podman,
            port: 17670,
            main: true,
        });
        // The registered main is healthy: its config is current and its service runs.
        let mut h = FakeHost::default();
        h.files.insert(
            "/h/.config/openshell/gateway.toml".into(),
            crate::gwops::gateway_toml(reg.get("openshell-podman").unwrap(), Os::Linux),
        );
        let steps = plan_gateways(
            &f,
            &with_gateways(&[GwKind::Podman]),
            &h,
            &gw_paths(),
            &reg,
            &[],
            &|_| false,
        )
        .unwrap();
        assert!(steps.is_empty());
        let legacy = [("openshell-docker".to_string(), 17671u16)];
        assert!(gw_plan(
            &f,
            &[GwKind::Docker],
            &crate::gwmodel::Registry::default(),
            &legacy
        )
        .unwrap()
        .is_empty());
    }

    fn registered(kinds: &[(GwKind, u16, bool)]) -> crate::gwmodel::Registry {
        let mut reg = crate::gwmodel::Registry::default();
        for (k, port, main) in kinds {
            reg.add(crate::gwmodel::GatewaySpec {
                name: k.name().into(),
                kind: *k,
                port: *port,
                main: *main,
            });
        }
        reg
    }

    fn running_facts() -> Facts {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        f.runtimes_installed = vec![Runtime::Docker, Runtime::Podman];
        f.runtimes_ready = vec![Runtime::Docker, Runtime::Podman];
        f
    }

    #[test]
    fn healthy_registered_gateways_need_nothing() {
        let f = running_facts();
        let reg = registered(&[
            (GwKind::Docker, 17670, true),
            (GwKind::Podman, 17671, false),
        ]);
        let mut h = FakeHost::default();
        h.files.insert(
            "/h/.config/openshell/gateway.toml".into(),
            crate::gwops::gateway_toml(reg.get("openshell-docker").unwrap(), Os::Linux),
        );
        let steps = plan_gateways(
            &f,
            &with_gateways(&[GwKind::Docker, GwKind::Podman]),
            &h,
            &gw_paths(),
            &reg,
            &[],
            &|_| false,
        )
        .unwrap();
        assert!(steps.is_empty(), "{:?}", lines(&steps));
    }

    #[test]
    fn a_registered_main_whose_service_is_down_gets_its_restart_steps() {
        let f = running_facts();
        let reg = registered(&[(GwKind::Docker, 17670, true)]);
        let mut h = FakeHost::default();
        h.files.insert(
            "/h/.config/openshell/gateway.toml".into(),
            crate::gwops::gateway_toml(reg.get("openshell-docker").unwrap(), Os::Linux),
        );
        h.failing.insert("systemctl".into());
        let steps = plan_gateways(
            &f,
            &with_gateways(&[GwKind::Docker]),
            &h,
            &gw_paths(),
            &reg,
            &[],
            &|_| false,
        )
        .unwrap();
        assert!(lines(&steps)
            .join("\n")
            .contains("systemctl --user restart openshell-gateway"));
    }

    #[test]
    fn a_registered_extra_that_is_not_running_is_started_again() {
        let f = running_facts();
        let reg = registered(&[
            (GwKind::Docker, 17670, true),
            (GwKind::Podman, 17671, false),
        ]);
        let mut h = FakeHost::default();
        h.files.insert(
            "/h/.config/openshell/gateway.toml".into(),
            crate::gwops::gateway_toml(reg.get("openshell-docker").unwrap(), Os::Linux),
        );
        h.failing.insert(
            "systemctl --user is-active --quiet osh-gateway-openshell-podman.service".into(),
        );
        let steps = plan_gateways(
            &f,
            &with_gateways(&[GwKind::Docker, GwKind::Podman]),
            &h,
            &gw_paths(),
            &reg,
            &[],
            &|_| false,
        )
        .unwrap();
        let t = lines(&steps).join("\n");
        assert!(
            t.contains("enable --now osh-gateway-openshell-podman.service"),
            "{t}"
        );
        assert!(
            !t.contains("openshell-gateway"),
            "the healthy main is left alone: {t}"
        );
    }

    #[test]
    fn a_legacy_openshell_gateway_counts_as_an_existing_main() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        f.runtimes_installed = vec![Runtime::Podman];
        f.runtimes_ready = vec![Runtime::Podman];
        let legacy = [("openshell".to_string(), 17670u16)];
        let steps = gw_plan(
            &f,
            &[GwKind::Podman],
            &crate::gwmodel::Registry::default(),
            &legacy,
        )
        .unwrap();
        let t = lines(&steps).join("\n");
        assert!(
            t.contains("osh-gateway-openshell-podman.service"),
            "an extra, the legacy main is untouched: {t}"
        );
        assert!(!t.contains("write /h/.config/openshell/gateway.toml"));
        assert!(written_json(&steps).last().unwrap().contains("17671"));
    }

    #[test]
    fn gateway_steps_wait_for_openshell_and_for_their_prerequisites() {
        let f = facts(Os::Linux, PkgMgr::Apt);
        let steps = gw_plan(
            &f,
            &[GwKind::Podman],
            &crate::gwmodel::Registry::default(),
            &[],
        )
        .unwrap();
        let ids: Vec<_> = steps.iter().map(|s| s.id).collect();
        let first_gw = steps
            .iter()
            .find(|s| s.id.starts_with("gw-podman"))
            .unwrap();
        assert_eq!(first_gw.after, Some("openshell"));
        for s in &steps {
            if let Some(a) = s.after {
                assert!(ids.contains(&a), "{} waits for missing {a}", s.id);
            }
        }
    }

    #[test]
    fn parse_gateways_accepts_a_list_of_types_in_order() {
        assert_eq!(
            parse_gateways("docker,podman").unwrap(),
            vec![GwKind::Docker, GwKind::Podman]
        );
        assert_eq!(parse_gateways("openshell-vm").unwrap(), vec![GwKind::Vm]);
        assert_eq!(
            parse_gateways("docker,docker").unwrap(),
            vec![GwKind::Docker]
        );
        assert!(parse_gateways("docker,lxc").unwrap_err().contains("lxc"));
        assert!(parse_gateways("").is_err());
    }

    #[test]
    fn existing_gateways_are_read_from_the_cli_listing() {
        let mut h = FakeHost::default();
        h.outputs.insert(
            "openshell gateway list -o json".into(),
            r#"[{"name":"openshell","endpoint":"https://127.0.0.1:17670","active":true},{"name":"openshell-docker","endpoint":"https://localhost:17671"}]"#.into(),
        );
        assert_eq!(
            existing_gateways(&h),
            vec![
                ("openshell".to_string(), 17670),
                ("openshell-docker".to_string(), 17671)
            ]
        );
        assert!(existing_gateways(&FakeHost::default()).is_empty());
    }

    #[test]
    fn execute_events_reports_start_lines_and_done() {
        let mut h = FakeHost::default();
        h.stream_lines.insert(
            "sudo apt-get install -y tmux".into(),
            vec!["Reading lists".into(), "Done".into()],
        );
        let f = facts(Os::Linux, PkgMgr::Apt);
        let steps: Vec<Step> = plan(&f, &Options::default())
            .into_iter()
            .filter(|s| s.id == "tmux")
            .collect();
        let o = Options {
            capture: true,
            ..Default::default()
        };
        let mut events = Vec::new();
        let r = execute_events(&steps, &h, &o, &mut yes, &mut |e| events.push(e));
        assert_eq!(r[0].outcome, Outcome::Done);
        assert_eq!(
            events,
            vec![
                Event::Start(0),
                Event::Line(0, "Reading lists".into()),
                Event::Line(0, "Done".into()),
                Event::Done(0, Outcome::Done),
            ]
        );
    }

    #[test]
    fn execute_events_reports_blocked_and_manual_steps_too() {
        let h = FakeHost::default();
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &Options::default());
        let mut events = Vec::new();
        execute_events(&steps, &h, &Options::default(), &mut yes, &mut |e| {
            events.push(e)
        });
        let done: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::Done(..)))
            .collect();
        assert_eq!(done.len(), steps.len(), "every step ends with a Done event");
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::Done(_, Outcome::Manual))));
    }

    #[test]
    fn sudo_noninteractive_rewrites_sudo_to_sudo_n() {
        let h = FakeHost::default();
        let f = facts(Os::Linux, PkgMgr::Apt);
        let steps: Vec<Step> = plan(&f, &Options::default())
            .into_iter()
            .filter(|s| s.id == "tmux")
            .collect();
        let o = Options {
            capture: true,
            sudo_noninteractive: true,
            ..Default::default()
        };
        execute_events(&steps, &h, &o, &mut yes, &mut |_| {});
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|c| c == "sudo -n apt-get install -y tmux"),
            "{:?}",
            h.calls.borrow()
        );
    }

    fn yes(_: &Step) -> Answer {
        Answer::Yes
    }

    #[test]
    fn execute_continues_after_failed_independent_step() {
        let mut h = FakeHost::default();
        h.failing.insert("sudo apt-get install -y tmux".into());
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &Options::default());
        let r = execute(&steps, &h, &Options::default(), &mut yes);
        let tmux = r.iter().find(|r| r.id == "tmux").unwrap();
        assert!(matches!(tmux.outcome, Outcome::Failed(_)));
        let gh = r.iter().find(|r| r.id == "gh").unwrap();
        assert_eq!(gh.outcome, Outcome::Done);
    }

    #[test]
    fn execute_blocks_steps_whose_dependency_failed() {
        let mut h = FakeHost::default();
        h.failing.insert("curl".into());
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_curl = false;
        f.has_tmux = true;
        f.has_gh = true;
        f.has_clipboard = true;
        h.failing.insert("sudo apt-get install -y curl".into());
        let r = execute(
            &plan(&f, &Options::default()),
            &h,
            &Options::default(),
            &mut yes,
        );
        assert_eq!(
            r.iter().find(|r| r.id == "openshell").unwrap().outcome,
            Outcome::Blocked
        );
    }

    #[test]
    fn execute_dry_run_runs_nothing() {
        let h = FakeHost::default();
        let o = Options {
            dry_run: true,
            ..Default::default()
        };
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &o);
        let r = execute(&steps, &h, &o, &mut yes);
        assert!(h.calls.borrow().is_empty());
        assert!(r.iter().any(|r| r.outcome == Outcome::DryRun));
    }

    #[test]
    fn declined_step_is_not_run() {
        let h = FakeHost::default();
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &Options::default());
        let r = execute(&steps, &h, &Options::default(), &mut |_| Answer::No);
        assert!(h.calls.borrow().is_empty());
        // Steps behind the unresolved runtime choice are Blocked, not Declined.
        assert!(r.iter().all(|r| matches!(
            r.outcome,
            Outcome::Declined | Outcome::Manual | Outcome::Blocked
        )));
    }

    #[test]
    fn manual_steps_are_reported_not_run() {
        let h = FakeHost::default();
        let steps = plan(&facts(Os::MacOs, PkgMgr::None), &Options::default());
        let r = execute(&steps, &h, &Options::default(), &mut yes);
        assert_eq!(r[0].outcome, Outcome::Manual);
    }

    #[test]
    fn hint_names_the_install_command_for_the_package_manager() {
        assert_eq!(
            hint("tmux", Os::Linux, PkgMgr::Apt, false),
            "tmux is not installed: sudo apt-get install -y tmux"
        );
        assert_eq!(
            hint("gh", Os::Linux, PkgMgr::Pacman, false),
            "gh is not installed: sudo pacman -S --noconfirm --needed github-cli"
        );
        assert_eq!(
            hint("tmux", Os::MacOs, PkgMgr::Brew, false),
            "tmux is not installed: brew install tmux"
        );
        assert!(hint("tmux", Os::Linux, PkgMgr::None, false).contains("your package manager"));
    }

    #[test]
    fn clipboard_hint_names_the_tool_for_the_os() {
        assert!(hint("clipboard", Os::MacOs, PkgMgr::Brew, false).contains("brew install pngpaste"));
        assert!(hint("clipboard", Os::Linux, PkgMgr::Apt, false).contains("xclip"));
        assert!(hint("clipboard", Os::Linux, PkgMgr::Apt, true).contains("wl-clipboard"));
    }

    #[test]
    fn display_quotes_so_the_line_can_be_pasted_into_a_shell() {
        let sh = |script: &str| Step {
            id: "a",
            title: "t".into(),
            kind: StepKind::Cmd {
                prog: "sh".into(),
                args: vec!["-c".into(), script.into()],
            },
            optional: false,
            after: None,
        };
        assert_eq!(sh("echo hi").display(), "sh -c 'echo hi'");
        // A single quote inside the script is closed, escaped and reopened.
        assert_eq!(
            sh("trap 'rm -f x' EXIT").display(),
            r#"sh -c 'trap '\''rm -f x'\'' EXIT'"#
        );
        assert_eq!(sh("set -e\nrm -rf x").display(), "sh -c 'set -e; rm -rf x'");
        // After `then`, `else`, `do`, `{`, `&&`, `||` the next line continues the same command.
        let branchy = sh("if a; then\n  b\nelse\n  c\nfi");
        assert_eq!(branchy.display(), "sh -c 'if a; then b; else c; fi'");
        let shown = branchy.display();
        let word = shown.strip_prefix("sh -c ").unwrap();
        let parsed = std::process::Command::new("sh")
            .args(["-n", "-c", &format!("sh -c {word}")])
            .status()
            .unwrap();
        assert!(parsed.success(), "the shown line must parse as shell");
        let long = sh("1\n2\n3\n4\n5\n6\nrm -rf x");
        assert_eq!(long.display(), "sh -c '1 ...'");
        assert!(long.command_line().contains("rm -rf x"));
    }

    #[test]
    fn sh_quote_round_trips_through_a_real_shell() {
        for text in ["plain", "it's", "a b", "$(x) `y` \"z\"", "", "'"] {
            let out = std::process::Command::new("sh")
                .args(["-c", &format!("printf %s {}", sh_quote(text))])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), text, "{text:?}");
        }
    }

    /// Runs the OpenShell install script with a fake `curl` (and optionally a fake `openshell`).
    fn run_install_script(curl_body: &str, with_openshell: bool) -> std::process::Output {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!(
            "osh-install-{}-{}",
            std::process::id(),
            curl_body.len() + usize::from(with_openshell)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let put = |name: &str, body: &str| {
            let p = d.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        put("curl", curl_body);
        if with_openshell {
            put("openshell", "exit 0");
        }
        let out = std::process::Command::new("sh")
            .args(["-c", OPENSHELL_INSTALL])
            .env("PATH", format!("{}:/usr/bin:/bin", d.display()))
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&d);
        out
    }

    #[test]
    fn openshell_install_fails_when_the_download_fails() {
        let out = run_install_script("exit 22", true);
        assert!(!out.status.success(), "a failed curl must fail the step");
    }

    #[test]
    fn openshell_install_fails_when_openshell_is_still_missing_afterwards() {
        // curl writes a script that does nothing, so the installer "succeeds" without installing.
        let out = run_install_script(
            "while [ $# -gt 0 ]; do [ \"$1\" = -o ] && echo : > \"$2\"; shift; done",
            false,
        );
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("openshell"));
    }

    #[test]
    fn openshell_install_passes_when_the_installer_ran_and_openshell_exists() {
        let out = run_install_script(
            "while [ $# -gt 0 ]; do [ \"$1\" = -o ] && echo : > \"$2\"; shift; done",
            true,
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn consent_is_asked_with_a_terminal_and_refused_without_one_unless_yes() {
        let ask = Options::default();
        let yes = Options {
            yes: true,
            ..Default::default()
        };
        let dry = Options {
            dry_run: true,
            ..Default::default()
        };
        assert_eq!(consent(&ask, true), Consent::Ask);
        assert_eq!(consent(&ask, false), Consent::Refuse);
        assert_eq!(consent(&yes, false), Consent::All);
        assert_eq!(consent(&yes, true), Consent::All);
        assert_eq!(
            consent(&dry, false),
            Consent::All,
            "a dry run changes nothing"
        );
    }

    #[test]
    fn plan_enables_the_podman_socket_when_podman_answers_without_it() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        f.runtimes_installed = vec![Runtime::Podman];
        f.podman_needs_socket = true;
        let steps = plan(&f, &Options::default());
        let s = steps.iter().find(|s| s.id == "runtime").unwrap();
        assert_eq!(
            s.command_line(),
            "systemctl --user enable --now podman.socket"
        );
        assert!(!has(&steps, "apt-get install -y podman"));
    }

    #[test]
    fn plan_adds_the_user_to_the_docker_group_on_permission_denied() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.has_openshell = true;
        f.runtimes_installed = vec![Runtime::Docker];
        f.docker_permission_denied = true;
        let steps = plan(&f, &Options::default());
        assert!(has(&steps, "sudo usermod -aG docker ana"));
        let relogin = steps.iter().find(|s| s.id == "runtime-relogin").unwrap();
        assert!(matches!(&relogin.kind, StepKind::Manual(t) if t.contains("log out")));
        assert!(
            !has(&steps, "systemctl start docker"),
            "the daemon is already running"
        );
    }

    #[test]
    fn summary_lists_failures_and_manual_steps() {
        let steps = plan(&facts(Os::MacOs, PkgMgr::None), &Options::default());
        let r = vec![
            StepResult {
                id: "homebrew",
                outcome: Outcome::Manual,
            },
            StepResult {
                id: "openshell",
                outcome: Outcome::Failed("boom".into()),
            },
        ];
        let text = summary(&steps[..2], &r);
        assert!(text.contains("do yourself: Install Homebrew"));
        assert!(text.contains("failed: Install OpenShell (boom)"));
    }

    #[test]
    fn doctor_flags_missing_openshell_and_runtime() {
        let f = gather_for(&FakeHost::default(), Os::Linux);
        let (text, ok) = doctor(&f);
        assert!(!ok);
        assert!(text.contains("MISSING  openshell"));
        assert!(text.contains("MISSING  container runtime"));
        assert!(text.contains("optional tmux"));
    }

    #[test]
    fn doctor_tells_a_permission_problem_from_a_stopped_daemon() {
        let mut f = facts(Os::Linux, PkgMgr::Apt);
        f.runtimes_installed = vec![Runtime::Docker];
        f.docker_permission_denied = true;
        let (text, ok) = doctor(&f);
        assert!(!ok);
        assert!(
            text.contains("not allowed to use docker") && text.contains("log out"),
            "{text}"
        );
        f.docker_permission_denied = false;
        assert!(doctor(&f).0.contains("not answering"));
    }

    #[test]
    fn a_dry_run_succeeds_even_when_it_lists_steps_only_the_user_can_do() {
        let steps = plan(&facts(Os::Linux, PkgMgr::Apt), &Options::default());
        let results: Vec<StepResult> = steps
            .iter()
            .map(|s| StepResult {
                id: s.id,
                outcome: Outcome::DryRun,
            })
            .collect();
        let dry = Options {
            dry_run: true,
            ..Default::default()
        };
        assert!(run_ok(&steps, &results, &dry));
        // A real run with the runtime choice still open did not finish.
        let manual: Vec<StepResult> = steps
            .iter()
            .map(|s| StepResult {
                id: s.id,
                outcome: if matches!(s.kind, StepKind::Manual(_)) {
                    Outcome::Manual
                } else {
                    Outcome::Done
                },
            })
            .collect();
        assert!(!run_ok(&steps, &manual, &Options::default()));
    }

    #[test]
    fn doctor_is_ok_with_openshell_and_a_ready_runtime() {
        let h = FakeHost::with_bins(&["openshell", "podman"]);
        let (text, ok) = doctor(&gather_for(&h, Os::Linux));
        assert!(ok, "{text}");
        assert!(text.contains("ok       container runtime (podman)"));
    }
}
