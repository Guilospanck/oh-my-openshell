//! Every general osh setting in one registry, resolved as environment variable > `osh.toml` >
//! default. The registry is the single source for `osh config`, the Settings screen, the
//! `osh.toml` template and the README table.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The shape a setting's value must have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    /// Whole seconds in `min..=max`.
    Secs {
        min: u64,
        max: u64,
    },
    Choice(&'static [&'static str]),
    /// Absolute paths. Shape only: whether they exist is checked where they are used.
    PathList,
    /// Environment variable names.
    NameList,
}

/// When a changed value reaches the running program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Live,
    /// After the dashboard restarts.
    Restart,
    /// At the next start of `osh mcp`.
    RestartMcp,
}

#[derive(Debug)]
pub struct Def {
    /// `table.name`; the table is the part before the dot.
    pub key: &'static str,
    pub kind: Kind,
    /// The default as a TOML literal.
    pub default: &'static str,
    pub effect: Effect,
    /// The environment variable that can supersede the file, when there is one.
    pub env: Option<&'static str>,
    pub desc: &'static str,
}

impl Def {
    pub fn table(&self) -> &'static str {
        self.key.split_once('.').map_or(self.key, |(t, _)| t)
    }
    pub fn name(&self) -> &'static str {
        self.key.split_once('.').map_or(self.key, |(_, n)| n)
    }
}

const EDITORS: &[&str] = &["auto", "own", "builtin"];

pub const TMUX_ENV: &str = "OSH_TMUX_PASSTHROUGH";
pub const MCP_SHARE_ENV: &str = "OSH_MCP_ALLOW_SHARE";
pub const MCP_ROOTS_ENV: &str = "OSH_MCP_SHARE_ROOT";
pub const MCP_VARS_ENV: &str = "OSH_MCP_REPO_VARS";

const fn secs(
    key: &'static str,
    min: u64,
    max: u64,
    default: &'static str,
    effect: Effect,
    desc: &'static str,
) -> Def {
    Def {
        key,
        kind: Kind::Secs { min, max },
        default,
        effect,
        env: None,
        desc,
    }
}

static REGISTRY: [Def; 14] = [
    Def {
        key: "general.editor",
        kind: Kind::Choice(EDITORS),
        default: "\"auto\"",
        effect: Effect::Live,
        env: None,
        desc: "Which editor the edit actions use: \"auto\" (yours when found, else the built-in one), \"own\" (always yours), \"builtin\" (always the built-in one).",
    },
    Def {
        key: "shell.tmux_keys",
        kind: Kind::Bool,
        default: "true",
        effect: Effect::Live,
        env: Some(TMUX_ENV),
        desc: "Pass the host tmux keys into sandbox shells. Setting the environment variable to 0 forces this off.",
    },
    secs("refresh.gateway_secs", 1, 60, "2", Effect::Live, "How often gateway, sandbox and provider state is read."),
    secs("refresh.ports_secs", 1, 60, "3", Effect::Live, "How often the sandbox in view is asked for listening ports."),
    secs("refresh.usage_secs", 1, 300, "5", Effect::Live, "How often the CPU, memory and disk meter reads."),
    secs("refresh.usage_max_backoff_secs", 5, 600, "40", Effect::Live, "Longest wait after meter reads keep failing."),
    secs("refresh.alerts_secs", 1, 60, "4", Effect::Restart, "How often shared folders are scanned for risky or changed files."),
    secs("alerts.settle_secs", 0, 3600, "60", Effect::Live, "After an allow, repeats of the same denial within this time count as in flight, not a new request."),
    secs("ui.toast_secs", 1, 60, "5", Effect::Live, "How long an alert toast stays on screen."),
    secs("ui.status_secs", 1, 120, "6", Effect::Live, "How long a status message shows over a shell."),
    secs("ui.status_allowed_secs", 1, 300, "20", Effect::Live, "How long an \"Allowed ...\" message shows (the cue to retry the blocked request)."),
    Def {
        key: "mcp.allow_share",
        kind: Kind::Bool,
        default: "false",
        effect: Effect::RestartMcp,
        env: Some(MCP_SHARE_ENV),
        desc: "Let agents mount host folders into sandboxes through osh mcp. Setting the environment variable to 1 forces this on.",
    },
    Def {
        key: "mcp.share_roots",
        kind: Kind::PathList,
        default: "[]",
        effect: Effect::RestartMcp,
        env: Some(MCP_ROOTS_ENV),
        desc: "Absolute folders agents may share; a share must lie inside one. The environment variable (separated like PATH) replaces this list.",
    },
    Def {
        key: "mcp.repo_vars",
        kind: Kind::NameList,
        default: "[]",
        effect: Effect::RestartMcp,
        env: Some(MCP_VARS_ENV),
        desc: "Environment variable names agents may name as a git credential. The environment variable (comma-separated) replaces this list.",
    },
];

pub fn registry() -> &'static [Def] {
    &REGISTRY
}

