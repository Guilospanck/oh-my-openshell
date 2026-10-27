//! Gateway lifecycle in Rust: config install, service restart, the macOS Docker gateway,
//! reset and uninstall. Each returns `Step`s for `setup::execute`, so the logic is testable
//! without touching the host.
use crate::assets;
use crate::platform::{Facts, Host, Os, PkgMgr, Runtime};
use crate::setup::{Options, Step, StepKind};
use std::path::{Path, PathBuf};

pub const DOCKER_LABEL: &str = "dev.guilospanck.openshell-docker";
pub const DOCKER_GATEWAY: &str = "openshell-docker";

/// Polls the gateway for up to 20 seconds, then prints its status either way.
pub(crate) const WAIT_SCRIPT: &str = r#"for _ in $(seq 20); do openshell -g "$1" status >/dev/null 2>&1 && break; sleep 1; done; openshell -g "$1" status"#;

/// Deletes the gateway's sandboxes through the gateway. $1 = gateway name.
pub(crate) const DELETE_SANDBOXES_SCRIPT: &str = r#"if openshell -g "$1" status >/dev/null 2>&1; then
  openshell -g "$1" sandbox delete --all || echo "sandbox delete failed; sweeping leftovers next"
else
  echo "gateway not reachable; sweeping leftovers next"
fi"#;

/// Removes leftover containers and volumes of a Docker or Podman driver.
/// $1 = runtime binary, $2 = state dir, $3 = config dir, $4 = darwin flag.
pub(crate) const SWEEP_CONTAINERS_SCRIPT: &str = r#"[ -f "$3/gateway.env" ] && { set -a; . "$3/gateway.env"; set +a; }
[ "$4" = darwin ] && export DOCKER_HOST="${DOCKER_HOST:-unix://$HOME/.colima/docker.sock}"
if command -v "$1" >/dev/null 2>&1 && "$1" info >/dev/null 2>&1; then
  for f in label=openshell.ai/managed-by=openshell label=openshell.managed=true; do
    ids="$("$1" ps -aq --filter "$f")"
    [ -z "$ids" ] || "$1" rm -f $ids
  done
  vols="$("$1" volume ls -q --filter label=openshell.ai/managed-by=openshell)"
  [ -z "$vols" ] || "$1" volume rm $vols
else
  echo "$1 unreachable; containers not swept" >&2
