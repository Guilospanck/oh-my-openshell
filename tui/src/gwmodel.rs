//! What a gateway is to osh: its kind, its name and port, and the registry of the ones osh made.
#![allow(dead_code)] // used by the tasks that follow in the plan; remove when wired (Task 7)
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
                Some(GatewaySpec {
                    name: g["name"].as_str()?.to_string(),
                    kind: GwKind::from_driver(g["kind"].as_str()?)?,
                    port: u16::try_from(g["port"].as_u64()?).ok()?,
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
    use std::path::PathBuf;

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