pub fn def(key: &str) -> Option<&'static Def> {
    REGISTRY.iter().find(|d| d.key == key)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Int(u64),
    Str(String),
    List(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    Env,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub def: &'static Def,
    pub value: Value,
    pub source: Source,
    /// The environment variable is currently the one deciding the value.
    pub env_active: bool,
    /// The file holds a valid value for this key.
    pub in_file: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub key: Option<String>,
    pub line: Option<usize>,
    pub col: Option<usize>,
    pub msg: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    pub problems: Vec<Problem>,
    /// False when `osh.toml` exists but is not valid TOML.
    pub file_valid: bool,
    pub data_dir: PathBuf,
}

pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

pub const FILE: &str = "osh.toml";

/// Whole-seconds ceiling for the meter's backoff, never below the meter's own interval.
pub fn effective_backoff(usage: u64, max: u64) -> u64 {
    max.max(usage)
}

fn valid_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn strings(v: &toml::Value) -> Result<Vec<String>, String> {
    let a = v.as_array().ok_or("expected a list of text values")?;
    a.iter()
        .map(|x| {
            x.as_str()
                .map(String::from)
                .ok_or_else(|| "expected a list of text values".to_string())
        })
        .collect()
}

/// Check a TOML value against the setting's shape.
pub(crate) fn from_toml(d: &Def, v: &toml::Value) -> Result<Value, String> {
    match d.kind {
        Kind::Bool => v
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| "expected true or false".into()),
        Kind::Secs { min, max } => match v.as_integer() {
            Some(n) if n >= min as i64 && n <= max as i64 => Ok(Value::Int(n as u64)),
            Some(n) => Err(format!("{n} is outside {min} to {max}")),
            None => Err("expected a whole number of seconds".into()),
        },
        Kind::Choice(c) => match v.as_str() {
            Some(s) if c.contains(&s) => Ok(Value::Str(s.into())),
            _ => Err(format!("expected one of: {}", c.join(", "))),
        },
        Kind::PathList => {
            let l = strings(v)?;
            if let Some(bad) = l.iter().find(|p| !Path::new(p).is_absolute()) {
                return Err(format!("{bad:?} is not an absolute path"));
            }
            Ok(Value::List(l))
        }
        Kind::NameList => {
            let l = strings(v)?;
            if let Some(bad) = l.iter().find(|n| !valid_name(n)) {
                return Err(format!("{bad:?} is not a valid variable name"));
            }
            Ok(Value::List(l))
        }
    }
}

/// Parse text typed by a person (`osh config set`, the Settings screen); lists are comma-separated.
pub fn parse_value(d: &Def, text: &str) -> Result<Value, String> {
    let t = text.trim();
    let v = match d.kind {
        Kind::Bool => match t {
            "true" => toml::Value::Boolean(true),
            "false" => toml::Value::Boolean(false),
            _ => return Err("expected true or false".into()),
        },
        Kind::Secs { .. } => toml::Value::Integer(
            t.parse::<i64>()
                .map_err(|_| format!("{t:?} is not a whole number of seconds"))?,
        ),
        Kind::Choice(_) => toml::Value::String(t.into()),
        Kind::PathList | Kind::NameList => toml::Value::Array(
            t.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(|x| toml::Value::String(x.into()))
                .collect(),
        ),
    };
    from_toml(d, &v)
}

fn default_value(d: &Def) -> Value {
    let t: toml::Table =
        toml::from_str(&format!("v = {}", d.default)).expect("registry default is TOML");
    from_toml(d, &t["v"]).expect("registry default fits its own shape")
}

/// What the environment variable decides for this setting, if it decides anything now.
fn env_value(d: &Def, env: Env, problems: &mut Vec<Problem>) -> Option<Value> {
    let var = d.env?;
    let raw = env(var)?;
    match d.key {
        "shell.tmux_keys" => (raw == "0").then_some(Value::Bool(false)),
        "mcp.allow_share" => (raw == "1").then_some(Value::Bool(true)),
        "mcp.share_roots" => {
            let mut out = Vec::new();
            for p in std::env::split_paths(&raw).filter(|p| !p.as_os_str().is_empty()) {
                if p.is_absolute() {
                    out.push(p.to_string_lossy().into_owned());
                } else {
                    problems.push(Problem {
                        key: Some(d.key.into()),
                        line: None,
                        col: None,
                        msg: format!(
                            "{var} entry {} is not an absolute path; ignored",
                            p.display()
                        ),
                    });
                }
            }
            Some(Value::List(out))
        }
        "mcp.repo_vars" => {
            let mut out = Vec::new();
            for n in raw
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
            {
                if valid_name(n) {
                    out.push(n.to_string());
                } else {
                    problems.push(Problem {
                        key: Some(d.key.into()),
                        line: None,
                        col: None,
                        msg: format!("{var} entry {n:?} is not a valid variable name; ignored"),
                    });
                }
            }
            Some(Value::List(out))
        }
        _ => None,
    }
}

/// 1-based line of `name` inside `[table]` in `text` (best effort).
fn find_line(text: &str, table: &str, name: &str) -> Option<usize> {
    let mut cur = String::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            if let Some(end) = t.find(']') {
                cur = t[1..end].trim().trim_matches('[').trim().to_string();
            }
        } else if cur == table {
            if let Some(rest) = t.strip_prefix(name) {
                if rest.trim_start().starts_with('=') {
                    return Some(i + 1);
                }
            }
        }
    }
    None
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

