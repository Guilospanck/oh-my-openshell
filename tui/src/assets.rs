//! Static data compiled into the binary and written to the data directory, so a lone `osh`
//! needs no checkout. The layout matches the repo root, which is what `OSH_DIR` points at in dev.
use serde_json::{Map, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

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
    let mut manifest: Map<String, Value> = fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut report = Report::default();
    for (rel, body) in FILES {
        let path = dir.join(rel);
        let new = hash(body.as_bytes());
        let on_disk = fs::read(&path).ok().map(|b| hash(&b));
        let recorded = manifest.get(*rel).and_then(Value::as_str).map(String::from);
        match on_disk {
            // Recorded as written once and gone now: the user removed it on purpose.
            None if recorded.is_some() => continue,
            Some(d) if d == new => {
                manifest.insert((*rel).into(), new.into());
                continue;
            }
            Some(d) if recorded.as_deref() != Some(d.as_str()) => {
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
