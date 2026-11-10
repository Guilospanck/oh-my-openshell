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
/// run in order. They only add rules (`policy set` would replace the whole policy and drop rules
/// added since). Each host is its own update: OpenShell rejects a whole update when one host
/// clashes with a rule already on the sandbox (a provider's, say), and one clash must not block
/// the other hosts. Only the last waits for the sandbox to load the policy. The git POST is a
/// separate update after the endpoints because OpenShell does not take `--add-allow` and
/// `--add-endpoint` together; it needs the endpoint to exist first.
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
        a
    };
    let mut out: Vec<Vec<String>> = Vec::new();
    for h in plain.iter().chain(&git) {
        out.push(base(vec![
            "--add-endpoint".into(),
            format!("{h}:443:read-only:rest:enforce"),
            "--binary".into(),
            "/**".into(),
        ]));
    }
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
    if let Some(last) = out.last_mut() {
        last.extend(["--wait".into(), "--timeout".into(), "60".into()]);
    }
    out
}

/// The host an `update_commands` list entry is about, for progress and reports.
pub fn command_host(args: &[String]) -> String {
    args.iter()
        .find_map(|a| a.split_once(":443:").map(|(h, _)| h.to_string()))
        .unwrap_or_default()
}

/// What a refused update means in plain words. OpenShell's own text is a wall of internals; the
/// usual cause (another rule, often a provider's, already covers the host) gets a short name.
fn refusal(err: &str) -> Option<String> {
    if err.contains("undeclared authorization") || err.contains("would grant binary") {
        return None;
    }
    let first = err.lines().map(str::trim).find(|l| !l.is_empty())?;
    let clean = first
        .trim_start_matches("Error:")
        .trim_start_matches('\u{d7}')
        .trim();
    Some(clean.chars().take(120).collect())
}

/// The popup text after the hosts were sent: what opened, what a rule already covered, and what
/// failed for another reason. `results` holds one entry per update, host first.
pub fn report(sandbox: &str, results: &[(String, Result<(), String>)]) -> String {
    let mut ok: Vec<&str> = Vec::new();
    let mut covered: Vec<&str> = Vec::new();
    let mut failed: Vec<(&str, String)> = Vec::new();
    for (host, r) in results {
        match r {
            Ok(()) => {
                if !ok.contains(&host.as_str()) {
                    ok.push(host);
                }
            }
            Err(e) => match refusal(e) {
                None => covered.push(host),
                Some(why) => failed.push((host, why)),
            },
        }
    }
    let mut out = format!("\u{2713} {} hosts are open in {sandbox}.", ok.len());
    if !covered.is_empty() {
        out.push_str(&format!(
            "\n\n\u{2022} {} were already allowed by another rule (a provider's, usually), so nothing changed for them:\n  {}",
            covered.len(),
            covered.join(", ")
        ));
    }
    if !failed.is_empty() {
        out.push_str(&format!("\n\n\u{2717} {} failed:", failed.len()));
        for (h, why) in failed {
            out.push_str(&format!("\n  {h}  {why}"));
        }
    }
    out
}

/// The picker line of a group: its name first, so `group_of` can read the pick back.
pub fn label(g: &Group) -> String {
    let n = g.hosts.len();
    format!("{}  ({n} host{})", g.name, if n == 1 { "" } else { "s" })
}

/// The group a picker line (`label`) stands for.
pub fn group_of(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or_default()
}

/// The groups with exactly the named ones switched on, for one sandbox's choice.
pub fn select(groups: &[Group], picks: &[String]) -> Vec<Group> {
    groups
        .iter()
        .map(|g| Group {
            on: picks.iter().any(|p| p == &g.name),
            ..g.clone()
        })
        .collect()
}

