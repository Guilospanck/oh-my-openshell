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

/// Start of the name of every network rule the list makes: one rule per host (`rule_name`), so
/// closing a host in a running sandbox is `--remove-rule` of exactly that rule. OpenShell's other
/// remove, `--remove-endpoint`, takes the host out of every rule of the sandbox, the user's own too.
pub const RULE_PREFIX: &str = "osh_tc_";
/// The rules osh wrote at creation before it made one per host: all the hosts of all chosen groups
/// in one rule (and the git hosts in a second), so one group cannot be closed by removing a rule.
pub const LEGACY_RULES: [&str; 2] = ["osh_toolchains", "osh_toolchains_git"];

/// The rule osh's list uses for `host`. Hosts are `[a-z0-9.-]` (`parse`), so turning the dots into
/// underscores keeps two hosts from sharing a name.
pub fn rule_name(host: &str) -> String {
    format!("{RULE_PREFIX}{}", host.replace('.', "_"))
}

/// The `network_policies` block for a policy file: one rule per host (`rule_name`), read-only for
/// the plain hosts, read-only plus the git fetch POST for git hosts, for any program. Empty when
/// no group is on.
pub fn network_policies(groups: &[Group]) -> String {
    let (plain, git) = hosts(groups);
    let mut out = String::new();
    if plain.is_empty() && git.is_empty() {
        return out;
    }
    out.push_str("network_policies:\n");
    let mut all: Vec<(&String, bool)> = plain.iter().map(|h| (h, false)).collect();
    all.extend(git.iter().map(|h| (h, true)));
    all.sort();
    for (h, is_git) in all {
        out.push_str(&format!("  {}:\n    endpoints:\n", rule_name(h)));
        if is_git {
            out.push_str(&format!(
                "      - host: {h}\n        ports: [443]\n        protocol: rest\n        enforcement: enforce\n        rules:\n          - allow: {{ method: GET, path: \"/**\" }}\n          - allow: {{ method: HEAD, path: \"/**\" }}\n          - allow: {{ method: OPTIONS, path: \"/**\" }}\n          - allow: {{ method: POST, path: \"/**/git-upload-pack\" }}\n"
            ));
        } else {
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
    out
}

/// The `openshell policy update` argument lists that give a running sandbox the same hosts, to
/// run in order. They only add rules (`policy set` would replace the whole policy and drop rules
/// added since). Each host is its own update: OpenShell rejects a whole update when one host
/// clashes with a rule already on the sandbox (a provider's, say), and one clash must not block
/// the other hosts. Only the last waits for the sandbox to load the policy. The git POST is a
/// separate update after the endpoints because OpenShell does not take `--add-allow` and
/// `--add-endpoint` together; it needs the endpoint to exist first. Each host goes into its own
/// rule (`rule_name`), as at creation, so it can be closed alone; when another rule of the
/// sandbox already has the host, OpenShell folds the endpoint into that one instead.
pub fn update_commands(gateway: &str, sandbox: &str, groups: &[Group]) -> Vec<Vec<String>> {
    // The command line cannot set `allow_encoded_slash`, and a rule without it would clash with the
    // npm provider's for the same host, so those hosts are left out here (the creation policy
    // sets it). Attach the `npm` or `js` profile, or answer the prompt, for them.
    let (plain, git) = hosts(groups);
    let plain: Vec<String> = plain
        .into_iter()
        .filter(|h| !ENCODED_SLASH_HOSTS.contains(&h.as_str()))
        .collect();
    let base = |extra: Vec<String>| update_base(gateway, sandbox, extra);
    let mut out: Vec<Vec<String>> = Vec::new();
    for h in plain.iter().chain(&git) {
        out.push(base(vec![
            "--add-endpoint".into(),
            format!("{h}:443:read-only:rest:enforce"),
            "--rule-name".into(),
            rule_name(h),
            "--binary".into(),
            "/**".into(),
        ]));
    }
    for h in &git {
        out.push(base(vec![
            "--rule-name".into(),
            rule_name(h),
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

/// `[-g GATEWAY] policy update SANDBOX EXTRA...`.
fn update_base(gateway: &str, sandbox: &str, extra: Vec<String>) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if !gateway.is_empty() {
        a.extend(["-g".into(), gateway.into()]);
    }
    a.extend(["policy".into(), "update".into(), sandbox.into()]);
    a.extend(extra);
    a
}

/// The read-only call whose output `parse_rules` reads: the sandbox's effective policy, provider
/// rules included, so the close can say which other rules still allow a host.
pub fn policy_get_args(gateway: &str, sandbox: &str) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if !gateway.is_empty() {
        a.extend(["-g".into(), gateway.into()]);
    }
    a.extend(
        ["policy", "get", sandbox, "--full", "-o", "json"]
            .iter()
            .map(|s| s.to_string()),
    );
    a
}

/// Rule name to the hosts it allows on port 443, lowercased (wildcards as written).
pub type Rules = std::collections::BTreeMap<String, Vec<String>>;

/// The network rules of `openshell policy get --full -o json` (`.policy.network_policies`).
pub fn parse_rules(json: &str) -> Result<Rules, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("policy get: not JSON ({e})"))?;
    let Some(rules) = v.pointer("/policy/network_policies") else {
        return Ok(Rules::new());
    };
    let rules = rules
        .as_object()
        .ok_or("policy get: network_policies is not a map")?;
    let mut out = Rules::new();
    for (name, rule) in rules {
        let hosts = rule
            .get("endpoints")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter(|e| {
                let port = e.get("port").and_then(serde_json::Value::as_u64);
                let ports = e.get("ports").and_then(serde_json::Value::as_array);
                port == Some(443)
                    || ports.is_some_and(|p| p.iter().any(|x| x.as_u64() == Some(443)))
            })
            .filter_map(|e| e.get("host").and_then(serde_json::Value::as_str))
            .map(str::to_ascii_lowercase)
            .collect();
        out.insert(name.clone(), hosts);
    }
    Ok(out)
}

/// A rule host covers `host`: the same name, or a `*.` / `**.` wildcard over it.
fn covers(pattern: &str, host: &str) -> bool {
    if pattern == host {
        return true;
    }
    pattern
        .strip_prefix("**.")
        .or_else(|| pattern.strip_prefix("*."))
        .is_some_and(|suffix| host.ends_with(&format!(".{suffix}")))
}

/// What closing does to one host of an unmarked group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// osh's rules that hold it are removed (its own `rule_name`, and the old combined rule
    /// when every host in that one is being closed too).
    Remove(Vec<String>),
    /// A group that stays marked needs it too, so it stays open.
    Shared(Vec<String>),
    /// Only the old combined rule (`LEGACY_RULES`) holds it, and that rule also holds hosts of
    /// groups that stay: removing it would close those too.
    Legacy(String),
    /// No rule of osh's holds it (already closed, or never opened by osh): nothing to remove.
    NotOpen,
}

/// One host of the close: what is done, and the rules that are not osh's and still allow it
/// (a provider's, the user's own), which closing leaves alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostClose {
    pub host: String,
    pub how: How,
    pub also: Vec<String>,
}

/// A rule osh's toolchain list made: the only ones a close may remove. Never a provider rule.
pub fn is_osh_rule(name: &str) -> bool {
    !name.starts_with("_provider")
        && (name.starts_with(RULE_PREFIX) || LEGACY_RULES.contains(&name))
}

/// What closing the groups named in `close` does to each of their hosts, given the groups that
/// stay marked (`keep`) and the sandbox's rules. A host a kept group also lists stays open; a host
/// only the old combined rule holds stays open unless every host of that rule is being closed.
pub fn close_plan(
    all: &[Group],
    keep: &[String],
    close: &[String],
    rules: &Rules,
) -> Vec<HostClose> {
    let hosts_of = |names: &[String]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for g in all.iter().filter(|g| names.contains(&g.name)) {
            for (h, _) in &g.hosts {
                if !out.contains(h) {
                    out.push(h.clone());
                }
            }
        }
        out
    };
    let needed = hosts_of(keep);
    let closing: Vec<String> = hosts_of(close)
        .into_iter()
        .filter(|h| !needed.contains(h))
        .collect();
    // An old combined rule can go only when nothing in it is still needed.
    let legacy_free = |name: &str| {
        rules
            .get(name)
            .is_some_and(|hs| hs.iter().all(|h| closing.contains(h)))
    };
    let mut out = Vec::new();
    for h in hosts_of(close) {
        let also: Vec<String> = rules
            .iter()
            .filter(|(n, hs)| !is_osh_rule(n) && hs.iter().any(|p| covers(p, &h)))
            .map(|(n, _)| n.clone())
            .collect();
        let how = if needed.contains(&h) {
            How::Shared(
                all.iter()
                    .filter(|g| keep.contains(&g.name) && g.hosts.iter().any(|(x, _)| x == &h))
                    .map(|g| g.name.clone())
                    .collect(),
            )
        } else {
            let own = rule_name(&h);
            let legacy: Vec<&str> = LEGACY_RULES
                .iter()
                .copied()
                .filter(|n| rules.get(*n).is_some_and(|hs| hs.contains(&h)))
                .collect();
            if let Some(stuck) = legacy.iter().find(|n| !legacy_free(n)) {
                How::Legacy(stuck.to_string())
            } else {
                let mut remove: Vec<String> = legacy.iter().map(|s| s.to_string()).collect();
                if rules.contains_key(&own) {
                    remove.insert(0, own);
                }
                if remove.is_empty() {
                    How::NotOpen
                } else {
                    How::Remove(remove)
                }
            }
        };
        out.push(HostClose { host: h, how, also });
    }
    out
}

