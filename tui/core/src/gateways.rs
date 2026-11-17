//! The gateways an MCP client may name: the ones osh knows (`<data>/gateways.json`). A name from
//! outside is refused before any `openshell` call, so a client cannot aim osh at an arbitrary
//! endpoint or smuggle an option through `-g`.

use crate::ops::Ctx;
use std::path::Path;

/// The gateways listed in `<dir>/gateways.json`, with the same trust rules as the registry the
/// dashboard reads: the name must be exactly what osh gives that kind, and the port must leave
/// room for the health and metrics ports above it.
pub fn names(dir: &Path) -> Vec<String> {
    let v: Option<serde_json::Value> = std::fs::read_to_string(dir.join("gateways.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    v.and_then(|v| v["gateways"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|g| {
            let driver = g["kind"].as_str()?;
            let name = g["name"].as_str()?;
            if !["vm", "docker", "podman"].contains(&driver)
                || name != format!("openshell-{driver}")
            {
                return None;
            }
            let port = u16::try_from(g["port"].as_u64()?).ok()?;
            (port != 0 && port <= u16::MAX - 20).then(|| name.to_string())
        })
        .collect()
}

/// The gateway a call runs against: the context's default without an argument, else the named
/// one, which must be known to osh.
pub fn resolve(ctx: &Ctx, arg: Option<&str>) -> Result<Option<String>, String> {
    let Some(name) = arg else {
        return Ok(ctx.gateway.clone());
    };
    if names(&ctx.dir).iter().any(|n| n == name) {
        return Ok(Some(name.to_string()));
    }
    Err(format!(
        "unknown gateway {name:?}: it is not a gateway osh knows (use one of {:?})",
        names(&ctx.dir)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dir_with(registry: &str, tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-core-gw-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("gateways.json"), registry).unwrap();
        d
    }

    fn ctx(dir: PathBuf, gateway: Option<&str>) -> Ctx {
        Ctx {
            dir,
            gateway: gateway.map(String::from),
            allow_share: false,
            refuse_risky_share: true,
            repo_vars: Some(Vec::new()),
            share_roots: Some(Vec::new()),
            version: "t".into(),
        }
    }

    const REG: &str = r#"{"gateways":[
        {"name":"openshell-vm","kind":"vm","port":17670,"main":true},
        {"name":"wrong-name","kind":"docker","port":17671,"main":false},
        {"name":"openshell-podman","kind":"podman","port":0,"main":false}]}"#;

    #[test]
    fn names_reads_valid_entries_only() {
        let d = dir_with(REG, "names");
        assert_eq!(names(&d), vec!["openshell-vm".to_string()]);
        assert!(names(&d.join("missing")).is_empty());
    }

    #[test]
    fn resolve_rejects_unusable_names() {
        let c = ctx(dir_with(REG, "reject"), None);
        for bad in ["", "-x", "a/b", "nope", "openshell-docker"] {
            let e = resolve(&c, Some(bad)).unwrap_err();
            assert!(e.contains(&format!("{bad:?}")), "{e}");
        }
        assert_eq!(
            resolve(&c, Some("openshell-vm")).unwrap(),
            Some("openshell-vm".to_string())
        );
    }

    #[test]
    fn resolve_none_keeps_the_default() {
        let c = ctx(dir_with(REG, "none"), Some("openshell-docker"));
        assert_eq!(resolve(&c, None).unwrap(), Some("openshell-docker".into()));
        let c = ctx(dir_with(REG, "none2"), None);
        assert_eq!(resolve(&c, None).unwrap(), None);
    }
}
