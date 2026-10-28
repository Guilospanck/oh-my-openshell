//! What a gateway is to osh: its kind, its name and port, and the registry of the ones osh made.
use crate::platform::{Facts, Kvm, Os, PodmanMachine, Runtime};
use serde_json::{json, Value};
use std::{fs, io, path::Path};

/// The compute driver behind a gateway, which is also how it is named: `openshell-<kind>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GwKind {
    Vm,
    Docker,
    Podman,
}

impl GwKind {
    pub const ALL: [GwKind; 3] = [GwKind::Vm, GwKind::Docker, GwKind::Podman];

    pub fn name(self) -> &'static str {
        match self {
            GwKind::Vm => "openshell-vm",
            GwKind::Docker => "openshell-docker",
            GwKind::Podman => "openshell-podman",
        }
    }

    /// The `compute_driver` value in a gateway config.
    pub fn driver(self) -> &'static str {
        match self {
            GwKind::Vm => "vm",
            GwKind::Docker => "docker",
            GwKind::Podman => "podman",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GwKind::Vm => "VM",
            GwKind::Docker => "Docker",
            GwKind::Podman => "Podman",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            GwKind::Vm => "Each sandbox in its own microVM. Strongest isolation.",
            GwKind::Docker => "Sandboxes run as containers. Honours CPU and memory limits.",
            GwKind::Podman => "Like Docker, but daemonless and rootless.",
        }
    }

    /// `vm`, `docker`, `podman`, or the full gateway name.
    pub fn parse(s: &str) -> Option<GwKind> {
        GwKind::ALL
            .into_iter()
            .find(|k| s == k.driver() || s == k.name())
    }

    pub fn from_driver(d: &str) -> Option<GwKind> {
        GwKind::ALL.into_iter().find(|k| k.driver() == d)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GatewaySpec {
    pub name: String,
    pub kind: GwKind,
    pub port: u16,
    pub main: bool,
}

impl GatewaySpec {
    pub fn health_port(&self) -> u16 {
        self.port + 10
    }

    pub fn metrics_port(&self) -> u16 {
        self.port + 20
    }
}

/// The gateways osh created, in `<data dir>/gateways.json`. Gateways the CLI knows that are not
/// listed here are "existing": osh never changes those.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Registry {
    pub gateways: Vec<GatewaySpec>,
}

const REGISTRY: &str = "gateways.json";
const PENDING: &str = "wizard.json";

impl Registry {
    /// A missing or unreadable file is an empty registry.
    pub fn load(dir: &Path) -> Registry {
        let v: Option<Value> = fs::read_to_string(dir.join(REGISTRY))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let gateways = v
            .and_then(|v| v["gateways"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|g| {
                let kind = GwKind::from_driver(g["kind"].as_str()?)?;
                let name = g["name"].as_str()?;
                // Names reach `rm -f` paths and service labels, so only the exact name osh
                // gives each kind is trusted; the health and metrics ports sit above the
                // gateway port and must fit.
                if name != kind.name() {
                    return None;
                }
                let port = u16::try_from(g["port"].as_u64()?).ok()?;
                if port == 0 || port > u16::MAX - 20 {
                    return None;
                }
                Some(GatewaySpec {
                    name: name.to_string(),
                    kind,
                    port,
                    main: g["main"].as_bool().unwrap_or(false),
                })
            })
            .collect();
        Registry { gateways }
    }

    pub fn to_json(&self) -> String {
        let list: Vec<Value> = self
            .gateways
            .iter()
            .map(|g| json!({"name": g.name, "kind": g.kind.driver(), "port": g.port, "main": g.main}))
            .collect();
        serde_json::to_string_pretty(&json!({"gateways": list})).unwrap_or_default() + "\n"
    }

    /// The plan writes the registry through a `Write` step; this is for tests.
    #[cfg(test)]
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        fs::write(dir.join(REGISTRY), self.to_json())
    }

    pub fn main(&self) -> Option<&GatewaySpec> {
        self.gateways.iter().find(|g| g.main)
    }

    pub fn get(&self, name: &str) -> Option<&GatewaySpec> {
        self.gateways.iter().find(|g| g.name == name)
    }

    pub fn has_kind(&self, k: GwKind) -> bool {
        self.gateways.iter().any(|g| g.kind == k)
    }

    /// Replaces an entry of the same name.
    pub fn add(&mut self, s: GatewaySpec) {
        self.gateways.retain(|g| g.name != s.name);
        self.gateways.push(s);
    }

    pub fn remove(&mut self, name: &str) {
        self.gateways.retain(|g| g.name != name);
    }
}

