//! A full-screen text editor for files in the data directory (`toolchains.toml` first), so a user
//! who has only the `osh` binary never needs an outside editor. The core (text, cursor, selection,
//! undo, find) is pure and driven by `on_key`; it never touches the disk except in `save`, which
//! runs the file's own check first and refuses to write text the check rejects.
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use std::{cell::Cell, path::PathBuf};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Columns between tab stops when a tab character in the file is shown.
const TAB_STOP: usize = 4;
/// Spaces the Tab key inserts (to the next multiple): the indent of TOML and YAML files.
const INDENT: usize = 2;
/// Undo steps kept.
const UNDO_MAX: usize = 1000;

/// Checks the text before it is written; Err holds the reason (its first line is shown).
pub type Check = fn(&str) -> Result<(), String>;

/// A place in the text: `col` is a byte offset into the line, always on a grapheme boundary.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

const fn pos(row: usize, col: usize) -> Pos {
    Pos { row, col }
}

#[derive(Clone)]
struct Snap {
    lines: Vec<String>,
    at: Pos,
}

/// What the last edit was, so a run of typed characters (or deletions) undoes as one step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Type,
    Delete,
    Other,
}

/// The line under the text: editing, or one of the prompts that take the keys for a moment.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Edit,
    /// Incremental find; `from` is where the cursor was when it started (Esc goes back there).
    Find {
        query: String,
        from: Pos,
    },
    /// Go to `line` or `line:col`.
    Goto(String),
    /// Closing with unsaved changes: save, discard or keep editing.
    Quit,
}

/// What the caller has to do after a key.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    Stay,
    /// Ctrl-S: call `save`.
    Save,
    /// Save, then close when it worked.
    SaveQuit,
    /// Close without saving.
    Quit,
}

pub struct Editor {
    /// The file name shown in the title and status bar.
    pub name: String,
    /// Successful saves since the editor opened, so the caller knows the file changed on disk.
    pub saves: usize,
    pub path: PathBuf,
    lines: Vec<String>,
    at: Pos,
    /// The other end of the selection, when there is one.
    anchor: Option<Pos>,
    /// Ctrl-Space mark: plain moves extend the selection until it is cleared.
    sticky: bool,
    /// Display column that up and down keep to, across shorter lines.
    want: Option<usize>,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    last: Option<Kind>,
    /// The lines as they are on disk, for the modified flag.
    saved: Vec<String>,
    /// The file ended with a newline (kept on save).
    eol: bool,
    /// The internal clipboard; `clip_line` when it holds whole lines copied without a selection.
    clip: String,
    clip_line: bool,
    /// Text just copied, for the caller to put on the host clipboard.
    copied: Option<String>,
    pub mode: Mode,
    /// The last find query, for Ctrl-N / Ctrl-P after the find bar closed.
    query: String,
    /// A message for the status bar until the next key; true when it is an error.
    note: Option<(String, bool)>,
    check: Option<Check>,
    /// Scroll position and text area size (rows, columns), kept by `draw`.
    top: Cell<usize>,
    left: Cell<usize>,
    view: Cell<(usize, usize)>,
}

/// Width of one grapheme drawn at display column `x`.
fn gwidth(g: &str, x: usize) -> usize {
    if g == "\t" {
        TAB_STOP - x % TAB_STOP
    } else if g.chars().any(char::is_control) {
        1
    } else {
        g.width()
    }
}

/// Display width of `s` up to byte `col`.
fn width_to(s: &str, col: usize) -> usize {
    s[..col.min(s.len())]
        .graphemes(true)
        .fold(0, |x, g| x + gwidth(g, x))
}

/// The byte offset of the last grapheme boundary whose display column is at most `x`.
fn col_at(s: &str, x: usize) -> usize {
    let mut w = 0;
    for (i, g) in s.grapheme_indices(true) {
        let n = gwidth(g, w);
        if w + n > x {
            return i;
        }
        w += n;
    }
    s.len()
}

fn prev_b(s: &str, col: usize) -> usize {
    s[..col]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}

fn next_b(s: &str, col: usize) -> usize {
    s[col..]
        .graphemes(true)
        .next()
        .map_or(col, |g| col + g.len())
}

