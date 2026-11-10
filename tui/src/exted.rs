//! Editing a data file in the user's own editor ($VISUAL, $EDITOR, or one found on PATH), run
//! inside osh's embedded terminal so osh never exits or suspends. The text goes to a private
//! temporary copy; when the editor exits it is read back, checked with the file's validator and
//! only then written to the real file. The built-in editor (`editor.rs`) is the fallback.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::editor::Check;
use crate::platform::Host;
use crate::term::Session;

/// Editors tried, in order, when neither $VISUAL nor $EDITOR names one.
pub const FALLBACKS: [&str; 4] = ["nvim", "vim", "nano", "vi"];

/// File in the data dir that remembers the choice; absent means `Auto`.
pub const PREF_FILE: &str = "editor-mode";

/// Which editor the "edit this file" actions use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pref {
    /// The user's editor when one is found and starts, else the built-in one.
    Auto,
    /// Always the user's editor; refuse rather than fall back.
    External,
    /// Always the built-in editor.
    Builtin,
}

impl Pref {
    /// The next choice in the palette's cycle: auto, external, built-in, auto.
    pub fn next(self) -> Self {
        match self {
            Pref::Auto => Pref::External,
            Pref::External => Pref::Builtin,
            Pref::Builtin => Pref::Auto,
        }
    }
}

pub fn load_pref(dir: &Path) -> Pref {
    match std::fs::read_to_string(dir.join(PREF_FILE))
        .as_deref()
        .map(str::trim)
    {
        Ok("external") => Pref::External,
        Ok("builtin") => Pref::Builtin,
        _ => Pref::Auto,
    }
}

pub fn save_pref(dir: &Path, p: Pref) -> std::io::Result<()> {
    let path = dir.join(PREF_FILE);
    match p {
        Pref::Auto => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        Pref::External => std::fs::write(path, "external\n"),
        Pref::Builtin => std::fs::write(path, "builtin\n"),
    }
}

/// Split an $EDITOR value into words the way a POSIX shell would (quotes and backslashes), with
/// no expansion and without running a shell.
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match it.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("unclosed single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match it.next() {
                        Some('"') => break,
                        Some('\\') => match it.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => cur.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                cur.push('\\');
                                cur.push(c);
                            }
                            None => return Err("unclosed double quote".into()),
                        },
                        Some(c) => cur.push(c),
                        None => return Err("unclosed double quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match it.next() {
                    Some('\n') => {}
                    Some(c) => cur.push(c),
                    None => return Err("trailing backslash".into()),
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    if words.is_empty() {
        return Err("empty".into());
    }
    Ok(words)
}

/// An editor command and where it came from, for the status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub argv: Vec<String>,
    pub from: String,
}

impl Resolved {
    /// The program's own name (`nvim` for `/opt/bin/nvim -u NONE`).
    pub fn program(&self) -> &str {
        let p = self.argv[0].as_str();
        p.rsplit('/').next().unwrap_or(p)
    }
}

/// $VISUAL, then $EDITOR, then the first of `FALLBACKS` on PATH. A variable that does not parse
/// or names a program that is not there is skipped, and the reason ends up in the error.
pub fn resolve(h: &dyn Host) -> Result<Resolved, String> {
    let mut skipped = Vec::new();
    for var in ["VISUAL", "EDITOR"] {
        let Some(v) = h.env(var).filter(|v| !v.trim().is_empty()) else {
            continue;
        };
        match split_words(&v) {
            Ok(argv) => {
                let prog = &argv[0];
                let found = if prog.contains('/') {
                    h.exists(Path::new(prog))
                } else {
                    h.which(prog)
                };
                if found {
                    return Ok(Resolved {
                        argv,
                        from: format!("${var}"),
                    });
                }
                skipped.push(format!("${var} names {prog}, which is not there"));
            }
            Err(e) => skipped.push(format!("${var} does not parse ({e})")),
        }
    }
    if let Some(b) = FALLBACKS.iter().find(|b| h.which(b)) {
        return Ok(Resolved {
            argv: vec![b.to_string()],
            from: "found on PATH".into(),
        });
    }
    skipped.push(format!(
        "$VISUAL and $EDITOR name no editor and none of {} is on PATH",
        FALLBACKS.join(", ")
    ));
    Err(skipped.join("; "))
}

#[cfg(test)]
thread_local! {
    /// The editor unit tests of `App` run; `None` means none can be found.
    pub static FAKE: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// The editor of this machine. Unit tests never look at the real environment.
pub fn host_editor() -> Result<Resolved, String> {
    #[cfg(test)]
    {
        FAKE.with(|f| f.borrow().clone())
            .map(|argv| Resolved {
                argv,
                from: "test".into(),
            })
            .ok_or_else(|| "no editor in unit tests".to_string())
    }
    #[cfg(not(test))]
    {
        resolve(&crate::platform::RealHost)
    }
}

/// What an "edit this file" action does, given the preference and what `host_editor` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    External(Resolved),
    /// The built-in editor, and why (shown on its status bar).
    Builtin(String),
    /// External was forced but there is none: say why and open nothing.
    Refuse(String),
}

