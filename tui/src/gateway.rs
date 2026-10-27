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
const WAIT_SCRIPT: &str = r#"for _ in $(seq 20); do openshell -g "$1" status >/dev/null 2>&1 && break; sleep 1; done; openshell -g "$1" status"#;

/// Registers the Docker gateway, sharing the Homebrew gateway's client certificates.
/// $1 = config dir. Keeps the previously selected gateway active.
const DOCKER_REGISTER_SCRIPT: &str = r#"set -e
mkdir -p "$1/gateways/openshell-docker"
cp -R "$1/gateways/openshell/mtls" "$1/gateways/openshell-docker/"
previous="$(cat "$1/active_gateway" 2>/dev/null || echo openshell)"
if ! openshell gateway list -o json | grep -q '"openshell-docker"'; then
  openshell gateway add https://localhost:17671 --local --name openshell-docker
  openshell gateway select "$previous"
fi
sleep 3"#;

/// Deletes the gateway's sandboxes through the gateway. $1 = gateway name.
const DELETE_SANDBOXES_SCRIPT: &str = r#"if openshell -g "$1" status >/dev/null 2>&1; then
  openshell -g "$1" sandbox delete --all || echo "sandbox delete failed; sweeping leftovers next"
else
  echo "gateway not reachable; sweeping leftovers next"
fi"#;

/// Removes leftover containers and volumes of a Docker or Podman driver.
/// $1 = runtime binary, $2 = state dir, $3 = config dir, $4 = darwin flag.
const SWEEP_CONTAINERS_SCRIPT: &str = r#"[ -f "$3/gateway.env" ] && { set -a; . "$3/gateway.env"; set +a; }
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
const SWEEP_VMS_SCRIPT: &str = r#"pkill -f "$1/vm-driver/sandboxes" 2>/dev/null || true
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
    /// This binary, for the launchd agent to run.
    pub exe: PathBuf,
}

impl Paths {
    pub fn detect() -> Paths {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
        };
        Paths {
            config_home: xdg("XDG_CONFIG_HOME", ".config").join("openshell"),
            state: home.join(".local/state/openshell"),
            share: home.join(".local/share/openshell"),
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("osh")),
            home,
        }
    }

    fn plist(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{DOCKER_LABEL}.plist"))
    }
}

fn step(id: &'static str, title: &str, kind: StepKind) -> Step {
    Step {
        id,
        title: title.into(),
        kind,
        optional: false,
        after: None,
    }
}

fn cmd(prog: &str, args: &[&str]) -> StepKind {
    StepKind::Cmd {
        prog: prog.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
    }
}

/// `sh -c SCRIPT sh ARG...`: a fixed script with its inputs as positional parameters, never
/// spliced into the script text.
fn script(body: &str, args: &[&str]) -> StepKind {
    let mut a = vec!["-c", body, "sh"];
    a.extend_from_slice(args);
    cmd("sh", &a)
}

