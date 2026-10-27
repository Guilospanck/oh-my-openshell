//! Turns facts about the host into an ordered list of install steps and runs them with consent.
//! Planning is pure (Facts in, steps out); only `execute` touches the host, through `Host`.
use crate::platform::{gather, Facts, Host, Os, PkgMgr, RealHost, Runtime};
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
                    format!(
                        "sh -c {}",
                        sh_quote(&script.lines().collect::<Vec<_>>().join("; "))
                    )
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

struct Planner<'a> {
    f: &'a Facts,
    steps: Vec<Step>,
    apt_updated: bool,
}

impl Planner<'_> {
    fn push(&mut self, id: &'static str, title: &str, kind: StepKind, optional: bool) {
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
        let f = self.f;
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
                        "no container runtime found: rerun with --runtime docker or --runtime podman"
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
                self.steps.last_mut().unwrap().after = Some("runtime");
                if rt == Runtime::Docker && !f.is_root && !f.user.is_empty() {
                    let kind = self.privileged(
                        ["usermod", "-aG", "docker", f.user.as_str()]
                            .map(String::from)
                            .to_vec(),
                    );
                    self.push(
                        "runtime-group",
                        "Let your user run docker (log out and in afterwards)",
                        kind,
                        false,
                    );
                    self.steps.last_mut().unwrap().after = Some("runtime");
                }
            }
        }
    }
}

pub fn plan(f: &Facts, o: &Options) -> Vec<Step> {
    let mut p = Planner {
        f,
        steps: Vec::new(),
        apt_updated: false,
    };
    let mac_brew = f.os == Os::MacOs && f.pkg == PkgMgr::Brew;
    if f.os == Os::MacOs && f.pkg == PkgMgr::None {
        p.push(
            "homebrew",
            "Install Homebrew",
            StepKind::Manual("install Homebrew from https://brew.sh, then run osh again".into()),
            false,
        );
    }
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
    p.runtime(o);
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

pub fn execute(
    steps: &[Step],
    h: &dyn Host,
    o: &Options,
    ask: &mut dyn FnMut(&Step) -> Answer,
) -> Vec<StepResult> {
    let mut results = Vec::new();
    let mut succeeded: Vec<&'static str> = Vec::new();
    for step in steps {
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
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    h.run_inherit(prog, &args)
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
            "osh setup --runtime docker|podman installs one".to_string()
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
    steps
        .iter()
        .zip(&results)
        .all(|(s, r)| s.optional || matches!(r.outcome, Outcome::Done | Outcome::DryRun))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testing::FakeHost;
    use crate::platform::{gather_for, PkgMgr};

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
            && matches!(s.kind, StepKind::Manual(ref t) if t.contains("--runtime"))));
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
        assert_eq!(openshell.after, Some("curl"));
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
        assert!(r
            .iter()
            .all(|r| matches!(r.outcome, Outcome::Declined | Outcome::Manual)));
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
    fn doctor_is_ok_with_openshell_and_a_ready_runtime() {
        let h = FakeHost::with_bins(&["openshell", "podman"]);
        let (text, ok) = doctor(&gather_for(&h, Os::Linux));
        assert!(ok, "{text}");
        assert!(text.contains("ok       container runtime (podman)"));
    }
}
