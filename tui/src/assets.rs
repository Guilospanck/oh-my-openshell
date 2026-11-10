//! Static data compiled into the binary and written to the data directory, so a lone `osh`
//! needs no checkout. The layout matches the repo root, which is what `OSH_DIR` points at in dev.
use saphyr::LoadableYamlNode;
use serde_json::{Map, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Profiles the user attaches on purpose (language toolchains, web browsing). `K` (agent installs)
/// never picks them, and a blocked-request prompt offers them only for hosts they are made for.
pub const OPT_IN_PROFILES: &[&str] = &["go", "js", "rust"];

/// (path relative to the data directory, contents).
const FILES: &[(&str, &str)] = &[
    (
        "providers/claude-code.yaml",
        include_str!("../../providers/claude-code.yaml"),
    ),
    (
        "providers/claude-marketplace.yaml",
        include_str!("../../providers/claude-marketplace.yaml"),
    ),
    (
        "providers/forgejo.yaml",
        include_str!("../../providers/forgejo.yaml"),
    ),
    (
        "providers/github.yaml",
        include_str!("../../providers/github.yaml"),
    ),
    ("providers/go.yaml", include_str!("../../providers/go.yaml")),
    ("providers/js.yaml", include_str!("../../providers/js.yaml")),
    (
        "providers/rust.yaml",
        include_str!("../../providers/rust.yaml"),
    ),
    (
        "providers/npm.yaml",
        include_str!("../../providers/npm.yaml"),
    ),
    (
        "providers/pypi.yaml",
        include_str!("../../providers/pypi.yaml"),
    ),
    (
        "images/Dockerfile-claude_code",
        include_str!("../../images/Dockerfile-claude_code"),
    ),
    (
        "images/Dockerfile-pi",
        include_str!("../../images/Dockerfile-pi"),
    ),
    ("gateway.toml", include_str!("../../gateway.toml")),
    (
        "gateway-linux.toml",
        include_str!("../../gateway-linux.toml"),
    ),
    (
        "gateway-podman.toml",
        include_str!("../../gateway-podman.toml"),
    ),
    ("gateway.env", include_str!("../../gateway.env")),
    (
        "sandbox-policy.yaml",
        include_str!("../../sandbox-policy.yaml"),
    ),
    ("toolchains.toml", include_str!("../../toolchains.toml")),
    (".gitleaks.toml", include_str!("../../.gitleaks.toml")),
    (
        "gateway-docker.toml",
        include_str!("../../gateway-docker.toml"),
    ),
];

const MANIFEST: &str = ".osh-manifest.json";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub written: Vec<PathBuf>,
    /// Files the user changed since osh wrote them; left as they are.
    pub kept_user_edits: Vec<PathBuf>,
    /// The part of `kept_user_edits` whose shipped default changed since the user's copy was
    /// made: an upgrade brought a fix the user's copy does not have.
    pub newer_defaults: Vec<PathBuf>,
}

