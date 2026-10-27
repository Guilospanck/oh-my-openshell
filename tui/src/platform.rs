//! What the host is and which tools it has, so setup can plan without touching anything.
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    MacOs,
    Linux,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PkgMgr {
    Brew,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    None,
}

/// An OCI runtime OpenShell can drive: its docker and podman compute drivers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runtime {
    Docker,
    Podman,
}

impl Runtime {
    pub fn bin(self) -> &'static str {
        match self {
            Runtime::Docker => "docker",
            Runtime::Podman => "podman",
        }
    }
}

/// The only door to the host, so planning and detection run against a fake in tests.
pub trait Host {
    fn which(&self, bin: &str) -> bool;
    /// Runs `prog` directly (never through a shell); `Ok` is stdout, `Err` a failure message.
    fn run(&self, prog: &str, args: &[&str]) -> Result<String, String>;
    fn read(&self, path: &Path) -> Option<String>;
    fn env(&self, key: &str) -> Option<String>;
    /// Writes `content` to `path`, creating parent directories and replacing a symlink with a
    /// regular file (the gateway refuses a symlinked config).
    fn write(&self, path: &Path, content: &str) -> Result<(), String>;
    /// Like `run`, but the child shares the terminal, so prompts (sudo) and progress show.
    fn run_inherit(&self, prog: &str, args: &[&str]) -> Result<(), String>;
}

pub struct RealHost;

impl Host for RealHost {
    fn which(&self, bin: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|p| {
            std::env::split_paths(&p).any(|d| {
                let f = d.join(bin);
                f.is_file() && is_executable(&f)
            })
        })
    }

    fn run(&self, prog: &str, args: &[&str]) -> Result<String, String> {
        let out = Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{prog}: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err.lines().next().unwrap_or("");
            Err(format!("{prog} exited with {}: {first}", out.status))
        }
    }

    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn write(&self, path: &Path, content: &str) -> Result<(), String> {
        let err = |e: std::io::Error| format!("{}: {e}", path.display());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        if path.symlink_metadata().is_ok() {
            std::fs::remove_file(path).map_err(err)?;
        }
        std::fs::write(path, content).map_err(err)
    }

    fn run_inherit(&self, prog: &str, args: &[&str]) -> Result<(), String> {
        let st = Command::new(prog)
            .args(args)
            .status()
            .map_err(|e| format!("{prog}: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("{prog} exited with {st}"))
        }
    }
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub os: Os,
    pub pkg: PkgMgr,
    pub is_root: bool,
    pub has_sudo: bool,
    pub user: String,
    pub wayland: bool,
    pub has_openshell: bool,
    pub has_curl: bool,
    pub has_tmux: bool,
    pub has_gh: bool,
    /// A tool that reads host clipboard images: pngpaste (macOS), wl-paste or xclip (Linux).
    pub has_clipboard: bool,
    /// Binary on PATH, whether or not its daemon answers.
    pub runtimes_installed: Vec<Runtime>,
    /// `<bin> info` succeeded: usable right now. Docker first when both are.
    pub runtimes_ready: Vec<Runtime>,
    /// Linux: `podman info` answers but the user's `podman.socket` (the API OpenShell talks to)
    /// is not active, so Podman is installed and usable but not ready for the gateway.
    pub podman_needs_socket: bool,
    /// `docker info` failed with "permission denied": the daemon is up but this user is not in
    /// the docker group (the state right after installing Docker, until the next login).
    pub docker_permission_denied: bool,
}

impl Facts {
    /// The runtime osh should use: a ready Docker, else a ready Podman.
    pub fn runtime(&self) -> Option<Runtime> {
        self.runtimes_ready.first().copied()
    }
}

/// Maps `ID` / `ID_LIKE` of /etc/os-release to the package manager of that family.
pub fn parse_os_release(text: &str) -> PkgMgr {
    let mut ids = Vec::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k == "ID" || k == "ID_LIKE" {
                ids.extend(v.trim_matches('"').split_whitespace().map(str::to_string));
            }
        }
    }
    for id in &ids {
        match id.as_str() {
            "debian" | "ubuntu" => return PkgMgr::Apt,
            "fedora" | "rhel" | "centos" => return PkgMgr::Dnf,
            "arch" => return PkgMgr::Pacman,
            "opensuse" | "suse" | "sles" => return PkgMgr::Zypper,
            _ => {}
        }
    }
    PkgMgr::None
}

pub fn gather(h: &dyn Host) -> Facts {
    let os = if cfg!(target_os = "macos") {
        Os::MacOs
    } else {
        Os::Linux
    };
    gather_for(h, os)
}