fn is_word(g: &str) -> bool {
    g.chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// Leading spaces and tabs of a line.
fn indent_of(s: &str) -> &str {
    &s[..s.len() - s.trim_start_matches([' ', '\t']).len()]
}

/// The line (and column, when given) an error message points at, both 1-based: TOML says
/// `at line 3, column 5`, YAML `at line 3 column 5`.
pub fn error_at(msg: &str) -> Option<(usize, usize)> {
    let num = |s: &str| -> Option<usize> {
        let d: String = s.chars().take_while(char::is_ascii_digit).collect();
        d.parse().ok()
    };
    let i = msg.find("line ")?;
    let rest = &msg[i + 5..];
    let line = num(rest)?;
    let col = rest
        .find("column ")
        .and_then(|j| num(&rest[j + 7..]))
        .unwrap_or(1);
    Some((line, col))
}

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

impl Editor {
    pub fn new(name: &str, path: PathBuf, text: &str, check: Option<Check>) -> Self {
        let text = text.replace("\r\n", "\n");
        let eol = text.is_empty() || text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(&text);
        let lines: Vec<String> = body.split('\n').map(String::from).collect();
        Editor {
            name: name.into(),
            saves: 0,
            path,
            saved: lines.clone(),
            lines,
            at: Pos::default(),
            anchor: None,
            sticky: false,
            want: None,
            undo: Vec::new(),
            redo: Vec::new(),
            last: None,
            eol,
            clip: String::new(),
            clip_line: false,
            copied: None,
            mode: Mode::Edit,
            query: String::new(),
            note: None,
            check,
            top: Cell::new(0),
            left: Cell::new(0),
            view: Cell::new((20, 80)),
        }
    }

    /// A file that does not exist yet, started from `template`. It counts as unsaved, so closing
    /// without Ctrl-S asks first, and nothing is written until the user saves.
    pub fn create(name: &str, path: PathBuf, template: &str, check: Option<Check>) -> Self {
        let mut ed = Self::new(name, path, template, check);
        ed.saved = Vec::new();
        ed.note = Some((
            "New file: it is created when you save (Ctrl-S)".into(),
            false,
        ));
        ed
    }

    /// Open `path`; a file that does not exist yet starts empty and is created on save.
    pub fn open(name: &str, path: PathBuf, check: Option<Check>) -> Result<Self, String> {
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Self::new(name, path, &text, check)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut ed = Self::new(name, path, "", check);
                ed.note = Some(("New file: it is created when you save".into(), false));
                Ok(ed)
            }
            Err(e) => Err(format!("Cannot open {name}: {e}")),
        }
    }

    /// The text as it would be saved; a document of one empty line is the empty file.
    pub fn text(&self) -> String {
        let mut t = self.lines.join("\n");
        if self.eol && !t.is_empty() {
            t.push('\n');
        }
        t
    }

    pub fn modified(&self) -> bool {
        self.lines != self.saved
    }

    /// Line and display column of the cursor, both 1-based.
    pub fn cursor(&self) -> (usize, usize) {
        (
            self.at.row + 1,
            width_to(&self.lines[self.at.row], self.at.col) + 1,
        )
    }

    pub fn selection(&self) -> Option<String> {
        self.range().map(|(a, b)| self.slice(a, b))
    }

    pub fn note(&self) -> Option<&(String, bool)> {
        self.note.as_ref()
    }

    /// Say on the status bar why this editor was opened, unless it already says something.
    pub fn hint(&mut self, msg: &str) {
        if self.note.is_none() {
            self.note = Some((msg.to_string(), false));
        }
    }

    /// Text copied since the last call, for the host clipboard.
    pub fn take_copied(&mut self) -> Option<String> {
        self.copied.take()
    }

    /// Run the file's check. On a failure the status bar shows why and the cursor goes to the
    /// line the error names, so the fix starts where the problem is.
    pub fn validate(&mut self) -> Result<(), String> {
        let Some(check) = self.check else {
            return Ok(());
        };
        check(&self.text()).map_err(|e| {
            let msg = format!("Not saved: {}", error_summary(&e));
            if let Some((line, col)) = error_at(&e) {
                let row = line.saturating_sub(1).min(self.lines.len() - 1);
                let l = &self.lines[row];
                self.anchor = None;
                self.sticky = false;
                self.at = pos(row, col_at(l, col.saturating_sub(1)));
                self.want = None;
            }
            self.note = Some((msg.clone(), true));
            msg
        })
    }

    /// Check, then write the file (through a temporary file, so a failed write leaves the old
    /// one whole). Errors are also put in the status bar.
    pub fn save(&mut self) -> Result<(), String> {
        self.validate()?;
        let text = self.text();
        // The name can be a relative path (`providers/x.yaml`): the temporary file sits next to
        // the real one, named after its last part.
        let base = self
            .path
            .file_name()
            .map_or_else(|| self.name.clone(), |n| n.to_string_lossy().into_owned());
        let tmp = self.path.with_file_name(format!(".{base}.osh-edit"));
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let done = std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &self.path));
        match done {
            Ok(()) => {
                self.saves += 1;
                self.saved = self.lines.clone();
                self.note = Some((format!("Saved {}", self.name), false));
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                let msg = format!("Could not save {}: {e}", self.name);
                self.note = Some((msg.clone(), true));
                Err(msg)
            }
        }
    }

    // ---- text primitives ---------------------------------------------------

    fn line(&self) -> &str {
        &self.lines[self.at.row]
    }

    fn range(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        (a != self.at).then(|| (a.min(self.at), a.max(self.at)))
    }

    fn slice(&self, a: Pos, b: Pos) -> String {
        if a.row == b.row {
            return self.lines[a.row][a.col..b.col].to_string();
        }
        let mut out = self.lines[a.row][a.col..].to_string();
        for l in &self.lines[a.row + 1..b.row] {
            out.push('\n');
            out.push_str(l);
        }
        out.push('\n');
        out.push_str(&self.lines[b.row][..b.col]);
        out
    }

    fn remove(&mut self, a: Pos, b: Pos) {
        if a.row == b.row {
            self.lines[a.row].replace_range(a.col..b.col, "");
        } else {
            let tail = self.lines[b.row][b.col..].to_string();
            self.lines[a.row].truncate(a.col);
            self.lines[a.row].push_str(&tail);
            self.lines.drain(a.row + 1..=b.row);
        }
        self.at = a;
    }

    /// Insert `s` (may hold newlines) at the cursor and move past it.
    fn insert(&mut self, s: &str) {
        let Pos { row, col } = self.at;
        let tail = self.lines[row].split_off(col);
        let mut parts = s.split('\n');
        self.lines[row].push_str(parts.next().unwrap_or_default());
        let mut r = row;
        for p in parts {
            r += 1;
            self.lines.insert(r, p.to_string());
        }
        let c = self.lines[r].len();
        self.lines[r].push_str(&tail);
        self.at = pos(r, c);
    }

    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.range() else {
            self.anchor = None;
            return false;
        };
        self.remove(a, b);
        self.anchor = None;
        self.sticky = false;
        true
    }

    /// Record the state before an edit. A run of the same kind of edit is one undo step.
    fn begin(&mut self, k: Kind) {
        if !(k != Kind::Other && self.last == Some(k)) {
            self.undo.push(Snap {
                lines: self.lines.clone(),
                at: self.at,
            });
            if self.undo.len() > UNDO_MAX {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last = Some(k);
        self.want = None;
    }

    fn restore(&mut self, from_undo: bool) {
        let (src, dst) = if from_undo {
            (&mut self.undo, &mut self.redo)
        } else {
            (&mut self.redo, &mut self.undo)
        };
        let Some(s) = src.pop() else {
            self.note = Some((
                if from_undo {
                    "Nothing to undo"
                } else {
                    "Nothing to redo"
                }
                .into(),
                false,
            ));
            return;
        };
        dst.push(Snap {
            lines: std::mem::replace(&mut self.lines, s.lines),
            at: self.at,
        });
        self.at = s.at;
        self.at.row = self.at.row.min(self.lines.len() - 1);
        self.at.col = self.at.col.min(self.lines[self.at.row].len());
        self.anchor = None;
        self.sticky = false;
        self.last = None;
        self.want = None;
    }

    // ---- moves -------------------------------------------------------------

    fn left_of(&self, p: Pos) -> Pos {
        if p.col > 0 {
            pos(p.row, prev_b(&self.lines[p.row], p.col))
        } else if p.row > 0 {
            pos(p.row - 1, self.lines[p.row - 1].len())
        } else {
            p
        }
    }

    fn right_of(&self, p: Pos) -> Pos {
        let l = &self.lines[p.row];
        if p.col < l.len() {
            pos(p.row, next_b(l, p.col))
        } else if p.row + 1 < self.lines.len() {
            pos(p.row + 1, 0)
        } else {
            p
        }
    }

    fn word_left(&self, p: Pos) -> Pos {
        if p.col == 0 {
            return self.left_of(p);
        }
        let l = &self.lines[p.row];
        let gs: Vec<(usize, &str)> = l[..p.col].grapheme_indices(true).collect();
        let mut i = gs.len();
        while i > 0 && !is_word(gs[i - 1].1) {
            i -= 1;
        }
        while i > 0 && is_word(gs[i - 1].1) {
            i -= 1;
        }
        pos(p.row, gs.get(i).map_or(0, |g| g.0))
    }

    fn word_right(&self, p: Pos) -> Pos {
        let l = &self.lines[p.row];
        if p.col >= l.len() {
            return self.right_of(p);
        }
        let mut it = l[p.col..].grapheme_indices(true).peekable();
        while it.peek().is_some_and(|(_, g)| !is_word(g)) {
            it.next();
        }
        while it.peek().is_some_and(|(_, g)| is_word(g)) {
            it.next();
        }
        pos(p.row, it.peek().map_or(l.len(), |(i, _)| p.col + i))
    }

    /// `n` rows down (negative: up), keeping the display column.
    fn vertical(&mut self, n: isize) -> Pos {
        let want = self
            .want
            .unwrap_or_else(|| width_to(self.line(), self.at.col));
        let row = (self.at.row as isize + n).clamp(0, self.lines.len() as isize - 1) as usize;
        self.want = Some(want);
        pos(row, col_at(&self.lines[row], want))
    }

    /// Move the cursor to `to`; with `select` the selection grows from where it was.
    fn go(&mut self, to: Pos, select: bool) {
        if select || self.sticky {
            self.anchor.get_or_insert(self.at);
        } else {
            self.anchor = None;
        }
        self.at = to;
        self.last = None;
    }

    // ---- find --------------------------------------------------------------

    /// Every match of `q` in the text, in order. Smart case: a query with a capital letter is
    /// case-sensitive. Matches do not span lines.
    fn matches(&self, q: &str) -> Vec<(Pos, Pos)> {
        if q.is_empty() {
            return Vec::new();
        }
        let exact = q.chars().any(|c| c.is_uppercase());
        let q = if exact {
            q.to_string()
        } else {
            q.to_lowercase()
        };
        let mut out = Vec::new();
        for (r, l) in self.lines.iter().enumerate() {
            // ASCII folding keeps byte offsets valid in the original line.
            let hay = if exact {
                l.clone()
            } else {
                l.to_ascii_lowercase()
            };
            for (i, m) in hay.match_indices(q.as_str()) {
                out.push((pos(r, i), pos(r, i + m.len())));
            }
        }
        out
    }

    /// Select the first match at or after `from` (forward) or before it, wrapping around.
    fn seek(&mut self, q: &str, from: Pos, forward: bool) -> bool {
        let all = self.matches(q);
        let hit = if forward {
            all.iter().find(|(a, _)| *a >= from).or(all.first())
        } else {
            all.iter().rev().find(|(a, _)| *a < from).or(all.last())
        };
        match hit.copied() {
            Some((a, b)) => {
                let wrapped = if forward { a < from } else { a >= from };
                self.anchor = Some(a);
                self.at = b;
                self.sticky = false;
                self.want = None;
                let n = all.iter().position(|m| m.0 == a).unwrap_or(0) + 1;
                let w = if wrapped { ", wrapped" } else { "" };
                self.note = Some((format!("Match {n} of {}{w}", all.len()), false));
                true
            }
            None => {
                self.note = Some((format!("No match for \"{q}\""), true));
                false
            }
        }
    }

    fn find_next(&mut self, forward: bool) {
        if self.query.is_empty() {
            self.note = Some(("Nothing to find yet: Ctrl-F starts a search".into(), false));
            return;
        }
        let q = self.query.clone();
        let from = match (forward, self.range()) {
            (true, Some((a, _))) => self.right_of(a),
            (true, None) => self.at,
            (false, Some((a, _))) => a,
            (false, None) => self.at,
        };
        self.seek(&q, from, forward);
    }

    // ---- editing -----------------------------------------------------------

    pub fn type_char(&mut self, c: char) {
        if self.range().is_some() {
            // Replacing a selection and the typing after it undo together.
            self.begin(Kind::Other);
            self.delete_selection();
            self.last = Some(Kind::Type);
        } else {
            self.begin(Kind::Type);
        }
        self.insert(c.encode_utf8(&mut [0; 4]));
    }

    /// Insert text from a paste (the terminal's or the internal clipboard) as one undo step.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if text.is_empty() {
            return;
        }
        self.begin(Kind::Other);
        self.delete_selection();
        self.insert(&text);
        self.last = None;
    }

    fn newline(&mut self) {
        self.begin(Kind::Other);
        self.delete_selection();
        let l = self.line();
        let ind = indent_of(l);
        let ind = ind[..ind.len().min(self.at.col)].to_string();
        // The new line's indent replaces spaces the cursor stood before.
        let rest = &l[self.at.col..];
        let skip = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        if skip > 0 {
            let a = self.at;
            self.remove(a, pos(a.row, a.col + skip));
        }
        self.insert(&format!("\n{ind}"));
        self.last = None;
    }

    fn backspace(&mut self) {
        if self.range().is_some() {
            self.begin(Kind::Other);
            self.delete_selection();
            return;
        }
        let Pos { row, col } = self.at;
        if col == 0 && row == 0 {
            return;
        }
        self.begin(Kind::Delete);
        let before = &self.lines[row][..col];
        let to = if col > 0 && before.bytes().all(|b| b == b' ') {
            // In leading spaces, step back one indent level.
            pos(row, (col - 1) / INDENT * INDENT)
        } else {
            self.left_of(self.at)
        };
        self.remove(to, self.at);
    }

    fn delete(&mut self) {
        if self.range().is_some() {
            self.begin(Kind::Other);
            self.delete_selection();
            return;
        }
        let to = self.right_of(self.at);
        if to != self.at {
            self.begin(Kind::Delete);
            self.remove(self.at, to);
        }
    }

    fn delete_to(&mut self, to: Pos) {
        if self.delete_selection_step() {
            return;
        }
        if to != self.at {
            self.begin(Kind::Other);
            let (a, b) = (to.min(self.at), to.max(self.at));
            self.remove(a, b);
        }
    }

    fn delete_selection_step(&mut self) -> bool {
        if self.range().is_some() {
            self.begin(Kind::Other);
            self.delete_selection();
            true
        } else {
            false
        }
    }

    /// Ctrl-K: cut to the end of the line, or the line break when the cursor is at the end.
    fn kill_line(&mut self) {
        let end = if self.at.col < self.line().len() {
            pos(self.at.row, self.line().len())
        } else {
            self.right_of(self.at)
        };
        if end == self.at {
            return;
        }
        self.begin(Kind::Other);
        let text = self.slice(self.at, end);
        self.remove(self.at, end);
        self.anchor = None;
        self.set_clip(text, false);
    }

    /// Rows the selection covers, or the cursor row.
    fn rows(&self) -> (usize, usize) {
        match self.range() {
            Some((a, b)) if b.col == 0 && b.row > a.row => (a.row, b.row - 1),
            Some((a, b)) => (a.row, b.row),
            None => (self.at.row, self.at.row),
        }
    }

    fn tab(&mut self) {
        let (a, b) = self.rows();
        if self.range().is_some() && b > a {
            return self.shift_rows(true);
        }
        self.begin(Kind::Other);
        self.delete_selection();
        let x = width_to(self.line(), self.at.col);
        self.insert(&" ".repeat(INDENT - x % INDENT));
    }

    /// Indent (or outdent) the selected rows by one level, keeping the selection.
    fn shift_rows(&mut self, right: bool) {
        let (a, b) = self.rows();
        self.begin(Kind::Other);
        let mut delta: Vec<isize> = Vec::new();
        for l in &mut self.lines[a..=b] {
            if right {
                if !l.is_empty() {
                    l.insert_str(0, &" ".repeat(INDENT));
                    delta.push(INDENT as isize);
                } else {
                    delta.push(0);
                }
            } else {
                let n = l.len() - l.trim_start_matches(' ').len();
                let n = n.min(INDENT);
                l.replace_range(..n, "");
                delta.push(-(n as isize));
            }
        }
        let fix = |p: Pos, lines: &[String]| -> Pos {
            if p.row < a || p.row > b {
                return p;
            }
            let d = delta[p.row - a];
            let c = (p.col as isize + d).max(0) as usize;
            pos(p.row, c.min(lines[p.row].len()))
        };
        self.at = fix(self.at, &self.lines);
        if let Some(an) = self.anchor {
            self.anchor = Some(fix(an, &self.lines));
        }
        self.last = None;
    }

    fn set_clip(&mut self, text: String, line: bool) {
        self.note = Some((format!("Copied {} chars", text.chars().count()), false));
        self.clip = text.clone();
        self.clip_line = line;
        self.copied = Some(text);
    }

    /// Ctrl-C / Ctrl-X: the selection, or the whole line when nothing is selected.
    fn copy(&mut self, cut: bool) {
        if let Some((a, b)) = self.range() {
            let text = self.slice(a, b);
            if cut {
                self.begin(Kind::Other);
                self.remove(a, b);
            }
            self.anchor = None;
            self.sticky = false;
            self.set_clip(text, false);
            return;
        }
        let row = self.at.row;
        let text = format!("{}\n", self.lines[row]);
        if cut {
            self.begin(Kind::Other);
            if self.lines.len() == 1 {
                self.lines[0].clear();
            } else {
                self.lines.remove(row);
            }
            let r = row.min(self.lines.len() - 1);
            self.at = pos(r, 0);
        }
        self.set_clip(text, true);
    }

    fn paste_clip(&mut self) {
        if self.clip.is_empty() {
            self.note = Some(("The clipboard is empty".into(), false));
            return;
        }
        let text = self.clip.clone();
        if self.clip_line && self.range().is_none() {
            // A copied line goes in above the cursor's line, as a whole line.
            let keep = self.at;
            self.at = pos(self.at.row, 0);
            self.paste(&text);
            self.at = pos(keep.row + text.matches('\n').count(), keep.col);
        } else {
            self.paste(&text);
        }
    }

    fn goto(&mut self, spec: &str) {
        let mut it = spec.split(':');
        let line: Option<usize> = it.next().and_then(|s| s.trim().parse().ok());
        let col: usize = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(1);
        let Some(line) = line.filter(|l| *l > 0) else {
            self.note = Some((format!("Not a line number: {spec}"), true));
            return;
        };
        let row = (line - 1).min(self.lines.len() - 1);
        let to = pos(row, col_at(&self.lines[row], col.saturating_sub(1)));
        self.go(to, false);
        self.want = None;
    }

    /// Ask before closing with unsaved changes.
    fn quit(&mut self) -> Out {
        if self.modified() {
            self.mode = Mode::Quit;
            Out::Stay
        } else {
            Out::Quit
        }
    }

    // ---- keys --------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) -> Out {
        self.note = None;
        match self.mode.clone() {
            Mode::Edit => self.edit_key(key),
            Mode::Find { query, from } => {
                self.find_key(key, query, from);
                Out::Stay
            }
            Mode::Goto(buf) => {
                self.goto_key(key, buf);
                Out::Stay
            }
            Mode::Quit => {
                self.mode = Mode::Edit;
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('s' | 'S' | 'y' | 'Y') => Out::SaveQuit,
                    KeyCode::Char('d' | 'D' | 'n' | 'N') if !ctrl => Out::Quit,
                    _ => Out::Stay,
                }
            }
        }
    }

    fn find_key(&mut self, key: KeyEvent, mut query: String, from: Pos) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.at = from;
                self.anchor = None;
                self.mode = Mode::Edit;
                return;
            }
            KeyCode::Enter => {
                self.query = query;
                self.mode = Mode::Edit;
                return;
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::F(3)
                if !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.query = query.clone();
                self.find_next(true);
            }
            KeyCode::Char('n') if ctrl => {
                self.query = query.clone();
                self.find_next(true);
            }
            KeyCode::Up | KeyCode::BackTab | KeyCode::F(3) => {
                self.query = query.clone();
                self.find_next(false);
            }
            KeyCode::Char('p') if ctrl => {
                self.query = query.clone();
                self.find_next(false);
            }
            KeyCode::Backspace => {
                query.pop();
                self.incremental(&query, from);
            }
            KeyCode::Char(c) if !ctrl => {
                query.push(c);
                self.incremental(&query, from);
            }
            _ => {}
        }
        self.mode = Mode::Find { query, from };
    }

    fn incremental(&mut self, q: &str, from: Pos) {
        if q.is_empty() || !self.seek(q, from, true) {
            self.at = from;
            self.anchor = None;
        }
    }

    fn goto_key(&mut self, key: KeyEvent, mut buf: String) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Edit;
                return;
            }
            KeyCode::Enter => {
                self.mode = Mode::Edit;
                self.goto(&buf);
                return;
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c @ ('0'..='9' | ':')) => buf.push(c),
            _ => {}
        }
        self.mode = Mode::Goto(buf);
    }

    fn edit_key(&mut self, key: KeyEvent) -> Out {
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let alt = m.contains(KeyModifiers::ALT);
        let shift = m.contains(KeyModifiers::SHIFT);
        let rows = self.view.get().0.max(2) as isize - 1;
        match key.code {
            KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
                's' => return Out::Save,
                'q' => return self.quit(),
                'a' => self.go(pos(self.at.row, 0), false),
                'e' => self.go(pos(self.at.row, self.line().len()), false),
                'f' => {
                    let query = self
                        .selection()
                        .filter(|s| !s.contains('\n'))
                        .unwrap_or_default();
                    let from = self.range().map_or(self.at, |(a, _)| a);
                    self.mode = Mode::Find { query, from };
                }
                'n' => self.find_next(true),
                'p' => self.find_next(false),
                'g' => self.mode = Mode::Goto(String::new()),
                'z' => self.restore(true),
                'y' => self.restore(false),
                'c' => self.copy(false),
                'x' => self.copy(true),
                'v' => self.paste_clip(),
                'k' => self.kill_line(),
                'w' => {
                    let to = self.word_left(self.at);
                    self.delete_to(to);
                }
                ' ' | '@' => {
                    self.sticky = !self.sticky;
                    self.anchor = self.sticky.then_some(self.at);
                    self.note = Some((
                        if self.sticky {
                            "Mark set: moves select (Ctrl-Space again clears)"
                        } else {
                            "Mark cleared"
                        }
                        .into(),
                        false,
                    ));
                }
                _ => {}
            },
            KeyCode::Char(c) if alt => match c {
                'b' => self.go(self.word_left(self.at), shift),
                'f' => self.go(self.word_right(self.at), shift),
                'd' => {
                    let to = self.word_right(self.at);
                    self.delete_to(to);
                }
                'a' => {
                    let end = self.lines.len() - 1;
                    self.anchor = Some(pos(0, 0));
                    self.at = pos(end, self.lines[end].len());
                }
                _ => {}
            },
            KeyCode::Char(c) => self.type_char(c),
            KeyCode::Esc => {
                if self.anchor.is_some() || self.sticky {
                    self.anchor = None;
                    self.sticky = false;
                } else {
                    return self.quit();
                }
            }
            KeyCode::Enter => self.newline(),
            KeyCode::Tab => self.tab(),
            KeyCode::BackTab => self.shift_rows(false),
            KeyCode::Backspace if ctrl || alt => {
                let to = self.word_left(self.at);
                self.delete_to(to);
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete if ctrl || alt => {
                let to = self.word_right(self.at);
                self.delete_to(to);
            }
            KeyCode::Delete => self.delete(),
            KeyCode::Left if ctrl || alt => self.go(self.word_left(self.at), shift),
            KeyCode::Right if ctrl || alt => self.go(self.word_right(self.at), shift),
            KeyCode::Left => match self.range() {
                Some((a, _)) if !shift && !self.sticky => self.go(a, false),
                _ => self.go(self.left_of(self.at), shift),
            },
            KeyCode::Right => match self.range() {
                Some((_, b)) if !shift && !self.sticky => self.go(b, false),
                _ => self.go(self.right_of(self.at), shift),
            },
            KeyCode::Up => {
                let to = self.vertical(-1);
                self.go(to, shift);
            }
            KeyCode::Down => {
                let to = self.vertical(1);
                self.go(to, shift);
            }
            KeyCode::PageUp => {
                let to = self.vertical(-rows);
                self.top.set(self.top.get().saturating_sub(rows as usize));
                self.go(to, shift);
            }
            KeyCode::PageDown => {
                let to = self.vertical(rows);
                self.top.set(self.top.get() + rows as usize);
                self.go(to, shift);
            }
            KeyCode::Home if ctrl => self.go(pos(0, 0), shift),
            KeyCode::End if ctrl => {
                let r = self.lines.len() - 1;
                self.go(pos(r, self.lines[r].len()), shift);
            }
            KeyCode::Home => {
                // First non-blank, then the line start (smart home).
                let ind = indent_of(self.line()).len();
                let to = if self.at.col == ind { 0 } else { ind };
                self.go(pos(self.at.row, to), shift);
            }
            KeyCode::End => self.go(pos(self.at.row, self.line().len()), shift),
            KeyCode::F(3) if shift => self.find_next(false),
            KeyCode::F(3) => self.find_next(true),
            _ => {}
        }
        if !matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) {
            self.want = None;
        }
        Out::Stay
    }

    // ---- view --------------------------------------------------------------

    /// Scroll so the cursor is on screen.
    fn follow(&self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let last = self.lines.len().saturating_sub(rows);
        let mut top = self.top.get().min(last);
        if self.at.row < top {
            top = self.at.row;
        } else if self.at.row >= top + rows {
            top = self.at.row + 1 - rows;
        }
        self.top.set(top);
        let x = width_to(self.line(), self.at.col);
        let mut left = self.left.get();
        if x < left {
            left = x.saturating_sub(cols / 4);
        } else if x >= left + cols {
            left = x + 1 - cols + cols / 4;
        }
        self.left.set(left);
    }

    /// One text row as spans, from display column `left`, `cols` wide.
    fn render_row(&self, row: usize, left: usize, cols: usize) -> Line<'static> {
        let l = &self.lines[row];
        let sel = self.range().and_then(|(a, b)| {
            if row < a.row || row > b.row {
                return None;
            }
            let s = if row == a.row { a.col } else { 0 };
            let e = if row == b.row { b.col } else { l.len() };
            Some((s, e, row < b.row))
        });
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut push = |text: String, on: bool| {
            let st = if on { rev } else { Style::default() };
            match spans.last_mut() {
                Some(s) if s.style == st => s.content.to_mut().push_str(&text),
                _ => spans.push(Span::styled(text, st)),
            }
        };
        let end = left + cols;
        let mut x = 0;
        for (i, g) in l.grapheme_indices(true) {
            let w = gwidth(g, x);
            let on = sel.is_some_and(|(s, e, _)| i >= s && i < e);
            if x + w > left && x < end {
                let shown = if g == "\t" || x < left || x + w > end {
                    " ".repeat((x + w).min(end) - x.max(left))
                } else if g.chars().any(char::is_control) {
                    "?".into()
                } else {
                    g.to_string()
                };
                push(shown, on);
            }
            x += w;
            if x >= end {
                break;
            }
        }
        // A selected line break shows as one selected cell after the text.
        if sel.is_some_and(|(_, _, nl)| nl) && x >= left && x < end {
            push(" ".into(), true);
        }
        Line::from(spans)
    }
}

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