/// One line of "what it needs": met (`ok`) or not, and what osh will do about it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Need {
    pub text: String,
    pub ok: bool,
    /// Empty when the need is met.
    pub action: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum State {
    Ready,
    /// Something is missing that osh installs, enables, starts, or asks the user to do.
    NeedsInstall,
    /// Cannot work on this machine; the text says why.
    Unavailable(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Availability {
    pub state: State,
    pub needs: Vec<Need>,
}

fn need(text: &str, ok: bool, action: &str) -> Need {
    Need {
        text: text.into(),
        ok,
        action: if ok { String::new() } else { action.into() },
    }
}

fn finish(needs: Vec<Need>) -> Availability {
    let state = if needs.iter().all(|n| n.ok) {
        State::Ready
    } else {
        State::NeedsInstall
    };
    Availability { state, needs }
}

/// What it takes to run a gateway of kind `k` on this machine, from the facts alone.
pub fn availability(f: &Facts, k: GwKind) -> Availability {
    let mac = f.os == Os::MacOs;
    match k {
        GwKind::Vm => {
            let mut needs = Vec::new();
            if !mac {
                match f.kvm {
                    Kvm::Absent => {
                        return Availability {
                            state: State::Unavailable(
                                "no /dev/kvm: virtualization is off or unsupported on this machine"
                                    .into(),
                            ),
                            needs: Vec::new(),
                        }
                    }
                    Kvm::NoAccess => needs.push(need(
                        "KVM access",
                        false,
                        "your user will join the kvm group (log out and back in after)",
                    )),
                    Kvm::Usable => needs.push(need("KVM access", true, "")),
                }
            }
            needs.push(need("e2fsprogs", f.has_e2fsprogs, "will be installed"));
            finish(needs)
        }
        GwKind::Docker => {
            if f.runtimes_ready.contains(&Runtime::Docker) {
                return finish(vec![need("Docker running", true, "")]);
            }
            let installed = f.runtimes_installed.contains(&Runtime::Docker);
            let mut needs = Vec::new();
            if mac {
                if f.docker_desktop {
                    needs.push(need(
                        "Docker Desktop running",
                        false,
                        "start Docker Desktop yourself",
                    ));
                } else {
                    needs.push(need("docker + Colima", f.colima, "will be installed"));
                    needs.push(need("Colima running", false, "will be started"));
                }
            } else if f.docker_permission_denied {
                needs.push(need(
                    "docker group",
                    false,
                    "your user will join the docker group (log out and back in after)",
                ));
            } else if installed {
                needs.push(need(
                    "Docker daemon running",
                    false,
                    "start it: sudo systemctl start docker",
                ));
            } else {
                needs.push(need("docker", false, "will be installed"));
                needs.push(need(
                    "docker group",
                    false,
                    "your user will join the docker group (log out and back in after)",
                ));
            }
            finish(needs)
        }
        GwKind::Podman => {
            if f.runtimes_ready.contains(&Runtime::Podman) {
                return finish(vec![need("Podman running", true, "")]);
            }
            let installed = f.runtimes_installed.contains(&Runtime::Podman);
            let mut needs = vec![need("podman", installed, "will be installed")];
            if mac {
                let action = match f.podman_machine {
                    PodmanMachine::Stopped => "will be started",
                    _ => "will be created and started",
                };
                needs.push(need(
                    "podman machine",
                    f.podman_machine == PodmanMachine::Running,
                    action,
                ));
            } else {
                needs.push(need(
                    "podman.socket",
                    installed && !f.podman_needs_socket,
                    "will be enabled",
                ));
            }
            finish(needs)
        }
    }
}

/// The lowest port from 17671 for an extra gateway whose port, health port (+10) and metrics port
/// (+20) are all free: not in `taken` and, for the gateway port, not currently bound.
pub fn next_port(taken: &[u16], is_bound: &dyn Fn(u16) -> bool) -> u16 {
    let used = |p: u16| taken.contains(&p);
    (17671..)
        .find(|p| {
            !used(*p)
                && !used(p + 10)
                && !used(p + 20)
                && !is_bound(*p)
                // Nobody else's health or metrics port may be this one's gateway port.
                && !taken.iter().any(|t| t + 10 == *p || t + 20 == *p)
        })
        .unwrap_or(17671)
}

/// The types ticked in the wizard, kept while an install is in flight so a stop for a new login
/// can be resumed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pending {
    pub kinds: Vec<GwKind>,
}

impl Pending {
    pub fn load(dir: &Path) -> Option<Pending> {
        let v: Value = serde_json::from_str(&fs::read_to_string(dir.join(PENDING)).ok()?).ok()?;
        let kinds: Vec<GwKind> = v["kinds"]
            .as_array()?
            .iter()
            .filter_map(|k| GwKind::from_driver(k.as_str()?))
            .collect();
        (!kinds.is_empty()).then_some(Pending { kinds })
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let kinds: Vec<&str> = self.kinds.iter().map(|k| k.driver()).collect();
        fs::write(dir.join(PENDING), json!({"kinds": kinds}).to_string())
    }

    pub fn clear(dir: &Path) {
        let _ = fs::remove_file(dir.join(PENDING));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::PkgMgr;
    use std::path::PathBuf;

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
            kvm: if os == Os::MacOs {
                Kvm::Usable
            } else {
                Kvm::Absent
            },
            colima: false,
            docker_desktop: false,
            podman_machine: PodmanMachine::None,
            has_e2fsprogs: false,
        }
    }

    fn unmet(a: &Availability) -> Vec<&str> {
        a.needs
            .iter()
            .filter(|n| !n.ok)
            .map(|n| n.text.as_str())
            .collect()
    }

    #[test]
    fn vm_is_unavailable_without_kvm_with_a_reason() {
        let a = availability(&facts(Os::Linux), GwKind::Vm);
        match a.state {
            State::Unavailable(why) => assert!(why.contains("/dev/kvm"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vm_needs_the_kvm_group_when_present_but_not_accessible() {
        let mut f = facts(Os::Linux);
        f.kvm = Kvm::NoAccess;
        let a = availability(&f, GwKind::Vm);
        assert_eq!(a.state, State::NeedsInstall);
        assert!(unmet(&a).iter().any(|t| t.contains("KVM")));
        assert!(unmet(&a).iter().any(|t| t.contains("e2fsprogs")));
        f.kvm = Kvm::Usable;
        f.has_e2fsprogs = true;
        assert_eq!(availability(&f, GwKind::Vm).state, State::Ready);
    }

    #[test]
    fn vm_on_macos_only_needs_e2fsprogs() {
        let mut f = facts(Os::MacOs);
        assert_eq!(unmet(&availability(&f, GwKind::Vm)), ["e2fsprogs"]);
        f.has_e2fsprogs = true;
        assert_eq!(availability(&f, GwKind::Vm).state, State::Ready);
    }

    #[test]
    fn docker_ready_when_the_daemon_answers() {
        let mut f = facts(Os::Linux);
        f.runtimes_installed = vec![Runtime::Docker];
        f.runtimes_ready = vec![Runtime::Docker];
        let a = availability(&f, GwKind::Docker);
        assert_eq!(a.state, State::Ready);
        assert!(a.needs.iter().all(|n| n.ok && n.action.is_empty()));
    }

    #[test]
    fn docker_on_linux_needs_install_group_and_login() {
        let a = availability(&facts(Os::Linux), GwKind::Docker);
        assert_eq!(a.state, State::NeedsInstall);
        let u = unmet(&a);
        assert!(
            u.contains(&"docker") && u.iter().any(|t| t.contains("docker group")),
            "{u:?}"
        );
        let mut f = facts(Os::Linux);
        f.runtimes_installed = vec![Runtime::Docker];
        f.docker_permission_denied = true;
        let u = availability(&f, GwKind::Docker);
        assert!(unmet(&u).iter().any(|t| t.contains("docker group")));
        assert!(!unmet(&u).contains(&"docker"));
    }

    #[test]
    fn docker_on_macos_offers_colima_or_asks_to_start_docker_desktop() {
        let a = availability(&facts(Os::MacOs), GwKind::Docker);
        assert!(unmet(&a).iter().any(|t| t.contains("Colima")));
        let mut f = facts(Os::MacOs);
        f.docker_desktop = true;
        let a = availability(&f, GwKind::Docker);
        let n = a.needs.iter().find(|n| !n.ok).unwrap();
        assert!(
            n.text.contains("Docker Desktop") && n.action.contains("yourself"),
            "{n:?}"
        );
        let mut f = facts(Os::MacOs);
        f.colima = true;
        let u = unmet(&availability(&f, GwKind::Docker)).join("|");
        assert!(
            u.contains("Colima running") && !u.contains("colima install"),
            "{u}"
        );
    }

    #[test]
    fn podman_on_linux_needs_the_package_and_its_socket() {
        let a = availability(&facts(Os::Linux), GwKind::Podman);
        assert_eq!(unmet(&a), ["podman", "podman.socket"]);
        let mut f = facts(Os::Linux);
        f.runtimes_installed = vec![Runtime::Podman];
        f.podman_needs_socket = true;
        assert_eq!(unmet(&availability(&f, GwKind::Podman)), ["podman.socket"]);
    }

    #[test]
    fn podman_on_macos_needs_the_machine() {
        let mut f = facts(Os::MacOs);
        f.runtimes_installed = vec![Runtime::Podman];
        assert_eq!(unmet(&availability(&f, GwKind::Podman)), ["podman machine"]);
        f.podman_machine = PodmanMachine::Stopped;
        let a = availability(&f, GwKind::Podman);
        assert!(a
            .needs
            .iter()
            .any(|n| n.text == "podman machine" && n.action.contains("started")));
    }

    #[test]
    fn a_need_that_is_met_has_no_action() {
        let mut f = facts(Os::MacOs);
        f.has_e2fsprogs = true;
        let a = availability(&f, GwKind::Vm);
        assert!(a.needs[0].ok && a.needs[0].action.is_empty());
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-gw-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn kind_names_and_parsing() {
        assert_eq!(GwKind::Podman.name(), "openshell-podman");
        assert_eq!(GwKind::parse("podman"), Some(GwKind::Podman));
        assert_eq!(GwKind::parse("openshell-vm"), Some(GwKind::Vm));
        assert_eq!(GwKind::parse("lxc"), None);
        assert_eq!(GwKind::from_driver("docker"), Some(GwKind::Docker));
        assert_eq!(GwKind::from_driver("kubernetes"), None);
        assert_eq!(GwKind::Vm.label(), "VM");
        assert!(GwKind::Docker.describe().contains("containers"));
    }

    #[test]
    fn ports_derive_from_the_gateway_port() {
        let s = GatewaySpec {
            name: "openshell-docker".into(),
            kind: GwKind::Docker,
            port: 17671,
            main: false,
        };
        assert_eq!((s.health_port(), s.metrics_port()), (17681, 17691));
    }

    #[test]
    fn registry_round_trips_through_a_file() {
        let d = tmp("round");
        let mut r = Registry::default();
        r.add(GatewaySpec {
            name: "openshell-docker".into(),
            kind: GwKind::Docker,
            port: 17670,
            main: true,
        });
        r.add(GatewaySpec {
            name: "openshell-podman".into(),
            kind: GwKind::Podman,
            port: 17671,
            main: false,
        });
        r.save(&d).unwrap();
        let back = Registry::load(&d);
        assert_eq!(back, r);
        assert_eq!(back.main().unwrap().name, "openshell-docker");
        assert!(back.has_kind(GwKind::Podman) && !back.has_kind(GwKind::Vm));
        assert_eq!(back.get("openshell-podman").unwrap().port, 17671);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn registry_drops_entries_whose_name_is_not_the_kinds_name() {
        let d = tmp("badnames");
        let entry = |name: &str, kind: &str, port: u64| {
            format!(r#"{{"name":"{name}","kind":"{kind}","port":{port},"main":false}}"#)
        };
        let json = format!(
            r#"{{"gateways":[{},{},{},{},{}]}}"#,
            entry("openshell-docker", "docker", 17670),
            entry("../../etc", "docker", 17671),
            entry("openshell-vm", "docker", 17672),
            entry("openshell-podman", "podman", 0),
            entry("openshell-vm", "vm", 65530),
        );
        std::fs::write(d.join("gateways.json"), json).unwrap();
        let r = Registry::load(&d);
        let names: Vec<_> = r.gateways.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["openshell-docker"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn garbled_or_missing_registry_loads_empty() {
        let d = tmp("garbled");
        assert_eq!(Registry::load(&d), Registry::default());
        std::fs::write(d.join("gateways.json"), "not json").unwrap();
        assert_eq!(Registry::load(&d), Registry::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn add_and_remove_keep_one_entry_per_name() {
        let mut r = Registry::default();
        let s = |n: &str, k, p, m| GatewaySpec {
            name: n.into(),
            kind: k,
            port: p,
            main: m,
        };
        r.add(s("openshell-vm", GwKind::Vm, 17670, true));
        r.add(s("openshell-vm", GwKind::Vm, 17670, true));
        assert_eq!(r.gateways.len(), 1);
        r.remove("openshell-vm");
        assert!(r.gateways.is_empty() && r.main().is_none());
    }

    #[test]
    fn next_port_skips_taken_and_bound_ports() {
        assert_eq!(next_port(&[], &|_| false), 17671);
        assert_eq!(next_port(&[17671], &|p| p == 17672), 17673);
        // 17672's health port would collide with a taken 17682.
        assert_eq!(next_port(&[17682], &|_| false), 17671);
        assert_eq!(next_port(&[17671, 17672], &|_| false), 17673);
        // A gateway on 17661 already uses 17671 as its health port.
        assert_eq!(next_port(&[17661], &|_| false), 17672);
        // 17681 is 17671's health port.
        assert_eq!(next_port(&[17681], &|_| false), 17672);
    }

    #[test]
    fn pending_selection_round_trips_and_clears() {
        let d = tmp("pending");
        assert_eq!(Pending::load(&d), None);
        Pending {
            kinds: vec![GwKind::Docker, GwKind::Podman],
        }
        .save(&d)
        .unwrap();
        assert_eq!(
            Pending::load(&d).unwrap().kinds,
            vec![GwKind::Docker, GwKind::Podman]
        );
        Pending::clear(&d);
        assert_eq!(Pending::load(&d), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