/// The `policy update --remove-rule` calls for a plan: one per rule, each rule once, only rules
/// osh made (`is_osh_rule`). Only the last waits for the sandbox to load the policy.
pub fn close_commands(
    gateway: &str,
    sandbox: &str,
    plan: &[HostClose],
) -> Vec<(String, Vec<String>)> {
    let mut rules: Vec<String> = Vec::new();
    for hc in plan {
        if let How::Remove(rs) = &hc.how {
            for r in rs {
                if is_osh_rule(r) && !rules.contains(r) {
                    rules.push(r.clone());
                }
            }
        }
    }
    let mut out: Vec<(String, Vec<String>)> = rules
        .into_iter()
        .map(|r| {
            let args = update_base(gateway, sandbox, vec!["--remove-rule".into(), r.clone()]);
            (r, args)
        })
        .collect();
    if let Some((_, last)) = out.last_mut() {
        last.extend(["--wait".into(), "--timeout".into(), "60".into()]);
    }
    out
}

/// Whether each host of the plan closed, from the result of each removed rule: closed when every
/// rule it needed removed went; `Shared`, `Legacy` and `NotOpen` hosts send nothing.
pub fn close_results(
    plan: &[HostClose],
    results: &[(String, Result<(), String>)],
) -> Vec<(String, Result<(), String>)> {
    plan.iter()
        .filter_map(|hc| match &hc.how {
            How::Remove(rs) => {
                let err = rs.iter().find_map(|r| {
                    results
                        .iter()
                        .find(|(n, _)| n == r)
                        .and_then(|(_, res)| res.clone().err())
                });
                Some((hc.host.clone(), err.map_or(Ok(()), Err)))
            }
            _ => None,
        })
        .collect()
}