/// The editor's keys, for the help screen and the manual.
pub const KEYS: &[(&str, &str)] = &[
    (
        "Ctrl-S",
        "Save (the file is checked first; a broken file is not written)",
    ),
    (
        "Esc / Ctrl-Q",
        "Close; asks to save or discard unsaved changes",
    ),
    (
        "Arrows",
        "Move; Home / End or Ctrl-A / Ctrl-E line start and end",
    ),
    ("Ctrl-arrows", "Word left / right (Alt-b / Alt-f too)"),
    (
        "PgUp / PgDn",
        "Page; Ctrl-Home / Ctrl-End file start and end",
    ),
    (
        "Shift-moves",
        "Select; Ctrl-Space marks, then moves select; Alt-a all",
    ),
    (
        "Ctrl-C / X / V",
        "Copy, cut, paste (the line when nothing is selected)",
    ),
    ("Ctrl-Z / Ctrl-Y", "Undo / redo"),
    (
        "Ctrl-K / Ctrl-W",
        "Cut to the line end / delete the word before",
    ),
    (
        "Tab / Shift-Tab",
        "Indent / outdent (the selected lines too)",
    ),
    (
        "Ctrl-F",
        "Find as you type; Ctrl-N / Ctrl-P next and previous",
    ),
    ("Ctrl-G", "Go to a line (or line:col)"),
];

