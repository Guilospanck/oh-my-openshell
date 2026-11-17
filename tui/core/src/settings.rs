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
    /// A whole number in `min..=max` that is not a duration (a port).
    Count {
        min: u64,
        max: u64,
    },
    /// Text that must satisfy `TextRule`.
    Text(TextRule),
}

/// What a `Kind::Text` value must look like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRule {
    /// An IPv4 or IPv6 address.
    IpAddress,
    /// Empty, or a gateway name (letters, digits, `.`, `_`, `-`; not starting with `-`).
    GatewayName,
}

/// When a changed value reaches the running program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Live,
    /// After the dashboard restarts.
    Restart,
    /// At the next start of `osh mcp serve`.
    RestartMcp,
    /// On the HTTP server: the next request (it reads `osh.toml` each time). Over stdio: the
    /// next start of `osh mcp`.
    McpRequest,
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

static REGISTRY: [Def; 19] = [
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
        effect: Effect::McpRequest,
        env: Some(MCP_SHARE_ENV),
        desc: "Let agents mount host folders into sandboxes through osh mcp. Setting the environment variable to 1 forces this on.",
    },
    Def {
        key: "mcp.share_roots",
        kind: Kind::PathList,
        default: "[]",
        effect: Effect::McpRequest,
        env: Some(MCP_ROOTS_ENV),
        desc: "Absolute folders agents may share; a share must lie inside one. The environment variable (separated like PATH) replaces this list.",
    },
    Def {
        key: "mcp.repo_vars",
        kind: Kind::NameList,
        default: "[]",
        effect: Effect::McpRequest,
        env: Some(MCP_VARS_ENV),
        desc: "Environment variable names agents may name as a git credential. The environment variable (comma-separated) replaces this list.",
    },
    Def {
        key: "mcp.server_bind",
        kind: Kind::Text(TextRule::IpAddress),
        default: "\"127.0.0.1\"",
        effect: Effect::RestartMcp,
        env: None,
        desc: "The address osh mcp serve listens on. Anything but a loopback address (127.0.0.1, ::1) is reachable by other machines, so it is protected by the token and should sit behind a VPN or tunnel.",
    },
    Def {
        key: "mcp.server_port",
        kind: Kind::Count { min: 1024, max: 65535 },
        default: "17800",
        effect: Effect::RestartMcp,
        env: None,
        desc: "The port osh mcp serve listens on.",
    },
    Def {
        key: "mcp.server_gateway",
        kind: Kind::Text(TextRule::GatewayName),
        default: "\"\"",
        effect: Effect::McpRequest,
        env: None,
        desc: "The gateway the MCP server uses when a call names none. Empty means OpenShell's active gateway.",
    },
    secs("mcp.client_idle_secs", 10, 86400, "600", Effect::Live, "How long a client may stay silent before the MCP server stops listing it as connected."),
    secs("mcp.job_max_secs", 0, 604800, "14400", Effect::McpRequest, "The longest a background job may run before it is ended; 0 means no limit. A caller's max_secs can lower it, never raise it."),
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
        Kind::Count { min, max } => match v.as_integer() {
            Some(n) if n >= min as i64 && n <= max as i64 => Ok(Value::Int(n as u64)),
            Some(n) => Err(format!("{n} is outside {min} to {max}")),
            None => Err("expected a whole number".into()),
        },
        Kind::Text(rule) => {
            let s = v.as_str().ok_or("expected text")?;
            match rule {
                TextRule::IpAddress => s
                    .parse::<std::net::IpAddr>()
                    .map(|_| Value::Str(s.into()))
                    .map_err(|_| format!("{s:?} is not an IP address")),
                TextRule::GatewayName => {
                    let ok = !s.starts_with('-')
                        && s.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
                    if ok {
                        Ok(Value::Str(s.into()))
                    } else {
                        Err(format!("{s:?} is not a gateway name"))
                    }
                }
            }
        }
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

/// Parse text typed by a person (`osh config set`, the Settings screen). A list of paths is
/// separated by `:` like `PATH` (a comma can be part of a path); a list of names by `,`.
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
        Kind::Count { .. } => toml::Value::Integer(
            t.parse::<i64>()
                .map_err(|_| format!("{t:?} is not a whole number"))?,
        ),
        Kind::Choice(_) | Kind::Text(_) => toml::Value::String(t.into()),
        Kind::PathList | Kind::NameList => toml::Value::Array(
            t.split(if d.kind == Kind::PathList { ':' } else { ',' })
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
    build(read_text(dir), env)
}

/// What the editor's save check says about the text of `osh.toml`: invalid TOML and values that
/// do not fit their setting are refused, with line and column. Keys osh does not know are kept
/// (a newer osh may add them), so they do not block a save.
pub fn check(text: &str) -> Result<(), String> {
    let snap = build(Ok(Some(text.to_string())), &|_| None);
    let bad = snap
        .problems
        .iter()
        .find(|p| !p.msg.starts_with("unknown setting"));
    match bad {
        None => Ok(()),
        Some(p) => Err(match (p.line, p.col) {
            (Some(l), Some(c)) => format!("{FILE}:{l}:{c}: {}", p.msg),
            (Some(l), None) => format!("{FILE}:{l}: {}", p.msg),
            _ => format!("{FILE}: {}", p.msg),
        }),
    }
}