pub fn choose(pref: Pref, found: Result<Resolved, String>) -> Choice {
    match (pref, found) {
        (Pref::Builtin, _) => Choice::Builtin("built-in editor (chosen in the palette)".into()),
        (_, Ok(r)) => Choice::External(r),
        (Pref::Auto, Err(why)) => Choice::Builtin(format!("built-in editor: {why}")),
        (Pref::External, Err(why)) => Choice::Refuse(why),
    }
}

/// The text being edited, in a 0700 directory of its own (editors put swap and backup files next
/// to the file) as a 0600 file with the real file's name, so the editor picks the right syntax.
/// Both are removed when this is dropped.
pub struct Scratch {
    dir: PathBuf,
    pub file: PathBuf,
    /// What the editor was given, to tell an unchanged file from an edit.
    pub original: String,
}

impl Scratch {
    pub fn new(root: &Path, name: &str, text: &str) -> std::io::Result<Self> {
        let base = Path::new(name)
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut n = 0;
        let dir = loop {
            let dir = root.join(format!("osh-edit-{}-{stamp}-{n}", std::process::id()));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(e),
            }
        };
        let file = dir.join(base);
        let done = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
            .and_then(|mut f| f.write_all(text.as_bytes()));
        if let Err(e) = done {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        Ok(Scratch {
            dir,
            file,
            original: text.to_string(),
        })
    }

    pub fn read(&self) -> std::io::Result<String> {
        std::fs::read_to_string(&self.file)
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write through a temporary file next to `path` and a rename, so a failed write leaves the old
/// file whole.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let base = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let tmp = path.with_file_name(format!(".{base}.osh-edit"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("Cannot write {}: {e}", path.display())
        })
}

/// What came of an editor run.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Checked and written to the real file.
    Saved,
    /// The file is as the editor got it: nothing written.
    Unchanged,
    /// The editor failed, or the copy could not be read or written back: nothing written.
    Cancelled(String),
    /// The check failed (its summary); the edits stay in the temporary file for another go.
    Invalid(String),
}

/// One file being edited in the user's editor.
pub struct Run {
    /// The editor while it runs; `None` while osh asks to reopen or discard.
    pub session: Option<Session>,
    pub scratch: Scratch,
    /// Shown name (`providers/npm.yaml`).
    pub name: String,
    pub dest: PathBuf,
    pub check: Option<Check>,
    pub editor: Resolved,
    /// The last check failure, while osh asks to reopen the editor or discard.
    pub error: Option<String>,
}

impl Run {
    pub fn start(
        editor: Resolved,
        name: &str,
        dest: PathBuf,
        text: &str,
        check: Option<Check>,
        size: (u16, u16),
    ) -> Result<Self, String> {
        let scratch = Scratch::new(&std::env::temp_dir(), name, text)
            .map_err(|e| format!("cannot make a temporary copy: {e}"))?;
        let mut run = Run {
            session: None,
            scratch,
            name: name.to_string(),
            dest,
            check,
            editor,
            error: None,
        };
        run.spawn(size)?;
        Ok(run)
    }

    /// Start (or start again) the editor on the temporary copy.
    pub fn spawn(&mut self, (cols, rows): (u16, u16)) -> Result<(), String> {
        let mut args = self.editor.argv[1..].to_vec();
        args.push(self.scratch.file.to_string_lossy().into_owned());
        let s = Session::spawn(
            self.name.clone(),
            String::new(),
            &self.editor.argv[0],
            &args,
            cols,
            rows,
        )?;
        self.session = Some(s);
        self.error = None;
        Ok(())
    }