fn chain(mut steps: Vec<Step>, first_after: Option<&'static str>) -> Vec<Step> {
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

/// Installs the gateway config and (re)starts the service. Empty when OpenShell is already
/// installed, the config is current and `force` is off.
pub fn plan(f: &Facts, o: &Options, h: &dyn Host, p: &Paths, force: bool) -> Vec<Step> {
    let name = config_name(f, o.runtime);
    let wanted = assets::embedded(name).unwrap_or_default();
    let target = p.config_home.join("gateway.toml");
    let installing = !f.has_openshell;
    let current = h.read(&target).is_some_and(|c| c == wanted);
    if !force && !installing && current {
        return Vec::new();
    }
    let mut steps = vec![step(
        "gateway-config",
        &format!("Install the gateway config ({name})"),
        StepKind::Write {
            path: target,
            content: wanted.to_string(),
        },
    )];
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

/// The launchd agent that runs `osh gateway-service docker`.
pub fn docker_gateway_plist(exe: &Path, logs: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{DOCKER_LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>gateway-service</string><string>docker</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardOutPath</key><string>{logs}/gateway.out.log</string>
  <key>StandardErrorPath</key><string>{logs}/gateway.err.log</string>
</dict></plist>
"#,
        exe = exe.display(),
        logs = logs.display()
    )
}

/// macOS only: a second gateway on the Docker driver, which honours per-sandbox CPU/memory.
/// Needs the Homebrew gateway to have run once, for the shared TLS material.
pub fn docker_gateway_steps(f: &Facts, p: &Paths, h: &dyn Host) -> Result<Vec<Step>, String> {
    if f.os != Os::MacOs {
        return Err("not needed on Linux: its only gateway already uses the Docker driver".into());
    }
    let logs = p.state.join("gateway-docker");
    let mut steps = vec![step(
        "docker-gateway-plist",
        "Write the launchd agent",
        StepKind::Write {
            path: p.plist(),
            content: docker_gateway_plist(&p.exe, &logs),
        },
    )];
    if h.read(&p.config_home.join("gateway.env")).is_none() {
        steps.push(step(
            "docker-gateway-env",
            "Write gateway.env (Colima Docker socket)",
            StepKind::Write {
                path: p.config_home.join("gateway.env"),
                content: assets::embedded("gateway.env").unwrap_or_default().into(),
            },
        ));
    }
    let plist = p.plist().to_string_lossy().into_owned();
    steps.push(step(
        "docker-gateway-logs",
        "Create the log directory",
        cmd("mkdir", &["-p", &logs.to_string_lossy()]),
    ));
    steps.push(step(
        "docker-gateway-stop",
        "Stop a running agent",
        script(
            r#"launchctl bootout "gui/$(id -u)/$1" 2>/dev/null || true"#,
            &[DOCKER_LABEL],
        ),
    ));
    steps.push(step(
        "docker-gateway-start",
        "Start the agent",
        script(r#"launchctl bootstrap "gui/$(id -u)" "$1""#, &[&plist]),
    ));
    steps.push(step(
        "docker-gateway-register",
        "Register the gateway with the OpenShell CLI",
        script(DOCKER_REGISTER_SCRIPT, &[&p.config_home.to_string_lossy()]),
    ));
    steps.push(step(
        "docker-gateway-wait",
        "Wait for the gateway to answer",
        script(WAIT_SCRIPT, &[DOCKER_GATEWAY]),
    ));
    Ok(chain(steps, None))
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
        steps.push(step(
            "uninstall-packages",
            "Remove the OpenShell packages",
            StepKind::Manual(
                "Debian/Ubuntu: sudo apt remove 'openshell*'    Fedora/RHEL: sudo dnf remove 'openshell*'"
                    .into(),
            ),
        ));
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
fn gateway_binary() -> PathBuf {
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
    match (
        assets::embedded(name),
        h.read(&p.config_home.join("gateway.toml")),
    ) {
        (Some(w), Some(installed)) => w != installed,
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testing::FakeHost;
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
        }
    }

    fn paths() -> Paths {
        Paths {
            home: "/h".into(),
            config_home: "/h/.config/openshell".into(),
            state: "/h/.local/state/openshell".into(),
            share: "/h/.local/share/openshell".into(),
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
    fn plist_runs_the_osh_binary_as_the_gateway_service() {
        let x = docker_gateway_plist(Path::new("/usr/local/bin/osh"), Path::new("/logs"));
        assert!(x.contains(DOCKER_LABEL));
        assert!(x.contains("<string>/usr/local/bin/osh</string><string>gateway-service</string><string>docker</string>"));
        assert!(x.contains("/logs/gateway.err.log"));
    }

    #[test]
    fn docker_gateway_is_macos_only() {
        let h = FakeHost::default();
        assert!(docker_gateway_steps(&facts(Os::Linux), &paths(), &h).is_err());
        let steps = docker_gateway_steps(&facts(Os::MacOs), &paths(), &h).unwrap();
        let t = texts(&steps);
        assert!(t.contains("Library/LaunchAgents/dev.guilospanck.openshell-docker.plist"));
        assert!(t.contains("write /h/.config/openshell/gateway.env"));
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