/// Resolve every setting: environment variable, then `osh.toml` in `dir`, then the default.
/// Never writes; a missing file means all defaults, an invalid one too (with the parse error).
pub fn load(dir: &Path, env: Env) -> Snapshot {
    let data_dir = crate::assets::resolve(env("OSH_DIR"), env("XDG_DATA_HOME"), env("HOME"));
    let mut problems = Vec::new();
    let mut file_valid = true;
    let mut table = toml::Table::new();
    let mut text = String::new();
    match std::fs::read_to_string(dir.join(FILE)) {
        Ok(t) => {
            text = t;
            match toml::from_str::<toml::Table>(&text) {
                Ok(tb) => table = tb,
                Err(e) => {
                    file_valid = false;
                    let (line, col) = e.span().map_or((None, None), |r| {
                        let (l, c) = line_col(&text, r.start);
                        (Some(l), Some(c))
                    });
                    problems.push(Problem {
                        key: None,
                        line,
                        col,
                        msg: e.message().to_string(),
                    });
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            file_valid = false;
            problems.push(Problem {
                key: None,
                line: None,
                col: None,
                msg: format!("cannot read {FILE}: {e}"),
            });
        }
    }
    for (t, v) in &table {
        match v.as_table() {
            Some(sub) => {
                for k in sub.keys().filter(|k| def(&format!("{t}.{k}")).is_none()) {
                    problems.push(Problem {
                        key: Some(format!("{t}.{k}")),
                        line: find_line(&text, t, k),
                        col: None,
                        msg: format!("unknown setting {t}.{k}"),
                    });
                }
            }
            None => problems.push(Problem {
                key: Some(t.clone()),
                line: find_line(&text, "", t),
                col: None,
                msg: format!("unknown setting {t}"),
            }),
        }
    }
    let mut rows = Vec::new();
    for d in registry() {
        let from_file = table
            .get(d.table())
            .and_then(|t| t.as_table())
            .and_then(|t| t.get(d.name()))
            .and_then(|v| match from_toml(d, v) {
                Ok(val) => Some(val),
                Err(msg) => {
                    problems.push(Problem {
                        key: Some(d.key.into()),
                        line: find_line(&text, d.table(), d.name()),
                        col: None,
                        msg,
                    });
                    None
                }
            });
        let in_file = from_file.is_some();
        let (value, source, env_active) = match env_value(d, env, &mut problems) {
            Some(v) => (v, Source::Env, true),
            None => match from_file {
                Some(v) => (v, Source::File, false),
                None => (default_value(d), Source::Default, false),
            },
        };
        rows.push(Row {
            def: d,
            value,
            source,
            env_active,
            in_file,
        });
    }
    problems.sort_by_key(|p| p.line.unwrap_or(usize::MAX));
    Snapshot {
        rows,
        problems,
        file_valid,
        data_dir,
    }
}

const HEADER: &str = "# osh settings. Every setting is listed below with its default, commented out.\n# Remove the leading # of a line to change it, or use `osh config set` or the Settings\n# screen in the dashboard; both keep your comments.\n";

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        if !cur.is_empty() && cur.len() + 1 + w.len() > width {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn block(d: &Def) -> String {
    let mut o = String::new();
    for l in wrap(d.desc, 88) {
        o.push_str(&format!("# {l}\n"));
    }
    if let Some(v) = d.env {
        o.push_str(&format!("# Overridden by the environment variable {v}.\n"));
    }
    o.push_str(&format!("# {} = {}\n", d.name(), d.default));
    o
}

/// The documented `osh.toml`: every setting as a comment block with its key commented out at the
/// default.
pub fn template() -> String {
    let mut tables: Vec<&str> = Vec::new();
    for d in registry() {
        if !tables.contains(&d.table()) {
            tables.push(d.table());
        }
    }
    let mut out = String::from(HEADER);
    for t in tables {
        out.push_str(&format!("\n[{t}]\n"));
        let blocks: Vec<String> = registry()
            .iter()
            .filter(|d| d.table() == t)
            .map(block)
            .collect();
        out.push_str(&blocks.join("\n"));
    }
    out
}

fn header_of(line: &str) -> Option<String> {
    let t = line.trim();
    (t.starts_with('[') && !t.starts_with("[["))
        .then(|| t.find(']').map(|e| t[1..e].trim().to_string()))
        .flatten()
}

/// Header line index and the end (exclusive) of `[table]`.
fn section(lines: &[String], table: &str) -> Option<(usize, usize)> {
    let h = lines
        .iter()
        .position(|l| header_of(l).as_deref() == Some(table))?;
    let end = lines[h + 1..]
        .iter()
        .position(|l| l.trim().starts_with('['))
        .map_or(lines.len(), |p| h + 1 + p);
    Some((h, end))
}

fn key_line(line: &str, name: &str) -> bool {
    line.trim()
        .strip_prefix(name)
        .is_some_and(|r| r.trim_start().starts_with('='))
}

fn live_idx(lines: &[String], table: &str, name: &str) -> Option<usize> {
    let (h, e) = section(lines, table)?;
    (h + 1..e).find(|&i| key_line(&lines[i], name))
}

fn commented_idx(lines: &[String], table: &str, name: &str) -> Option<usize> {
    let (h, e) = section(lines, table)?;
    (h + 1..e).find(|&i| {
        lines[i]
            .trim()
            .strip_prefix('#')
            .is_some_and(|r| key_line(r.trim_start(), name))
    })
}

fn split_lines(text: &str) -> Vec<String> {
    text.lines().map(String::from).collect()
}

fn join_lines(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

fn read_text(dir: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(dir.join(FILE)) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {FILE}: {e}")),
    }
}

/// Temp file in the same folder, then rename, so a reader never sees half a file.
fn write_atomic(dir: &Path, text: &str) -> Result<(), String> {
    let tmp = dir.join(format!(".{FILE}.tmp-{}", std::process::id()));
    let r = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, dir.join(FILE)));
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {FILE}: {e}"));
    }
    Ok(())
}