impl Report {
    /// What the dashboard tells the user about kept edits, or `None` when there is nothing to say.
    /// The data directory itself is never the subject: the Data files screen is (palette).
    pub fn notice(&self, dir: &Path) -> Option<String> {
        if self.kept_user_edits.is_empty() {
            return None;
        }
        let rel = |p: &PathBuf| {
            p.strip_prefix(dir)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        };
        let mut text = format!(
            "osh kept your edited copy of: {}.",
            self.kept_user_edits
                .iter()
                .map(rel)
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !self.newer_defaults.is_empty() {
            text.push_str(&format!(
                "\nA newer shipped default exists for: {}. Your copy does not get its fixes.",
                self.newer_defaults
                    .iter()
                    .map(rel)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        text.push_str(
            "\nOpen \"Data files\" from the command palette (: or Ctrl-P) to compare a file with the shipped default, edit it or restore it.",
        );
        Some(text)
    }
}

/// Where a data file stands against the copy compiled into this osh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    /// Same as the shipped default.
    Shipped,
    /// Changed by the user; `newer`: the shipped default changed since, too.
    Edited { newer: bool },
    /// osh wrote an older default and nobody changed it: the next start updates it.
    Outdated,
    /// A shipped file that is not on disk (deleted).
    Missing,
    /// A file the user added (a profile or Dockerfile osh does not ship).
    Own,
}

impl FileState {
    pub fn label(self) -> &'static str {
        match self {
            FileState::Shipped => "shipped default",
            FileState::Edited { newer: false } => "edited by you",
            FileState::Edited { newer: true } => "edited by you; newer default available",
            FileState::Outdated => "older default; updated at next start",
            FileState::Missing => "deleted; shipped default available",
            FileState::Own => "your own file",
        }
    }

    /// The file differs from (or lacks) the shipped default, so restoring it changes something.
    pub fn restorable(self) -> bool {
        matches!(
            self,
            FileState::Edited { .. } | FileState::Outdated | FileState::Missing
        )
    }
}

/// Files osh needs to work, as opposed to profiles and Dockerfiles a user may drop on purpose.
/// A required file that is gone is written again, so a deleted `sandbox-policy.yaml` cannot
/// break every sandbox creation.
pub fn required(rel: &str) -> bool {
    !(rel.starts_with("providers/") || rel.starts_with("images/"))
}

fn load_manifest(dir: &Path) -> Map<String, Value> {
    fs::read_to_string(dir.join(MANIFEST))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Where `rel` stands against the shipped copy.
pub fn state(dir: &Path, rel: &str) -> FileState {
    let on_disk = fs::read(dir.join(rel)).ok().map(|b| hash(&b));
    let Some(body) = embedded(rel) else {
        return if on_disk.is_some() {
            FileState::Own
        } else {
            FileState::Missing
        };
    };
    let new = hash(body.as_bytes());
    let recorded = load_manifest(dir)
        .get(rel)
        .and_then(Value::as_str)
        .map(String::from);
    match on_disk {
        None => FileState::Missing,
        Some(d) if d == new => FileState::Shipped,
        Some(d) if recorded.as_deref() == Some(d.as_str()) => FileState::Outdated,
        Some(_) => FileState::Edited {
            newer: recorded.is_some_and(|r| r != new),
        },
    }
}

/// Every data file: the shipped ones, then profiles (`providers/*.yaml|yml`) and Dockerfiles
/// (`images/Dockerfile-*`) the user added, each with its state.
pub fn files(dir: &Path) -> Vec<(String, FileState)> {
    let mut out: Vec<(String, FileState)> = FILES
        .iter()
        .map(|(rel, _)| (rel.to_string(), state(dir, rel)))
        .collect();
    let mut own = Vec::new();
    for (sub, keep) in [
        (
            "providers",
            (|n: &str| n.ends_with(".yaml") || n.ends_with(".yml")) as fn(&str) -> bool,
        ),
        ("images", |n: &str| n.starts_with("Dockerfile-")),
    ] {
        if let Ok(rd) = fs::read_dir(dir.join(sub)) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let rel = format!("{sub}/{name}");
                if keep(&name) && e.path().is_file() && embedded(&rel).is_none() {
                    own.push((rel, FileState::Own));
                }
            }
        }
    }
    own.sort_by(|a, b| a.0.cmp(&b.0));
    out.extend(own);
    out
}

/// Write the shipped default of `rel` back and record it, so later upgrades update it again.
pub fn restore(dir: &Path, rel: &str) -> io::Result<()> {
    let body = embedded(rel)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "osh ships no such file"))?;
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, body)?;
    let mut manifest = load_manifest(dir);
    manifest.insert(rel.into(), hash(body.as_bytes()).into());
    fs::write(dir.join(MANIFEST), serde_json::to_string_pretty(&manifest)?)
}

