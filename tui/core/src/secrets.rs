//! Tokens the operator stores for the MCP server (`osh mcp secret set NAME`), one file each in
//! `<data>/mcp-secrets/`, readable by the owner only. A tool call can only name one; the server
//! reads the value itself, so it never travels through a chat, a command line or shell history.
//! Storing a name is what allows it: the operator did it, on purpose, outside any agent session.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const DIR: &str = "mcp-secrets";

pub fn dir(data: &Path) -> PathBuf {
    data.join(DIR)
}

/// A name that is also a valid environment variable name, so it cannot step out of the folder.
pub fn valid_name(name: &str) -> bool {
    let mut c = name.chars();
    name.len() <= 64
        && c.next()
            .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|f| f.is_ascii_alphanumeric() || f == '_')
}

/// Store `value` under `name` (replacing any). Leading and trailing white space is dropped.
pub fn set(data: &Path, name: &str, value: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!(
            "{name:?} is not a usable name: letters, digits and _, not starting with a digit, at most 64"
        ));
    }
    let value = value.trim();
    if value.is_empty() {
        return Err("the value is empty".into());
    }
    if value.contains(['\n', '\r', '\0']) {
        return Err("the value has a line break in it".into());
    }
    let d = dir(data);
    if std::fs::symlink_metadata(&d).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{} is a symlink; remove it first", d.display()));
    }
    std::fs::create_dir_all(&d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot protect {}: {e}", d.display()))?;
    // Written beside the target with its final mode, then renamed: never visible to others.
    let tmp = d.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("cannot write the secret: {e}"))?;
    let written = f
        .write_all(value.as_bytes())
        .and_then(|_| f.write_all(b"\n"))
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("cannot write the secret: {e}"))
        .and_then(|_| {
            std::fs::rename(&tmp, d.join(name)).map_err(|e| format!("cannot store the secret: {e}"))
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The folder is a real folder: a symlink there would send the tokens, and a chmod, elsewhere.
fn folder_ok(data: &Path) -> bool {
    std::fs::symlink_metadata(dir(data)).is_ok_and(|m| m.is_dir())
}

/// The stored value, trimmed. `None` when there is none, the name is not valid, the entry is
/// not a plain file (a symlink is refused) or it is empty. A loose mode is tightened first.
/// The file is opened once and judged by that handle, so it cannot be swapped for a link
/// between the check and the read.
pub fn get(data: &Path, name: &str) -> Option<String> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    if !valid_name(name) || !folder_ok(data) {
        return None;
    }
    let p = dir(data).join(name);
    let link = std::fs::symlink_metadata(&p).ok()?;
    let mut f = std::fs::File::open(&p).ok()?;
    let meta = f.metadata().ok()?;
    if !link.is_file() || !meta.is_file() || (link.dev(), link.ino()) != (meta.dev(), meta.ino()) {
        return None;
    }
    if meta.permissions().mode() & 0o077 != 0 {
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut v = String::new();
    f.read_to_string(&mut v).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// The stored names, sorted. Never the values.
pub fn list(data: &Path) -> Vec<String> {
    if !folder_ok(data) {
        return Vec::new();
    }
    let mut names: Vec<String> = std::fs::read_dir(dir(data))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_name(n) && get(data, n).is_some())
        .collect();
    names.sort();
    names
}

/// Delete a stored secret; whether there was one.
pub fn remove(data: &Path, name: &str) -> Result<bool, String> {
    if !valid_name(name) {
        return Err(format!("{name:?} is not a usable name"));
    }
    match std::fs::remove_file(dir(data).join(name)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove it: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-secrets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn names_are_environment_variable_names() {
        for ok in ["A", "MILLHOUSE_TOKEN", "_x", "a1"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "1A",
            "A-B",
            "../x",
            "a/b",
            "a b",
            "a=b",
            ".hidden",
            &"A".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_stored_value_comes_back_trimmed_and_the_file_is_private() {
        let d = tmp("roundtrip");
        set(&d, "TOK", "  github_pat_abc\n").unwrap();
        assert_eq!(get(&d, "TOK").as_deref(), Some("github_pat_abc"));
        let file = std::fs::metadata(dir(&d).join("TOK")).unwrap();
        assert_eq!(file.permissions().mode() & 0o777, 0o600);
        let folder = std::fs::metadata(dir(&d)).unwrap();
        assert_eq!(folder.permissions().mode() & 0o777, 0o700);
        set(&d, "TOK", "second").unwrap();
        assert_eq!(get(&d, "TOK").as_deref(), Some("second"), "replaced");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_names_and_values_are_refused() {
        let d = tmp("refuse");
        assert!(set(&d, "../x", "v").is_err());
        assert!(set(&d, "T", "").is_err());
        assert!(set(&d, "T", "  \n").is_err());
        assert!(set(&d, "T", "a\nb").is_err());
        assert!(list(&d).is_empty());
        assert_eq!(get(&d, "../x"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn list_gives_names_only_and_remove_deletes() {
        let d = tmp("list");
        set(&d, "B_TOK", "secret-b").unwrap();
        set(&d, "A_TOK", "secret-a").unwrap();
        let names = list(&d);
        assert_eq!(names, ["A_TOK", "B_TOK"]);
        assert!(!names.join(",").contains("secret"));
        assert_eq!(remove(&d, "A_TOK"), Ok(true));
        assert_eq!(remove(&d, "A_TOK"), Ok(false));
        assert_eq!(list(&d), ["B_TOK"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_symlinked_secrets_folder_is_neither_read_nor_written() {
        let d = tmp("dirlink");
        let elsewhere = tmp("dirlink-target");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("TOK"), "stolen\n").unwrap();
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir(&d)).unwrap();
        assert_eq!(get(&d, "TOK"), None);
        assert!(list(&d).is_empty());
        assert!(set(&d, "NEW", "v").is_err());
        assert!(!elsewhere.join("NEW").exists());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn a_symlink_is_not_a_secret_and_a_loose_mode_is_tightened() {
        let d = tmp("symlink");
        set(&d, "REAL", "v").unwrap();
        std::os::unix::fs::symlink(dir(&d).join("REAL"), dir(&d).join("LINK")).unwrap();
        assert_eq!(get(&d, "LINK"), None);
        assert_eq!(list(&d), ["REAL"]);
        std::fs::set_permissions(dir(&d).join("REAL"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert_eq!(get(&d, "REAL").as_deref(), Some("v"));
        let mode = std::fs::metadata(dir(&d).join("REAL"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }
}