/// The package manager this host installs software with.
pub fn detect_pkg(h: &dyn Host, os: Os) -> PkgMgr {
    match os {
        Os::MacOs if h.which("brew") => PkgMgr::Brew,
        Os::MacOs => PkgMgr::None,
        Os::Linux => {
            let from_release = h
                .read(Path::new("/etc/os-release"))
                .map_or(PkgMgr::None, |t| parse_os_release(&t));
            if from_release != PkgMgr::None {
                from_release
            } else {
                [
                    ("apt-get", PkgMgr::Apt),
                    ("dnf", PkgMgr::Dnf),
                    ("pacman", PkgMgr::Pacman),
                    ("zypper", PkgMgr::Zypper),
                ]
                .into_iter()
                .find(|(b, _)| h.which(b))
                .map_or(PkgMgr::None, |(_, p)| p)
            }
        }
    }
}

pub fn gather_for(h: &dyn Host, os: Os) -> Facts {
    let pkg = detect_pkg(h, os);
    let is_root = h.run("id", &["-u"]).is_ok_and(|o| o.trim() == "0");
    let all = [Runtime::Docker, Runtime::Podman];
    let runtimes_installed: Vec<Runtime> = all.into_iter().filter(|r| h.which(r.bin())).collect();
    let mut docker_permission_denied = false;
    let mut podman_needs_socket = false;
    let mut runtimes_ready = Vec::new();
    for r in &runtimes_installed {
        match h.run(r.bin(), &["info"]) {
            Err(e) => {
                docker_permission_denied |=
                    *r == Runtime::Docker && e.contains("permission denied");
            }
            Ok(_) if *r == Runtime::Podman && os == Os::Linux => {
                // Podman is daemonless: `info` works with no service. The gateway needs the socket.
                if h.run(
                    "systemctl",
                    &["--user", "is-active", "--quiet", "podman.socket"],
                )
                .is_ok()
                {
                    runtimes_ready.push(*r);
                } else {
                    podman_needs_socket = true;
                }
            }
            Ok(_) => runtimes_ready.push(*r),
        }
    }
    Facts {
        os,
        pkg,
        is_root,
        has_sudo: h.which("sudo"),
        user: h
            .run("id", &["-un"])
            .map(|o| o.trim().to_string())
            .unwrap_or_default(),
        wayland: h.env("XDG_SESSION_TYPE").is_some_and(|t| t == "wayland")
            || h.env("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty()),
        has_openshell: h.which("openshell"),
        has_curl: h.which("curl"),
        has_tmux: h.which("tmux"),
        has_gh: h.which("gh"),
        has_clipboard: match os {
            Os::MacOs => h.which("pngpaste"),
            Os::Linux => h.which("wl-paste") || h.which("xclip"),
        },
        runtimes_installed,
        runtimes_ready,
        podman_needs_socket,
        docker_permission_denied,
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

    /// A host with a fixed set of binaries, files and command results; records every `run`.
    #[derive(Default)]
    pub struct FakeHost {
        pub bins: HashSet<String>,
        pub files: HashMap<String, String>,
        /// Programs whose `run` fails; every other `run` succeeds with `outputs` or "".
        pub failing: HashSet<String>,
        pub outputs: HashMap<String, String>,
        pub env: HashMap<String, String>,
        /// Command lines that fail with this message.
        pub fail_msgs: HashMap<String, String>,
        pub calls: RefCell<Vec<String>>,
        pub written: RefCell<Vec<(std::path::PathBuf, String)>>,
    }

    impl FakeHost {
        pub fn with_bins(bins: &[&str]) -> Self {
            FakeHost {
                bins: bins.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }
        }
    }

    impl Host for FakeHost {
        fn which(&self, bin: &str) -> bool {
            self.bins.contains(bin)
        }
        fn run(&self, prog: &str, args: &[&str]) -> Result<String, String> {
            let line = format!("{prog} {}", args.join(" ")).trim().to_string();
            self.calls.borrow_mut().push(line.clone());
            if let Some(m) = self.fail_msgs.get(&line) {
                return Err(m.clone());
            }
            if self.failing.contains(prog) || self.failing.contains(&line) {
                return Err(format!("{line} failed"));
            }
            Ok(self.outputs.get(&line).cloned().unwrap_or_default())
        }
        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path.to_str()?).cloned()
        }
        fn env(&self, key: &str) -> Option<String> {
            self.env.get(key).cloned()
        }
        fn write(&self, path: &Path, content: &str) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(format!("write {}", path.display()));
            if self.failing.contains("write") {
                return Err("write failed".into());
            }
            self.written
                .borrow_mut()
                .push((path.to_path_buf(), content.to_string()));
            Ok(())
        }
        fn run_inherit(&self, prog: &str, args: &[&str]) -> Result<(), String> {
            self.run(prog, args).map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeHost;
    use super::*;

    #[test]
    fn real_write_replaces_a_symlink_with_a_regular_file() {
        let d = std::env::temp_dir().join(format!("osh-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let target = d.join("target.toml");
        std::fs::write(&target, "old").unwrap();
        let link = d.join("sub/gateway.toml");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        RealHost.write(&link, "new").unwrap();
        assert!(!link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn os_release_ubuntu_is_apt() {
        assert_eq!(
            parse_os_release("NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n"),
            PkgMgr::Apt
        );
    }

    #[test]
    fn os_release_fedora_is_dnf() {
        assert_eq!(parse_os_release("ID=fedora\n"), PkgMgr::Dnf);
    }

    #[test]
    fn os_release_derivative_follows_id_like() {
        assert_eq!(
            parse_os_release("ID=manjaro\nID_LIKE=\"arch\"\n"),
            PkgMgr::Pacman
        );
    }

    #[test]
    fn os_release_alpine_is_none() {
        assert_eq!(parse_os_release("ID=alpine\n"), PkgMgr::None);
    }

    #[test]
    fn ready_runtime_requires_answering_daemon() {
        let mut h = FakeHost::with_bins(&["docker"]);
        h.failing.insert("docker info".into());
        let f = gather_for(&h, Os::Linux);
        assert_eq!(f.runtimes_installed, vec![Runtime::Docker]);
        assert!(f.runtimes_ready.is_empty());
        assert_eq!(f.runtime(), None);
    }

    #[test]
    fn podman_without_its_socket_is_not_ready() {
        let mut h = FakeHost::with_bins(&["podman"]);
        h.failing
            .insert("systemctl --user is-active --quiet podman.socket".into());
        let f = gather_for(&h, Os::Linux);
        assert_eq!(f.runtimes_installed, vec![Runtime::Podman]);
        assert!(f.runtimes_ready.is_empty());
        assert!(f.podman_needs_socket);
    }

    #[test]
    fn podman_on_macos_needs_no_socket_check() {
        let mut h = FakeHost::with_bins(&["podman"]);
        h.failing.insert("systemctl".into());
        let f = gather_for(&h, Os::MacOs);
        assert_eq!(f.runtimes_ready, vec![Runtime::Podman]);
        assert!(!f.podman_needs_socket);
    }

    #[test]
    fn docker_permission_denied_is_told_apart_from_a_stopped_daemon() {
        let mut h = FakeHost::with_bins(&["docker"]);
        h.fail_msgs.insert(
            "docker info".into(),
            "docker exited with 1: permission denied while trying to connect to the Docker daemon socket".into(),
        );
        assert!(gather_for(&h, Os::Linux).docker_permission_denied);
        let mut h = FakeHost::with_bins(&["docker"]);
        h.failing.insert("docker info".into());
        assert!(!gather_for(&h, Os::Linux).docker_permission_denied);
    }

    #[test]
    fn prefers_docker_when_both_ready() {
        let h = FakeHost::with_bins(&["docker", "podman"]);
        assert_eq!(gather_for(&h, Os::Linux).runtime(), Some(Runtime::Docker));
    }

    #[test]
    fn falls_back_to_podman_when_docker_is_stopped() {
        let mut h = FakeHost::with_bins(&["docker", "podman"]);
        h.failing.insert("docker info".into());
        assert_eq!(gather_for(&h, Os::Linux).runtime(), Some(Runtime::Podman));
    }

    #[test]
    fn macos_without_brew_has_no_package_manager() {
        let h = FakeHost::with_bins(&[]);
        assert_eq!(gather_for(&h, Os::MacOs).pkg, PkgMgr::None);
        let h = FakeHost::with_bins(&["brew"]);
        assert_eq!(gather_for(&h, Os::MacOs).pkg, PkgMgr::Brew);
    }

    #[test]
    fn linux_without_os_release_probes_binaries() {
        let h = FakeHost::with_bins(&["dnf"]);
        assert_eq!(gather_for(&h, Os::Linux).pkg, PkgMgr::Dnf);
    }

    #[test]
    fn root_and_sudo_are_detected() {
        let mut h = FakeHost::with_bins(&["sudo"]);
        h.outputs.insert("id -u".into(), "0\n".into());
        let f = gather_for(&h, Os::Linux);
        assert!(f.is_root && f.has_sudo);
    }
}