fn parse_doc(text: &str) -> Result<toml_edit::DocumentMut, String> {
    text.parse::<toml_edit::DocumentMut>().map_err(|e| {
        let at = e.span().map_or(String::new(), |r| {
            let (l, c) = line_col(text, r.start);
            format!(" at line {l}, column {c}")
        });
        format!("{FILE} is not valid TOML{at}: {}", e.message())
    })
}

fn create_if_missing(dir: &Path) -> Result<(), String> {
    if dir.join(FILE).exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    write_atomic(dir, &template())
}

fn literal(v: &Value) -> toml_edit::Value {
    match v {
        Value::Bool(b) => (*b).into(),
        Value::Int(n) => (*n as i64).into(),
        Value::Str(s) => s.as_str().into(),
        Value::List(l) => l
            .iter()
            .map(String::as_str)
            .collect::<toml_edit::Array>()
            .into(),
    }
}

fn literal_text(v: &Value) -> String {
    literal(v).to_string().trim().to_string()
}

/// Create `osh.toml` when missing, migrate the two older files, and append blocks for settings
/// added since. Never changes a value; does nothing to an invalid file.
pub fn ensure_file(dir: &Path) -> Result<(), String> {
    create_if_missing(dir)?;
    let Some(text) = read_text(dir)? else {
        return Ok(());
    };
    if parse_doc(&text).is_err() {
        return Ok(());
    }
    let mut lines = split_lines(&text);
    let mut changed = false;
    for d in registry() {
        if live_idx(&lines, d.table(), d.name()).is_some()
            || commented_idx(&lines, d.table(), d.name()).is_some()
        {
            continue;
        }
        let blk = split_lines(&block(d));
        match section(&lines, d.table()) {
            Some((_, end)) => {
                let mut at = end;
                while at > 0 && lines[at - 1].trim().is_empty() {
                    at -= 1;
                }
                let mut ins = vec![String::new()];
                ins.extend(blk);
                lines.splice(at..at, ins);
            }
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("[{}]", d.table()));
                lines.extend(blk);
            }
        }
        changed = true;
    }
    if changed {
        write_atomic(dir, &join_lines(&lines))?;
    }
    migrate(dir)
}

/// Write `key` into `osh.toml`, keeping everything else. Refused for unknown keys, bad values,
/// a key an environment variable decides right now, and an invalid file.
pub fn set(dir: &Path, key: &str, text: &str, env: Env) -> Result<(), String> {
    let d = def(key).ok_or_else(|| format!("unknown setting {key}"))?;
    let v = parse_value(d, text)?;
    if let Some(r) = load(dir, env)
        .rows
        .iter()
        .find(|r| r.def.key == key && r.env_active)
    {
        return Err(format!(
            "{key} is set by the environment variable {} right now; unset it to change the setting here",
            r.def.env.unwrap_or("")
        ));
    }
    create_if_missing(dir)?;
    let old = read_text(dir)?.unwrap_or_default();
    let mut doc = parse_doc(&old)?;
    let mut lines = split_lines(&old);
    let out = if live_idx(&lines, d.table(), d.name()).is_none() {
        if let Some(i) = commented_idx(&lines, d.table(), d.name()) {
            lines[i] = format!("{} = {}", d.name(), literal_text(&v));
            let mut s = join_lines(&lines);
            if !old.ends_with('\n') {
                s.pop();
            }
            s
        } else {
            insert(&mut doc, d, &v)?;
            doc.to_string()
        }
    } else {
        insert(&mut doc, d, &v)?;
        doc.to_string()
    };
    parse_doc(&out)?;
    write_atomic(dir, &out)
}

fn insert(doc: &mut toml_edit::DocumentMut, d: &Def, v: &Value) -> Result<(), String> {
    let mut t = toml_edit::Table::new();
    t.set_implicit(false);
    let tbl = doc
        .as_table_mut()
        .entry(d.table())
        .or_insert(toml_edit::Item::Table(t))
        .as_table_mut()
        .ok_or_else(|| format!("[{}] in {FILE} is not a table", d.table()))?;
    let mut nv = literal(v);
    if let Some(toml_edit::Item::Value(old)) = tbl.get_mut(d.name()) {
        *nv.decor_mut() = old.decor().clone();
        *old = nv;
    } else {
        tbl.insert(d.name(), toml_edit::Item::Value(nv));
    }
    Ok(())
}