fn build(src: Result<Option<String>, String>, env: Env) -> Snapshot {
    let data_dir = crate::assets::resolve(env("OSH_DIR"), env("XDG_DATA_HOME"), env("HOME"));
    let mut problems = Vec::new();
    let mut file_valid = true;
    let mut table = toml::Table::new();
    let mut text = String::new();
    match src {
        Ok(Some(t)) => {
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
        Ok(None) => {}
        Err(msg) => {
            file_valid = false;
            problems.push(Problem {
                key: None,
                line: None,
                col: None,
                msg,
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

/// Join with the line ending `like` uses (CRLF stays CRLF), ending the text with one unless
/// `like` is a non-empty text that did not.
fn join_lines(lines: &[String], like: &str) -> String {
    let eol = if like.contains("\r\n") { "\r\n" } else { "\n" };
    let mut s = lines.join(eol);
    if like.is_empty() || like.ends_with('\n') {
        s.push_str(eol);
    }
    s
}

fn read_text(dir: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(dir.join(FILE)) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {FILE}: {e}")),
    }
}

/// Temp file next to the real file, then rename, so a reader never sees half a file. A symlinked
/// `osh.toml` (dotfiles) stays a link and its target is what changes; the mode is kept.
fn write_atomic(dir: &Path, text: &str) -> Result<(), String> {
    let link = dir.join(FILE);
    let target = std::fs::canonicalize(&link).unwrap_or(link);
    let parent = target.parent().unwrap_or(dir);
    let tmp = parent.join(format!(".{FILE}.tmp-{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        if let Ok(m) = std::fs::metadata(&target) {
            f.set_permissions(m.permissions())?;
        }
        f.sync_all()?;
        std::fs::rename(&tmp, &target)
    };
    if let Err(e) = write() {
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
        let mut next = lines.clone();
        match section(&next, d.table()) {
            Some((_, end)) => {
                let mut at = end;
                while at > 0 && next[at - 1].trim().is_empty() {
                    at -= 1;
                }
                let mut ins = vec![String::new()];
                ins.extend(blk);
                let after = at + ins.len();
                next.splice(at..at, ins);
                // Keep the block apart from a table that follows it directly.
                if after < next.len() && !next[after].trim().is_empty() {
                    next.insert(after, String::new());
                }
            }
            None => {
                if next.last().is_some_and(|l| !l.trim().is_empty()) {
                    next.push(String::new());
                }
                next.push(format!("[{}]", d.table()));
                next.extend(blk);
            }
        }
        // A table the user wrote without a `[table]` header (dotted keys, an inline table, an
        // array of tables) cannot take a header too: skip the block instead of breaking the file.
        if parse_doc(&join_lines(&next, &text)).is_ok() {
            lines = next;
            changed = true;
        }
    }
    if changed {
        write_atomic(dir, &join_lines(&lines, &text))?;
    }
    migrate(dir)
}

/// Write the documented template over `osh.toml` (Data files: restore).
pub fn restore_template(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    write_atomic(dir, &template())
}

/// The reason `key` cannot be changed now: an environment variable decides it.
pub fn locked(dir: &Path, key: &str, env: Env) -> Option<String> {
    load(dir, env)
        .rows
        .iter()
        .find(|r| r.def.key == key && r.env_active)
        .map(|r| {
            format!(
                "{key} is set by the environment variable {} right now; unset it to change the setting",
                r.def.env.unwrap_or("")
            )
        })
}

fn to_toml(v: &Value) -> toml::Value {
    match v {
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Int(n) => toml::Value::Integer(*n as i64),
        Value::Str(s) => toml::Value::String(s.clone()),
        Value::List(l) => toml::Value::Array(l.iter().cloned().map(toml::Value::String).collect()),
    }
}

/// What `old` must parse to after `d` is set to `v` (`None` removes it). A text edit that does
/// not give exactly this touched something else (a string, a comment, another key) and is dropped.
fn expected(old: &toml::Table, d: &Def, v: Option<&Value>) -> toml::Table {
    let mut t = old.clone();
    let sub = t
        .entry(d.table())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(sub) = sub.as_table_mut() {
        match v {
            Some(v) => {
                sub.insert(d.name().into(), to_toml(v));
            }
            None => {
                sub.remove(d.name());
            }
        }
    }
    t
}

fn table_of(text: &str) -> Option<toml::Table> {
    toml::from_str(text).ok()
}

/// Write `key` into `osh.toml`, keeping everything else. Refused for unknown keys, bad values,
/// a key an environment variable decides right now, and an invalid file.
pub fn set(dir: &Path, key: &str, text: &str, env: Env) -> Result<(), String> {
    let d = def(key).ok_or_else(|| format!("unknown setting {key}"))?;
    let v = parse_value(d, text)?;
    if let Some(why) = locked(dir, key, env) {
        return Err(why);
    }
    create_if_missing(dir)?;
    let old = read_text(dir)?.unwrap_or_default();
    let mut doc = parse_doc(&old)?;
    let old_table = table_of(&old).ok_or_else(|| format!("{FILE} is not valid TOML"))?;
    let mut lines = split_lines(&old);
    let mut out = None;
    if live_idx(&lines, d.table(), d.name()).is_none() {
        if let Some(i) = commented_idx(&lines, d.table(), d.name()) {
            lines[i] = format!("{} = {}", d.name(), literal_text(&v));
            let s = join_lines(&lines, &old);
            // The commented line may sit inside a string; then the check below fails and the
            // key is inserted properly instead.
            if table_of(&s) == Some(expected(&old_table, d, Some(&v))) {
                out = Some(s);
            }
        }
    }
    let out = match out {
        Some(s) => s,
        None => {
            insert(&mut doc, d, &v)?;
            doc.to_string()
        }
    };
    if table_of(&out) != Some(expected(&old_table, d, Some(&v))) {
        return Err(format!(
            "{key} is written in a form osh cannot edit safely (a dotted or quoted key, say); edit {FILE} by hand"
        ));
    }
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
    let old_table = table_of(&old).ok_or_else(|| format!("{FILE} is not valid TOML"))?;
    let present = old_table
        .get(d.table())
        .and_then(|t| t.as_table())
        .is_some_and(|t| t.contains_key(d.name()));
    let lines = split_lines(&old);
    let Some(i) = live_idx(&lines, d.table(), d.name()) else {
        if present {
            return Err(format!(
                "{key} is written in a form osh cannot edit safely (a dotted or quoted key, say); edit {FILE} by hand"
            ));
        }
        return Ok(());
    };
    // The value may run over several lines: take the fewest lines whose replacement by the
    // commented default leaves exactly the other settings, strings and comments as they were.
    let want = expected(&old_table, d, None);
    let replacement = format!("# {} = {}", d.name(), d.default);
    let mut done = None;
    for end in i..lines.len() {
        let mut next = lines.clone();
        next.splice(i..=end, [replacement.clone()]);
        let s = join_lines(&next, &old);
        if table_of(&s) == Some(want.clone()) {
            done = Some(s);
            break;
        }
    }
    let out = done.ok_or_else(|| format!("cannot remove {key} safely; edit {FILE} by hand"))?;
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

use std::sync::Arc;
use std::time::Duration;

/// The current settings of this process, with the file's modification time so a running
/// dashboard can notice an edit.
pub struct Store {
    dir: PathBuf,
    mtime: Option<std::time::SystemTime>,
    snap: Arc<Snapshot>,
    /// What this process started with, to tell which edits still wait for a restart.
    start: Arc<Snapshot>,
}

fn mtime_of(dir: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(dir.join(FILE))
        .and_then(|m| m.modified())
        .ok()
}

fn real_env(k: &str) -> Option<OsString> {
    std::env::var_os(k)
}

impl Store {
    pub fn open(dir: &Path, env: Env) -> Store {
        let snap = Arc::new(load(dir, env));
        Store {
            dir: dir.to_path_buf(),
            mtime: mtime_of(dir),
            start: snap.clone(),
            snap,
        }
    }
    /// Re-read `osh.toml` when it changed. An invalid file keeps the last good values and
    /// replaces only the problems. True when anything changed.
    pub fn reload(&mut self, env: Env) -> bool {
        let m = mtime_of(&self.dir);
        if m == self.mtime {
            return false;
        }
        self.mtime = m;
        let fresh = load(&self.dir, env);
        if fresh.file_valid {
            self.snap = Arc::new(fresh);
        } else {
            let mut kept = (*self.snap).clone();
            kept.problems = fresh.problems;
            kept.file_valid = false;
            self.snap = Arc::new(kept);
        }
        true
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.clone()
    }
}

static STORE: std::sync::Mutex<Option<Store>> = std::sync::Mutex::new(None);
static DEFAULTS: std::sync::OnceLock<Arc<Snapshot>> = std::sync::OnceLock::new();

thread_local! {
    static TEST_SNAPSHOT: std::cell::RefCell<Option<Arc<Snapshot>>> = const { std::cell::RefCell::new(None) };
}

/// Load the process-wide settings once, with the real environment. Call at start.
pub fn init(dir: &Path) {
    if let Ok(mut g) = STORE.lock() {
        *g = Some(Store::open(dir, &real_env));
    }
}

/// Re-read `osh.toml` if it changed; cheap (one `stat`). True when the values changed.
pub fn reload_if_changed() -> bool {
    STORE
        .lock()
        .ok()
        .and_then(|mut g| g.as_mut().map(|s| s.reload(&real_env)))
        .unwrap_or(false)
}

/// The process-wide settings. Under a test: the snapshot installed on this thread, else
/// defaults, so a test never reads the real data folder or its neighbours' settings.
pub fn snapshot() -> Arc<Snapshot> {
    if let Some(s) = TEST_SNAPSHOT.with(|t| t.borrow().clone()) {
        return s;
    }
    if !crate::testmode::under_test() {
        if let Some(s) = STORE
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(Store::snapshot))
        {
            return s;
        }
    }
    DEFAULTS
        .get_or_init(|| Arc::new(load(Path::new("/nonexistent-osh-settings"), &|_| None)))
        .clone()
}

/// The settings this process started with (what a setting that needs a restart still runs on).
pub fn started() -> Arc<Snapshot> {
    if !crate::testmode::under_test() {
        if let Some(s) = STORE
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.start.clone()))
        {
            return s;
        }
    }
    snapshot()
}

/// Make `snapshot()` return `snap` on this thread until the guard drops. Tests only.
pub fn install_for_test(snap: Snapshot) -> TestGuard {
    let prev = TEST_SNAPSHOT.with(|t| t.replace(Some(Arc::new(snap))));
    TestGuard(prev)
}

pub struct TestGuard(Option<Arc<Snapshot>>);

impl Drop for TestGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        TEST_SNAPSHOT.with(|t| *t.borrow_mut() = prev);
    }
}

fn value_of(key: &str) -> Value {
    snapshot()
        .rows
        .iter()
        .find(|r| r.def.key == key)
        .map(|r| r.value.clone())
        .unwrap_or_else(|| default_value(def(key).expect("registry key")))
}

fn int(key: &str) -> u64 {
    match value_of(key) {
        Value::Int(n) => n,
        _ => 0,
    }
}
fn flag(key: &str) -> bool {
    matches!(value_of(key), Value::Bool(true))
}
fn text(key: &str) -> String {
    match value_of(key) {
        Value::Str(s) => s,
        _ => String::new(),
    }
}
fn list(key: &str) -> Vec<String> {
    match value_of(key) {
        Value::List(l) => l,
        _ => Vec::new(),
    }
}

pub fn refresh_gateway() -> Duration {
    Duration::from_secs(int("refresh.gateway_secs"))
}
pub fn refresh_ports() -> Duration {
    Duration::from_secs(int("refresh.ports_secs"))
}
pub fn refresh_usage() -> Duration {
    Duration::from_secs(int("refresh.usage_secs"))
}
pub fn usage_max_backoff() -> Duration {
    Duration::from_secs(effective_backoff(
        int("refresh.usage_secs"),
        int("refresh.usage_max_backoff_secs"),
    ))
}
pub fn refresh_alerts() -> Duration {
    Duration::from_secs(int("refresh.alerts_secs"))
}
pub fn alerts_settle() -> Duration {
    Duration::from_secs(int("alerts.settle_secs"))
}
pub fn toast_for() -> Duration {
    Duration::from_secs(int("ui.toast_secs"))
}
pub fn status_for() -> Duration {
    Duration::from_secs(int("ui.status_secs"))
}
pub fn status_allowed_for() -> Duration {
    Duration::from_secs(int("ui.status_allowed_secs"))
}
/// `auto`, `own` or `builtin`.
pub fn editor_pref() -> String {
    text("general.editor")
}
pub fn tmux_keys() -> bool {
    flag("shell.tmux_keys")
}
pub fn mcp_allow_share() -> bool {
    flag("mcp.allow_share")
}
pub fn mcp_share_roots() -> Vec<PathBuf> {
    list("mcp.share_roots")
        .into_iter()
        .map(PathBuf::from)
        .collect()
}
pub fn mcp_repo_vars() -> Vec<String> {
    list("mcp.repo_vars")
}
pub fn mcp_server_bind() -> String {
    text("mcp.server_bind")
}
pub fn mcp_server_port() -> u16 {
    u16::try_from(int("mcp.server_port")).unwrap_or(17800)
}
/// `None` = the CLI's active gateway.
pub fn mcp_server_gateway() -> Option<String> {
    Some(text("mcp.server_gateway")).filter(|g| !g.is_empty())
}
pub fn mcp_job_max_secs() -> u64 {
    int("mcp.job_max_secs")
}
pub fn mcp_client_idle() -> Duration {
    Duration::from_secs(int("mcp.client_idle_secs"))
}

/// A snapshot from `osh.toml` text with no environment, for tests of the code that reads
/// settings. Pair with `install_for_test`.
pub fn test_snapshot(toml_text: &str) -> Snapshot {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "osh-settings-snap-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).expect("temp dir");
    std::fs::write(d.join(FILE), toml_text).expect("write osh.toml");
    let snap = load(&d, &|_| None);
    let _ = std::fs::remove_dir_all(&d);
    snap
}

pub struct Out {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

const CONFIG_USAGE: &str = "\
usage:
  osh config [--json]            every setting: value, source, default, environment variable, description
  osh config get KEY [--json]
  osh config set KEY VALUE [--json]   path lists are separated by : like PATH, name lists by ,
  osh config unset KEY [--json]       back to the default
  osh config path                the osh.toml file
  osh config edit                open osh.toml in your editor";

fn value_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Bool(b) => (*b).into(),
        Value::Int(n) => (*n).into(),
        Value::Str(s) => s.as_str().into(),
        Value::List(l) => l.iter().map(String::as_str).collect::<Vec<_>>().into(),
    }
}

