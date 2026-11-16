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
}
