//! The toolchain host list (`toolchains.toml` in the data directory): hosts every new sandbox may
//! read from its first command, so an install does not fail once per host. OpenShell has no "any
//! host", so the list is exact hosts, grouped so a group can be switched off.
use std::collections::BTreeSet;

/// Hosts that a provider profile (`npm`, `js`) also defines with `allow_encoded_slash: true`.
/// OpenShell rejects two endpoints for one host that differ in a setting, so the same value has
/// to go into the list or attaching that provider to a sandbox with the list would time out.
const ENCODED_SLASH_HOSTS: &[&str] = &["registry.npmjs.org"];

/// One `[group] on|off` section of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub on: bool,
    /// `(host, git)`: `git` also allows clone and fetch over https.
    pub hosts: Vec<(String, bool)>,
}

/// Parse the list (`toolchains.toml`). A host that is not a plain exact name (a wildcard, an
/// address) is skipped. Err with the parse error when the file is not valid TOML, so the caller
/// can say so instead of silently opening nothing.
pub fn parse(text: &str) -> Result<Vec<Group>, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let list = |t: &toml::Table, key: &str| -> Vec<String> {
        t.get(key)
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .filter(|h| {
                        crate::cli::valid_host(h) && !h.contains('*') && !crate::watch::is_ip(h)
                    })
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut groups = Vec::new();
    for (name, v) in &table {
        let Some(t) = v.as_table() else { continue };
        let mut hosts: Vec<(String, bool)> =
            list(t, "hosts").into_iter().map(|h| (h, false)).collect();
        hosts.extend(list(t, "git_hosts").into_iter().map(|h| (h, true)));
        groups.push(Group {
            name: name.clone(),
            on: t
                .get("enabled")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
            hosts,
        });
    }
    Ok(groups)
}

/// The hosts of the groups that are on, each once. A host is a git host when any group says so.
pub fn hosts(groups: &[Group]) -> (Vec<String>, Vec<String>) {
    let mut plain = BTreeSet::new();
    let mut git = BTreeSet::new();
    for (h, g) in groups.iter().filter(|g| g.on).flat_map(|g| &g.hosts) {
        if *g {
            git.insert(h.clone());
        } else {
            plain.insert(h.clone());
        }
    }
    let plain = plain.into_iter().filter(|h| !git.contains(h)).collect();
    (plain, git.into_iter().collect())
}

/// The `network_policies` block for a policy file: read-only for the plain hosts, read-only plus
/// the git fetch POST for git hosts, for any program. Empty when no group is on.
pub fn network_policies(groups: &[Group]) -> String {
    let (plain, git) = hosts(groups);
    let mut out = String::new();
    if plain.is_empty() && git.is_empty() {
        return out;
    }
    out.push_str("network_policies:\n");
    if !plain.is_empty() {
        out.push_str("  osh_toolchains:\n    endpoints:\n");
        for h in &plain {
            let slash = if ENCODED_SLASH_HOSTS.contains(&h.as_str()) {
                ", allow_encoded_slash: true"
            } else {
                ""
            };
            out.push_str(&format!(
                "      - {{ host: {h}, ports: [443], protocol: rest, enforcement: enforce, access: read-only{slash} }}\n"
            ));
        }
        out.push_str("    binaries:\n      - { path: \"/**\" }\n");
    }
    if !git.is_empty() {
        out.push_str("  osh_toolchains_git:\n    endpoints:\n");
        for h in &git {
            out.push_str(&format!(
                "      - host: {h}\n        ports: [443]\n        protocol: rest\n        enforcement: enforce\n        rules:\n          - allow: {{ method: GET, path: \"/**\" }}\n          - allow: {{ method: HEAD, path: \"/**\" }}\n          - allow: {{ method: OPTIONS, path: \"/**\" }}\n          - allow: {{ method: POST, path: \"/**/git-upload-pack\" }}\n"
            ));
        }
        out.push_str("    binaries:\n      - { path: \"/**\" }\n");
    }
    out
}