/// The value as a person types and reads it (see `parse_value` for the list separators).
pub fn text_of(d: &Def, v: &Value) -> String {
    match v {
        Value::List(l) => l.join(if d.kind == Kind::PathList { ":" } else { "," }),
        other => value_text(other),
    }
}

pub fn value_text(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Str(s) => s.clone(),
        Value::List(l) => l.join(","),
    }
}

fn source_name(s: Source) -> &'static str {
    match s {
        Source::Default => "default",
        Source::File => "file",
        Source::Env => "env",
    }
}

fn effect_name(e: Effect) -> &'static str {
    match e {
        Effect::Live => "live",
        Effect::Restart => "restart",
        Effect::RestartMcp => "restart of osh mcp",
        Effect::McpRequest => "next request (osh mcp over stdio: restart)",
    }
}

/// One row as the JSON contract shows it (also used by the Settings screen's tests).
pub fn row_json(r: &Row) -> serde_json::Value {
    serde_json::json!({
        "key": r.def.key,
        "value": value_json(&r.value),
        "source": source_name(r.source),
        "default": value_json(&default_value(r.def)),
        "env": r.def.env,
        "env_active": r.env_active,
        "takes_effect": effect_name(r.def.effect),
        "description": r.def.desc,
    })
}

fn problem_json(p: &Problem) -> serde_json::Value {
    serde_json::json!({"key": p.key, "line": p.line, "col": p.col, "message": p.msg})
}