fi
rm -rf "$2"/docker-sandbox-tokens/*"#;

/// vm driver microVMs are child processes; kill orphans, drop their state. $1 = state dir.
pub(crate) const SWEEP_VMS_SCRIPT: &str = r#"pkill -f "$1/vm-driver/sandboxes" 2>/dev/null || true
rm -rf "$1"/vm-driver/sandboxes/*"#;

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    /// `~/.config/openshell`: gateway.toml and gateway.env live here.
    pub config_home: PathBuf,
    /// `~/.local/state/openshell`
    pub state: PathBuf,
    /// `~/.local/share/openshell`
    pub share: PathBuf,
    /// The osh data dir: edited copies of the gateway configs there win over the embedded ones.
    pub data: PathBuf,
    /// This binary, for the launchd agent to run.
    pub exe: PathBuf,
}

impl Paths {
    /// From the process environment. Exits when HOME is not an absolute path: every path below
    /// hangs off it, and relative ones would act on the current directory.
    pub fn detect() -> Paths {
        let data = Some(assets::data_dir());
        Paths::from_env(
            std::env::var_os("HOME"),
            std::env::var_os("XDG_CONFIG_HOME"),
            data,
        )
        .unwrap_or_else(|e| {
            eprintln!("osh: {e}");
            std::process::exit(1);
        })
    }

    pub fn from_env(
        home: Option<std::ffi::OsString>,
        xdg_config: Option<std::ffi::OsString>,
        data: Option<PathBuf>,
    ) -> Result<Paths, String> {
        let home = crate::platform::valid_home(home)
            .ok_or("HOME is not set to an absolute path; refusing to touch relative paths")?;
        Ok(Paths {
            config_home: crate::platform::absolute(xdg_config)
                .unwrap_or_else(|| home.join(".config"))
                .join("openshell"),
            state: home.join(".local/state/openshell"),
            share: home.join(".local/share/openshell"),
            data: data.unwrap_or_else(|| home.join(".local/share/osh")),
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("osh")),
            home,
        })
    }

    fn plist(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{DOCKER_LABEL}.plist"))
    }
}

pub(crate) fn step(id: &'static str, title: &str, kind: StepKind) -> Step {
    Step {
        id,
        title: title.into(),
        kind,
        optional: false,
        after: None,
    }
}

pub(crate) fn cmd(prog: &str, args: &[&str]) -> StepKind {
    StepKind::Cmd {
        prog: prog.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
    }
}

/// `sh -c SCRIPT sh ARG...`: a fixed script with its inputs as positional parameters, never
/// spliced into the script text.
pub(crate) fn script(body: &str, args: &[&str]) -> StepKind {
    let mut a = vec!["-c", body, "sh"];
    a.extend_from_slice(args);
    cmd("sh", &a)
}

pub(crate) fn chain(mut steps: Vec<Step>, first_after: Option<&'static str>) -> Vec<Step> {
    let mut prev = first_after;
    for s in &mut steps {
        s.after = prev;
        prev = Some(s.id);
    }
    steps
}

/// Which embedded gateway config this host should run.
pub fn config_name(f: &Facts, wanted: Option<Runtime>) -> &'static str {
    let rt = f
        .runtime()
        .or(f.runtimes_installed.first().copied())
        .or(wanted);
    config_name_for(f.os, rt)
}

/// `rt` is the runtime in use or chosen; Docker when there is none.
pub fn config_name_for(os: Os, rt: Option<Runtime>) -> &'static str {
    if os == Os::MacOs {
        return "gateway.toml";
    }
    match rt.unwrap_or(Runtime::Docker) {
        Runtime::Docker => "gateway-linux.toml",
        Runtime::Podman => "gateway-podman.toml",
    }
}

/// The config to install: the data dir's copy (so edits there apply), else the embedded one.
fn wanted_config(name: &str, h: &dyn Host, p: &Paths) -> String {
    h.read(&p.data.join(name))
        .unwrap_or_else(|| assets::embedded(name).unwrap_or_default().to_string())
}

pub(crate) fn service_running(os: Os, h: &dyn Host) -> bool {
    match os {
        Os::MacOs => h.run("openshell", &["status"]).is_ok(),
        Os::Linux => h
            .run(
                "systemctl",
                &["--user", "is-active", "--quiet", "openshell-gateway"],
            )
            .is_ok(),
    }
}

/// Installs the gateway config and (re)starts the service. Empty when OpenShell is already
/// installed, the config is current, the service is running and `force` is off. A config that
/// is right but a service that is down still gets its restart steps, so a re-run resumes.
pub fn plan(f: &Facts, o: &Options, h: &dyn Host, p: &Paths, force: bool) -> Vec<Step> {
    let name = config_name(f, o.runtime);
    let wanted = wanted_config(name, h, p);
    let target = p.config_home.join("gateway.toml");
    let installing = !f.has_openshell;
    let installed = h.read(&target);
    let current = installed.as_deref() == Some(wanted.as_str());
    if !force && !installing && current && service_running(f.os, h) {
        return Vec::new();
    }
    let mut steps = Vec::new();
    if !current {
        if let Some(old) = installed.filter(|t| !t.is_empty()) {
            steps.push(step(
                "gateway-backup",
                "Keep a copy of your gateway.toml before replacing it",
                StepKind::Write {
                    path: p.config_home.join("gateway.toml.osh-bak"),
                    content: old,
                },
            ));
        }
        steps.push(step(
            "gateway-config",
            &format!("Install the gateway config ({name})"),
            StepKind::Write {
                path: target,
                content: wanted,
            },
        ));
    }
    match f.os {
        Os::MacOs if f.pkg == PkgMgr::Brew => steps.push(step(
            "gateway-restart",
            "Restart the OpenShell gateway service",
            cmd("brew", &["services", "restart", "openshell"]),
        )),
        Os::MacOs => steps.push(step(
            "gateway-restart",
            "Restart the OpenShell gateway service",
            StepKind::Manual("brew services restart openshell".into()),
        )),
        Os::Linux => {
            steps.push(step(
                "gateway-reload",
                "Reload systemd user units",
                cmd("systemctl", &["--user", "daemon-reload"]),
            ));
            steps.push(step(
                "gateway-enable",
                "Enable the gateway service",
                cmd("systemctl", &["--user", "enable", "openshell-gateway"]),
            ));
            steps.push(step(
                "gateway-restart",
                "Restart the OpenShell gateway service",
                cmd("systemctl", &["--user", "restart", "openshell-gateway"]),
            ));
        }
    }
    steps.push(step(
        "gateway-wait",
        "Wait for the gateway to answer",
        script(WAIT_SCRIPT, &["openshell"]),
    ));
    chain(steps, installing.then_some("openshell"))
}

/// Reinstalls OpenShell with its official installer, refreshes the config, restarts the
/// service, and restarts the Docker gateway agent when it exists.
pub fn upgrade_steps(f: &Facts, o: &Options, h: &dyn Host, p: &Paths) -> Vec<Step> {
    let mut f = f.clone();
    f.has_openshell = false;
    let mut steps = vec![step(
        "openshell",
        "Install the latest OpenShell",
        cmd("sh", &["-c", crate::setup::OPENSHELL_INSTALL]),
    )];
    steps.extend(plan(&f, o, h, p, true));
    if f.os == Os::MacOs {
        let mut s = step(
            "upgrade-docker-gateway",
            "Restart the Docker gateway if it is set up",
            script(
                r#"if launchctl print "gui/$(id -u)/$1" >/dev/null 2>&1; then launchctl kickstart -k "gui/$(id -u)/$1"; fi"#,
                &[DOCKER_LABEL],
            ),
        );
        s.after = steps.last().map(|l| l.id);
        steps.push(s);
    }
    steps
}

/// The container runtime the installed gateway.toml drives, from its `compute_driver` line.
pub fn driver_runtime(config: &str) -> Option<Runtime> {
    let line = config
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("compute_driver"))?;
    match line.split('"').nth(1)? {
        "docker" => Some(Runtime::Docker),
        "podman" => Some(Runtime::Podman),
        _ => None,
    }
}

/// Typed confirmation for wiping a gateway: even `--yes` does not skip it.
pub fn confirm_reset(typed: &str) -> bool {
    typed.trim() == "reset"
}

/// Wipes a local gateway: its sandboxes, then its database, then starts it empty.
/// Mac knows `openshell` (vm) and `openshell-docker`; Linux only `openshell`.
pub fn reset_steps(
    f: &Facts,
    p: &Paths,
    gateway: &str,
    rt: Option<Runtime>,
) -> Result<Vec<Step>, String> {
    #[derive(PartialEq)]
    enum Kind {
        Vm,
        Container,
    }
    let (kind, db_dir) = match (f.os, gateway) {
        (Os::MacOs, "openshell") => (Kind::Vm, p.state.join("gateway")),
        (Os::MacOs, DOCKER_GATEWAY) => (Kind::Container, p.state.join("gateway-docker")),
        (Os::Linux, "openshell") => (Kind::Container, p.state.join("gateway")),
        (Os::MacOs, _) => return Err("usage: openshell or openshell-docker".into()),
        (Os::Linux, _) => {
            return Err("only the gateway openshell exists on Linux (no openshell-docker)".into())
        }
    };
    let state = p.state.to_string_lossy().into_owned();
    let config = p.config_home.to_string_lossy().into_owned();
    let uid_target = |label: &str| format!("gui/$(id -u)/{label}");
    let (stop, start) = match (f.os, gateway) {
        (Os::Linux, _) => (
            cmd("systemctl", &["--user", "stop", "openshell-gateway"]),
            cmd("systemctl", &["--user", "start", "openshell-gateway"]),
        ),
        (Os::MacOs, "openshell") => (
            cmd("brew", &["services", "stop", "openshell"]),
            cmd("brew", &["services", "start", "openshell"]),
        ),
        _ => (
            script(
                &format!(
                    r#"launchctl bootout "{}" 2>/dev/null || true"#,
                    uid_target(DOCKER_LABEL)
                ),
                &[],
            ),
            script(
                r#"launchctl bootstrap "gui/$(id -u)" "$1""#,
                &[&p.plist().to_string_lossy()],
            ),
        ),
    };
    let sweep = match kind {
        Kind::Vm => script(SWEEP_VMS_SCRIPT, &[&state]),
        Kind::Container => script(
            SWEEP_CONTAINERS_SCRIPT,
            &[
                rt.unwrap_or(Runtime::Docker).bin(),
                &state,
                &config,
                if f.os == Os::MacOs { "darwin" } else { "linux" },
            ],
        ),
    };
    let db = |ext: &str| {
        db_dir
            .join(format!("openshell.db{ext}"))
            .to_string_lossy()
            .into_owned()
    };
    let steps = vec![
        step(
            "reset-sandboxes",
            "Delete the sandboxes through the gateway",
            script(DELETE_SANDBOXES_SCRIPT, &[gateway]),
        ),
        step("reset-stop", "Stop the gateway", stop),
        step("reset-settle", "Let it settle", cmd("sleep", &["2"])),
        step("reset-sweep", "Remove leftover sandbox runtimes", sweep),
        step(
            "reset-db",
            "Delete the database",
            cmd("rm", &["-f", &db(""), &db("-shm"), &db("-wal")]),
        ),
        step("reset-start", "Start the gateway", start),
        step(
            "reset-wait",
            "Wait for the gateway to answer",
            script(WAIT_SCRIPT, &[gateway]),
        ),
    ];
    Ok(chain(steps, None))
}

/// Removes OpenShell's files and services. The osh data directory stays.
pub fn uninstall_steps(f: &Facts, p: &Paths) -> Vec<Step> {
    let mut steps = Vec::new();
    match f.os {
        Os::MacOs => {
            steps.push(step(
                "uninstall-agent",
                "Remove the launchd agent",
                script(
                    r#"launchctl bootout "gui/$(id -u)/$1" 2>/dev/null || true; rm -f "$2""#,
                    &[DOCKER_LABEL, &p.plist().to_string_lossy()],
                ),
            ));
            steps.push(step(
                "uninstall-brew",
                "Stop and uninstall the Homebrew formula",
                script(
                    "brew services stop openshell 2>/dev/null || true; brew uninstall openshell 2>/dev/null || true",
                    &[],
                ),
            ));
        }
        Os::Linux => {
            steps.push(step(
                "uninstall-service",
                "Stop and disable the systemd user service",
                script(
                    "systemctl --user disable --now openshell-gateway 2>/dev/null || true",
                    &[],
                ),
            ));
        }
    }
    // The gateways osh created: their services (extras), then osh's own record of them.
    let units = p
        .config_home
        .parent()
        .unwrap_or(&p.home)
        .join("systemd/user");
    match f.os {
        Os::Linux => steps.push(step(
            "uninstall-extras",
            "Remove the gateways osh created",
            script(
                r#"for u in "$1"/osh-gateway-*.service; do [ -e "$u" ] || continue; systemctl --user disable --now "$(basename "$u")" 2>/dev/null || true; rm -f "$u"; done; systemctl --user daemon-reload 2>/dev/null || true"#,
                &[&units.to_string_lossy()],
            ),
        )),
        Os::MacOs => steps.push(step(
            "uninstall-extras",
            "Remove the gateways osh created",
            script(
                r#"for a in "$1"/dev.guilospanck.openshell-*.plist; do [ -e "$a" ] || continue; launchctl bootout "gui/$(id -u)/$(basename "$a" .plist)" 2>/dev/null || true; rm -f "$a"; done"#,
                &[&p.home.join("Library/LaunchAgents").to_string_lossy()],
            ),
        )),
    }
    steps.push(step(
        "uninstall-registry",
        "Forget the gateways osh created",
        cmd(
            "rm",
            &[
                "-rf",
                &p.data.join("gateways").to_string_lossy(),
                &p.data.join("gateways.json").to_string_lossy(),
                &p.data.join("wizard.json").to_string_lossy(),
            ],
        ),
    ));
    // rm -rf does not follow symlinks, so linked config entries only lose the link.
    steps.push(step(
        "uninstall-files",
        "Remove OpenShell config, state and data",
        cmd(
            "rm",
            &[
                "-rf",
                &p.config_home.to_string_lossy(),
                &p.state.to_string_lossy(),
                &p.share.to_string_lossy(),
            ],
        ),
    ));
    if f.os == Os::Linux {
        let mut s = step(
            "uninstall-packages",
            "Remove the OpenShell packages",
            StepKind::Manual(
                "Debian/Ubuntu: sudo apt remove 'openshell*'    Fedora/RHEL: sudo dnf remove 'openshell*'"
                    .into(),
            ),
        );
        // Left to the user, so it must not make the run count as failed.
        s.optional = true;
        steps.push(s);
    }
    chain(steps, None)
}

/// `KEY=value` lines as `gateway.env` writes them: optional quotes, `${HOME}` expanded.
pub fn parse_env_file(text: &str, home: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            let v = v.trim().trim_matches('"').trim_matches('\'');
            let v = v.replace("${HOME}", home).replace("$HOME", home);
            Some((k.trim().to_string(), v))
        })
        .collect()
}

/// Runs the Docker-driver gateway in the foreground; the launchd agent calls this. Reuses the
/// TLS material the Homebrew service wrote, with its own database.
pub fn exec_docker_service(p: &Paths, data: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let home = p.home.to_string_lossy().into_owned();
    let mut c = std::process::Command::new(gateway_binary());
    c.arg("--config").arg(data.join("gateway-docker.toml"));
    if let Ok(t) = std::fs::read_to_string(p.config_home.join("gateway.env")) {
        for (k, v) in parse_env_file(&t, &home) {
            c.env(k, v);
        }
    }
    let dir = p.state.join("gateway-docker");
    let _ = std::fs::create_dir_all(&dir);
    if std::env::var_os("OPENSHELL_LOCAL_TLS_DIR").is_none() {
        c.env("OPENSHELL_LOCAL_TLS_DIR", p.state.join("homebrew/tls"));
    }
    c.env(
        "OPENSHELL_DB_URL",
        format!("sqlite:{}?mode=rwc", dir.join("openshell.db").display()),
    );
    c.exec()
}

/// `openshell-gateway` from the Homebrew prefix, else from PATH.
pub(crate) fn gateway_binary() -> PathBuf {
    std::process::Command::new("brew")
        .args(["--prefix", "openshell"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("bin/openshell-gateway")
        })
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("openshell-gateway"))
}

/// True when the installed gateway.toml is missing or differs from the embedded `name`.
pub fn stale(name: &str, h: &dyn Host, p: &Paths) -> bool {
    if assets::embedded(name).is_none() {
        return false;
    }
    h.read(&p.config_home.join("gateway.toml"))
        .is_none_or(|installed| installed != wanted_config(name, h, p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testing::FakeHost;
    use crate::platform::{Kvm, PodmanMachine};
    use crate::setup::{execute, Answer, Outcome};

    fn facts(os: Os) -> Facts {
        Facts {
            os,
            pkg: if os == Os::MacOs {
                PkgMgr::Brew
            } else {
                PkgMgr::Apt
            },
            is_root: false,
            has_sudo: true,
            user: "ana".into(),
            wayland: false,
            has_openshell: true,
            has_curl: true,
            has_tmux: true,
            has_gh: true,
            has_clipboard: true,
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

    fn paths() -> Paths {
        Paths {
            home: "/h".into(),
            config_home: "/h/.config/openshell".into(),
            state: "/h/.local/state/openshell".into(),
            share: "/h/.local/share/openshell".into(),
            data: "/h/.local/share/osh".into(),
            exe: "/usr/local/bin/osh".into(),
        }
    }

    fn texts(steps: &[Step]) -> String {
        steps.iter().map(|s| s.command_line() + "\n").collect()
    }

    #[test]
    fn config_for_linux_podman_is_podman_toml() {
        let mut f = facts(Os::Linux);
        f.runtimes_ready = vec![Runtime::Podman];
        f.runtimes_installed = vec![Runtime::Podman];
        assert_eq!(config_name(&f, None), "gateway-podman.toml");
        assert_eq!(config_name(&facts(Os::Linux), None), "gateway-linux.toml");
        assert_eq!(
            config_name(&facts(Os::Linux), Some(Runtime::Podman)),
            "gateway-podman.toml"
        );
        assert_eq!(
            config_name(&facts(Os::MacOs), Some(Runtime::Podman)),
            "gateway.toml"
        );
    }

    #[test]
    fn plan_writes_config_and_restarts_service_on_linux() {
        let h = FakeHost::default();
        let steps = plan(&facts(Os::Linux), &Options::default(), &h, &paths(), false);
        let t = texts(&steps);
        assert!(t.contains("write /h/.config/openshell/gateway.toml"));
        assert!(t.contains("systemctl --user restart openshell-gateway"));
        assert!(t.contains("systemctl --user enable openshell-gateway"));
    }

    #[test]
    fn plan_uses_brew_services_on_macos() {
        let h = FakeHost::default();
        let steps = plan(&facts(Os::MacOs), &Options::default(), &h, &paths(), false);
        assert!(texts(&steps).contains("brew services restart openshell"));
    }

    #[test]
    fn plan_is_empty_when_config_is_current() {
        let mut h = FakeHost::default();
        h.files.insert(
            "/h/.config/openshell/gateway.toml".into(),
            assets::embedded("gateway-linux.toml").unwrap().into(),
        );
        let f = facts(Os::Linux);
        assert!(plan(&f, &Options::default(), &h, &paths(), false).is_empty());
        assert!(!plan(&f, &Options::default(), &h, &paths(), true).is_empty());
        assert!(!stale("gateway-linux.toml", &h, &paths()));
        assert!(stale("gateway-podman.toml", &h, &paths()));
    }

    #[test]
    fn plan_restarts_the_service_when_the_config_is_current_but_it_is_not_running() {
        let wanted = assets::embedded("gateway-linux.toml").unwrap();
        let mut h = FakeHost::default();
        h.files
            .insert("/h/.config/openshell/gateway.toml".into(), wanted.into());
        h.failing.insert("systemctl".into());
        let steps = plan(&facts(Os::Linux), &Options::default(), &h, &paths(), false);
        let t = texts(&steps);
        assert!(t.contains("systemctl --user restart openshell-gateway"));
        assert!(!t.contains("write "), "the config is already right: {t}");
        let mut h = FakeHost::default();
        h.files.insert(
            "/h/.config/openshell/gateway.toml".into(),
            assets::embedded("gateway.toml").unwrap().into(),
        );
        h.failing.insert("openshell".into());
        let steps = plan(&facts(Os::MacOs), &Options::default(), &h, &paths(), false);
        assert!(texts(&steps).contains("brew services restart openshell"));
    }

    #[test]
    fn plan_prefers_the_data_dir_copy_of_the_config() {
        let mut h = FakeHost::default();
        h.files.insert(
            "/h/.local/share/osh/gateway-linux.toml".into(),
            "# mine\n".into(),
        );
        let steps = plan(&facts(Os::Linux), &Options::default(), &h, &paths(), false);
        let StepKind::Write { content, .. } = &steps[0].kind else {
            panic!("{:?}", steps[0])
        };
        assert_eq!(content, "# mine\n");
        assert!(stale("gateway-linux.toml", &h, &paths()));
        h.files.insert(
            "/h/.config/openshell/gateway.toml".into(),
            "# mine\n".into(),
        );
        assert!(!stale("gateway-linux.toml", &h, &paths()));
    }

    #[test]
    fn plan_keeps_a_copy_of_a_hand_edited_installed_config() {
        let mut h = FakeHost::default();
        h.files.insert(
            "/h/.config/openshell/gateway.toml".into(),
            "# hand edit\n".into(),
        );
        let steps = plan(&facts(Os::Linux), &Options::default(), &h, &paths(), false);
        let StepKind::Write { path, content } = &steps[0].kind else {
            panic!("{:?}", steps[0])
        };
        assert_eq!(path, Path::new("/h/.config/openshell/gateway.toml.osh-bak"));
        assert_eq!(content, "# hand edit\n");
        assert!(
            matches!(&steps[1].kind, StepKind::Write { path, .. } if path.ends_with("gateway.toml"))
        );
    }

    #[test]
    fn plan_waits_for_the_openshell_install_when_it_runs_in_the_same_plan() {
        let h = FakeHost::default();
        let mut f = facts(Os::Linux);
        f.has_openshell = false;
        let steps = plan(&f, &Options::default(), &h, &paths(), false);
        assert_eq!(steps[0].after, Some("openshell"));
        assert_eq!(steps[1].after, Some("gateway-config"));
    }

    #[test]
    fn plan_config_write_goes_through_host_write_not_a_symlink() {
        let h = FakeHost::default();
        let steps = plan(&facts(Os::Linux), &Options::default(), &h, &paths(), false);
        let r = execute(&steps, &h, &Options::default(), &mut |_| Answer::Yes);
        assert!(r.iter().all(|r| r.outcome == Outcome::Done));
        assert_eq!(h.written.borrow().len(), 1);
    }

    #[test]
    fn upgrade_reinstalls_openshell_and_restarts_the_docker_gateway_on_macos() {
        let h = FakeHost::default();
        let steps = upgrade_steps(&facts(Os::MacOs), &Options::default(), &h, &paths());
        let t = texts(&steps);
        assert!(t.contains(crate::setup::OPENSHELL_INSTALL));
        assert!(t.contains("brew services restart openshell"));
        assert!(t.contains("launchctl kickstart"));
        assert_eq!(steps[1].after, Some("openshell"));
        let linux = upgrade_steps(&facts(Os::Linux), &Options::default(), &h, &paths());
        assert!(!texts(&linux).contains("launchctl"));
    }

    #[test]
    fn the_installed_config_decides_which_runtime_a_reset_sweeps() {
        assert_eq!(
            driver_runtime("[openshell.gateway]\ncompute_driver = \"podman\"\n"),
            Some(Runtime::Podman)
        );
        assert_eq!(
            driver_runtime("compute_driver = \"docker\""),
            Some(Runtime::Docker)
        );
        assert_eq!(driver_runtime("compute_driver = \"vm\""), None);
        assert_eq!(driver_runtime("# compute_driver = \"podman\"\n"), None);
    }

    #[test]
    fn paths_need_an_absolute_home() {
        assert!(Paths::from_env(None, None, None).is_err());
        assert!(Paths::from_env(Some("rel".into()), None, None).is_err());
        let p = Paths::from_env(Some("/h".into()), Some("relative".into()), None).unwrap();
        assert_eq!(p.config_home, PathBuf::from("/h/.config/openshell"));
        let p =
            Paths::from_env(Some("/h".into()), Some("/c".into()), Some("/d/osh".into())).unwrap();
        assert_eq!(p.config_home, PathBuf::from("/c/openshell"));
        assert_eq!(p.data, PathBuf::from("/d/osh"));
    }

    #[test]
    fn reset_requires_typed_confirmation() {
        assert!(confirm_reset("reset\n"));
        assert!(!confirm_reset("yes"));
        assert!(!confirm_reset(""));
    }

    #[test]
    fn reset_unknown_gateway_errors() {
        assert!(reset_steps(&facts(Os::Linux), &paths(), "openshell-docker", None).is_err());
        assert!(reset_steps(&facts(Os::MacOs), &paths(), "other", None).is_err());
    }

    #[test]
    fn reset_docker_gateway_on_macos_uses_its_own_database_and_agent() {
        let s = reset_steps(
            &facts(Os::MacOs),
            &paths(),
            "openshell-docker",
            Some(Runtime::Docker),
        )
        .unwrap();
        let t = texts(&s);
        assert!(t.contains("/h/.local/state/openshell/gateway-docker/openshell.db-wal"));
        assert!(t.contains("launchctl bootout"));
        assert!(!t.contains("brew services"));
    }

    #[test]
    fn reset_vm_gateway_kills_orphan_vms_not_containers() {
        let s = reset_steps(&facts(Os::MacOs), &paths(), "openshell", None).unwrap();
        let t = texts(&s);
        assert!(t.contains("vm-driver/sandboxes"));
        assert!(t.contains("brew services stop openshell"));
    }

    #[test]
    fn reset_linux_podman_sweeps_with_podman() {
        let s = reset_steps(
            &facts(Os::Linux),
            &paths(),
            "openshell",
            Some(Runtime::Podman),
        )
        .unwrap();
        assert!(texts(&s).contains(" podman /h/.local/state/openshell "));
    }

    #[test]
    fn uninstall_plan_linux_lists_systemd_disable_and_dirs() {
        let t = texts(&uninstall_steps(&facts(Os::Linux), &paths()));
        assert!(t.contains("systemctl --user disable --now openshell-gateway"));
        assert!(t.contains(
            "rm -rf /h/.config/openshell /h/.local/state/openshell /h/.local/share/openshell"
        ));
        assert!(t.contains("apt remove"));
    }

    #[test]
    fn uninstall_removes_osh_managed_gateway_services_and_the_registry() {
        let linux = texts(&uninstall_steps(&facts(Os::Linux), &paths()));
        assert!(linux.contains("osh-gateway-*.service"), "{linux}");
        assert!(
            linux.contains("/h/.local/share/osh/gateways.json"),
            "{linux}"
        );
        assert!(linux.contains("/h/.local/share/osh/gateways"), "{linux}");
        let mac = texts(&uninstall_steps(&facts(Os::MacOs), &paths()));
        assert!(mac.contains("dev.guilospanck.openshell-"), "{mac}");
        assert!(mac.contains("/h/.local/share/osh/gateways.json"), "{mac}");
    }

    #[test]
    fn uninstall_manual_package_removal_does_not_fail_the_run() {
        let steps = uninstall_steps(&facts(Os::Linux), &paths());
        assert!(steps
            .iter()
            .filter(|s| matches!(s.kind, StepKind::Manual(_)))
            .all(|s| s.optional));
    }

    #[test]
    fn uninstall_plan_macos_removes_agent_and_formula_but_not_osh_data() {
        let t = texts(&uninstall_steps(&facts(Os::MacOs), &paths()));
        assert!(t.contains("brew uninstall openshell"));
        assert!(t.contains("launchctl bootout"));
        assert!(!t.contains("share/osh "));
    }

    #[test]
    fn env_file_expands_home_and_strips_quotes() {
        let v = parse_env_file(
            "DOCKER_HOST=\"unix://${HOME}/.colima/docker.sock\"\n# c\n\nPATH=\"/a:/b\"\n",
            "/h",
        );
        assert_eq!(
            v[0],
            ("DOCKER_HOST".into(), "unix:///h/.colima/docker.sock".into())
        );
        assert_eq!(v[1], ("PATH".into(), "/a:/b".into()));
    }
}
