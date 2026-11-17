//! The bearer token every HTTP request must present. It lives in `<data>/mcp-token`, readable
//! by the owner only, and is read again for every request so that replacing the file takes
//! effect at once. It is never put in a log, a state file or a command line.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const FILE: &str = "mcp-token";

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Make sure a usable token file exists: create one when it is missing or empty, and tighten the
/// mode of an existing one to `0600` (the token may have been copied or edited by hand).
pub fn ensure(dir: &Path) -> std::io::Result<()> {
    let p = path(dir);
    if read(dir).is_some() {
        let mode = std::fs::metadata(&p)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    // Written beside the target with its final mode, then renamed: the token is never visible
    // to other users, not even for an instant, and a reader never sees half of it.
    let tmp = dir.join(format!("{FILE}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(hex.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    std::fs::rename(&tmp, &p)
}

/// The token, trimmed; `None` when the file is missing, unreadable or empty.
pub fn read(dir: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path(dir)).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Whether `presented` is the current token. The file is read on every call, and the comparison
/// takes the same time wherever the texts differ.
pub fn matches(dir: &Path, presented: &str) -> bool {
    let Some(token) = read(dir) else {
        return false;
    };
    let (a, b) = (token.as_bytes(), presented.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("osh-token-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ensure_creates_a_0600_file_once() {
        let d = tmp("once");
        ensure(&d.0).unwrap();
        let first = read(&d.0).unwrap();
        assert_eq!(first.len(), 64, "32 random bytes as hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(mode(&path(&d.0)), 0o600);
        ensure(&d.0).unwrap();
        assert_eq!(read(&d.0).unwrap(), first, "an existing token is kept");
    }

    #[test]
    fn ensure_tightens_a_loose_file_and_replaces_an_empty_one() {
        let d = tmp("loose");
        std::fs::write(path(&d.0), "abc123\n").unwrap();
        std::fs::set_permissions(path(&d.0), std::fs::Permissions::from_mode(0o644)).unwrap();
        ensure(&d.0).unwrap();
        assert_eq!(mode(&path(&d.0)), 0o600);
        assert_eq!(
            read(&d.0).unwrap(),
            "abc123",
            "the content is kept, trimmed"
        );
        std::fs::write(path(&d.0), "\n").unwrap();
        ensure(&d.0).unwrap();
        assert_eq!(read(&d.0).unwrap().len(), 64);
    }

    #[test]
    fn matches_reads_the_file_every_time() {
        let d = tmp("live");
        std::fs::write(path(&d.0), "first-token\n").unwrap();
        assert!(matches(&d.0, "first-token"));
        std::fs::write(path(&d.0), "second-token\n").unwrap();
        assert!(!matches(&d.0, "first-token"), "the old token stops working");
        assert!(matches(&d.0, "second-token"));
    }

    #[test]
    fn matches_is_false_without_a_file_or_with_the_wrong_text() {
        let d = tmp("none");
        assert!(!matches(&d.0, "anything"));
        assert!(!matches(&d.0, ""));
        std::fs::write(path(&d.0), "secret\n").unwrap();
        assert!(!matches(&d.0, ""));
        assert!(!matches(&d.0, "secre"));
        assert!(!matches(&d.0, "secret-and-more"));
    }
}