/// Delete a data file. A shipped profile or Dockerfile stays deleted across upgrades (the
/// manifest still names it); a required file comes back at the next start.
pub fn remove(dir: &Path, rel: &str) -> io::Result<()> {
    fs::remove_file(dir.join(rel))
}

/// The text parses as YAML (duplicate keys included, which OpenShell would reject or drop).
pub fn check_yaml(text: &str) -> Result<(), String> {
    saphyr::Yaml::load_from_str(text)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `sandbox-policy.yaml`: YAML, a mapping, with the `version` OpenShell requires.
pub fn check_policy(text: &str) -> Result<(), String> {
    let docs = saphyr::Yaml::load_from_str(text).map_err(|e| e.to_string())?;
    let doc = docs.first().ok_or("the policy is empty")?;
    if doc.as_mapping().is_none() {
        return Err("the policy must be a mapping (version:, filesystem_policy:, ...)".into());
    }
    if doc.as_mapping_get("version").is_none() {
        return Err("the policy needs a version: line (version: 1)".into());
    }
    if let Some(n) = doc.as_mapping_get("network_policies") {
        if !n.is_null() && n.as_mapping().is_none() {
            return Err("network_policies must map policy names to policies".into());
        }
    }
    Ok(())
}

/// A Dockerfile has a `FROM` line before anything is built.
pub fn check_dockerfile(text: &str) -> Result<(), String> {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'));
    match first {
        Some(l)
            if l.to_ascii_uppercase().starts_with("FROM ")
                || l.to_ascii_uppercase().starts_with("ARG ") =>
        {
            Ok(())
        }
        _ => Err("a Dockerfile starts with a FROM line (ARG lines may come before it)".into()),
    }
}

fn check_toml(text: &str) -> Result<(), String> {
    text.parse::<toml::Table>()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The check the editor runs before saving `rel`, so a broken file is never written.
pub fn check_for(rel: &str) -> Option<crate::editor::Check> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    Some(match rel {
        "toolchains.toml" => crate::preload::check,
        "sandbox-policy.yaml" => check_policy,
        _ if rel.starts_with("providers/") => crate::cli::check_profile,
        _ if rel.starts_with("images/") => check_dockerfile,
        _ if name.ends_with(".toml") => check_toml,
        _ if name.ends_with(".yaml") || name.ends_with(".yml") => check_yaml,
        _ => return None,
    })
}

/// A line diff of `old` (the shipped default) against `new` (the user's copy): `-` lines are only
/// in the default, `+` lines only in the user's copy, with two lines of context around each change.
pub fn diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // Longest common subsequence table, from the end.
    let mut t = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            t[i][j] = if a[i] == b[j] {
                t[i + 1][j + 1] + 1
            } else {
                t[i + 1][j].max(t[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || t[i][j + 1] >= t[i + 1][j]) {
            ops.push(('+', b[j]));
            j += 1;
        } else {
            ops.push(('-', a[i]));
            i += 1;
        }
    }
    if ops.iter().all(|(k, _)| *k == ' ') {
        return "No differences: the file is the shipped default.".into();
    }
    const CONTEXT: usize = 2;
    let near = |k: usize| {
        ops[k.saturating_sub(CONTEXT)..(k + CONTEXT + 1).min(ops.len())]
            .iter()
            .any(|(c, _)| *c != ' ')
    };
    let mut out = String::from("- only in the shipped default    + only in your copy\n\n");
    let mut skipped = false;
    for (k, (c, l)) in ops.iter().enumerate() {
        if near(k) {
            if skipped {
                out.push_str("  ...\n");
                skipped = false;
            }
            out.push_str(&format!("{c} {l}\n"));
        } else {
            skipped = true;
        }
    }
    if skipped {
        out.push_str("  ...\n");
    }
    out
}

/// The embedded copy of a data file, by its path relative to the data directory.
pub fn embedded(rel: &str) -> Option<&'static str> {
    FILES.iter().find(|(r, _)| *r == rel).map(|(_, b)| *b)
}