    /// The editor has exited with `success` (`None`: unknown, taken as success). Read the copy
    /// back, check it, and write it to the real file when it changed and passes.
    pub fn finish(&mut self, success: Option<bool>) -> Outcome {
        self.session = None;
        let prog = self.editor.program().to_string();
        if success == Some(false) {
            return Outcome::Cancelled(format!(
                "{prog} exited with an error: {} not changed",
                self.name
            ));
        }
        let text = match self.scratch.read() {
            Ok(t) => t,
            Err(e) => {
                return Outcome::Cancelled(format!(
                    "cannot read back what {prog} saved: {e}; {} not changed",
                    self.name
                ))
            }
        };
        if text == self.scratch.original {
            return Outcome::Unchanged;
        }
        if let Some(check) = self.check {
            if let Err(e) = check(&text) {
                let msg = crate::editor::error_summary(&e);
                self.error = Some(msg.clone());
                return Outcome::Invalid(msg);
            }
        }
        match write_atomic(&self.dest, &text) {
            Ok(()) => Outcome::Saved,
            Err(e) => Outcome::Cancelled(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testing::FakeHost;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-exted-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn words(s: &str) -> Vec<String> {
        split_words(s).unwrap()
    }

    #[test]
    fn editor_values_split_like_a_shell() {
        assert_eq!(words("nvim"), ["nvim"]);
        assert_eq!(words("  code  --wait "), ["code", "--wait"]);
        assert_eq!(
            words("'/Apps/My Editor/bin/ed' -n"),
            ["/Apps/My Editor/bin/ed", "-n"]
        );
        assert_eq!(
            words(r#"vim -c "set ft=toml""#),
            ["vim", "-c", "set ft=toml"]
        );
        assert_eq!(words(r#"a\ b "x\"y" 'q\'"#), ["a b", "x\"y", "q\\"]);
        assert_eq!(
            words(r#"e "" x"#),
            ["e", "", "x"],
            "an empty quoted word stays"
        );
        assert_eq!(words("$HOME/ed"), ["$HOME/ed"], "no expansion");
        assert!(split_words("vim 'oops").is_err());
        assert!(split_words("vim \"oops").is_err());
        assert!(split_words("vim \\").is_err());
        assert!(split_words("   ").is_err());
    }

    fn host(env: &[(&str, &str)], bins: &[&str]) -> FakeHost {
        let mut h = FakeHost::with_bins(bins);
        for (k, v) in env {
            h.env.insert(k.to_string(), v.to_string());
        }
        h
    }

    #[test]
    fn visual_comes_before_editor_and_keeps_its_arguments() {
        let h = host(
            &[("VISUAL", "nvim -u NONE"), ("EDITOR", "nano")],
            &["nvim", "nano"],
        );
        let r = resolve(&h).unwrap();
        assert_eq!(r.argv, ["nvim", "-u", "NONE"]);
        assert_eq!(r.from, "$VISUAL");
        let h = host(&[("EDITOR", "'my ed' --wait")], &["my ed"]);
        let r = resolve(&h).unwrap();
        assert_eq!(
            (r.argv, r.from),
            (vec!["my ed".into(), "--wait".into()], "$EDITOR".into())
        );
    }

    #[test]
    fn a_path_in_editor_is_checked_as_a_path() {
        let mut h = host(&[("EDITOR", "/opt/e/vi -x")], &[]);
        h.paths.insert("/opt/e/vi".into());
        let r = resolve(&h).unwrap();
        assert_eq!(r.argv, ["/opt/e/vi", "-x"]);
        assert_eq!(r.program(), "vi");
    }

    #[test]
    fn unusable_variables_fall_through_to_path() {
        let h = host(
            &[("VISUAL", "gone-editor"), ("EDITOR", "vim 'x")],
            &["nano", "vi"],
        );
        let r = resolve(&h).unwrap();
        assert_eq!(
            r.argv,
            ["nano"],
            "nvim and vim are not there, nano is first"
        );
        assert_eq!(r.from, "found on PATH");
        let h = host(&[("EDITOR", "  ")], &["vi", "vim"]);
        assert_eq!(
            resolve(&h).unwrap().argv,
            ["vim"],
            "order is nvim, vim, nano, vi"
        );
    }

    #[test]
    fn no_editor_says_why() {
        let e = resolve(&host(&[("VISUAL", "nope")], &[])).unwrap_err();
        assert!(e.contains("$VISUAL names nope"), "{e}");
        assert!(e.contains("none of nvim, vim, nano, vi is on PATH"), "{e}");
    }

    #[test]
    fn the_preference_decides_and_auto_falls_back() {
        let ed = Resolved {
            argv: vec!["vi".into()],
            from: "found on PATH".into(),
        };
        assert_eq!(
            choose(Pref::Auto, Ok(ed.clone())),
            Choice::External(ed.clone())
        );
        assert_eq!(
            choose(Pref::External, Ok(ed.clone())),
            Choice::External(ed.clone())
        );
        assert!(
            matches!(choose(Pref::Builtin, Ok(ed)), Choice::Builtin(w) if w.contains("palette"))
        );
        assert!(
            matches!(choose(Pref::Auto, Err("none".into())), Choice::Builtin(w) if w.contains("none"))
        );
        assert_eq!(
            choose(Pref::External, Err("none".into())),
            Choice::Refuse("none".into())
        );
    }

    #[test]
    fn the_preference_is_remembered_in_the_data_dir() {
        let d = tmp("pref");
        assert_eq!(load_pref(&d), Pref::Auto);
        for p in [Pref::External, Pref::Builtin, Pref::Auto] {
            save_pref(&d, p).unwrap();
            assert_eq!(load_pref(&d), p);
        }
        assert!(!d.join(PREF_FILE).exists(), "auto leaves no file");
        assert_eq!(Pref::Auto.next().next().next(), Pref::Auto);
    }

    #[test]
    fn the_scratch_copy_is_private_keeps_the_name_and_goes_away() {
        let root = tmp("scratch");
        let s = Scratch::new(&root, "images/Dockerfile-foo", "FROM x\n").unwrap();
        assert_eq!(s.file.file_name().unwrap(), "Dockerfile-foo");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(s.dir()), 0o700);
        assert_eq!(mode(&s.file), 0o600);
        assert_eq!(s.read().unwrap(), "FROM x\n");
        let other = Scratch::new(&root, "toolchains.toml", "").unwrap();
        assert_ne!(s.dir(), other.dir(), "each edit gets its own folder");
        assert_eq!(other.file.file_name().unwrap(), "toolchains.toml");
        let dir = s.dir().to_path_buf();
        drop(s);
        assert!(!dir.exists(), "removed with its contents");
        drop(other);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn writes_replace_the_file_whole_and_make_folders() {
        let d = tmp("write");
        let p = d.join("providers/x.yaml");
        write_atomic(&p, "a: 1\n").unwrap();
        write_atomic(&p, "a: 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a: 2\n");
        assert_eq!(std::fs::read_dir(d.join("providers")).unwrap().count(), 1);
    }

    fn sh(script: &str) -> Resolved {
        Resolved {
            argv: vec![
                "sh".into(),
                "-c".into(),
                script.into(),
                "fake-editor".into(),
            ],
            from: "test".into(),
        }
    }

    fn not_empty(s: &str) -> Result<(), String> {
        if s.contains("bad") {
            Err("TOML parse error at line 1, column 1\n  |\n1 | bad\ninvalid key".into())
        } else {
            Ok(())
        }
    }

    /// Run the editor to its end and report the outcome.
    pub fn run_out(run: &mut Run) -> Outcome {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let s = run.session.as_mut().expect("editor running");
            s.pump();
            if s.exited {
                let ok = s.wait_exit();
                return run.finish(ok);
            }
            assert!(Instant::now() < end, "fake editor never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_fake_editor_edits_the_copy_and_only_a_valid_result_is_written() {
        let d = tmp("run");
        let dest = d.join("toolchains.toml");
        std::fs::write(&dest, "old\n").unwrap();
        // First go writes something the check refuses.
        let mut run = Run::start(
            sh(r#"printf 'bad\n' > "$1""#),
            "toolchains.toml",
            dest.clone(),
            "old\n",
            Some(not_empty),
            (40, 10),
        )
        .unwrap();
        let file = run.scratch.file.clone();
        assert_eq!(file.file_name().unwrap(), "toolchains.toml");
        match run_out(&mut run) {
            Outcome::Invalid(m) => assert!(m.contains("TOML parse error"), "{m}"),
            o => panic!("{o:?}"),
        }
        assert!(run.error.is_some());
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "old\n",
            "not written"
        );
        // Reopened on the same copy: it still holds the user's edit.
        run.editor = sh(r#"grep -q bad "$1" && printf 'good\n' > "$1""#);
        run.spawn((40, 10)).unwrap();
        assert!(run.error.is_none());
        assert_eq!(run_out(&mut run), Outcome::Saved);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "good\n");
        drop(run);
        assert!(
            !file.exists() && !file.parent().unwrap().exists(),
            "temp copy gone"
        );
    }

    #[test]
    fn an_unchanged_copy_or_a_failing_editor_writes_nothing() {
        let d = tmp("cancel");
        let dest = d.join("x.yaml");
        std::fs::write(&dest, "a\n").unwrap();
        let mut run =
            Run::start(sh("true"), "x.yaml", dest.clone(), "a\n", None, (40, 10)).unwrap();
        assert_eq!(run_out(&mut run), Outcome::Unchanged);
        let mut run = Run::start(
            sh(r#"printf 'b\n' > "$1"; exit 3"#),
            "x.yaml",
            dest.clone(),
            "a\n",
            None,
            (40, 10),
        )
        .unwrap();
        match run_out(&mut run) {
            Outcome::Cancelled(m) => assert!(m.contains("sh exited with an error"), "{m}"),
            o => panic!("{o:?}"),
        }
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "a\n");
    }

    #[test]
    fn an_editor_that_cannot_start_is_an_error() {
        let d = tmp("nostart");
        let r = Resolved {
            argv: vec!["/nonexistent/osh-no-editor".into()],
            from: "test".into(),
        };
        let e = Run::start(r, "x", d.join("x"), "", None, (40, 10));
        assert!(e.is_err());
    }
}