/// `text` with each group's `enabled` line set to `want`, so a default chosen in the TUI lands in
/// the file without losing its comments, hosts or order. A group without the line gets one under
/// its header. Groups not named stay as they are.
pub fn set_enabled(text: &str, want: &[(String, bool)]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut section: Option<&(String, bool)> = None;
    let mut seen = false;
    let finish = |out: &mut Vec<String>, section: Option<&(String, bool)>, seen: bool| {
        // A section that never had `enabled` gets it right after its header.
        if let (Some((_, on)), false) = (section, seen) {
            let at = out
                .iter()
                .rposition(|l| l.trim_start().starts_with('['))
                .map_or(out.len(), |i| i + 1);
            out.insert(at, format!("enabled = {on}"));
        }
    };
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            finish(&mut out, section, seen);
            let name = t.trim_matches(|c| c == '[' || c == ']').trim();
            section = want.iter().find(|(n, _)| n == name);
            seen = false;
        } else if let (Some((_, on)), true) = (section, t.starts_with("enabled")) {
            if t.trim_start_matches("enabled")
                .trim_start()
                .starts_with('=')
            {
                out.push(format!("enabled = {on}"));
                seen = true;
                continue;
            }
        }
        out.push(line.to_string());
    }
    finish(&mut out, section, seen);
    let mut joined = out.join("\n");
    joined.push('\n');
    joined
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
    fn a_running_sandbox_gets_one_update_per_host_then_one_git_allow_per_git_host() {
        let groups = g("[a]\nenabled = true\nhosts = [\"x.example\"]\ngit_hosts = [\"git-host.example\"]\n[b]\nenabled = false\nhosts = [\"z.example\"]\n");
        let cmds = update_commands("gw", "sb", &groups);
        assert_eq!(cmds.len(), 3);
        let first = cmds[0].join(" ");
        assert_eq!(
            first,
            "-g gw policy update sb --add-endpoint x.example:443:read-only:rest:enforce --binary /**"
        );
        assert!(cmds[1]
            .join(" ")
            .contains("--add-endpoint git-host.example:443:read-only:rest:enforce"));
        assert!(
            !cmds.concat().join(" ").contains("z.example"),
            "groups that are off are skipped"
        );
        assert!(
            !cmds[..2].concat().join(" ").contains("--add-allow"),
            "OpenShell refuses both in one update"
        );
        let second = cmds[2].join(" ");
        assert!(
            second.ends_with("--wait --timeout 60"),
            "only the last waits"
        );
        assert!(!first.contains("--wait") && !cmds[1].join(" ").contains("--wait"));
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
    fn the_report_names_what_opened_was_covered_and_failed() {
        let r = vec![
            ("a.example".to_string(), Ok(())),
            (
                "b.example".to_string(),
                Err("Error:   \u{d7} merge operation 0 add-rule 'x' would grant binary '/**' undeclared authorization".to_string()),
            ),
            ("c.example".to_string(), Err("Error: boom\nmore".to_string())),
        ];
        let t = report("sb", &r);
        assert!(t.starts_with("\u{2713} 1 hosts are open in sb."), "{t}");
        assert!(t.contains("1 were already allowed by another rule"), "{t}");
        assert!(t.contains("  b.example"), "{t}");
        assert!(t.contains("\u{2717} 1 failed:\n  c.example  boom"), "{t}");
        assert!(!t.contains("merge operation"), "{t}");
        assert_eq!(
            command_host(&["x".into(), "h.example:443:read-only:rest:enforce".into()]),
            "h.example"
        );
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
    #[test]
    fn select_switches_exactly_the_named_groups_on() {
        let groups = select(&g(SAMPLE), &["b".to_string()]);
        assert!(!groups[0].on && groups[1].on && !groups[2].on);
        assert_eq!(group_of(&label(&groups[0])), "a");
        assert_eq!(label(&groups[2]), "c  (1 host)");
    }

    #[test]
    fn set_enabled_rewrites_the_flag_and_keeps_everything_else() {
        let out = set_enabled(
            SAMPLE,
            &[("a".into(), false), ("b".into(), true), ("c".into(), true)],
        );
        let groups = g(&out);
        assert!(!groups[0].on && groups[1].on && groups[2].on);
        assert!(out.contains("# c\n"), "comments stay");
        assert!(
            out.contains("hosts = [\"x.example\", \"*.bad\""),
            "hosts stay"
        );
        assert_eq!(set_enabled(&out, &[("a".into(), false)]), out, "idempotent");
    }
}