/// The key line: it wraps onto as many rows as it needs, so every key is always on screen.
pub const HINTS: &str = "Ctrl-S save  Ctrl-F find  Ctrl-N/P next/previous  Ctrl-G line  Ctrl-Z/Y undo/redo  Shift-arrows select  Ctrl-C/X/V copy/cut/paste  Esc close";
/// The keys of the find, go-to-line and unsaved-changes prompts.
pub const FIND_HINTS: &str = "Enter keep  ↓/Ctrl-N next  ↑/Ctrl-P previous  Esc back";
pub const GOTO_HINTS: &str = "line or line:col  Enter go  Esc cancel";
pub const QUIT_HINTS: &str = "s save and close  d discard changes  Esc keep editing";

/// The status bar: `left` (the path or a note) and `right` (the cursor) on one row when they fit,
/// else the left part wraps and the cursor gets its own row, so neither is cut.
fn status_rows(left: Line<'static>, right: &str, width: usize) -> Vec<Line<'static>> {
    let rw = right.width();
    let right_span = Span::styled(right.to_string(), Style::default().fg(ACCENT));
    if left.width() + rw < width {
        let pad = width - left.width() - rw;
        let mut spans = left.spans;
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(right_span);
        return vec![Line::from(spans)];
    }
    let mut out = crate::ui::wrap_line(&left, width);
    out.push(Line::from(vec![
        Span::raw(" ".repeat(width.saturating_sub(rw))),
        right_span,
    ]));
    out
}

/// The hints of the editor's mode.
fn mode_hints(mode: &Mode) -> &'static str {
    match mode {
        Mode::Find { .. } => FIND_HINTS,
        Mode::Goto(_) => GOTO_HINTS,
        Mode::Quit => QUIT_HINTS,
        Mode::Edit => HINTS,
    }
}