/// Put `key` back to its default by commenting its line out. A key not in the file is fine.
pub fn unset(dir: &Path, key: &str) -> Result<(), String> {
    let d = def(key).ok_or_else(|| format!("unknown setting {key}"))?;
    let Some(old) = read_text(dir)? else {
        return Ok(());
    };
    parse_doc(&old)?;
    let mut lines = split_lines(&old);
    let Some(i) = live_idx(&lines, d.table(), d.name()) else {
        return Ok(());
    };
    // A value written over several lines (a list) runs until its brackets balance.
    let mut end = i;
    let mut depth = 0i32;
    loop {
        let l = &lines[end];
        let from = if end == i {
            l.find('=').map_or(0, |p| p + 1)
        } else {
            0
        };
        depth += l[from..].matches('[').count() as i32 - l[from..].matches(']').count() as i32;
        if depth <= 0 || end + 1 >= lines.len() {
            break;
        }
        end += 1;
    }
    lines.splice(i..=end, [format!("# {} = {}", d.name(), d.default)]);
    let mut out = join_lines(&lines);
    if !old.ends_with('\n') {
        out.pop();
    }
    parse_doc(&out)?;
    write_atomic(dir, &out)
}

/// Move `editor-mode` and `.tmux-passthrough-off` into `osh.toml`, then delete them. A value
/// already in `osh.toml` wins; the old file is still removed. Nothing is deleted unless the
/// write succeeded.
pub fn migrate(dir: &Path) -> Result<(), String> {
    let em = dir.join("editor-mode");
    let tm = dir.join(".tmux-passthrough-off");
    if !em.exists() && !tm.exists() {
        return Ok(());
    }
    create_if_missing(dir)?;
    let none = |_: &str| None;
    let snap = load(dir, &none);
    if !snap.file_valid {
        return Err(format!("{FILE} is not valid TOML; the old files stay"));
    }
    let in_file = |k: &str| snap.rows.iter().any(|r| r.def.key == k && r.in_file);
    if em.exists() {
        let v = match std::fs::read_to_string(&em).unwrap_or_default().trim() {
            "external" => Some("own"),
            "builtin" => Some("builtin"),
            _ => None,
        };
        if let (Some(v), false) = (v, in_file("general.editor")) {
            set(dir, "general.editor", v, &none)?;
        }
        std::fs::remove_file(&em).map_err(|e| format!("cannot remove editor-mode: {e}"))?;
    }
    if tm.exists() {
        if !in_file("shell.tmux_keys") {
            set(dir, "shell.tmux_keys", "false", &none)?;
        }
        std::fs::remove_file(&tm)
            .map_err(|e| format!("cannot remove .tmux-passthrough-off: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "osh-settings-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn row<'a>(s: &'a Snapshot, key: &str) -> &'a Row {
        s.rows.iter().find(|r| r.def.key == key).unwrap()
    }

    fn with_file(text: &str) -> Snapshot {
        let d = tmp();
        std::fs::write(d.join(FILE), text).unwrap();
        load(&d, &no_env)
    }

    #[test]
    fn defaults_when_no_file() {
        let s = load(&tmp(), &no_env);
        assert_eq!(s.rows.len(), registry().len());
        assert!(s.rows.iter().all(|r| r.source == Source::Default));
        assert!(s.file_valid && s.problems.is_empty());
        assert_eq!(row(&s, "general.editor").value, Value::Str("auto".into()));
        assert_eq!(row(&s, "shell.tmux_keys").value, Value::Bool(true));
        assert_eq!(row(&s, "refresh.gateway_secs").value, Value::Int(2));
        assert_eq!(
            row(&s, "refresh.usage_max_backoff_secs").value,
            Value::Int(40)
        );
        assert_eq!(row(&s, "mcp.allow_share").value, Value::Bool(false));
        assert_eq!(row(&s, "mcp.share_roots").value, Value::List(vec![]));
    }

    #[test]
    fn file_value_wins_over_default() {
        let s = with_file("[refresh]\nports_secs = 9\n[general]\neditor = \"own\"\n");
        let r = row(&s, "refresh.ports_secs");
        assert_eq!(
            (r.value.clone(), r.source, r.in_file),
            (Value::Int(9), Source::File, true)
        );
        assert_eq!(row(&s, "general.editor").value, Value::Str("own".into()));
        assert!(s.problems.is_empty());
    }

    #[test]
    fn env_forces_only_one_direction() {
        let d = tmp();
        std::fs::write(
            d.join(FILE),
            "[shell]\ntmux_keys = true\n[mcp]\nallow_share = false\n",
        )
        .unwrap();
        let with = |tmux: Option<&'static str>, share: Option<&'static str>| {
            let env = move |k: &str| match k {
                TMUX_ENV => tmux.map(OsString::from),
                MCP_SHARE_ENV => share.map(OsString::from),
                _ => None,
            };
            load(&d, &env)
        };
        let s = with(Some("0"), Some("1"));
        let t = row(&s, "shell.tmux_keys");
        assert_eq!(
            (t.value.clone(), t.source, t.env_active),
            (Value::Bool(false), Source::Env, true)
        );
        let m = row(&s, "mcp.allow_share");
        assert_eq!(
            (m.value.clone(), m.source, m.env_active),
            (Value::Bool(true), Source::Env, true)
        );
        for t in [Some("1"), Some("x"), None] {
            let r = with(t, None);
            let r = row(&r, "shell.tmux_keys");
            assert_eq!(
                (r.value.clone(), r.source, r.env_active),
                (Value::Bool(true), Source::File, false)
            );
        }
        for v in [Some("0"), Some("true"), Some(""), None] {
            let r = with(None, v);
            let r = row(&r, "mcp.allow_share");
            assert_eq!(
                (r.value.clone(), r.env_active),
                (Value::Bool(false), false),
                "{v:?}"
            );
        }
    }

    #[test]
    fn list_env_replaces_file_list() {
        let d = tmp();
        std::fs::write(
            d.join(FILE),
            "[mcp]\nshare_roots = [\"/from/file\"]\nrepo_vars = [\"FILE_VAR\"]\n",
        )
        .unwrap();
        let env = |k: &str| match k {
            MCP_ROOTS_ENV => Some(OsString::from("/a/b::/c/d:")),
            MCP_VARS_ENV => Some(OsString::from(" ONE, ,TWO ")),
            _ => None,
        };
        let s = load(&d, &env);
        let r = row(&s, "mcp.share_roots");
        assert_eq!(r.value, Value::List(vec!["/a/b".into(), "/c/d".into()]));
        assert_eq!((r.source, r.env_active), (Source::Env, true));
        assert_eq!(
            row(&s, "mcp.repo_vars").value,
            Value::List(vec!["ONE".into(), "TWO".into()])
        );
        // An environment variable set to nothing still replaces the list.
        let empty = |k: &str| (k == MCP_ROOTS_ENV).then(|| OsString::from(""));
        let s = load(&d, &empty);
        assert_eq!(row(&s, "mcp.share_roots").value, Value::List(vec![]));
        assert_eq!(
            row(&s, "mcp.repo_vars").value,
            Value::List(vec!["FILE_VAR".into()])
        );
    }

    #[test]
    fn out_of_range_and_wrong_type_are_problems_not_panics() {
        let s = with_file(
            "[refresh]\nports_secs = 0\ngateway_secs = \"x\"\n[mcp]\nshare_roots = [\"rel/path\"]\nrepo_vars = [\"bad name\"]\n",
        );
        for (key, line) in [
            ("refresh.ports_secs", 2),
            ("refresh.gateway_secs", 3),
            ("mcp.share_roots", 5),
            ("mcp.repo_vars", 6),
        ] {
            let p = s
                .problems
                .iter()
                .find(|p| p.key.as_deref() == Some(key))
                .unwrap_or_else(|| panic!("{key}"));
            assert_eq!(p.line, Some(line), "{key}");
            assert!(!row(&s, key).in_file);
        }
        assert_eq!(row(&s, "refresh.ports_secs").value, Value::Int(3));
        assert_eq!(row(&s, "refresh.ports_secs").source, Source::Default);
        assert!(s.file_valid);
    }

    #[test]
    fn unknown_key_is_listed_with_line() {
        let s = with_file("[refresh]\nports_secs = 4\nbogus = 1\n[nope]\nx = 1\n");
        assert_eq!(row(&s, "refresh.ports_secs").value, Value::Int(4));
        let p: Vec<_> = s
            .problems
            .iter()
            .filter(|p| p.msg.contains("unknown"))
            .collect();
        assert_eq!(p.len(), 2, "{:?}", s.problems);
        assert_eq!(p[0].line, Some(3));
        assert_eq!(p[1].line, Some(5));
    }

    #[test]
    fn invalid_toml_gives_line_and_col_and_file_valid_false() {
        let s = with_file("[refresh]\nports_secs = = 4\n");
        assert!(!s.file_valid);
        assert!(s.rows.iter().all(|r| r.source == Source::Default));
        let p = &s.problems[0];
        assert_eq!(p.line, Some(2));
        assert!(p.col.is_some());
    }

    #[test]
    fn registry_keys_unique_and_defaults_parse() {
        let mut keys: Vec<_> = registry().iter().map(|d| d.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), registry().len());
        for d in registry() {
            let t: toml::Table = toml::from_str(&format!("v = {}", d.default))
                .unwrap_or_else(|e| panic!("{}: {e}", d.key));
            assert!(super::from_toml(d, &t["v"]).is_ok(), "{}", d.key);
        }
    }

    #[test]
    fn parse_value_checks_shape_and_range() {
        let d = def("refresh.ports_secs").unwrap();
        assert_eq!(parse_value(d, " 7 "), Ok(Value::Int(7)));
        assert!(parse_value(d, "0").unwrap_err().contains('1'));
        assert!(parse_value(d, "61").is_err());
        assert!(parse_value(d, "x").is_err());
        let e = def("general.editor").unwrap();
        assert_eq!(parse_value(e, "own"), Ok(Value::Str("own".into())));
        assert!(parse_value(e, "vim").unwrap_err().contains("auto"));
        let b = def("shell.tmux_keys").unwrap();
        assert_eq!(parse_value(b, "false"), Ok(Value::Bool(false)));
        assert!(parse_value(b, "yes").is_err());
        let r = def("mcp.share_roots").unwrap();
        assert_eq!(
            parse_value(r, "/a, /b"),
            Ok(Value::List(vec!["/a".into(), "/b".into()]))
        );
        assert_eq!(parse_value(r, ""), Ok(Value::List(vec![])));
        assert!(parse_value(r, "rel").is_err());
        let v = def("mcp.repo_vars").unwrap();
        assert_eq!(
            parse_value(v, "A_1,B"),
            Ok(Value::List(vec!["A_1".into(), "B".into()]))
        );
        assert!(parse_value(v, "1BAD").is_err());
    }

    #[test]
    fn backoff_never_below_usage_secs() {
        assert_eq!(effective_backoff(5, 40), 40);
        assert_eq!(effective_backoff(100, 40), 100);
    }

    #[test]
    fn data_dir_follows_osh_dir_then_xdg_then_home() {
        let env = |k: &str| (k == "OSH_DIR").then(|| OsString::from("/x/osh-data"));
        assert_eq!(load(&tmp(), &env).data_dir, PathBuf::from("/x/osh-data"));
        let home = |k: &str| (k == "HOME").then(|| OsString::from("/h"));
        assert_eq!(
            load(&tmp(), &home).data_dir,
            PathBuf::from("/h/.local/share/osh")
        );
    }

    fn read(d: &Path) -> String {
        std::fs::read_to_string(d.join(FILE)).unwrap()
    }

    #[test]
    fn template_parses_and_lists_every_key_commented() {
        let t = template();
        let table: toml::Table = toml::from_str(&t).unwrap();
        assert!(
            table.is_empty()
                || table
                    .values()
                    .all(|v| v.as_table().is_some_and(|x| x.is_empty()))
        );
        for d in registry() {
            assert!(
                t.contains(&format!("# {} = {}", d.name(), d.default)),
                "{}",
                d.key
            );
            assert!(t.contains(&format!("[{}]", d.table())));
        }
    }

    #[test]
    fn template_names_env_variables() {
        let t = template();
        for d in registry().iter().filter(|d| d.env.is_some()) {
            assert!(
                t.contains(&format!("environment variable {}", d.env.unwrap())),
                "{}",
                d.key
            );
        }
    }

    #[test]
    fn set_uncomments_in_place_and_keeps_comments_blank_lines_unknown_keys() {
        let d = tmp();
        let mine = "# my own note\n\n[custom]\nthing = 1 # keep\n\n".to_string() + &template();
        std::fs::write(d.join(FILE), &mine).unwrap();
        set(&d, "refresh.ports_secs", "7", &no_env).unwrap();
        let after = read(&d);
        assert_eq!(after, mine.replace("# ports_secs = 3", "ports_secs = 7"));
        assert_eq!(
            load(&d, &no_env)
                .rows
                .iter()
                .find(|r| r.def.key == "refresh.ports_secs")
                .unwrap()
                .value,
            Value::Int(7)
        );
    }

    #[test]
    fn set_replaces_a_live_value_keeping_its_trailing_comment() {
        let d = tmp();
        std::fs::write(d.join(FILE), "[refresh]\nports_secs = 4 # slow link\n").unwrap();
        set(&d, "refresh.ports_secs", "9", &no_env).unwrap();
        assert_eq!(read(&d), "[refresh]\nports_secs = 9 # slow link\n");
    }

    #[test]
    fn set_inserts_under_existing_table_or_creates_table() {
        let d = tmp();
        std::fs::write(d.join(FILE), "[refresh]\n# nothing here\n").unwrap();
        set(&d, "refresh.usage_secs", "10", &no_env).unwrap();
        set(&d, "ui.toast_secs", "8", &no_env).unwrap();
        let s = load(&d, &no_env);
        assert_eq!(
            s.rows
                .iter()
                .find(|r| r.def.key == "refresh.usage_secs")
                .unwrap()
                .value,
            Value::Int(10)
        );
        assert_eq!(
            s.rows
                .iter()
                .find(|r| r.def.key == "ui.toast_secs")
                .unwrap()
                .value,
            Value::Int(8)
        );
        assert!(read(&d).contains("# nothing here"));
    }

    #[test]
    fn set_creates_the_file_when_missing_and_writes_lists_and_text() {
        let d = tmp();
        set(&d, "general.editor", "builtin", &no_env).unwrap();
        set(&d, "mcp.share_roots", "/a,/b", &no_env).unwrap();
        let s = load(&d, &no_env);
        assert_eq!(
            s.rows
                .iter()
                .find(|r| r.def.key == "general.editor")
                .unwrap()
                .value,
            Value::Str("builtin".into())
        );
        assert_eq!(
            s.rows
                .iter()
                .find(|r| r.def.key == "mcp.share_roots")
                .unwrap()
                .value,
            Value::List(vec!["/a".into(), "/b".into()])
        );
        assert!(s.problems.is_empty(), "{:?}", s.problems);
    }

    #[test]
    fn set_refuses_invalid_toml_with_line_col_and_leaves_file_byte_identical() {
        let d = tmp();
        let bad = "[refresh]\nports_secs = = 4\n";
        std::fs::write(d.join(FILE), bad).unwrap();
        let e = set(&d, "refresh.usage_secs", "10", &no_env).unwrap_err();
        assert!(e.contains("line 2"), "{e}");
        assert_eq!(read(&d), bad);
        assert!(unset(&d, "refresh.usage_secs").is_err());
        assert_eq!(read(&d), bad);
    }

    #[test]
    fn set_refuses_env_forced_key_naming_variable() {
        let d = tmp();
        let env = |k: &str| (k == MCP_SHARE_ENV).then(|| OsString::from("1"));
        let e = set(&d, "mcp.allow_share", "false", &env).unwrap_err();
        assert!(e.contains(MCP_SHARE_ENV), "{e}");
        assert!(!d.join(FILE).exists());
        // Not forcing: the same variable set to something else is no obstacle.
        let env = |k: &str| (k == MCP_SHARE_ENV).then(|| OsString::from("0"));
        set(&d, "mcp.allow_share", "true", &env).unwrap();
    }

    #[test]
    fn set_rejects_bad_value_and_unknown_key() {
        let d = tmp();
        assert!(set(&d, "refresh.ports_secs", "0", &no_env)
            .unwrap_err()
            .contains("outside"));
        assert!(set(&d, "nope.nothing", "1", &no_env)
            .unwrap_err()
            .contains("unknown"));
    }

    #[test]
    fn unset_comments_out_only_that_line_and_missing_is_ok() {
        let d = tmp();
        std::fs::write(d.join(FILE), "[refresh]\nports_secs = 7\nusage_secs = 9\n").unwrap();
        unset(&d, "refresh.ports_secs").unwrap();
        assert_eq!(read(&d), "[refresh]\n# ports_secs = 3\nusage_secs = 9\n");
        unset(&d, "refresh.ports_secs").unwrap();
        unset(&d, "ui.toast_secs").unwrap();
        assert_eq!(read(&d), "[refresh]\n# ports_secs = 3\nusage_secs = 9\n");
        let empty = tmp();
        unset(&empty, "ui.toast_secs").unwrap();
    }

    #[test]
    fn append_adds_only_new_commented_blocks_and_never_values() {
        let d = tmp();
        let old = "# mine\n[refresh]\nports_secs = 7\n\n[ui]\ntoast_secs = 8\n";
        std::fs::write(d.join(FILE), old).unwrap();
        ensure_file(&d).unwrap();
        let after = read(&d);
        assert!(after.starts_with("# mine\n[refresh]\nports_secs = 7\n"));
        let s = load(&d, &no_env);
        assert!(s.problems.is_empty(), "{:?}", s.problems);
        let val = |k: &str| s.rows.iter().find(|r| r.def.key == k).unwrap().clone();
        assert_eq!(val("refresh.ports_secs").value, Value::Int(7));
        assert_eq!(val("ui.toast_secs").value, Value::Int(8));
        assert!(after.contains("# usage_secs = 5") && after.contains("# editor = \"auto\""));
        assert_eq!(
            after.matches("# ports_secs").count(),
            0,
            "a live key gets no second block"
        );
        let again = read(&d);
        ensure_file(&d).unwrap();
        assert_eq!(read(&d), again, "a second run changes nothing");
    }

    #[test]
    fn ensure_file_creates_the_template_and_skips_an_invalid_file() {
        let d = tmp();
        ensure_file(&d).unwrap();
        assert_eq!(read(&d), template());
        let bad = tmp();
        std::fs::write(bad.join(FILE), "[x\n").unwrap();
        std::fs::write(bad.join("editor-mode"), "builtin\n").unwrap();
        ensure_file(&bad).unwrap();
        assert_eq!(read(&bad), "[x\n");
        assert!(
            bad.join("editor-mode").exists(),
            "an invalid osh.toml leaves the old files alone"
        );
    }

    #[test]
    fn migrate_editor_mode_and_tmux_marker() {
        let d = tmp();
        std::fs::write(d.join("editor-mode"), "external\n").unwrap();
        std::fs::write(d.join(".tmux-passthrough-off"), "").unwrap();
        ensure_file(&d).unwrap();
        let s = load(&d, &no_env);
        let val = |k: &str| {
            s.rows
                .iter()
                .find(|r| r.def.key == k)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(val("general.editor"), Value::Str("own".into()));
        assert_eq!(val("shell.tmux_keys"), Value::Bool(false));
        assert!(!d.join("editor-mode").exists() && !d.join(".tmux-passthrough-off").exists());
        let b = tmp();
        std::fs::write(b.join("editor-mode"), "builtin\n").unwrap();
        ensure_file(&b).unwrap();
        let s = load(&b, &no_env);
        assert_eq!(
            s.rows
                .iter()
                .find(|r| r.def.key == "general.editor")
                .unwrap()
                .value,
            Value::Str("builtin".into())
        );
    }

    #[test]
    fn migrate_idempotent_and_osh_toml_wins_when_both() {
        let d = tmp();
        std::fs::write(d.join(FILE), "[general]\neditor = \"builtin\"\n").unwrap();
        std::fs::write(d.join("editor-mode"), "external\n").unwrap();
        migrate(&d).unwrap();
        migrate(&d).unwrap();
        let s = load(&d, &no_env);
        assert_eq!(
            s.rows
                .iter()
                .find(|r| r.def.key == "general.editor")
                .unwrap()
                .value,
            Value::Str("builtin".into())
        );
        assert!(!d.join("editor-mode").exists());
    }

    #[test]
    fn migrate_unwritable_dir_keeps_old_files() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        std::fs::write(d.join("editor-mode"), "builtin\n").unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = migrate(&d);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        if r.is_ok() {
            // Running as a user the mode does not bind (root): nothing to prove here.
            return;
        }
        assert!(d.join("editor-mode").exists());
    }
}