fn table_text(s: &Snapshot) -> String {
    let head = ["KEY", "VALUE", "SOURCE", "DEFAULT", "ENV", "DESCRIPTION"];
    let mut rows: Vec<[String; 6]> = vec![head.map(String::from)];
    for r in &s.rows {
        rows.push([
            r.def.key.into(),
            text_of(r.def, &r.value),
            source_name(r.source).into(),
            text_of(r.def, &default_value(r.def)),
            r.def.env.unwrap_or("").into(),
            r.def.desc.into(),
        ]);
    }
    let w: Vec<usize> = (0..5)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for r in &rows {
        for (c, cell) in r.iter().enumerate().take(5) {
            out.push_str(&format!("{cell:<width$}  ", width = w[c]));
        }
        out.push_str(&r[5]);
        out.push('\n');
    }
    out.push_str(&format!("\nData folder: {}\n", s.data_dir.display()));
    for p in &s.problems {
        let at = match (p.line, p.col) {
            (Some(l), Some(c)) => format!(":{l}:{c}"),
            (Some(l), None) => format!(":{l}"),
            _ => String::new(),
        };
        out.push_str(&format!("problem: {FILE}{at}: {}\n", p.msg));
    }
    out
}

/// `osh config ...`. `edit` opens a path in the user's editor and waits. Only `set`, `unset`,
/// `edit` and `path` may create or extend `osh.toml`.
pub fn run_cli(
    args: &[String],
    dir: &Path,
    env: Env,
    edit: &dyn Fn(&Path) -> Result<(), String>,
) -> Out {
    let json = args.iter().any(|a| a == "--json");
    let pos: Vec<&str> = args
        .iter()
        .filter(|a| *a != "--json")
        .map(String::as_str)
        .collect();
    let usage = || Out {
        stdout: String::new(),
        stderr: format!("{CONFIG_USAGE}\n"),
        code: 2,
    };
    let done = |r: Result<serde_json::Value, String>, plain: String| match r {
        Ok(v) if json => Out {
            stdout: format!("{v}\n"),
            stderr: String::new(),
            code: 0,
        },
        Ok(_) => Out {
            stdout: plain,
            stderr: String::new(),
            code: 0,
        },
        Err(e) if json => Out {
            stdout: format!("{}\n", serde_json::json!({"ok": false, "error": e})),
            stderr: String::new(),
            code: 1,
        },
        Err(e) => Out {
            stdout: String::new(),
            stderr: format!("osh: config: {e}\n"),
            code: 1,
        },
    };
    let row_of = |key: &str| -> Result<Row, String> {
        load(dir, env)
            .rows
            .into_iter()
            .find(|r| r.def.key == key)
            .ok_or_else(|| format!("unknown setting {key}"))
    };
    let show = |r: Result<Row, String>| {
        let plain = r
            .as_ref()
            .map(|r| {
                format!(
                    "{} = {} ({})\n",
                    r.def.key,
                    value_text(&r.value),
                    source_name(r.source)
                )
            })
            .unwrap_or_default();
        done(r.map(|r| row_json(&r)), plain)
    };
    match pos.as_slice() {
        [] => {
            let s = load(dir, env);
            let v = serde_json::json!({
                "settings": s.rows.iter().map(row_json).collect::<Vec<_>>(),
                "problems": s.problems.iter().map(problem_json).collect::<Vec<_>>(),
                "data_dir": s.data_dir,
            });
            done(Ok(v), table_text(&s))
        }
        ["get", key] => {
            let r = row_of(key);
            let plain = r
                .as_ref()
                .map(|r| format!("{}\n", text_of(r.def, &r.value)))
                .unwrap_or_default();
            done(r.map(|r| row_json(&r)), plain)
        }
        ["set", key, value] => show(set(dir, key, value, env).and_then(|_| row_of(key))),
        ["unset", key] => show(unset(dir, key).and_then(|_| row_of(key))),
        ["path"] => {
            let r = ensure_file(dir).map(|_| dir.join(FILE).display().to_string());
            let plain = r.as_ref().map(|p| format!("{p}\n")).unwrap_or_default();
            done(r.map(|p| serde_json::json!({"path": p})), plain)
        }
        ["edit"] => {
            let r = ensure_file(dir).and_then(|_| edit(&dir.join(FILE)));
            done(r.map(|_| serde_json::json!({"ok": true})), String::new())
        }
        _ => usage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch folder that goes away with the test.
    struct Tmp(PathBuf);

    impl std::ops::Deref for Tmp {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    impl AsRef<Path> for Tmp {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "osh-settings-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
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
            parse_value(r, "/a:/b"),
            Ok(Value::List(vec!["/a".into(), "/b".into()]))
        );
        assert_eq!(
            parse_value(r, "/with,comma"),
            Ok(Value::List(vec!["/with,comma".into()])),
            "paths split on : like PATH"
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
        set(&d, "mcp.share_roots", "/a:/b", &no_env).unwrap();
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

    #[test]
    fn check_accepts_valid_and_unknown_keys_and_refuses_the_rest_with_line_and_column() {
        assert!(check(&template()).is_ok());
        assert!(check("[refresh]\nports_secs = 4\nfuture_key = 1\n").is_ok());
        let e = check("[refresh]\nports_secs = 0\n").unwrap_err();
        assert!(e.starts_with("osh.toml:2:") && e.contains("outside"), "{e}");
        let e = check("[refresh]\nports_secs = = 1\n").unwrap_err();
        assert!(e.starts_with("osh.toml:2:"), "{e}");
    }

    #[test]
    fn only_the_listening_address_needs_a_restart_of_the_http_server() {
        for key in [
            "mcp.allow_share",
            "mcp.share_roots",
            "mcp.repo_vars",
            "mcp.server_gateway",
        ] {
            assert_eq!(
                effect_name(def(key).unwrap().effect),
                "next request (osh mcp over stdio: restart)",
                "{key}"
            );
        }
        for key in ["mcp.server_bind", "mcp.server_port"] {
            assert_eq!(
                effect_name(def(key).unwrap().effect),
                "restart of osh mcp",
                "{key}"
            );
        }
    }

    #[test]
    fn the_job_cap_setting_has_a_default_and_a_range() {
        let s = load(&tmp(), &no_env);
        assert_eq!(row(&s, "mcp.job_max_secs").value, Value::Int(14_400));
        let ok = with_file("[mcp]\njob_max_secs = 0\n");
        assert_eq!(
            row(&ok, "mcp.job_max_secs").value,
            Value::Int(0),
            "0 means no cap"
        );
        let bad = with_file("[mcp]\njob_max_secs = 604801\n");
        assert!(bad
            .problems
            .iter()
            .any(|p| p.key.as_deref() == Some("mcp.job_max_secs")));
        assert_eq!(row(&bad, "mcp.job_max_secs").value, Value::Int(14_400));
        assert_eq!(
            effect_name(def("mcp.job_max_secs").unwrap().effect),
            "next request (osh mcp over stdio: restart)"
        );
    }

    #[test]
    fn the_server_settings_have_defaults_and_refuse_bad_values() {
        let s = load(&tmp(), &no_env);
        assert_eq!(
            row(&s, "mcp.server_bind").value,
            Value::Str("127.0.0.1".into())
        );
        assert_eq!(row(&s, "mcp.server_port").value, Value::Int(17800));
        assert_eq!(
            row(&s, "mcp.server_gateway").value,
            Value::Str(String::new())
        );
        assert_eq!(row(&s, "mcp.client_idle_secs").value, Value::Int(600));
        let s = with_file(
            "[mcp]\nserver_bind = \"not an ip\"\nserver_port = 80\nserver_gateway = \"-x\"\nclient_idle_secs = 1\n",
        );
        for key in [
            "mcp.server_bind",
            "mcp.server_port",
            "mcp.server_gateway",
            "mcp.client_idle_secs",
        ] {
            assert!(
                s.problems.iter().any(|p| p.key.as_deref() == Some(key)),
                "{key}: {:?}",
                s.problems
            );
        }
        assert_eq!(
            row(&s, "mcp.server_bind").value,
            Value::Str("127.0.0.1".into())
        );
        assert_eq!(row(&s, "mcp.server_port").value, Value::Int(17800));
    }

    #[test]
    fn the_server_settings_parse_typed_text() {
        let d = |k| def(k).unwrap();
        assert_eq!(
            parse_value(d("mcp.server_port"), "9000"),
            Ok(Value::Int(9000))
        );
        assert!(parse_value(d("mcp.server_port"), "80").is_err());
        assert!(parse_value(d("mcp.server_port"), "x").is_err());
        assert_eq!(
            parse_value(d("mcp.server_bind"), " ::1 "),
            Ok(Value::Str("::1".into()))
        );
        assert!(parse_value(d("mcp.server_bind"), "localhost").is_err());
        assert_eq!(
            parse_value(d("mcp.server_gateway"), "openshell-docker"),
            Ok(Value::Str("openshell-docker".into()))
        );
        assert_eq!(
            parse_value(d("mcp.server_gateway"), ""),
            Ok(Value::Str(String::new()))
        );
        assert!(parse_value(d("mcp.server_gateway"), "a/b").is_err());
    }

    #[test]
    fn readme_settings_table_matches_the_registry() {
        let readme =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md"))
                .unwrap();
        let sec = readme
            .split("\n## Settings\n")
            .nth(1)
            .expect("README has a `## Settings` section");
        let sec = sec.split("\n## ").next().unwrap();
        let rows: Vec<Vec<&str>> = sec
            .lines()
            .filter(|l| l.starts_with("| `"))
            .map(|l| {
                l.trim()
                    .trim_matches('|')
                    .split('|')
                    .map(str::trim)
                    .collect()
            })
            .collect();
        assert_eq!(
            rows.len(),
            registry().len(),
            "one README row per setting: {rows:?}"
        );
        for d in registry() {
            let r = rows
                .iter()
                .find(|r| r[0] == format!("`{}`", d.key))
                .unwrap_or_else(|| panic!("README lacks {}", d.key));
            assert_eq!(
                r.len(),
                6,
                "{}: Setting | Type | Default | Takes effect | Environment variable | What it does",
                d.key
            );
            assert_eq!(r[2], format!("`{}`", d.default), "{} default", d.key);
            assert_eq!(r[3], effect_name(d.effect), "{} takes effect", d.key);
            let env = d.env.map_or("none".to_string(), |e| format!("`{e}`"));
            assert_eq!(r[4], env, "{} environment variable", d.key);
        }
    }

    #[test]
    fn append_skips_a_table_written_without_a_header() {
        for body in [
            "refresh.ports_secs = 4\n",
            "refresh = { ports_secs = 4 }\n",
            "[[general]]\nx = 1\n",
        ] {
            let d = tmp();
            std::fs::write(d.join(FILE), body).unwrap();
            ensure_file(&d).unwrap();
            let after = read(&d);
            assert!(
                toml::from_str::<toml::Table>(&after).is_ok(),
                "still valid TOML:\n{after}"
            );
            assert!(
                after.starts_with(body),
                "the user's text is untouched:\n{after}"
            );
            assert!(
                after.contains("# toast_secs = 5"),
                "other tables still get their blocks"
            );
            if body.starts_with("refresh") {
                let s = load(&d, &no_env);
                assert!(s.problems.is_empty(), "{:?}", s.problems);
                assert_eq!(row(&s, "refresh.ports_secs").value, Value::Int(4));
            }
        }
    }

    #[test]
    fn unset_never_removes_other_lines_even_with_brackets_in_strings_and_comments() {
        let d = tmp();
        let file =
            "[mcp]\nshare_roots = [\"/a[\"]\nrepo_vars = [\"GH\"]\n[ui]\n# mine\ntoast_secs = 9\n";
        std::fs::write(d.join(FILE), file).unwrap();
        unset(&d, "mcp.share_roots").unwrap();
        assert_eq!(
            read(&d),
            "[mcp]\n# share_roots = []\nrepo_vars = [\"GH\"]\n[ui]\n# mine\ntoast_secs = 9\n"
        );
        let file = "[mcp]\nshare_roots = [\"/a\"] # see [docs\nrepo_vars = [\"GH\"]\n";
        std::fs::write(d.join(FILE), file).unwrap();
        unset(&d, "mcp.share_roots").unwrap();
        assert_eq!(
            read(&d),
            "[mcp]\n# share_roots = []\nrepo_vars = [\"GH\"]\n"
        );
    }

    #[test]
    fn unset_removes_a_list_written_over_several_lines_and_nothing_else() {
        let d = tmp();
        std::fs::write(
            d.join(FILE),
            "[mcp]\nshare_roots = [\n  \"/a\",\n  \"/b\",\n]\nrepo_vars = [\"GH\"]\n",
        )
        .unwrap();
        unset(&d, "mcp.share_roots").unwrap();
        assert_eq!(
            read(&d),
            "[mcp]\n# share_roots = []\nrepo_vars = [\"GH\"]\n"
        );
    }

    #[test]
    fn a_key_written_in_a_form_osh_cannot_edit_is_refused_not_ignored() {
        let d = tmp();
        let body = "refresh.ports_secs = 4\n";
        std::fs::write(d.join(FILE), body).unwrap();
        let e = unset(&d, "refresh.ports_secs").unwrap_err();
        assert!(e.contains("by hand"), "{e}");
        assert_eq!(read(&d), body);
        // `set` can still write it properly (toml_edit follows the dotted key), and says nothing
        // else changed.
        set(&d, "refresh.ports_secs", "9", &no_env).unwrap();
        let t: toml::Table = toml::from_str(&read(&d)).unwrap();
        assert_eq!(t["refresh"]["ports_secs"].as_integer(), Some(9));
    }

    #[test]
    fn set_does_not_edit_inside_a_multiline_string() {
        let d = tmp();
        let body = "[general]\nnote = \"\"\"\n# editor = \"auto\"\n\"\"\"\n";
        std::fs::write(d.join(FILE), body).unwrap();
        set(&d, "general.editor", "own", &no_env).unwrap();
        let t: toml::Table = toml::from_str(&read(&d)).unwrap();
        assert_eq!(t["general"]["editor"].as_str(), Some("own"));
        assert_eq!(
            t["general"]["note"].as_str(),
            Some("# editor = \"auto\"\n"),
            "the string is intact"
        );
    }

    #[test]
    fn a_write_keeps_a_symlinked_osh_toml_and_its_mode() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let d = tmp();
        let real = d.join("real.toml");
        std::fs::write(&real, "[refresh]\nports_secs = 4\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&real, d.join(FILE)).unwrap();
        set(&d, "refresh.ports_secs", "8", &no_env).unwrap();
        assert!(std::fs::symlink_metadata(d.join(FILE))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&real)
            .unwrap()
            .contains("ports_secs = 8"));
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn locked_names_the_variable_only_while_it_decides() {
        let d = tmp();
        let on = |k: &str| (k == TMUX_ENV).then(|| OsString::from("0"));
        assert!(locked(&d, "shell.tmux_keys", &on)
            .unwrap()
            .contains(TMUX_ENV));
        assert!(locked(&d, "shell.tmux_keys", &no_env).is_none());
        assert!(locked(&d, "refresh.ports_secs", &on).is_none());
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        let d = tmp();
        std::fs::write(d.join(FILE), "[refresh]\r\nports_secs = 4\r\n# note\r\n").unwrap();
        let only_crlf = |t: &str| t.matches('\n').count() == t.matches("\r\n").count();
        ensure_file(&d).unwrap();
        assert!(only_crlf(&read(&d)), "{:?}", read(&d));
        set(&d, "refresh.usage_secs", "9", &no_env).unwrap();
        assert!(only_crlf(&read(&d)));
        set(&d, "ui.toast_secs", "8", &no_env).unwrap();
        unset(&d, "refresh.ports_secs").unwrap();
        let t = read(&d);
        assert!(only_crlf(&t), "{t:?}");
        assert!(t.contains("# note\r\n") && t.contains("usage_secs = 9"));
    }

    #[test]
    fn an_appended_block_is_set_off_from_the_next_table() {
        let d = tmp();
        std::fs::write(
            d.join(FILE),
            "[refresh]\nports_secs = 7\n[ui]\ntoast_secs = 8\n",
        )
        .unwrap();
        ensure_file(&d).unwrap();
        let t = read(&d);
        let lines: Vec<&str> = t.lines().collect();
        for (i, l) in lines.iter().enumerate().skip(1) {
            if l.starts_with('[') {
                assert!(
                    lines[i - 1].trim().is_empty(),
                    "a blank line before {l:?}:\n{t}"
                );
            }
        }
    }

    #[test]
    fn path_lists_are_shown_and_edited_with_colons() {
        let d = tmp();
        assert_eq!(cli(&d, &["set", "mcp.share_roots", "/a:/b"]).code, 0);
        assert_eq!(cli(&d, &["get", "mcp.share_roots"]).stdout.trim(), "/a:/b");
        assert_eq!(cli(&d, &["set", "mcp.repo_vars", "A,B"]).code, 0);
        assert_eq!(cli(&d, &["get", "mcp.repo_vars"]).stdout.trim(), "A,B");
    }

    fn a(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn cli(dir: &Path, args: &[&str]) -> Out {
        run_cli(&a(args), dir, &no_env, &|_| Ok(()))
    }

    #[test]
    fn accessors_default_under_test() {
        assert_eq!(refresh_ports(), Duration::from_secs(3));
        assert_eq!(refresh_gateway(), Duration::from_secs(2));
        assert_eq!(status_allowed_for(), Duration::from_secs(20));
        assert_eq!(usage_max_backoff(), Duration::from_secs(40));
        assert!(tmux_keys() && !mcp_allow_share());
        assert_eq!(editor_pref(), "auto");
        assert!(mcp_share_roots().is_empty() && mcp_repo_vars().is_empty());
    }

    #[test]
    fn installed_snapshot_is_per_thread_and_ends_with_its_guard() {
        let d = tmp();
        std::fs::write(d.join(FILE), "[refresh]\nports_secs = 9\n").unwrap();
        {
            let _g = install_for_test(load(&d, &no_env));
            assert_eq!(refresh_ports(), Duration::from_secs(9));
            assert_eq!(
                std::thread::spawn(refresh_ports).join().unwrap(),
                Duration::from_secs(3)
            );
        }
        assert_eq!(refresh_ports(), Duration::from_secs(3));
    }

    #[test]
    fn backoff_accessor_is_clamped_to_the_meter_interval() {
        let d = tmp();
        std::fs::write(
            d.join(FILE),
            "[refresh]\nusage_secs = 100\nusage_max_backoff_secs = 20\n",
        )
        .unwrap();
        let _g = install_for_test(load(&d, &no_env));
        assert_eq!(usage_max_backoff(), Duration::from_secs(100));
    }

    #[test]
    fn store_reloads_on_change_and_survives_an_invalid_file() {
        let d = tmp();
        let mut st = Store::open(&d, &no_env);
        assert!(!st.reload(&no_env), "nothing changed yet");
        std::fs::write(d.join(FILE), "[refresh]\nports_secs = 8\n").unwrap();
        assert!(st.reload(&no_env));
        assert_eq!(
            row(&st.snapshot(), "refresh.ports_secs").value,
            Value::Int(8)
        );
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(d.join(FILE), "[refresh\nports_secs = 8\n").unwrap();
        assert!(st.reload(&no_env));
        let s = st.snapshot();
        assert!(!s.file_valid, "the screen can show the parse error");
        assert_eq!(
            row(&s, "refresh.ports_secs").value,
            Value::Int(8),
            "last good values stay"
        );
        assert!(!s.problems.is_empty());
    }

    #[test]
    fn config_json_lists_every_registry_key_with_env_fields() {
        let d = tmp();
        let env = |k: &str| (k == MCP_SHARE_ENV).then(|| OsString::from("1"));
        let out = run_cli(&a(&["--json"]), &d, &env, &|_| Ok(()));
        assert_eq!(out.code, 0, "{}", out.stderr);
        let v: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
        let rows = v["settings"].as_array().unwrap();
        assert_eq!(rows.len(), registry().len());
        let m = rows.iter().find(|r| r["key"] == "mcp.allow_share").unwrap();
        assert_eq!(m["value"], true);
        assert_eq!(m["source"], "env");
        assert_eq!(m["env"], MCP_SHARE_ENV);
        assert_eq!(m["env_active"], true);
        assert_eq!(m["default"], false);
        assert_eq!(
            m["takes_effect"],
            "next request (osh mcp over stdio: restart)"
        );
        let g = rows.iter().find(|r| r["key"] == "general.editor").unwrap();
        assert_eq!(g["env"], serde_json::Value::Null);
        assert!(v["problems"].as_array().unwrap().is_empty());
        assert!(v["data_dir"].is_string());
    }

    #[test]
    fn config_table_shows_key_value_source_env_and_description() {
        let out = cli(&tmp(), &[]);
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("refresh.ports_secs") && out.stdout.contains("default"));
        assert!(out.stdout.contains(TMUX_ENV) && out.stdout.contains("Pass the host tmux keys"));
    }

    #[test]
    fn config_get_prints_value() {
        let d = tmp();
        assert_eq!(cli(&d, &["get", "refresh.ports_secs"]).stdout.trim(), "3");
        assert_eq!(cli(&d, &["get", "nope.x"]).code, 1);
    }

    #[test]
    fn config_set_then_get_roundtrip_in_temp_dir() {
        let d = tmp();
        let o = cli(&d, &["set", "refresh.ports_secs", "7"]);
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert_eq!(cli(&d, &["get", "refresh.ports_secs"]).stdout.trim(), "7");
        assert!(read(&d).contains("ports_secs = 7"));
        assert_eq!(cli(&d, &["unset", "refresh.ports_secs"]).code, 0);
        assert_eq!(cli(&d, &["get", "refresh.ports_secs"]).stdout.trim(), "3");
    }

    #[test]
    fn config_set_refused_by_env_exits_1_names_variable() {
        let d = tmp();
        let env = |k: &str| (k == MCP_SHARE_ENV).then(|| OsString::from("1"));
        let o = run_cli(
            &a(&["set", "mcp.allow_share", "false", "--json"]),
            &d,
            &env,
            &|_| Ok(()),
        );
        assert_eq!(o.code, 1);
        let v: serde_json::Value = serde_json::from_str(o.stdout.trim()).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains(MCP_SHARE_ENV));
    }

    #[test]
    fn config_missing_args_and_unknown_subcommand_exit_2() {
        let d = tmp();
        assert_eq!(cli(&d, &["get"]).code, 2);
        assert_eq!(cli(&d, &["set", "refresh.ports_secs"]).code, 2);
        assert_eq!(cli(&d, &["frobnicate"]).code, 2);
        assert!(cli(&d, &["frobnicate"]).stderr.contains("usage"));
    }

    #[test]
    fn read_only_commands_do_not_create_the_file() {
        let d = tmp();
        cli(&d, &[]);
        cli(&d, &["--json"]);
        cli(&d, &["get", "refresh.ports_secs"]);
        assert!(!d.join(FILE).exists());
        let o = cli(&d, &["path"]);
        assert_eq!(o.stdout.trim(), d.join(FILE).display().to_string());
        assert!(d.join(FILE).exists(), "path is a write command");
    }

    #[test]
    fn config_edit_opens_the_file_in_the_editor() {
        let d = tmp();
        let seen = std::cell::RefCell::new(None);
        let o = run_cli(&a(&["edit"]), &d, &no_env, &|p| {
            *seen.borrow_mut() = Some(p.to_path_buf());
            Ok(())
        });
        assert_eq!(o.code, 0);
        assert_eq!(seen.borrow().clone(), Some(d.join(FILE)));
        let o = run_cli(&a(&["edit"]), &d, &no_env, &|_| {
            Err("no editor found".into())
        });
        assert_eq!(o.code, 1);
        assert!(o.stderr.contains("no editor found"));
    }
}