/// The `openshell policy update` argument lists that give a running sandbox the same hosts, to
/// run in order, each with `--wait`. They only add rules (`policy set` would replace the whole
/// policy and drop rules added since). The git POST is a separate update because OpenShell does
/// not take `--add-allow` and `--add-endpoint` together; it needs the endpoint to exist first.
pub fn update_commands(gateway: &str, sandbox: &str, groups: &[Group]) -> Vec<Vec<String>> {
    // The command line cannot set `allow_encoded_slash`, and a rule without it would clash with the
    // npm provider's for the same host, so those hosts are left out here (the creation policy
    // sets it). Attach the `npm` or `js` profile, or answer the prompt, for them.
    let (plain, git) = hosts(groups);
    let plain: Vec<String> = plain
        .into_iter()
        .filter(|h| !ENCODED_SLASH_HOSTS.contains(&h.as_str()))
        .collect();
    let base = |extra: Vec<String>| {
        let mut a: Vec<String> = Vec::new();
        if !gateway.is_empty() {
            a.extend(["-g".into(), gateway.into()]);
        }
        a.extend(["policy".into(), "update".into(), sandbox.into()]);
        a.extend(extra);
        a.extend(["--wait".into(), "--timeout".into(), "60".into()]);
        a
    };
    let mut out = Vec::new();
    let mut add: Vec<String> = Vec::new();
    for h in plain.iter().chain(&git) {
        add.extend([
            "--add-endpoint".into(),
            format!("{h}:443:read-only:rest:enforce"),
        ]);
    }
    if add.is_empty() {
        return out;
    }
    add.extend(["--binary".into(), "/**".into()]);
    out.push(base(add));
    for h in &git {
        out.push(base(vec![
            "--rule-name".into(),
            format!("allow_{}_443", h.replace(['.', '-'], "_")),
            "--binary".into(),
            "/**".into(),
            "--add-allow".into(),
            format!("{h}:443:POST:/**/git-upload-pack"),
        ]));
    }
    out
}

/// The creation policy: the filesystem policy as shipped plus the host list.
pub fn creation_policy(base: &str, groups: &[Group]) -> String {
    let mut out = base.trim_end().to_string();
    out.push('\n');
    let net = network_policies(groups);
    if !net.is_empty() {
        out.push('\n');
        out.push_str(&net);
    }
    out
}

/// Hosts in the list that are not used because they are not exact names (wildcards, addresses,
/// junk), so the editor and the picker can say so instead of dropping them silently.
pub fn skipped_hosts(text: &str) -> Vec<String> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for v in table.values().filter_map(toml::Value::as_table) {
        for key in ["hosts", "git_hosts"] {
            for h in v
                .get(key)
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
            {
                if !crate::cli::valid_host(h) || h.contains('*') || crate::watch::is_ip(h) {
                    out.push(h.to_string());
                }
            }
        }
    }
    out
}