/// The bottom bar of the editor's mode as rows of `width` columns, every hint whole (the bar
/// grows instead), and the cursor column in the first row (for the prompts).
fn bar_rows(ed: &Editor, width: usize) -> (Vec<Line<'static>>, Option<u16>) {
    let label = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let rows = |lead: Vec<Span<'static>>, help: &str| -> Vec<Line<'static>> {
        // Rows after the first start with one space, so the packing runs one column narrower
        // and the first row's lead counts without that space.
        let lw: usize = lead
            .iter()
            .map(Span::width)
            .sum::<usize>()
            .saturating_sub(1);
        let entries = crate::ui::hint_entries(help);
        crate::ui::pack_hints(&entries, width.saturating_sub(1), lw)
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut spans = if i == 0 {
                    lead.clone()
                } else {
                    vec![Span::raw(" ")]
                };
                spans.extend(crate::ui::hint_spans(r));
                Line::from(spans)
            })
            .collect()
    };
    let help = mode_hints(&ed.mode);
    match &ed.mode {
        Mode::Find { query, .. } => (
            rows(
                vec![
                    Span::styled(" Find: ", label),
                    Span::raw(query.clone()),
                    Span::raw("  "),
                ],
                help,
            ),
            Some((7 + query.width()) as u16),
        ),
        Mode::Goto(buf) => (
            rows(
                vec![
                    Span::styled(" Go to line: ", label),
                    Span::raw(buf.clone()),
                    Span::raw("  "),
                ],
                help,
            ),
            Some((13 + buf.width()) as u16),
        ),
        Mode::Quit => {
            let warn = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            let mut out = crate::ui::wrap_line(
                &Line::styled(format!(" Unsaved changes in {}.", ed.name), warn),
                width,
            );
            out.extend(rows(vec![Span::raw(" ")], help));
            (out, None)
        }
        Mode::Edit => (rows(vec![Span::raw(" ")], help), None),
    }
}

