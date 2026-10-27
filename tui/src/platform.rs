//! What the host is and which tools it has, so setup can plan without touching anything.
#![allow(dead_code)] // consumed by the setup tasks that follow; remove once wired
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
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("{prog}: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!("{prog} exited with {}", out.status))
        }
    }

    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
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
    /// Binary on PATH, whether or not its daemon answers.
    pub runtimes_installed: Vec<Runtime>,
    /// `<bin> info` succeeded: usable right now. Docker first when both are.
    pub runtimes_ready: Vec<Runtime>,
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

pub fn gather_for(h: &dyn Host, os: Os) -> Facts {
    let pkg = match os {
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
    };
    let is_root = h.run("id", &["-u"]).is_ok_and(|o| o.trim() == "0");
    let all = [Runtime::Docker, Runtime::Podman];
    let runtimes_installed: Vec<Runtime> = all.into_iter().filter(|r| h.which(r.bin())).collect();
    let runtimes_ready = runtimes_installed
        .iter()
        .copied()
        .filter(|r| h.run(r.bin(), &["info"]).is_ok())
        .collect();
    Facts {
        os,
        pkg,
        is_root,
        has_sudo: h.which("sudo"),
        runtimes_installed,
        runtimes_ready,
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
        pub calls: RefCell<Vec<String>>,
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
            if self.failing.contains(prog) || self.failing.contains(&line) {
                return Err(format!("{line} failed"));
            }
            Ok(self.outputs.get(&line).cloned().unwrap_or_default())
        }
        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path.to_str()?).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeHost;
    use super::*;

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