/// `OSH_DIR` (a checkout, for development), else `$XDG_DATA_HOME/osh`, else `~/.local/share/osh`.
pub fn data_dir() -> PathBuf {
    resolve(
        std::env::var_os("OSH_DIR"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
}

fn resolve(
    osh_dir: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(d) = osh_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let base = crate::platform::absolute(xdg)
        .unwrap_or_else(|| PathBuf::from(home.unwrap_or_default()).join(".local/share"));
    base.join("osh")
}

const SEEN: &str = ".osh-seen";

/// Identifies this binary: its resolved path and modification time. A reinstall or upgrade
/// changes the time, so the id differs from the one the previous install recorded.
pub fn exe_id() -> String {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_default();
    let secs = fs::metadata(&exe)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    format!("{}\n{secs}", exe.display())
}

/// True once per binary when `dir` already holds data osh wrote for an earlier install, e.g.
/// after a bare `brew uninstall`, which leaves the data folder behind. Call before `materialize`.
/// The id is recorded either way, so later runs of the same binary stay quiet.
pub fn leftover_from_earlier_install(dir: &Path, id: &str) -> bool {
    let had_data = dir.join(MANIFEST).is_file();
    let seen = fs::read_to_string(dir.join(SEEN)).ok();
    if seen.as_deref() == Some(id) {
        return false;
    }
    let _ = fs::create_dir_all(dir);
    let _ = fs::write(dir.join(SEEN), id);
    // No marker but data present: written by an osh that predates the marker, still leftover.
    had_data
}

/// FNV-1a 64-bit; stable across Rust versions, unlike `DefaultHasher`.
pub fn hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Writes each embedded file unless the user edited it. A file counts as edited when it exists
/// and its hash differs from the one recorded when osh last wrote it.
pub fn materialize(dir: &Path) -> io::Result<Report> {
    fs::create_dir_all(dir)?;
    let manifest_path = dir.join(MANIFEST);
    let mut manifest = load_manifest(dir);
    let mut report = Report::default();
    for (rel, body) in FILES {
        let path = dir.join(rel);
        let new = hash(body.as_bytes());
        let on_disk = fs::read(&path).ok().map(|b| hash(&b));
        let recorded = manifest.get(*rel).and_then(Value::as_str).map(String::from);
        match on_disk {
            // Recorded as written once and gone now: a profile or Dockerfile the user removed on
            // purpose. A file osh cannot work without comes back.
            None if recorded.is_some() && !required(rel) => continue,
            Some(d) if d == new => {
                manifest.insert((*rel).into(), new.into());
                continue;
            }
            Some(d) if recorded.as_deref() != Some(d.as_str()) => {
                if recorded.as_deref().is_some_and(|r| r != new) {
                    report.newer_defaults.push(path.clone());
                }
                report.kept_user_edits.push(path);
                continue;
            }
            _ => {}
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, body)?;
        manifest.insert((*rel).into(), new.into());
        report.written.push(path);
    }
    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_yaml_rejects_duplicate_keys_and_survives_deep_nesting() {
        assert!(check_yaml("a: 1\nb: 2\n").is_ok());
        assert!(check_yaml("a: 1\na: 2\n").is_err(), "duplicate keys");
        assert!(check_yaml("a: [\n").is_err(), "broken YAML");
        // A pathologically nested file must give an error or pass, never overflow the stack.
        let deep = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
        let _ = check_yaml(&deep);
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-assets-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// The default OpenShell policy denies opening a PTY, which broke tmux in every sandbox once.
    /// `just test-sandbox` proves the real thing; this keeps the policy from losing the grant.
    #[test]
    fn sandbox_policy_lets_a_terminal_open_and_is_written() {
        let p = embedded("sandbox-policy.yaml").expect("policy is embedded");
        let writable: Vec<&str> = p
            .lines()
            .skip_while(|l| !l.trim_start().starts_with("read_write:"))
            .skip(1)
            .take_while(|l| l.trim_start().starts_with("- "))
            .map(|l| l.trim_start().trim_start_matches("- ").trim())
            .collect();
        let covers = |path: &str| {
            writable
                .iter()
                .any(|w| *w == "/" || path == *w || path.starts_with(&format!("{w}/")))
        };
        assert!(covers("/dev/ptmx"), "tmux opens /dev/ptmx: {writable:?}");
        assert!(
            covers("/dev/pts/0"),
            "and the slave in /dev/pts: {writable:?}"
        );
        assert!(covers("/tmp"), "{writable:?}");
        let d = tmp("policy");
        materialize(&d).unwrap();
        assert!(d.join("sandbox-policy.yaml").is_file());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn leftover_is_reported_once_per_binary() {
        let d = tmp("leftover");
        assert!(!leftover_from_earlier_install(&d, "a\n1"), "fresh dir");
        materialize(&d).unwrap();
        assert!(!leftover_from_earlier_install(&d, "a\n1"), "same binary");
        assert!(
            leftover_from_earlier_install(&d, "a\n2"),
            "reinstalled binary"
        );
        assert!(!leftover_from_earlier_install(&d, "a\n2"), "only once");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn materialize_writes_all_files_into_empty_dir() {
        let d = tmp("empty");
        let r = materialize(&d).unwrap();
        assert_eq!(r.written.len(), FILES.len());
        assert!(d.join("providers/npm.yaml").is_file());
        assert!(d.join("images/Dockerfile-pi").is_file());
        assert!(d.join("gateway-linux.toml").is_file());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn materialize_twice_writes_nothing_the_second_time() {
        let d = tmp("twice");
        materialize(&d).unwrap();
        let r = materialize(&d).unwrap();
        assert_eq!(r, Report::default());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn materialize_overwrites_file_when_disk_hash_equals_manifest() {
        let d = tmp("upgrade");
        materialize(&d).unwrap();
        // Simulate an older osh: the file differs from the embedded one, but the manifest
        // recorded that older content, so the user has not touched it.
        let p = d.join("providers/npm.yaml");
        fs::write(&p, "old content").unwrap();
        let mut m: Map<String, Value> =
            serde_json::from_str(&fs::read_to_string(d.join(MANIFEST)).unwrap()).unwrap();
        m.insert("providers/npm.yaml".into(), hash(b"old content").into());
        fs::write(d.join(MANIFEST), serde_json::to_string(&m).unwrap()).unwrap();
        let r = materialize(&d).unwrap();
        assert_eq!(r.written, vec![p.clone()]);
        assert_ne!(fs::read_to_string(&p).unwrap(), "old content");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn materialize_keeps_user_edited_file_and_reports_it() {
        let d = tmp("edited");
        materialize(&d).unwrap();
        let p = d.join("providers/npm.yaml");
        fs::write(&p, "my edit").unwrap();
        let r = materialize(&d).unwrap();
        assert_eq!(r.kept_user_edits, vec![p.clone()]);
        assert_eq!(fs::read_to_string(&p).unwrap(), "my edit");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn materialize_does_not_bring_back_a_file_the_user_deleted() {
        let d = tmp("deleted");
        materialize(&d).unwrap();
        let p = d.join("providers/npm.yaml");
        fs::remove_file(&p).unwrap();
        let r = materialize(&d).unwrap();
        assert!(!p.exists());
        assert!(r.written.is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_deleted_required_file_comes_back_and_a_deleted_profile_does_not() {
        let d = tmp("required");
        materialize(&d).unwrap();
        fs::remove_file(d.join("sandbox-policy.yaml")).unwrap();
        fs::remove_file(d.join("providers/npm.yaml")).unwrap();
        let r = materialize(&d).unwrap();
        assert_eq!(r.written, vec![d.join("sandbox-policy.yaml")]);
        assert!(!d.join("providers/npm.yaml").exists());
        assert_eq!(state(&d, "providers/npm.yaml"), FileState::Missing);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn states_follow_edits_restores_and_newer_defaults() {
        let d = tmp("states");
        materialize(&d).unwrap();
        assert_eq!(state(&d, "providers/npm.yaml"), FileState::Shipped);
        fs::write(d.join("providers/npm.yaml"), "mine").unwrap();
        assert_eq!(
            state(&d, "providers/npm.yaml"),
            FileState::Edited { newer: false }
        );
        // The copy osh wrote last time was an older default: the user's edit misses an upgrade.
        let mut m = load_manifest(&d);
        m.insert("providers/npm.yaml".into(), hash(b"older").into());
        fs::write(d.join(MANIFEST), serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(
            state(&d, "providers/npm.yaml"),
            FileState::Edited { newer: true }
        );
        let r = materialize(&d).unwrap();
        assert_eq!(r.newer_defaults, vec![d.join("providers/npm.yaml")]);
        let n = r.notice(&d).unwrap();
        assert!(n.contains("providers/npm.yaml") && n.contains("newer shipped default"));
        assert!(n.contains("Data files"), "{n}");
        restore(&d, "providers/npm.yaml").unwrap();
        assert_eq!(state(&d, "providers/npm.yaml"), FileState::Shipped);
        assert_eq!(materialize(&d).unwrap(), Report::default());
        fs::write(d.join("providers/mine.yml"), "id: mine").unwrap();
        let all = files(&d);
        assert!(all.contains(&("providers/mine.yml".into(), FileState::Own)));
        assert!(all.iter().any(|(r, _)| r == "sandbox-policy.yaml"));
        remove(&d, "providers/mine.yml").unwrap();
        assert!(!files(&d).iter().any(|(r, _)| r == "providers/mine.yml"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_diff_marks_both_sides_and_says_when_there_is_none() {
        let d = diff("a\nb\nc\n", "a\nB\nc\nd\n");
        assert!(d.contains("- b\n+ B\n") || d.contains("+ B\n- b\n"), "{d}");
        assert!(d.contains("+ d\n"), "{d}");
        assert!(d.contains("  a\n"), "context is kept: {d}");
        assert!(diff("x\n", "x\n").starts_with("No differences"));
        let long_a: String = (0..20).map(|i| format!("{i}\n")).collect();
        let long_b = long_a.replace("10\n", "ten\n");
        let d = diff(&long_a, &long_b);
        assert!(d.contains("  ...\n") && !d.contains("  0\n"), "{d}");
    }

    #[test]
    fn data_dir_ignores_a_relative_xdg_data_home() {
        let s = |v: &str| Some(std::ffi::OsString::from(v));
        assert_eq!(
            resolve(None, s("rel"), s("/h")),
            PathBuf::from("/h/.local/share/osh")
        );
    }

    #[test]
    fn materialize_leaves_unknown_user_files_alone() {
        let d = tmp("unknown");
        materialize(&d).unwrap();
        let mine = d.join("images/Dockerfile-mine");
        fs::write(&mine, "FROM scratch").unwrap();
        materialize(&d).unwrap();
        assert_eq!(fs::read_to_string(&mine).unwrap(), "FROM scratch");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn data_dir_prefers_osh_dir_then_xdg_then_home() {
        let s = |v: &str| Some(std::ffi::OsString::from(v));
        assert_eq!(
            resolve(s("/repo"), s("/x"), s("/h")),
            PathBuf::from("/repo")
        );
        assert_eq!(resolve(None, s("/x"), s("/h")), PathBuf::from("/x/osh"));
        assert_eq!(
            resolve(None, None, s("/h")),
            PathBuf::from("/h/.local/share/osh")
        );
    }
}
