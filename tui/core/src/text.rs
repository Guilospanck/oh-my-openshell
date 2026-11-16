//! Small text helpers shared by the editor and the checks that feed it.

/// A check run on a file's text before it is saved; `Err` carries the reason.
pub type Check = fn(&str) -> Result<(), String>;

/// The line of an error to show: the first one, plus the last one when it says what was wrong
/// (TOML puts the location first and the reason after a code excerpt).
pub fn error_summary(msg: &str) -> String {
    let lines: Vec<&str> = msg
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match (lines.first(), lines.last()) {
        (Some(f), Some(l)) if lines.len() > 1 && !l.starts_with('|') && !l.contains('^') => {
            format!("{f}: {l}")
        }
        (Some(f), _) => f.to_string(),
        _ => "invalid".into(),
    }
}