/// The editor's check for `toolchains.toml`: valid TOML, and every host an exact name.
pub fn check(text: &str) -> Result<(), String> {
    parse(text)?;
    match skipped_hosts(text).first() {
        Some(h) => {
            let line = text
                .lines()
                .position(|l| l.contains(&format!("\"{h}\"")))
                .map_or(String::new(), |i| format!(" at line {}", i + 1));
            Err(format!(
                "{h:?}{line} is not an exact host name: OpenShell allows no wildcards or addresses here"
            ))
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# c
[a]
enabled = true
hosts = ["x.example", "*.bad", "10.0.0.1"]
git_hosts = ["Git.example"]

[b]
enabled = false
hosts = ["y.example", "x.example"]

[c]
hosts = ["z.example"]
"#;

    fn g(text: &str) -> Vec<Group> {
        parse(text).expect("valid")
    }

    #[test]
    fn parse_reads_groups_hosts_and_git_and_skips_junk() {
        let groups = g(SAMPLE);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, "a");
        assert!(
            groups[0].on && !groups[1].on && !groups[2].on,
            "enabled defaults to off"
        );
        assert_eq!(
            groups[0].hosts,
            vec![("x.example".into(), false), ("git.example".into(), true)],
            "wildcards and addresses are dropped, names are lowercased"
        );
    }

    #[test]
    fn a_file_that_is_not_toml_is_an_error_not_an_empty_list() {
        assert!(parse(
            "[a
enabled = true"
        )
        .is_err());
        assert_eq!(parse("").unwrap(), vec![]);
    }

    #[test]
    fn only_groups_that_are_on_count_and_a_git_host_is_not_also_plain() {
        let mut groups = g(SAMPLE);
        let (plain, git) = hosts(&groups);
        assert_eq!(
            (plain, git),
            (
                vec!["x.example".to_string()],
                vec!["git.example".to_string()]
            )
        );
        groups[1].on = true;
        groups[1].hosts.push(("git.example".into(), false));
        let (plain, git) = hosts(&groups);
        assert_eq!(
            plain,
            vec!["x.example".to_string(), "y.example".to_string()]
        );
        assert_eq!(git, vec!["git.example".to_string()]);
    }

    #[test]
    fn the_shipped_list_parses_and_has_the_six_default_groups_on() {
        let groups = g(crate::assets::embedded("toolchains.toml").expect("embedded"));
        let mut on: Vec<&str> = groups
            .iter()
            .filter(|g| g.on)
            .map(|g| g.name.as_str())
            .collect();
        on.sort();
        assert_eq!(on, ["github", "go", "node", "python", "rust", "zig"]);
        assert!(groups.iter().any(|g| !g.on), "optional groups ship off");
        let (_, git) = hosts(&groups);
        assert_eq!(git, vec!["github.com".to_string()]);
    }

    #[test]
    fn the_npm_registry_carries_the_encoded_slash_setting_at_creation_and_is_skipped_live() {
        let groups = g("[n]\nenabled = true\nhosts = [\"registry.npmjs.org\", \"x.example\"]\n");
        assert!(network_policies(&groups).contains(
            "host: registry.npmjs.org, ports: [443], protocol: rest, enforcement: enforce, access: read-only, allow_encoded_slash: true"
        ));
        assert!(!network_policies(&groups).contains("x.example, ports: [443], protocol: rest, enforcement: enforce, access: read-only, allow"));
        let cmds = update_commands("", "sb", &groups).concat().join(" ");
        assert!(
            cmds.contains("x.example") && !cmds.contains("registry.npmjs.org"),
            "{cmds}"
        );
    }

    #[test]
    fn a_running_sandbox_gets_one_endpoint_update_then_one_git_allow_per_git_host() {
        let groups = g("[a]\nenabled = true\nhosts = [\"x.example\"]\ngit_hosts = [\"git-host.example\"]\n[b]\nenabled = false\nhosts = [\"z.example\"]\n");
        let cmds = update_commands("gw", "sb", &groups);
        assert_eq!(cmds.len(), 2);
        let first = cmds[0].join(" ");
        assert!(
            first.starts_with("-g gw policy update sb --add-endpoint "),
            "{first}"
        );
        assert!(first.contains("--add-endpoint x.example:443:read-only:rest:enforce"));
        assert!(first.contains("--add-endpoint git-host.example:443:read-only:rest:enforce"));
        assert!(first.contains("--binary /** --wait"));
        assert!(
            !first.contains("z.example"),
            "groups that are off are skipped"
        );
        assert!(
            !first.contains("--add-allow"),
            "OpenShell refuses both in one update"
        );
        let second = cmds[1].join(" ");
        assert!(
            second.contains("--rule-name allow_git_host_example_443"),
            "{second}"
        );
        assert!(second.contains("--add-allow git-host.example:443:POST:/**/git-upload-pack"));
        assert!(update_commands(
            "",
            "sb",
            &g("[a]\nenabled = false\nhosts = [\"x.example\"]\n")
        )
        .is_empty());
    }

    #[test]
    fn creation_policy_keeps_the_base_and_appends_the_block() {
        let groups =
            g("[a]\nenabled = true\nhosts = [\"x.example\"]\ngit_hosts = [\"git.example\"]\n");
        let p = creation_policy("version: 1\nfilesystem_policy: {}\n", &groups);
        assert!(p.starts_with("version: 1\nfilesystem_policy: {}\n\nnetwork_policies:\n"));
        assert!(p.contains("host: x.example, ports: [443]"));
        assert!(p.contains("- host: git.example\n"));
        assert!(p.contains("POST, path: \"/**/git-upload-pack\""));
        assert_eq!(
            creation_policy(
                "version: 1\n",
                &g("[a]\nenabled = false\nhosts = [\"x.example\"]\n")
            ),
            "version: 1\n"
        );
    }

    #[test]
    fn wildcard_and_address_hosts_are_named_and_the_editor_check_refuses_them() {
        let text = "[a]\nhosts = [\"ok.example\", \"*.bad\"]\ngit_hosts = [\"10.0.0.1\"]\n";
        assert_eq!(skipped_hosts(text), vec!["*.bad", "10.0.0.1"]);
        let e = check(text).unwrap_err();
        assert!(e.contains("*.bad") && e.contains("line 2"), "{e}");
        assert!(check("[a]\nhosts = [\"ok.example\"]\n").is_ok());
        assert!(check("not toml [").is_err());
    }
}