/// The groups of `close` that are closed now: none of their hosts stayed open because of an old
/// combined rule or failed. Hosts a kept group needs, or that only a rule that is not osh's
/// allows, do not keep a group open: osh's part of it is gone.
pub fn closed_groups(
    all: &[Group],
    close: &[String],
    plan: &[HostClose],
    done: &[(String, Result<(), String>)],
) -> (Vec<String>, Vec<String>) {
    let mut closed = Vec::new();
    let mut stuck = Vec::new();
    for g in all.iter().filter(|g| close.contains(&g.name)) {
        let of = |h: &str| plan.iter().find(|hc| hc.host == h);
        let legacy = g
            .hosts
            .iter()
            .any(|(h, _)| of(h).is_some_and(|hc| matches!(hc.how, How::Legacy(_))));
        let failed = g
            .hosts
            .iter()
            .any(|(h, _)| done.iter().any(|(x, r)| x == h && r.is_err()));
        if legacy {
            stuck.push(g.name.clone());
        } else if !failed {
            closed.push(g.name.clone());
        }
    }
    (closed, stuck)
}

/// The popup text for a close: what closed, what stays open and why, what failed.
pub fn close_report(
    sandbox: &str,
    plan: &[HostClose],
    done: &[(String, Result<(), String>)],
) -> String {
    let ok: Vec<&str> = done
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(h, _)| h.as_str())
        .collect();
    let mut out = format!("\u{2713} {} hosts are closed in {sandbox}.", ok.len());
    if !ok.is_empty() {
        out.push_str(&format!("\n  {}", ok.join(", ")));
    }
    let shared: Vec<String> = plan
        .iter()
        .filter_map(|hc| match &hc.how {
            How::Shared(gs) => Some(format!("{} ({})", hc.host, gs.join(", "))),
            _ => None,
        })
        .collect();
    if !shared.is_empty() {
        out.push_str(&format!(
            "\n\n\u{2022} {} stay open because a group that is still marked needs them:\n  {}",
            shared.len(),
            shared.join(", ")
        ));
    }
    let legacy: Vec<&HostClose> = plan
        .iter()
        .filter(|hc| matches!(hc.how, How::Legacy(_)))
        .collect();
    if let Some(first) = legacy.first() {
        let How::Legacy(rule) = &first.how else {
            unreachable!()
        };
        out.push_str(&format!(
            "\n\n\u{2022} {} stay open: an older osh created this sandbox with the hosts of all its groups in one rule ({rule}), and removing it would also close the groups that stay marked. Unmark every group to close them all, or create a new sandbox:\n  {}",
            legacy.len(),
            legacy.iter().map(|hc| hc.host.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    let also: Vec<String> = plan
        .iter()
        .filter(|hc| !hc.also.is_empty() && !matches!(hc.how, How::Shared(_)))
        .map(|hc| format!("{} ({})", hc.host, hc.also.join(", ")))
        .collect();
    if !also.is_empty() {
        out.push_str(&format!(
            "\n\n\u{2022} {} are still allowed by a rule that is not one of osh's toolchain rules (a provider's, your own, or an allow_HOST_443 an older osh added), which osh does not remove:\n  {}",
            also.len(),
            also.join(", ")
        ));
    }
    let failed: Vec<(&str, String)> = done
        .iter()
        .filter_map(|(h, r)| {
            r.as_ref().err().map(|e| {
                (
                    h.as_str(),
                    refusal(e).unwrap_or_else(|| e.trim().to_string()),
                )
            })
        })
        .collect();
    if !failed.is_empty() {
        out.push_str(&format!("\n\n\u{2717} {} failed to close:", failed.len()));
        for (h, why) in failed {
            out.push_str(&format!("\n  {h}  {why}"));
        }
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
        .trim_start()
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

/// Picker tag of a group the file switches on for every new sandbox.
pub const TAG_DEFAULT: &str = "(default)";
/// Running-sandbox picker tag of a group osh recorded as open there, while it is marked.
pub const TAG_OPEN: &str = "(open here)";
/// The same group once unmarked: applying closes its hosts (`close_plan`).
pub const TAG_CLOSES: &str = "(closes)";
/// An unmarked open group a close already found held by the old combined rule (`How::Legacy`).
pub const TAG_STAYS: &str = "(stays open: older osh rule)";

/// The picker line of a group: its name first, so `group_of` can read the pick back, then the
/// tags (`TAG_*`) that tell the file's defaults and what a sandbox already has apart from the marks.
pub fn label(g: &Group, open_here: bool) -> String {
    let n = g.hosts.len();
    let mut out = format!("{}  ({n} host{})", g.name, if n == 1 { "" } else { "s" });
    if g.on {
        out.push_str(&format!("  {TAG_DEFAULT}"));
    }
    if open_here {
        out.push_str(&format!("  {TAG_OPEN}"));
    }
    out
}

/// A running-sandbox picker line after Space: a group that is open there says it closes when
/// unmarked, or that it stays open when an earlier close found its hosts in the combined rule an
/// older osh wrote (`stuck`), which cannot be removed while other groups stay.
pub fn retag_open(line: &str, marked: bool, stuck: bool) -> String {
    let line = line
        .replace(TAG_STAYS, TAG_OPEN)
        .replace(TAG_CLOSES, TAG_OPEN);
    if marked {
        line
    } else if stuck {
        line.replace(TAG_OPEN, TAG_STAYS)
    } else {
        line.replace(TAG_OPEN, TAG_CLOSES)
    }
}

/// `"a, b"`, or `"none"`: the groups the file switches on, for the status line after a save.
pub fn defaults_line(groups: &[Group]) -> String {
    let on: Vec<&str> = groups
        .iter()
        .filter(|g| g.on)
        .map(|g| g.name.as_str())
        .collect();
    if on.is_empty() {
        "none".into()
    } else {
        on.join(", ")
    }
}

/// The marks of a toolchain picker, kept while `toolchains.toml` is edited from it: `picked` are
/// the marked groups, `known` every group the picker showed, so a group the edit added can be told
/// from one the user unmarked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks {
    pub picked: Vec<String>,
    pub known: Vec<String>,
    /// The new-sandbox picker's "save as the default" line was marked.
    pub save: bool,
}

/// The marks for `groups` coming back from the editor: the user's own marks for groups the picker
/// showed before, the file's `enabled` for groups the edit added. Removed groups are gone with
/// `groups`.
pub fn remark(groups: &[Group], prev: &Marks) -> Vec<bool> {
    groups
        .iter()
        .map(|g| {
            if prev.known.contains(&g.name) {
                prev.picked.contains(&g.name)
            } else {
                g.on
            }
        })
        .collect()
}

/// Which groups osh opened in which sandbox (`<data>/sandbox-toolchains.json`, keyed by
/// `record_key`): written when osh creates a sandbox and after each apply, so the running picker
/// can start from what that sandbox has instead of the defaults for new ones.
pub const RECORD_FILE: &str = "sandbox-toolchains.json";

pub fn record_key(gateway: &str, sandbox: &str) -> String {
    format!("{gateway}/{sandbox}")
}

pub type Records = std::collections::BTreeMap<String, Vec<String>>;

/// The record; empty when it is missing or unreadable (every sandbox is then "unknown").
pub fn load_records(dir: &std::path::Path) -> Records {
    std::fs::read_to_string(dir.join(RECORD_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_records(dir: &std::path::Path, records: &Records) -> Result<(), String> {
    let text = serde_json::to_string_pretty(records).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(RECORD_FILE), text + "\n").map_err(|e| e.to_string())
}

/// Drop the entries of `gateway` whose sandbox is not in `live`. Other gateways are left alone:
/// osh only knows the sandboxes of the gateway it shows. True when something was dropped.
pub fn prune(records: &mut Records, gateway: &str, live: &[String]) -> bool {
    let before = records.len();
    let prefix = record_key(gateway, "");
    records.retain(|k, _| {
        k.strip_prefix(&prefix).is_none_or(|sb| {
            let sb = sb.strip_suffix(STUCK_SUFFIX).unwrap_or(sb);
            live.iter().any(|l| l == sb)
        })
    });
    records.len() != before
}

/// Suffix of the record key (`record_key` + this) that lists the open groups a close found held
/// by an older osh's combined rule (`How::Legacy`), so the picker can say they stay open.
pub const STUCK_SUFFIX: &str = "#stays";

/// The groups recorded as stuck open in a sandbox (`STUCK_SUFFIX`).
pub fn stuck_groups(records: &Records, gateway: &str, sandbox: &str) -> Vec<String> {
    records
        .get(&(record_key(gateway, sandbox) + STUCK_SUFFIX))
        .cloned()
        .unwrap_or_default()
}

/// Add `groups` to what is recorded as open in a sandbox (`replace`: at creation, the choice is
/// the whole of it, and nothing is stuck).
pub fn record_open(
    dir: &std::path::Path,
    gateway: &str,
    sandbox: &str,
    groups: &[String],
    replace: bool,
) -> Result<(), String> {
    if replace {
        let mut records = load_records(dir);
        records.remove(&(record_key(gateway, sandbox) + STUCK_SUFFIX));
        records.insert(record_key(gateway, sandbox), Vec::new());
        save_records(dir, &records)?;
    }
    record_apply(dir, gateway, sandbox, groups, &[], &[], &[])
}

/// After an apply: `opened` join the record, `closed` leave it, and the stuck list becomes what
/// this close found (`stuck`) plus what was stuck before and was not tried again (`tried`).
/// Only groups still recorded open can be stuck.
pub fn record_apply(
    dir: &std::path::Path,
    gateway: &str,
    sandbox: &str,
    opened: &[String],
    closed: &[String],
    stuck: &[String],
    tried: &[String],
) -> Result<(), String> {
    let mut records = load_records(dir);
    let key = record_key(gateway, sandbox);
    let entry = records.entry(key.clone()).or_default();
    for g in opened {
        if !entry.contains(g) {
            entry.push(g.clone());
        }
    }
    entry.retain(|g| !closed.contains(g));
    entry.sort();
    let open = entry.clone();
    let skey = key + STUCK_SUFFIX;
    let mut s: Vec<String> = records
        .get(&skey)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|g| !tried.contains(g))
        .chain(stuck.iter().cloned())
        .filter(|g| open.contains(g))
        .collect();
    s.sort();
    s.dedup();
    if s.is_empty() {
        records.remove(&skey);
    } else {
        records.insert(skey, s);
    }
    save_records(dir, &records)
}

/// The groups of `groups` (the ones that are on) that ended up open after the updates: every host
/// they have that was sent was opened or already allowed by another rule. A group with a failed
/// host is not counted, so the record never claims more than the sandbox has.
pub fn opened_groups(groups: &[Group], results: &[(String, Result<(), String>)]) -> Vec<String> {
    let failed = |h: &str| {
        results
            .iter()
            .any(|(host, r)| host == h && matches!(r, Err(e) if refusal(e).is_some()))
    };
    groups
        .iter()
        .filter(|g| g.on && !g.hosts.iter().any(|(h, _)| failed(h)))
        .map(|g| g.name.clone())
        .collect()
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

/// The creation policy: the filesystem policy plus the host list. When the base already has a
/// top-level `network_policies` block (the user added rules of their own), the list's entries go
/// into that block at its indentation: a second top-level key would be a duplicate the gateway
/// rejects or silently drops. Err when the base's block cannot take them (flow style, or a name
/// osh uses already); the caller then creates the sandbox without the list and says why.
pub fn creation_policy(base: &str, groups: &[Group]) -> Result<String, String> {
    let mut out = base.trim_end().to_string();
    out.push('\n');
    let net = network_policies(groups);
    if net.is_empty() {
        return Ok(out);
    }
    let lines: Vec<&str> = out.lines().collect();
    let Some(at) = lines
        .iter()
        .position(|l| l.starts_with("network_policies:"))
    else {
        out.push('\n');
        out.push_str(&net);
        return Ok(out);
    };
    let after = lines[at]["network_policies:".len()..]
        .split(" #")
        .next()
        .unwrap_or_default()
        .trim();
    let child = lines[at + 1..]
        .iter()
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter(|l| l.starts_with(' '));
    let indent = match (after, child) {
        ("" | "{}" | "~" | "null", None) => "  ".to_string(),
        ("", Some(l)) if !l.trim_start().starts_with('-') => {
            l[..l.len() - l.trim_start().len()].to_string()
        }
        _ => {
            return Err(
                "sandbox-policy.yaml writes network_policies in a form osh cannot add to (write it as a block: one policy name per line), so the toolchain hosts were left out".into(),
            )
        }
    };
    let names = net
        .lines()
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .map(str::trim);
    for name in names {
        if lines.iter().any(|l| l.trim_start() == name) {
            return Err(format!(
                "sandbox-policy.yaml already has a policy named {}, so the toolchain hosts were left out",
                name.trim_end_matches(':')
            ));
        }
    }
    let entries: Vec<String> = net
        .lines()
        .skip(1)
        .map(|l| format!("{indent}{}", l.strip_prefix("  ").unwrap_or(l)))
        .collect();
    let mut merged: Vec<String> = lines[..at].iter().map(|s| s.to_string()).collect();
    merged.push("network_policies:".into());
    merged.extend(entries);
    merged.extend(lines[at + 1..].iter().map(|s| s.to_string()));
    let merged = merged.join("\n") + "\n";
    crate::assets::check_yaml(&merged).map_err(|e| {
        format!("adding the toolchain hosts to sandbox-policy.yaml would not parse ({e}), so they were left out")
    })?;
    Ok(merged)
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
            "-g gw policy update sb --add-endpoint x.example:443:read-only:rest:enforce --rule-name osh_tc_x_example --binary /**"
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
            second.contains("--rule-name osh_tc_git-host_example"),
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
        let p = creation_policy("version: 1\nfilesystem_policy: {}\n", &groups).unwrap();
        assert!(p.starts_with("version: 1\nfilesystem_policy: {}\n\nnetwork_policies:\n"));
        assert!(p.contains("host: x.example, ports: [443]"));
        assert!(p.contains("- host: git.example\n"));
        assert!(p.contains("POST, path: \"/**/git-upload-pack\""));
        assert_eq!(
            creation_policy(
                "version: 1\n",
                &g("[a]\nenabled = false\nhosts = [\"x.example\"]\n")
            )
            .unwrap(),
            "version: 1\n"
        );
    }

    #[test]
    fn the_hosts_join_a_network_policies_block_the_base_already_has() {
        let groups = g("[a]\nenabled = true\nhosts = [\"x.example\"]\n");
        let base =
            "version: 1\nnetwork_policies:\n    mine:\n        endpoints: []\nlandlock: {}\n";
        let p = creation_policy(base, &groups).unwrap();
        assert_eq!(p.matches("network_policies:").count(), 1, "{p}");
        assert!(
            p.contains("\n    osh_tc_x_example:\n      endpoints:\n"),
            "{p}"
        );
        assert!(
            p.contains("\n    mine:\n        endpoints: []\nlandlock: {}\n"),
            "{p}"
        );
        // An empty block, flow or not, takes the entries at the usual indent.
        let p = creation_policy("network_policies: {}\nx: 1\n", &groups).unwrap();
        assert!(
            p.starts_with("network_policies:\n  osh_tc_x_example:\n"),
            "{p}"
        );
        assert!(p.ends_with("x: 1\n"), "{p}");
        // A one-line block cannot be added to: an error the caller shows.
        let e = creation_policy("network_policies: { mine: {} }\n", &groups).unwrap_err();
        assert!(e.contains("block"), "{e}");
        let e = creation_policy(
            "network_policies:\n  osh_tc_x_example:\n    x: 1\n",
            &groups,
        )
        .unwrap_err();
        assert!(e.contains("osh_tc_x_example"), "{e}");
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
        assert_eq!(group_of(&label(&groups[0], false)), "a");
        assert_eq!(label(&groups[2], false), "c  (1 host)");
    }

    #[test]
    fn labels_tag_the_defaults_and_what_is_open_and_say_when_it_closes_or_stays_open() {
        let groups = g(SAMPLE);
        assert_eq!(label(&groups[0], false), "a  (2 hosts)  (default)");
        assert_eq!(label(&groups[1], true), "b  (2 hosts)  (open here)");
        let open = label(&groups[0], true);
        assert_eq!(open, "a  (2 hosts)  (default)  (open here)");
        assert_eq!(group_of(&open), "a");
        let closes = retag_open(&open, false, false);
        assert_eq!(closes, "a  (2 hosts)  (default)  (closes)");
        assert_eq!(retag_open(&closes, true, false), open);
        let stays = retag_open(&open, false, true);
        assert_eq!(
            stays,
            "a  (2 hosts)  (default)  (stays open: older osh rule)"
        );
        assert_eq!(retag_open(&stays, true, true), open);
        assert_eq!(group_of(&stays), "a");
        assert_eq!(
            retag_open(&label(&groups[1], false), false, false),
            "b  (2 hosts)"
        );
        assert_eq!(defaults_line(&groups), "a");
        assert_eq!(defaults_line(&select(&groups, &[])), "none");
    }

    #[test]
    fn remark_keeps_the_users_marks_and_follows_the_file_for_new_groups() {
        let groups = g(SAMPLE);
        let prev = Marks {
            picked: vec!["b".into()],
            known: vec!["a".into(), "b".into(), "gone".into()],
            save: false,
        };
        // a was unmarked by the user, b marked; c is new and the file has it off.
        assert_eq!(remark(&groups, &prev), vec![false, true, false]);
        let groups = g("[c]\nenabled = true\nhosts = [\"z.example\"]\n");
        assert_eq!(
            remark(&groups, &prev),
            vec![true],
            "new and enabled: marked"
        );
    }

    #[test]
    fn the_record_reads_writes_adds_and_prunes() {
        let dir = std::env::temp_dir().join(format!("osh-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_records(&dir).is_empty(), "no file: nothing known");
        record_open(&dir, "gw", "sb", &["b".into(), "a".into()], true).unwrap();
        record_open(&dir, "gw", "sb", &["c".into(), "a".into()], false).unwrap();
        record_open(&dir, "gw", "old", &["a".into()], true).unwrap();
        record_open(&dir, "gw2", "old", &["a".into()], true).unwrap();
        let mut r = load_records(&dir);
        assert_eq!(r["gw/sb"], ["a", "b", "c"], "an apply only adds");
        record_open(&dir, "gw", "sb", &["c".into()], true).unwrap();
        assert_eq!(load_records(&dir)["gw/sb"], ["c"], "creation replaces");
        assert!(prune(&mut r, "gw", &["sb".to_string()]));
        assert_eq!(
            r.keys().collect::<Vec<_>>(),
            ["gw/sb", "gw2/old"],
            "other gateways stay"
        );
        assert!(!prune(&mut r, "gw", &["sb".to_string()]));
        std::fs::write(dir.join(RECORD_FILE), "not json").unwrap();
        assert!(load_records(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_group_with_a_failed_host_is_not_recorded_as_open() {
        let groups = select(
            &g("[a]\nhosts = [\"a.example\"]\n[b]\nhosts = [\"b.example\", \"c.example\"]\n[off]\nhosts = [\"o.example\"]\n"),
            &["a".into(), "b".into()],
        );
        let covered = "Error: would grant binary '/**' undeclared authorization".to_string();
        let results = vec![
            ("a.example".to_string(), Err(covered)),
            ("b.example".to_string(), Ok(())),
            ("c.example".to_string(), Err("Error: boom".to_string())),
        ];
        assert_eq!(opened_groups(&groups, &results), vec!["a".to_string()]);
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

    /// The groups of the close tests: `a` and `b` share `s.example`, `b` has a git host.
    const CLOSE: &str = "[a]\nhosts = [\"a.example\", \"s.example\"]\n[b]\nhosts = [\"s.example\"]\ngit_hosts = [\"g.example\"]\n[c]\nhosts = [\"c.example\"]\n";

    fn rules(pairs: &[(&str, &[&str])]) -> Rules {
        pairs
            .iter()
            .map(|(n, hs)| (n.to_string(), hs.iter().map(|h| h.to_string()).collect()))
            .collect()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_creation_policy_has_one_rule_per_host_and_grants_the_same_as_before() {
        use saphyr::LoadableYamlNode;
        let groups = select(&g(CLOSE), &names(&["a", "b"]));
        let p = creation_policy("version: 1\n", &groups).unwrap();
        let docs = saphyr::Yaml::load_from_str(&p).expect("valid YAML");
        let net = docs[0].as_mapping_get("network_policies").unwrap();
        let mut seen: Vec<(String, String, bool)> = Vec::new();
        for (k, rule) in net.as_mapping().unwrap() {
            let name = k.as_str().unwrap().to_string();
            let eps = rule.as_mapping_get("endpoints").unwrap().as_vec().unwrap();
            assert_eq!(eps.len(), 1, "one host per rule: {name}");
            let host = eps[0].as_mapping_get("host").unwrap().as_str().unwrap();
            assert_eq!(name, rule_name(host));
            assert!(is_osh_rule(&name));
            let bins = rule.as_mapping_get("binaries").unwrap().as_vec().unwrap();
            assert_eq!(
                bins[0].as_mapping_get("path").unwrap().as_str(),
                Some("/**"),
                "any program, as the combined rule had"
            );
            let git = eps[0].as_mapping_get("rules").is_some();
            if !git {
                assert_eq!(
                    eps[0].as_mapping_get("access").unwrap().as_str(),
                    Some("read-only")
                );
            }
            seen.push((name, host.to_string(), git));
        }
        let (plain, git) = hosts(&groups);
        let mut want: Vec<(String, String, bool)> = plain
            .iter()
            .map(|h| (rule_name(h), h.clone(), false))
            .chain(git.iter().map(|h| (rule_name(h), h.clone(), true)))
            .collect();
        want.sort();
        seen.sort();
        assert_eq!(seen, want, "the same hosts, git ones with the fetch POST");
        assert!(p.contains("POST, path: \"/**/git-upload-pack\""));
        assert_eq!(rule_name("a-b.example"), "osh_tc_a-b_example");
        assert_ne!(rule_name("a-b.example"), rule_name("a.b-example"));
    }

    #[test]
    fn the_sandbox_check_policy_is_what_osh_writes_for_its_two_groups() {
        let base = crate::assets::embedded("sandbox-policy.yaml").unwrap();
        let groups = select(
            &g("[one]\nhosts = [\"example.com\", \"www.example.org\"]\n[two]\nhosts = [\"www.example.org\"]\ngit_hosts = [\"codeberg.org\"]\n"),
            &names(&["one", "two"]),
        );
        let fixture = include_str!("../../tests/sandbox/toolchains-close.yaml");
        assert_eq!(
            creation_policy(base, &groups).unwrap(),
            format!("{}\n\n{fixture}", base.trim_end()),
            "tests/sandbox/checks.sh creates its sandbox from this block"
        );
    }

    #[test]
    fn closing_a_group_removes_only_its_own_rules_and_keeps_shared_hosts() {
        let all = g(CLOSE);
        let r = rules(&[
            ("osh_tc_a_example", &["a.example"]),
            ("osh_tc_s_example", &["s.example"]),
            ("osh_tc_g_example", &["g.example"]),
            ("_provider_npm", &["a.example"]),
            ("mine", &["*.example"]),
        ]);
        let plan = close_plan(&all, &names(&["b"]), &names(&["a"]), &r);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].host, "a.example");
        assert_eq!(plan[0].how, How::Remove(names(&["osh_tc_a_example"])));
        assert_eq!(
            plan[0].also,
            names(&["_provider_npm", "mine"]),
            "a provider's rule and a wildcard of the user's still allow it"
        );
        assert_eq!(plan[1].how, How::Shared(names(&["b"])), "b still needs it");
        let cmds = close_commands("gw", "sb", &plan);
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0].1.join(" "),
            "-g gw policy update sb --remove-rule osh_tc_a_example --wait --timeout 60"
        );
        // Both unmarked: the shared host goes too, once; a host no rule of osh's holds sends
        // nothing.
        let r2 = rules(&[("osh_tc_s_example", &["s.example"])]);
        let plan = close_plan(&all, &[], &names(&["a", "b"]), &r2);
        let hows: Vec<&How> = plan.iter().map(|h| &h.how).collect();
        assert_eq!(
            hows,
            [
                &How::NotOpen,
                &How::Remove(names(&["osh_tc_s_example"])),
                &How::NotOpen
            ]
        );
        assert_eq!(close_commands("", "sb", &plan).len(), 1);
    }

    #[test]
    fn a_rule_that_is_not_oshs_is_never_removed() {
        for bad in [
            "_provider_osh_tc_x",
            "allow_x_example_443",
            "mine",
            "osh_tc",
            "osh_toolchains_other",
        ] {
            assert!(!is_osh_rule(bad), "{bad}");
        }
        for ok in ["osh_tc_x_example", "osh_toolchains", "osh_toolchains_git"] {
            assert!(is_osh_rule(ok), "{ok}");
        }
        // Even a plan that names a foreign rule sends nothing for it.
        let plan = vec![HostClose {
            host: "x.example".into(),
            how: How::Remove(names(&["_provider_x", "mine", "osh_tc_x_example"])),
            also: vec![],
        }];
        let cmds = close_commands("", "sb", &plan);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].0, "osh_tc_x_example");
    }

    #[test]
    fn the_combined_rule_of_an_older_osh_goes_only_when_all_its_hosts_close() {
        let all = g(CLOSE);
        let old = rules(&[
            ("osh_toolchains", &["a.example", "s.example", "c.example"]),
            ("osh_toolchains_git", &["g.example"]),
        ]);
        // c stays marked: removing osh_toolchains would close c.example.
        let plan = close_plan(&all, &names(&["c"]), &names(&["a", "b"]), &old);
        assert_eq!(plan[0].how, How::Legacy("osh_toolchains".into()));
        assert_eq!(plan[1].how, How::Legacy("osh_toolchains".into()));
        assert_eq!(
            plan[2].how,
            How::Remove(names(&["osh_toolchains_git"])),
            "the git rule holds only hosts that close"
        );
        assert_eq!(close_commands("", "sb", &plan).len(), 1);
        let removed = vec![("osh_toolchains_git".to_string(), Ok(()))];
        let done = close_results(&plan, &removed);
        assert_eq!(done, vec![("g.example".to_string(), Ok(()))]);
        let (closed, stuck) = closed_groups(&all, &names(&["a", "b"]), &plan, &done);
        assert!(closed.is_empty(), "both groups have a host held open");
        assert_eq!(stuck, names(&["a", "b"]));
        let t = close_report("sb", &plan, &done);
        assert!(
            t.starts_with("\u{2713} 1 hosts are closed in sb.\n  g.example"),
            "{t}"
        );
        assert!(t.contains("an older osh created this sandbox"), "{t}");
        assert!(t.contains("(osh_toolchains)"), "{t}");
        assert!(t.contains("a.example, s.example"), "{t}");
        // Everything unmarked: the combined rule goes.
        let plan = close_plan(&all, &[], &names(&["a", "b", "c"]), &old);
        let cmds: Vec<String> = close_commands("", "sb", &plan)
            .into_iter()
            .map(|c| c.0)
            .collect();
        assert_eq!(cmds, names(&["osh_toolchains", "osh_toolchains_git"]));
    }

    #[test]
    fn the_close_report_and_the_record_follow_what_actually_closed() {
        let all = g(CLOSE);
        let r = rules(&[
            ("osh_tc_a_example", &["a.example"]),
            ("osh_tc_c_example", &["c.example"]),
            ("osh_tc_s_example", &["s.example"]),
            ("allow_c_example_443", &["c.example"]),
        ]);
        let plan = close_plan(&all, &names(&["b"]), &names(&["a", "c"]), &r);
        let removed = vec![
            ("osh_tc_a_example".to_string(), Ok(())),
            (
                "osh_tc_c_example".to_string(),
                Err("Error: \u{d7} boom".to_string()),
            ),
        ];
        let done = close_results(&plan, &removed);
        let (closed, stuck) = closed_groups(&all, &names(&["a", "c"]), &plan, &done);
        assert_eq!(closed, names(&["a"]), "c failed, so it is still open");
        assert!(stuck.is_empty());
        let t = close_report("sb", &plan, &done);
        assert!(
            t.starts_with("\u{2713} 1 hosts are closed in sb.\n  a.example"),
            "{t}"
        );
        assert!(
            t.contains(
                "1 stay open because a group that is still marked needs them:\n  s.example (b)"
            ),
            "{t}"
        );
        assert!(t.contains("c.example (allow_c_example_443)"), "{t}");
        assert!(
            t.contains("\u{2717} 1 failed to close:\n  c.example  boom"),
            "{t}"
        );

        let dir = std::env::temp_dir().join(format!("osh-rec-close-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        record_open(&dir, "gw", "sb", &names(&["a", "b", "c"]), true).unwrap();
        record_apply(
            &dir,
            "gw",
            "sb",
            &[],
            &closed,
            &names(&["c"]),
            &names(&["a", "c"]),
        )
        .unwrap();
        let rec = load_records(&dir);
        assert_eq!(rec["gw/sb"], names(&["b", "c"]));
        assert_eq!(stuck_groups(&rec, "gw", "sb"), names(&["c"]));
        assert!(
            !prune(&mut rec.clone(), "gw", &names(&["sb"])),
            "the stuck list is kept with its sandbox"
        );
        let mut gone = rec.clone();
        assert!(prune(&mut gone, "gw", &[]));
        assert!(gone.is_empty(), "and pruned with it: {gone:?}");
        // Trying c again and closing it: it leaves the record and the stuck list.
        record_apply(&dir, "gw", "sb", &[], &names(&["c"]), &[], &names(&["c"])).unwrap();
        let rec = load_records(&dir);
        assert_eq!(rec["gw/sb"], names(&["b"]));
        assert!(!rec.contains_key("gw/sb#stays"));
        // A new sandbox under the same name starts clean.
        record_apply(&dir, "gw", "sb", &[], &[], &names(&["b"]), &[]).unwrap();
        assert_eq!(stuck_groups(&load_records(&dir), "gw", "sb"), names(&["b"]));
        record_open(&dir, "gw", "sb", &names(&["b"]), true).unwrap();
        assert!(stuck_groups(&load_records(&dir), "gw", "sb").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_rules_are_read_from_policy_get_json() {
        let json = r#"{"policy": {"network_policies": {
            "_provider_x": {"endpoints": [{"host": "A.example", "port": 443}, {"host": "b.example", "port": 80}]},
            "osh_tc_c_example": {"endpoints": [{"host": "c.example", "ports": [80, 443]}]},
            "empty": {}
        }}}"#;
        let r = parse_rules(json).unwrap();
        assert_eq!(
            r["_provider_x"],
            names(&["a.example"]),
            "port 443 only, lowercased"
        );
        assert_eq!(r["osh_tc_c_example"], names(&["c.example"]));
        assert!(r["empty"].is_empty());
        assert!(parse_rules(r#"{"policy": {}}"#).unwrap().is_empty());
        assert!(parse_rules("Error: no sandbox").is_err());
        assert_eq!(
            policy_get_args("gw", "sb").join(" "),
            "-g gw policy get sb --full -o json"
        );
    }
}