/// The editor over the whole of `area`: text with line numbers, a status bar and the key rows (or
/// the prompt the editor is in). The bars grow to the rows their text needs, so no hint or path
/// is cut at the edge; a terminal too small to keep a row of text says so instead.
pub fn draw(f: &mut Frame, area: Rect, ed: &Editor) {
    f.render_widget(Clear, area);
    let flag = if ed.modified() { " [modified]" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("Edit {}", ed.name),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(flag, Style::default().fg(Color::Yellow)),
            Span::raw(" "),
        ]));
    let inner = block.inner(area);
    let width = inner.width as usize;

    let (line, col) = ed.cursor();
    let right = format!("Ln {line}, Col {col}  {} lines ", ed.lines.len());
    let left_txt = match ed.note() {
        Some((n, _)) => format!(" {n}"),
        None => format!(" {}{flag}", ed.path.display()),
    };
    let left_style = match ed.note() {
        Some((_, true)) => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some((_, false)) => Style::default().fg(Color::Green),
        None => Style::default().fg(DIM),
    };
    let status = status_rows(Line::styled(left_txt, left_style), &right, width);
    let (bar, cursor) = bar_rows(ed, width);
    let entries = crate::ui::hint_entries(mode_hints(&ed.mode));
    if (inner.height as usize) < 1 + status.len() + bar.len()
        || !crate::ui::hints_fit(&entries, width)
    {
        crate::ui::draw_too_small(f, area);
        return;
    }
    f.render_widget(block, area);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(status.len() as u16),
            Constraint::Length(bar.len() as u16),
        ])
        .split(inner);
    let (text, status_area, keys) = (parts[0], parts[1], parts[2]);
    f.render_widget(Paragraph::new(status), status_area);
    f.render_widget(Paragraph::new(bar), keys);

    let rows = text.height as usize;
    let digits = ed.lines.len().to_string().len().max(2);
    let gutter = digits + 1;
    let cols = (text.width as usize).saturating_sub(gutter);
    ed.view.set((rows, cols));
    ed.follow(rows, cols);
    let (top, left) = (ed.top.get(), ed.left.get());
    let mut out: Vec<Line<'static>> = Vec::new();
    for row in top..(top + rows).min(ed.lines.len()) {
        let num = if row == ed.at.row {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        let mut spans = vec![Span::styled(format!("{:>digits$} ", row + 1), num)];
        spans.extend(ed.render_row(row, left, cols).spans);
        out.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(out), text);

    match cursor {
        Some(x) => f.set_cursor_position(Position::new(
            keys.x + x.min(keys.width.saturating_sub(1)),
            keys.y,
        )),
        None => {
            let x = width_to(ed.line(), ed.at.col).saturating_sub(left) + gutter;
            let y = ed.at.row.saturating_sub(top);
            if (y as u16) < text.height && (x as u16) < text.width {
                f.set_cursor_position(Position::new(text.x + x as u16, text.y + y as u16));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn ed(text: &str) -> Editor {
        Editor::new("t.txt", PathBuf::from("/nonexistent/t.txt"), text, None)
    }
    fn key(e: &mut Editor, code: KeyCode) -> Out {
        e.on_key(KeyEvent::from(code))
    }
    fn with(e: &mut Editor, code: KeyCode, m: KeyModifiers) -> Out {
        e.on_key(KeyEvent::new(code, m))
    }
    fn ctrl(e: &mut Editor, c: char) -> Out {
        with(e, KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn typ(e: &mut Editor, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                key(e, KeyCode::Enter);
            } else {
                key(e, KeyCode::Char(c));
            }
        }
    }
    fn at(e: &Editor) -> (usize, usize) {
        (e.at.row, e.at.col)
    }
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("osh-ed-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn toml_check(t: &str) -> Result<(), String> {
        crate::preload::parse(t).map(|_| ())
    }

    #[test]
    fn text_round_trips_and_keeps_the_final_newline_choice() {
        assert_eq!(ed("a\nb\n").text(), "a\nb\n");
        assert_eq!(ed("a\nb").text(), "a\nb");
        assert_eq!(ed("a\r\nb\r\n").text(), "a\nb\n");
        assert_eq!(ed("").text(), "");
        let mut e = ed("");
        typ(&mut e, "x");
        assert_eq!(e.text(), "x\n", "a new file ends with a newline");
        assert!(e.modified());
    }

    #[test]
    fn typing_and_line_moves() {
        let mut e = ed("hello\n");
        key(&mut e, KeyCode::End);
        typ(&mut e, " world");
        assert_eq!(e.text(), "hello world\n");
        ctrl(&mut e, 'a');
        assert_eq!(at(&e), (0, 0));
        ctrl(&mut e, 'e');
        assert_eq!(at(&e), (0, 11));
        key(&mut e, KeyCode::Left);
        key(&mut e, KeyCode::Left);
        assert_eq!(e.cursor(), (1, 10));
        key(&mut e, KeyCode::Right);
        assert_eq!(e.cursor(), (1, 11));
        // Right at the end of the last line stays; left at the start goes nowhere.
        key(&mut e, KeyCode::End);
        key(&mut e, KeyCode::Right);
        assert_eq!(at(&e), (0, 11));
    }

    #[test]
    fn home_goes_to_the_first_non_blank_then_the_line_start() {
        let mut e = ed("    key = 1\n");
        key(&mut e, KeyCode::End);
        key(&mut e, KeyCode::Home);
        assert_eq!(at(&e), (0, 4));
        key(&mut e, KeyCode::Home);
        assert_eq!(at(&e), (0, 0));
        key(&mut e, KeyCode::Home);
        assert_eq!(at(&e), (0, 4));
    }

    #[test]
    fn up_and_down_keep_the_column_across_short_lines() {
        let mut e = ed("abcdef\nab\nabcdef\n");
        key(&mut e, KeyCode::End);
        key(&mut e, KeyCode::Down);
        assert_eq!(at(&e), (1, 2));
        key(&mut e, KeyCode::Down);
        assert_eq!(at(&e), (2, 6), "the wanted column comes back");
        key(&mut e, KeyCode::Down);
        assert_eq!(at(&e), (2, 6), "the last line stays");
        ctrl(&mut e, 'a');
        key(&mut e, KeyCode::Up);
        key(&mut e, KeyCode::Up);
        assert_eq!(at(&e), (0, 0));
    }

    #[test]
    fn wide_and_combined_characters_move_as_one_column_step() {
        // Each CJK character is two columns wide; e + combining acute is one grapheme.
        let mut e = ed("日本x\ne\u{301}z\n");
        key(&mut e, KeyCode::Right);
        assert_eq!(e.cursor(), (1, 3));
        key(&mut e, KeyCode::Right);
        assert_eq!(e.cursor(), (1, 5));
        key(&mut e, KeyCode::Down);
        // Column 5 on the shorter line: after "éz", which is two columns.
        assert_eq!(e.cursor(), (2, 3));
        key(&mut e, KeyCode::Home);
        key(&mut e, KeyCode::Right);
        assert_eq!(at(&e), (1, 3), "over the whole grapheme");
        key(&mut e, KeyCode::Backspace);
        assert_eq!(e.text(), "日本x\nz\n");
        let mut f = ed("a👨‍👩‍👧b\n");
        key(&mut f, KeyCode::Right);
        key(&mut f, KeyCode::Delete);
        assert_eq!(f.text(), "ab\n", "a ZWJ family is one grapheme");
    }

    #[test]
    fn tabs_in_the_file_count_to_the_next_tab_stop() {
        let mut e = ed("\tx\n");
        key(&mut e, KeyCode::Right);
        assert_eq!(e.cursor(), (1, 5));
        assert_eq!(col_at("\tx", 2), 0);
        assert_eq!(col_at("\tx", 4), 1);
    }

    #[test]
    fn word_moves_skip_punctuation_and_cross_lines() {
        let mut e = ed("hosts = [\"a.b\"]\nnext\n");
        with(&mut e, KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(at(&e), (0, 5));
        with(&mut e, KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(at(&e), (0, 11), "to the end of \"a\"");
        with(&mut e, KeyCode::Char('f'), KeyModifiers::ALT);
        assert_eq!(at(&e), (0, 13));
        with(&mut e, KeyCode::Char('f'), KeyModifiers::ALT);
        assert_eq!(at(&e), (0, 15), "end of line");
        with(&mut e, KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(at(&e), (1, 0), "then the next line");
        with(&mut e, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(at(&e), (0, 15));
        with(&mut e, KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(at(&e), (0, 12));
    }

    #[test]
    fn enter_keeps_the_indent_and_drops_spaces_before_the_cursor() {
        let mut e = ed("hosts = [\n  \"a\",\n");
        key(&mut e, KeyCode::Down);
        key(&mut e, KeyCode::End);
        typ(&mut e, "\n\"b\",");
        assert_eq!(e.text(), "hosts = [\n  \"a\",\n  \"b\",\n");
        let mut f = ed("a   b\n");
        key(&mut f, KeyCode::Right);
        key(&mut f, KeyCode::Enter);
        assert_eq!(f.text(), "a\nb\n");
    }

    #[test]
    fn backspace_and_delete_join_lines_and_step_back_an_indent() {
        let mut e = ed("ab\ncd\n");
        key(&mut e, KeyCode::Down);
        key(&mut e, KeyCode::Backspace);
        assert_eq!(e.text(), "abcd\n");
        assert_eq!(at(&e), (0, 2));
        key(&mut e, KeyCode::End);
        key(&mut e, KeyCode::Delete);
        assert_eq!(e.text(), "abcd\n", "nothing after the last line");
        let mut f = ed("     x\n");
        key(&mut f, KeyCode::Home);
        key(&mut f, KeyCode::Backspace);
        assert_eq!(f.text(), "    x\n", "five spaces step back to four");
        key(&mut f, KeyCode::Backspace);
        assert_eq!(f.text(), "  x\n");
        let mut g = ed("ab\ncd\n");
        key(&mut g, KeyCode::End);
        key(&mut g, KeyCode::Delete);
        assert_eq!(g.text(), "abcd\n");
    }

    #[test]
    fn ctrl_w_and_alt_d_delete_words() {
        let mut e = ed("enabled = true\n");
        key(&mut e, KeyCode::End);
        ctrl(&mut e, 'w');
        assert_eq!(e.text(), "enabled = \n");
        with(&mut e, KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(e.text(), "");
        let mut f = ed("one two\n");
        with(&mut f, KeyCode::Char('d'), KeyModifiers::ALT);
        assert_eq!(f.text(), " two\n");
    }

    #[test]
    fn undo_takes_back_a_run_of_typing_at_once_and_redo_replays_it() {
        let mut e = ed("x\n");
        key(&mut e, KeyCode::End);
        typ(&mut e, "abc");
        key(&mut e, KeyCode::Enter);
        typ(&mut e, "de");
        assert_eq!(e.text(), "xabc\nde\n");
        ctrl(&mut e, 'z');
        assert_eq!(e.text(), "xabc\n\n");
        ctrl(&mut e, 'z');
        assert_eq!(e.text(), "xabc\n");
        ctrl(&mut e, 'z');
        assert_eq!(e.text(), "x\n");
        assert!(!e.modified(), "back to what is on disk");
        ctrl(&mut e, 'z');
        assert_eq!(e.note().unwrap().0, "Nothing to undo");
        ctrl(&mut e, 'y');
        assert_eq!(e.text(), "xabc\n");
        assert_eq!(at(&e), (0, 4));
        ctrl(&mut e, 'y');
        ctrl(&mut e, 'y');
        assert_eq!(e.text(), "xabc\nde\n");
        ctrl(&mut e, 'z');
        typ(&mut e, "q");
        ctrl(&mut e, 'y');
        assert_eq!(
            e.note().unwrap().0,
            "Nothing to redo",
            "an edit clears redo"
        );
    }

    #[test]
    fn moving_ends_a_run_so_the_next_typing_is_its_own_undo_step() {
        let mut e = ed("\n");
        typ(&mut e, "ab");
        key(&mut e, KeyCode::Left);
        typ(&mut e, "X");
        ctrl(&mut e, 'z');
        assert_eq!(e.text(), "ab\n");
    }

    #[test]
    fn shift_moves_select_and_typing_replaces_the_selection() {
        let mut e = ed("hello world\n");
        for _ in 0..5 {
            with(&mut e, KeyCode::Right, KeyModifiers::SHIFT);
        }
        assert_eq!(e.selection().as_deref(), Some("hello"));
        typ(&mut e, "bye");
        assert_eq!(e.text(), "bye world\n");
        ctrl(&mut e, 'z');
        assert_eq!(e.text(), "hello world\n", "replacing is one undo step");
        // A plain Left collapses the selection to its start.
        with(&mut e, KeyCode::End, KeyModifiers::SHIFT);
        key(&mut e, KeyCode::Left);
        assert!(e.selection().is_none());
        // Selection across lines, then delete.
        let mut f = ed("ab\ncd\nef\n");
        key(&mut f, KeyCode::Right);
        with(&mut f, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(f.selection().as_deref(), Some("b\nc"));
        key(&mut f, KeyCode::Delete);
        assert_eq!(f.text(), "ad\nef\n");
        // Esc first clears a selection, then closes.
        with(&mut f, KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(key(&mut f, KeyCode::Esc), Out::Stay);
        assert!(f.selection().is_none());
    }

    #[test]
    fn ctrl_space_marks_and_plain_moves_then_select_and_alt_a_selects_all() {
        let mut e = ed("abc\ndef\n");
        ctrl(&mut e, ' ');
        key(&mut e, KeyCode::Right);
        key(&mut e, KeyCode::Down);
        assert_eq!(e.selection().as_deref(), Some("abc\nd"));
        ctrl(&mut e, 'c');
        assert!(e.selection().is_none(), "copying ends the mark");
        key(&mut e, KeyCode::Right);
        assert!(e.selection().is_none());
        with(&mut e, KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(e.selection().as_deref(), Some("abc\ndef"));
    }

    #[test]
    fn copy_cut_and_paste_a_selection_and_tell_the_caller_what_was_copied() {
        let mut e = ed("one two\n");
        with(
            &mut e,
            KeyCode::Right,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        ctrl(&mut e, 'c');
        assert_eq!(e.take_copied().as_deref(), Some("one"));
        assert_eq!(e.take_copied(), None, "taken once");
        key(&mut e, KeyCode::End);
        ctrl(&mut e, 'v');
        assert_eq!(e.text(), "one twoone\n");
        ctrl(&mut e, 'a');
        with(&mut e, KeyCode::Right, KeyModifiers::SHIFT);
        with(&mut e, KeyCode::Right, KeyModifiers::SHIFT);
        ctrl(&mut e, 'x');
        assert_eq!(e.text(), "e twoone\n");
        assert_eq!(e.take_copied().as_deref(), Some("on"));
        ctrl(&mut e, 'v');
        assert_eq!(e.text(), "one twoone\n");
    }

    #[test]
    fn without_a_selection_copy_and_cut_take_the_whole_line_and_paste_puts_it_above() {
        let mut e = ed("a\nb\nc\n");
        key(&mut e, KeyCode::Down);
        ctrl(&mut e, 'x');
        assert_eq!(e.text(), "a\nc\n");
        assert_eq!(e.take_copied().as_deref(), Some("b\n"));
        key(&mut e, KeyCode::End);
        ctrl(&mut e, 'v');
        assert_eq!(
            e.text(),
            "a\nb\nc\n",
            "above the cursor's line, cursor stays on its text"
        );
        assert_eq!(at(&e), (2, 1));
        ctrl(&mut e, 'c');
        ctrl(&mut e, 'v');
        assert_eq!(e.text(), "a\nb\nc\nc\n");
        let mut f = ed("only\n");
        ctrl(&mut f, 'x');
        assert_eq!(f.text(), "");
        let mut g = ed("x\n");
        ctrl(&mut g, 'v');
        assert_eq!(g.note().unwrap().0, "The clipboard is empty");
    }

    #[test]
    fn ctrl_k_cuts_to_the_line_end_then_the_line_break() {
        let mut e = ed("key = 1\nnext\n");
        key(&mut e, KeyCode::Right);
        ctrl(&mut e, 'k');
        assert_eq!(e.text(), "k\nnext\n");
        assert_eq!(e.take_copied().as_deref(), Some("ey = 1"));
        ctrl(&mut e, 'k');
        assert_eq!(e.text(), "knext\n");
    }

    #[test]
    fn a_terminal_paste_is_one_undo_step_with_line_breaks_normalised() {
        let mut e = ed("[g]\n");
        key(&mut e, KeyCode::End);
        e.paste("\r\nenabled = true\r\nhosts = []");
        assert_eq!(e.text(), "[g]\nenabled = true\nhosts = []\n");
        assert_eq!(at(&e), (2, 10));
        ctrl(&mut e, 'z');
        assert_eq!(e.text(), "[g]\n");
        e.paste("");
        assert!(!e.modified());
    }

    #[test]
    fn tab_indents_to_the_next_level_and_shift_tab_outdents_selected_lines() {
        let mut e = ed("a\nb\nc\n");
        key(&mut e, KeyCode::Tab);
        assert_eq!(e.text(), "  a\nb\nc\n");
        key(&mut e, KeyCode::Char('x'));
        key(&mut e, KeyCode::Tab);
        assert_eq!(e.text(), "  x a\nb\nc\n", "to the next multiple of two");
        let mut f = ed("a\nb\nc\n");
        with(&mut f, KeyCode::Down, KeyModifiers::SHIFT);
        with(&mut f, KeyCode::Down, KeyModifiers::SHIFT);
        key(&mut f, KeyCode::Tab);
        assert_eq!(
            f.text(),
            "  a\n  b\nc\n",
            "the line the selection ends at column 0 stays"
        );
        assert!(f.selection().is_some(), "the selection stays");
        key(&mut f, KeyCode::BackTab);
        assert_eq!(f.text(), "a\nb\nc\n");
        key(&mut f, KeyCode::BackTab);
        assert_eq!(f.text(), "a\nb\nc\n", "nothing left to take off");
    }

    #[test]
    fn find_as_you_type_wraps_and_esc_goes_back() {
        let mut e = ed("alpha\nbeta Alpha\ngamma alpha\n");
        key(&mut e, KeyCode::Down);
        ctrl(&mut e, 'f');
        assert!(matches!(e.mode, Mode::Find { .. }));
        typ(&mut e, "al");
        assert_eq!(at(&e), (1, 7), "first match after the cursor, any case");
        assert_eq!(e.selection().as_deref(), Some("Al"));
        assert_eq!(e.note().unwrap().0, "Match 2 of 3");
        key(&mut e, KeyCode::Down);
        assert_eq!(at(&e), (2, 8));
        key(&mut e, KeyCode::Down);
        assert_eq!(at(&e), (0, 2));
        assert!(e.note().unwrap().0.contains("wrapped"));
        key(&mut e, KeyCode::Up);
        assert_eq!(at(&e), (2, 8));
        typ(&mut e, "zz");
        assert!(e.note().unwrap().1, "no match is an error");
        assert_eq!(at(&e), (1, 0), "back where the find started");
        key(&mut e, KeyCode::Backspace);
        key(&mut e, KeyCode::Backspace);
        key(&mut e, KeyCode::Esc);
        assert_eq!(e.mode, Mode::Edit);
        assert_eq!(at(&e), (1, 0));
        assert!(e.selection().is_none());
    }

    #[test]
    fn a_capital_letter_makes_find_case_sensitive_and_ctrl_n_repeats_after_enter() {
        let mut e = ed("alpha\nAlpha\nalpha Alpha\n");
        ctrl(&mut e, 'f');
        typ(&mut e, "Al");
        assert_eq!(at(&e), (1, 2));
        key(&mut e, KeyCode::Enter);
        assert_eq!(e.mode, Mode::Edit);
        assert_eq!(
            e.selection().as_deref(),
            Some("Al"),
            "the match stays selected"
        );
        ctrl(&mut e, 'n');
        assert_eq!(at(&e), (2, 8));
        ctrl(&mut e, 'p');
        assert_eq!(at(&e), (1, 2));
        key(&mut e, KeyCode::F(3));
        assert_eq!(at(&e), (2, 8));
        let mut f = ed("x\n");
        ctrl(&mut f, 'n');
        assert!(f.note().unwrap().0.starts_with("Nothing to find"));
    }

    #[test]
    fn ctrl_f_starts_with_the_selected_text() {
        let mut e = ed("go.dev go.dev\n");
        with(&mut e, KeyCode::End, KeyModifiers::SHIFT);
        for _ in 0..7 {
            with(&mut e, KeyCode::Left, KeyModifiers::SHIFT);
        }
        ctrl(&mut e, 'f');
        assert_eq!(
            e.mode,
            Mode::Find {
                query: "go.dev".into(),
                from: pos(0, 0)
            }
        );
    }

    #[test]
    fn go_to_line_and_column() {
        let mut e = ed("a\nbcdef\nc\n");
        ctrl(&mut e, 'g');
        typ(&mut e, "2:4");
        assert_eq!(e.mode, Mode::Goto("2:4".into()));
        key(&mut e, KeyCode::Enter);
        assert_eq!(at(&e), (1, 3));
        ctrl(&mut e, 'g');
        typ(&mut e, "99x");
        key(&mut e, KeyCode::Enter);
        assert_eq!(
            at(&e),
            (2, 0),
            "past the end goes to the last line; letters are ignored"
        );
        ctrl(&mut e, 'g');
        key(&mut e, KeyCode::Enter);
        assert!(e.note().unwrap().1);
        ctrl(&mut e, 'g');
        typ(&mut e, "1");
        key(&mut e, KeyCode::Esc);
        assert_eq!(at(&e), (2, 0), "Esc cancels");
    }

    #[test]
    fn page_keys_move_by_the_visible_rows() {
        let text: String = (1..=50).map(|i| format!("{i}\n")).collect();
        let mut e = ed(&text);
        e.view.set((10, 40));
        key(&mut e, KeyCode::PageDown);
        assert_eq!(at(&e).0, 9);
        key(&mut e, KeyCode::PageDown);
        assert_eq!(at(&e).0, 18);
        key(&mut e, KeyCode::PageUp);
        assert_eq!(at(&e).0, 9);
        with(&mut e, KeyCode::End, KeyModifiers::CONTROL);
        assert_eq!(at(&e), (49, 2));
        with(&mut e, KeyCode::Home, KeyModifiers::CONTROL);
        assert_eq!(at(&e), (0, 0));
    }

    #[test]
    fn closing_asks_only_when_there_are_unsaved_changes() {
        let mut e = ed("x\n");
        assert_eq!(key(&mut e, KeyCode::Esc), Out::Quit);
        assert_eq!(ctrl(&mut e, 'q'), Out::Quit);
        typ(&mut e, "y");
        assert_eq!(key(&mut e, KeyCode::Esc), Out::Stay);
        assert_eq!(e.mode, Mode::Quit);
        assert_eq!(key(&mut e, KeyCode::Esc), Out::Stay, "Esc keeps editing");
        assert_eq!(e.mode, Mode::Edit);
        ctrl(&mut e, 'q');
        assert_eq!(key(&mut e, KeyCode::Char('s')), Out::SaveQuit);
        ctrl(&mut e, 'q');
        assert_eq!(key(&mut e, KeyCode::Char('d')), Out::Quit);
        assert_eq!(ctrl(&mut e, 's'), Out::Save);
    }

    #[test]
    fn save_refuses_text_the_check_rejects_and_points_at_the_error() {
        let dir = tmp("bad");
        let path = dir.join("toolchains.toml");
        std::fs::write(&path, "[a]\nenabled = true\n").unwrap();
        let mut e = Editor::open("toolchains.toml", path.clone(), Some(toml_check)).unwrap();
        key(&mut e, KeyCode::Down);
        key(&mut e, KeyCode::End);
        typ(&mut e, "\nhosts = [\"x.example\"\n[b]");
        let err = e.save().unwrap_err();
        assert!(
            err.starts_with("Not saved: TOML parse error at line"),
            "{err}"
        );
        let (note, is_err) = e.note().unwrap();
        assert!(*is_err && !note.contains('\n'), "one line: {note}");
        assert!(
            e.cursor().0 >= 3,
            "the cursor went to the error: {:?}",
            e.cursor()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[a]\nenabled = true\n",
            "the file is untouched"
        );
        assert!(e.modified());
        // Fix it and save.
        ctrl(&mut e, 'g');
        typ(&mut e, "3");
        key(&mut e, KeyCode::Enter);
        key(&mut e, KeyCode::End);
        typ(&mut e, "]");
        e.save().unwrap();
        assert!(!e.modified());
        assert_eq!(e.note().unwrap().0, "Saved toolchains.toml");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved, "[a]\nenabled = true\nhosts = [\"x.example\"]\n[b]\n");
        assert_eq!(crate::preload::parse(&saved).unwrap().len(), 2);
        assert!(
            !dir.join(".toolchains.toml.osh-edit").exists(),
            "no temporary file left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_opens_empty_and_is_created_on_save() {
        let dir = tmp("new");
        let path = dir.join("policy.yaml");
        let mut e = Editor::open("policy.yaml", path.clone(), None).unwrap();
        assert!(e.note().unwrap().0.starts_with("New file"));
        typ(&mut e, "a: 1");
        e.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a: 1\n");
        // A missing folder is made (a new profile in a fresh providers/).
        let mut n = Editor::create(
            "providers/a.yaml",
            dir.join("providers/a.yaml"),
            "a: 1\n",
            None,
        );
        assert!(n.modified(), "a new file counts as unsaved");
        n.save().unwrap();
        assert!(dir.join("providers/a.yaml").is_file() && n.saves == 1);
        // A folder that cannot be made (a file is in the way): the error is reported, not a panic.
        std::fs::write(dir.join("file"), "").unwrap();
        let mut g = Editor::new("x", dir.join("file/x"), "", None);
        typ(&mut g, "x");
        assert!(g.save().unwrap_err().starts_with("Could not save x"));
        assert!(g.modified());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_name_their_line_and_column() {
        assert_eq!(
            error_at("TOML parse error at line 3, column 5\n |"),
            Some((3, 5))
        );
        assert_eq!(
            error_at("did not find key at line 7 column 2"),
            Some((7, 2))
        );
        assert_eq!(error_at("bad line 4"), Some((4, 1)));
        assert_eq!(error_at("no position"), None);
        let e = toml_check("[a\n").unwrap_err();
        let s = error_summary(&e);
        assert!(s.starts_with("TOML parse error at line 1"), "{s}");
        assert!(!s.contains('\n'));
        assert_eq!(error_summary("one line"), "one line");
    }

    fn render(e: &Editor, w: u16, h: u16) -> (Vec<String>, ratatui::buffer::Buffer) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, f.area(), e)).unwrap();
        let buf = term.backend().buffer().clone();
        let rows = (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect();
        (rows, buf)
    }

    /// The row that contains `needle`.
    fn row_of(rows: &[String], needle: &str) -> usize {
        rows.iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not drawn:\n{}", rows.join("\n")))
    }

    #[test]
    fn draw_shows_line_numbers_text_status_and_keys() {
        let mut e = ed("[github]\nenabled = true\n");
        let (rows, _) = render(&e, 80, 10);
        assert!(rows[0].contains("Edit t.txt"), "{}", rows[0]);
        assert!(rows[1].contains(" 1 [github]"), "{}", rows[1]);
        assert!(rows[2].contains(" 2 enabled = true"), "{}", rows[2]);
        let status = row_of(&rows, "Ln 1, Col 1  2 lines");
        assert!(row_of(&rows, "Ctrl-S save") > status);
        typ(&mut e, "x");
        let (rows, _) = render(&e, 80, 10);
        assert!(rows[0].contains("[modified]"), "{}", rows[0]);
        row_of(&rows, "Ln 1, Col 2");
    }

    #[test]
    fn every_key_hint_is_whole_at_every_common_size() {
        // The user's report: at 80 columns the key line ended in a cut `Es`.
        let sizes = [60u16, 80, 100, 120, 160]
            .into_iter()
            .flat_map(|w| [20u16, 24, 40].map(move |h| (w, h)));
        for (w, h) in sizes {
            let mut e = ed("a\n");
            let (rows, _) = render(&e, w, h);
            assert!(
                !rows.join(" ").contains("too small"),
                "{w}x{h} is a usual size"
            );
            crate::ui::assert_hints_whole(&rows, &crate::ui::hint_entries(HINTS));
            row_of(&rows, "Esc close");
            ctrl(&mut e, 'f');
            let (rows, _) = render(&e, w, h);
            crate::ui::assert_hints_whole(&rows, &crate::ui::hint_entries(FIND_HINTS));
            key(&mut e, KeyCode::Esc);
            ctrl(&mut e, 'g');
            let (rows, _) = render(&e, w, h);
            crate::ui::assert_hints_whole(&rows, &crate::ui::hint_entries(GOTO_HINTS));
            key(&mut e, KeyCode::Esc);
            typ(&mut e, "z");
            key(&mut e, KeyCode::Esc);
            let (rows, _) = render(&e, w, h);
            crate::ui::assert_hints_whole(&rows, &crate::ui::hint_entries(QUIT_HINTS));
        }
        // Too small to show the keys and a row of text: it says so rather than hiding keys.
        let (rows, _) = render(&ed("a\n"), 30, 5);
        let flat = rows
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(flat.contains(crate::ui::TOO_SMALL), "{}", rows.join("\n"));
    }

    #[test]
    fn a_long_path_or_note_wraps_instead_of_being_cut() {
        let mut e = ed("a\n");
        e.path = std::path::PathBuf::from(format!("/{}/end-of-path.toml", "dir/".repeat(30)));
        let (rows, _) = render(&e, 60, 20);
        row_of(&rows, "end-of-path.toml");
        row_of(&rows, "Ln 1, Col 1");
    }

    #[test]
    fn draw_shows_the_prompt_of_each_mode() {
        let mut e = ed("abc\n");
        ctrl(&mut e, 'f');
        typ(&mut e, "b");
        let (rows, _) = render(&e, 80, 6);
        let find = row_of(&rows, "Find: b");
        assert!(
            rows[find - 1].contains("Match 1 of 1"),
            "{}",
            rows[find - 1]
        );
        let (rows, _) = render(&e, 40, 12);
        crate::ui::assert_hints_whole(&rows, &crate::ui::hint_entries(FIND_HINTS));
        key(&mut e, KeyCode::Esc);
        ctrl(&mut e, 'g');
        typ(&mut e, "1");
        let (rows, _) = render(&e, 80, 6);
        row_of(&rows, "Go to line: 1");
        key(&mut e, KeyCode::Esc);
        typ(&mut e, "z");
        key(&mut e, KeyCode::Esc);
        let (rows, _) = render(&e, 80, 8);
        row_of(&rows, "Unsaved changes in t.txt");
        row_of(&rows, "d discard");
        let (rows, _) = render(&e, 40, 12);
        crate::ui::assert_hints_whole(&rows, &crate::ui::hint_entries(QUIT_HINTS));
    }

    #[test]
    fn draw_reverses_the_selection_and_scrolls_to_the_cursor() {
        let mut e = ed("abcdef\n");
        key(&mut e, KeyCode::Right);
        with(&mut e, KeyCode::Right, KeyModifiers::SHIFT);
        with(&mut e, KeyCode::Right, KeyModifiers::SHIFT);
        let (_, buf) = render(&e, 40, 16);
        // Border (1) + gutter of 3: text starts at column 4.
        let rev = |x: u16| buf[(x, 1)].modifier.contains(Modifier::REVERSED);
        assert!(!rev(4) && rev(5) && rev(6) && !rev(7));
        // A long line scrolls sideways so the cursor stays on screen.
        let long = format!("{}END\n", "x".repeat(200));
        let mut f = ed(&long);
        key(&mut f, KeyCode::End);
        let (rows, _) = render(&f, 40, 16);
        assert!(rows[1].contains("END"), "{}", rows[1]);
        row_of(&rows, "Ln 1, Col 204");
        // Many lines scroll down.
        let text: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        let mut g = ed(&text);
        with(&mut g, KeyCode::End, KeyModifiers::CONTROL);
        let (rows, _) = render(&g, 40, 16);
        row_of(&rows, "100 line100");
    }

    #[test]
    fn draw_puts_wide_characters_in_two_cells() {
        let e = ed("日本\n");
        let (rows, _) = render(&e, 30, 20);
        assert!(rows[1].starts_with("│ 1 日"), "{}", rows[1]);
        let line = e.render_row(0, 1, 10);
        let shown: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(
            shown, " 本",
            "a wide char cut by the left edge shows as a space"
        );
    }
}
