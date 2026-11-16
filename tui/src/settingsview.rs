//! The Settings screen's model: which settings are listed, the cursor, the filter and what a key
//! does. Pure apart from `osh.toml` writes through `osh_core::settings`; drawing is in `ui.rs`.

use osh_core::settings::{self, Effect, Kind, Row, Snapshot, Source, Value};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

/// Files that hold other settings and have their own editors; listed after the settings.
pub const OTHER_FILES: [(&str, &str); 2] = [
    (
        "toolchains.toml",
        "the toolchain list and the defaults for every new sandbox",
    ),
    (
        "sandbox-policy.yaml",
        "the filesystem policy every new sandbox gets",
    ),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Index into `snap.rows`.
    Setting(usize),
    File(&'static str, &'static str),
    DataFiles,
}

/// What the app does after a key.
#[derive(Debug, PartialEq, Eq)]
pub enum Act {
    None,
    Close,
    /// Ask for a new value of this setting, starting from the text shown.
    Prompt {
        key: &'static str,
        current: String,
    },
    /// Open one of the other files in the editor.
    File(&'static str),
    DataFiles,
}

pub struct View {
    env: fn(&str) -> Option<OsString>,
    pub snap: Snapshot,
    pub started: Arc<Snapshot>,
    pub cursor: usize,
    pub filter: String,
    pub filtering: bool,
    pub msg: String,
}

fn real_env(k: &str) -> Option<OsString> {
    std::env::var_os(k)
}

impl View {
    pub fn new(dir: &Path) -> View {
        Self::with_env(dir, real_env)
    }

    pub fn with_env(dir: &Path, env: fn(&str) -> Option<OsString>) -> View {
        View {
            env,
            snap: settings::load(dir, &env),
            started: settings::started(),
            cursor: 0,
            filter: String::new(),
            filtering: false,
            msg: String::new(),
        }
    }

    fn matches(&self, text: &[&str]) -> bool {
        let f = self.filter.to_lowercase();
        f.is_empty() || text.iter().any(|t| t.to_lowercase().contains(&f))
    }

    /// What is listed: the settings that match the filter, then the other files.
    pub fn entries(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = self
            .snap
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| self.matches(&[r.def.key, r.def.desc, r.def.env.unwrap_or("")]))
            .map(|(i, _)| Entry::Setting(i))
            .collect();
        for (name, what) in OTHER_FILES {
            if self.matches(&[name, what]) {
                out.push(Entry::File(name, what));
            }
        }
        if self.matches(&["data files", "profiles dockerfiles"]) {
            out.push(Entry::DataFiles);
        }
        out
    }

    pub fn selected(&self) -> Option<Entry> {
        self.entries().get(self.cursor).cloned()
    }

    fn row(&self) -> Option<&Row> {
        match self.selected()? {
            Entry::Setting(i) => self.snap.rows.get(i),
            _ => None,
        }
    }

    pub fn select_key(&mut self, key: &str) {
        let at = self
            .entries()
            .iter()
            .position(|e| matches!(e, Entry::Setting(i) if self.snap.rows[*i].def.key == key));
        if let Some(at) = at {
            self.cursor = at;
        }
    }

    fn refresh(&mut self, dir: &Path) {
        self.snap = settings::load(dir, &self.env);
    }

    /// Write `text` as the value of `key` and say what happened.
    pub fn apply(&mut self, dir: &Path, key: &str, text: &str) {
        self.msg = match settings::set(dir, key, text, &self.env) {
            Ok(()) => format!("{key} saved"),
            Err(e) => e,
        };
        self.refresh(dir);
    }

    fn locked(&self, row: &Row) -> Option<String> {
        row.env_active.then(|| {
            format!(
                "{} is set by the environment variable {} right now; unset it to change the setting",
                row.def.key,
                row.def.env.unwrap_or("")
            )
        })
    }

    pub fn on_key(&mut self, key: KeyEvent, dir: &Path) -> Act {
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.cursor = 0;
            return Act::None;
        }
        let n = self.entries().len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') if !self.filter.is_empty() => {
                self.filter.clear();
                self.cursor = 0;
            }
            KeyCode::Esc | KeyCode::Char('q') => return Act::Close,
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.cursor = n.saturating_sub(1),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('e') => return Act::File(settings::FILE),
            KeyCode::Char('r') => {
                if let Some(row) = self.row().cloned() {
                    self.msg = match self.locked(&row) {
                        Some(m) => m,
                        None => match settings::unset(dir, row.def.key) {
                            Ok(()) => format!("{} is back to its default", row.def.key),
                            Err(e) => e,
                        },
                    };
                    self.refresh(dir);
                }
            }
            KeyCode::Enter => match self.selected() {
                Some(Entry::File(name, _)) => return Act::File(name),
                Some(Entry::DataFiles) => return Act::DataFiles,
                Some(Entry::Setting(_)) => return self.edit(dir),
                None => {}
            },
            _ => {}
        }
        Act::None
    }

    fn edit(&mut self, dir: &Path) -> Act {
        let Some(row) = self.row().cloned() else {
            return Act::None;
        };
        if let Some(m) = self.locked(&row) {
            self.msg = m;
            return Act::None;
        }
        let key = row.def.key;
        match row.def.kind {
            Kind::Bool => {
                let next = if row.value == Value::Bool(true) {
                    "false"
                } else {
                    "true"
                };
                self.apply(dir, key, next);
                Act::None
            }
            Kind::Choice(all) => {
                let now = settings::value_text(&row.value);
                let at = all.iter().position(|c| *c == now).unwrap_or(0);
                self.apply(dir, key, all[(at + 1) % all.len()]);
                Act::None
            }
            _ => Act::Prompt {
                key,
                current: settings::value_text(&row.value),
            },
        }
    }

    /// One line per entry, scrolled so the cursor stays in view.
    pub fn list_lines(&self, shown: usize, width: usize) -> Vec<Line<'static>> {
        let entries = self.entries();
        let start = (self.cursor + 1).saturating_sub(shown.max(1));
        let key_w = settings::registry()
            .iter()
            .map(|d| d.key.len())
            .max()
            .unwrap_or(0);
        let dim = Style::default().fg(Color::DarkGray);
        let mut out = Vec::new();
        for (at, e) in entries.iter().enumerate().skip(start).take(shown) {
            let sel = at == self.cursor;
            let base = if sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let mut spans = Vec::new();
            match e {
                Entry::Setting(i) => {
                    let r = &self.snap.rows[*i];
                    let src = match r.source {
                        Source::Default => "default",
                        Source::File => "file",
                        Source::Env => "env",
                    };
                    let src_style = match r.source {
                        Source::Env => Style::default().fg(Color::Yellow),
                        Source::File => Style::default().fg(Color::Green),
                        Source::Default => dim,
                    };
                    spans.push(Span::styled(format!("{:<key_w$}  ", r.def.key), base));
                    spans.push(Span::styled(
                        format!("{:<22}  ", clip(&settings::value_text(&r.value), 22)),
                        base,
                    ));
                    spans.push(Span::styled(
                        format!("{src:<7}  "),
                        if sel { base } else { src_style },
                    ));
                    if let Some(v) = r.def.env {
                        let st = if r.env_active {
                            Style::default().fg(Color::Yellow)
                        } else {
                            dim
                        };
                        spans.push(Span::styled(v.to_string(), if sel { base } else { st }));
                    }
                    if self.restart_pending(r) {
                        spans.push(Span::styled(
                            "  applies after restart",
                            Style::default().fg(Color::Yellow),
                        ));
                    }
                }
                Entry::File(name, _) => {
                    spans.push(Span::styled(
                        format!("{name:<key_w$}  (opens in the editor)"),
                        base,
                    ));
                }
                Entry::DataFiles => {
                    spans.push(Span::styled(
                        "Data files  (profiles, Dockerfiles, configs)".to_string(),
                        base,
                    ));
                }
            }
            let mut line = Line::from(spans);
            line = clip_line(line, width);
            out.push(line);
        }
        out
    }

    /// An edit to a setting that only a restart picks up shows until the dashboard is restarted.
    pub fn restart_pending(&self, r: &Row) -> bool {
        r.def.effect == Effect::Restart
            && self
                .started
                .rows
                .iter()
                .find(|s| s.def.key == r.def.key)
                .is_some_and(|s| s.value != r.value)
    }

    /// What the selected entry means, where its value comes from and what overrides it.
    pub fn detail_lines(&self, width: usize) -> Vec<Line<'static>> {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let mut out: Vec<Line<'static>> = Vec::new();
        match self.selected() {
            Some(Entry::Setting(i)) => {
                let r = &self.snap.rows[i];
                out.push(Line::styled(r.def.key.to_string(), bold));
                out.extend(crate::ui::wrap_text(r.def.desc, width.max(10)));
                let effect = match r.def.effect {
                    Effect::Live => "applies at once",
                    Effect::Restart => "applies after the dashboard restarts",
                    Effect::RestartMcp => "applies at the next start of osh mcp",
                };
                out.push(Line::styled(
                    format!("Default {} · {effect}", r.def.default),
                    dim,
                ));
                let order = match r.def.env {
                    Some(v) => format!("Wins in this order: {v} (if set) > osh.toml > default"),
                    None => "Wins in this order: osh.toml > default".to_string(),
                };
                out.push(Line::styled(order, dim));
            }
            Some(Entry::File(name, what)) => {
                out.push(Line::styled(name.to_string(), bold));
                out.push(Line::raw(format!(
                    "Holds {what}. Enter opens it in the editor."
                )));
            }
            Some(Entry::DataFiles) => {
                out.push(Line::styled("Data files".to_string(), bold));
                out.push(Line::raw(
                    "Every file osh ships or you added: view, edit, compare, restore.",
                ));
            }
            None => out.push(Line::styled("Nothing matches the filter.".to_string(), dim)),
        }
        if !self.snap.file_valid {
            if let Some(p) = self.snap.problems.first() {
                let at = match (p.line, p.col) {
                    (Some(l), Some(c)) => format!(":{l}:{c}"),
                    _ => String::new(),
                };
                out.push(Line::styled(
                    format!("osh.toml{at} is not valid: {} (e opens it)", p.msg),
                    Style::default().fg(Color::Red),
                ));
            }
        } else {
            // Every setting the file gets wrong and every key it has that osh does not know
            // (usually a typo), with the line to fix.
            let shown = 3;
            for p in self.snap.problems.iter().take(shown) {
                let at = p.line.map_or(String::new(), |l| format!(":{l}"));
                out.push(Line::styled(
                    format!("osh.toml{at}: {}", p.msg),
                    Style::default().fg(Color::Red),
                ));
            }
            if self.snap.problems.len() > shown {
                out.push(Line::styled(
                    format!(
                        "{} more problems in osh.toml (osh config lists them all)",
                        self.snap.problems.len() - shown
                    ),
                    Style::default().fg(Color::Red),
                ));
            }
        }
        if !self.msg.is_empty() {
            out.push(Line::styled(
                self.msg.clone(),
                Style::default().fg(Color::Cyan),
            ));
        }
        out
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn clip_line(line: Line<'static>, width: usize) -> Line<'static> {
    let mut left = width;
    let mut spans = Vec::new();
    for s in line.spans {
        let n = s.content.chars().count();
        if n <= left {
            left -= n;
            spans.push(s);
        } else {
            spans.push(Span::styled(clip(&s.content, left), s.style));
            break;
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "osh-sview-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn none(_: &str) -> Option<OsString> {
        None
    }

    fn forced(k: &str) -> Option<OsString> {
        (k == settings::MCP_SHARE_ENV).then(|| OsString::from("1"))
    }

    fn press(v: &mut View, d: &Path, c: KeyCode) -> Act {
        v.on_key(KeyEvent::new(c, KeyModifiers::NONE), d)
    }

    fn type_text(v: &mut View, d: &Path, t: &str) {
        for c in t.chars() {
            press(v, d, KeyCode::Char(c));
        }
    }

    fn go_to(v: &mut View, key: &str) {
        v.select_key(key);
        assert!(
            matches!(v.selected(), Some(Entry::Setting(i)) if v.snap.rows[i].def.key == key),
            "{key}"
        );
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn lists_every_registry_key_then_the_other_files() {
        let d = tmp();
        let v = View::with_env(&d, none);
        let e = v.entries();
        assert_eq!(e.len(), settings::registry().len() + OTHER_FILES.len() + 1);
        assert!(matches!(e[0], Entry::Setting(0)));
        assert!(matches!(e[e.len() - 1], Entry::DataFiles));
        let all = text(&v.list_lines(100, 120));
        for def in settings::registry() {
            assert!(all.contains(def.key), "{}", def.key);
        }
        assert!(
            all.contains("OSH_MCP_ALLOW_SHARE"),
            "the environment column names the variable"
        );
    }

    #[test]
    fn slash_filters_by_key_or_description_and_esc_clears() {
        let d = tmp();
        let mut v = View::with_env(&d, none);
        press(&mut v, &d, KeyCode::Char('/'));
        type_text(&mut v, &d, "toast");
        press(&mut v, &d, KeyCode::Enter);
        let e = v.entries();
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(matches!(e[0], Entry::Setting(i) if v.snap.rows[i].def.key == "ui.toast_secs"));
        press(&mut v, &d, KeyCode::Esc);
        assert!(v.filter.is_empty());
        press(&mut v, &d, KeyCode::Char('/'));
        type_text(&mut v, &d, "colour");
        assert!(v.entries().is_empty(), "nothing matches");
        assert!(text(&v.detail_lines(60)).contains("Nothing matches"));
        let mut v = View::with_env(&d, none);
        press(&mut v, &d, KeyCode::Char('/'));
        type_text(&mut v, &d, "interval");
        assert!(v.entries().is_empty());
        let mut v = View::with_env(&d, none);
        press(&mut v, &d, KeyCode::Char('/'));
        type_text(&mut v, &d, "tmux");
        assert!(!v.entries().is_empty(), "the description is searched too");
    }

    #[test]
    fn enter_toggles_a_bool_and_writes_the_file() {
        let d = tmp();
        let mut v = View::with_env(&d, none);
        go_to(&mut v, "shell.tmux_keys");
        assert_eq!(press(&mut v, &d, KeyCode::Enter), Act::None);
        assert!(std::fs::read_to_string(d.join("osh.toml"))
            .unwrap()
            .contains("tmux_keys = false"));
        assert_eq!(
            v.snap
                .rows
                .iter()
                .find(|r| r.def.key == "shell.tmux_keys")
                .unwrap()
                .value,
            Value::Bool(false)
        );
        press(&mut v, &d, KeyCode::Enter);
        assert_eq!(
            v.snap
                .rows
                .iter()
                .find(|r| r.def.key == "shell.tmux_keys")
                .unwrap()
                .value,
            Value::Bool(true)
        );
    }

    #[test]
    fn enter_on_a_choice_cycles_through_the_choices() {
        let d = tmp();
        let mut v = View::with_env(&d, none);
        go_to(&mut v, "general.editor");
        let now = |v: &View| {
            settings::value_text(
                &v.snap
                    .rows
                    .iter()
                    .find(|r| r.def.key == "general.editor")
                    .unwrap()
                    .value,
            )
        };
        let seen: Vec<String> = (0..3)
            .map(|_| {
                press(&mut v, &d, KeyCode::Enter);
                now(&v)
            })
            .collect();
        assert_eq!(seen, ["own", "builtin", "auto"]);
    }

    #[test]
    fn enter_on_a_number_asks_and_apply_validates_with_a_reason() {
        let d = tmp();
        let mut v = View::with_env(&d, none);
        go_to(&mut v, "refresh.ports_secs");
        assert_eq!(
            press(&mut v, &d, KeyCode::Enter),
            Act::Prompt {
                key: "refresh.ports_secs",
                current: "3".into()
            }
        );
        v.apply(&d, "refresh.ports_secs", "0");
        assert!(v.msg.contains("outside"), "{}", v.msg);
        assert!(
            !d.join("osh.toml").exists()
                || !std::fs::read_to_string(d.join("osh.toml"))
                    .unwrap()
                    .contains("ports_secs = 0")
        );
        v.apply(&d, "refresh.ports_secs", "8");
        assert_eq!(v.msg, "refresh.ports_secs saved");
        assert_eq!(
            v.snap
                .rows
                .iter()
                .find(|r| r.def.key == "refresh.ports_secs")
                .unwrap()
                .value,
            Value::Int(8)
        );
    }

    #[test]
    fn r_resets_the_row_by_removing_the_key() {
        let d = tmp();
        let mut v = View::with_env(&d, none);
        v.apply(&d, "refresh.ports_secs", "8");
        go_to(&mut v, "refresh.ports_secs");
        press(&mut v, &d, KeyCode::Char('r'));
        assert!(v.msg.contains("back to its default"));
        let row = v
            .snap
            .rows
            .iter()
            .find(|r| r.def.key == "refresh.ports_secs")
            .unwrap();
        assert_eq!(
            (row.value.clone(), row.source),
            (Value::Int(3), Source::Default)
        );
    }

    #[test]
    fn an_environment_forced_row_is_marked_and_refuses_edits_naming_the_variable() {
        let d = tmp();
        let mut v = View::with_env(&d, forced);
        go_to(&mut v, "mcp.allow_share");
        assert!(text(&v.list_lines(100, 120)).contains("env"));
        assert_eq!(press(&mut v, &d, KeyCode::Enter), Act::None);
        assert!(v.msg.contains("OSH_MCP_ALLOW_SHARE"), "{}", v.msg);
        press(&mut v, &d, KeyCode::Char('r'));
        assert!(v.msg.contains("OSH_MCP_ALLOW_SHARE"));
        assert!(!d.join("osh.toml").exists(), "nothing was written");
        let d = text(&v.detail_lines(80));
        assert!(
            d.contains("OSH_MCP_ALLOW_SHARE (if set) > osh.toml > default"),
            "{d}"
        );
    }

    #[test]
    fn an_invalid_file_shows_the_parse_error_and_e_opens_the_editor() {
        let d = tmp();
        std::fs::write(d.join("osh.toml"), "[refresh]\nports_secs = = 4\n").unwrap();
        let mut v = View::with_env(&d, none);
        let t = text(&v.detail_lines(80));
        assert!(t.contains("osh.toml:2:") && t.contains("not valid"), "{t}");
        assert_eq!(press(&mut v, &d, KeyCode::Char('e')), Act::File("osh.toml"));
    }

    #[test]
    fn every_problem_in_the_file_is_listed_with_its_line() {
        let d = tmp();
        std::fs::write(
            d.join("osh.toml"),
            "[refresh]\nport_secs = 4\nusage_secs = 0\n[ui]\ntoast_secs = 5\n",
        )
        .unwrap();
        let v = View::with_env(&d, none);
        let t = text(&v.detail_lines(100));
        assert!(
            t.contains("osh.toml:2: unknown setting refresh.port_secs"),
            "{t}"
        );
        assert!(t.contains("osh.toml:3:") && t.contains("outside"), "{t}");
    }

    #[test]
    fn other_files_and_data_files_open_from_the_list() {
        let d = tmp();
        let mut v = View::with_env(&d, none);
        press(&mut v, &d, KeyCode::Char('G'));
        assert_eq!(press(&mut v, &d, KeyCode::Enter), Act::DataFiles);
        press(&mut v, &d, KeyCode::Char('k'));
        assert_eq!(
            press(&mut v, &d, KeyCode::Enter),
            Act::File("sandbox-policy.yaml")
        );
        assert_eq!(press(&mut v, &d, KeyCode::Esc), Act::Close);
    }

    #[test]
    fn a_setting_that_needs_a_restart_shows_it_until_the_dashboard_restarts() {
        let d = tmp();
        let mut v = View::with_env(&d, none);
        v.started = Arc::new(settings::load(&d, &none));
        assert!(!text(&v.list_lines(100, 120)).contains("applies after restart"));
        v.apply(&d, "refresh.alerts_secs", "9");
        v.apply(&d, "refresh.ports_secs", "9");
        v.apply(&d, "mcp.allow_share", "true");
        let rows: Vec<String> = v
            .list_lines(100, 140)
            .iter()
            .map(|l| text(std::slice::from_ref(l)))
            .collect();
        let marked: Vec<&String> = rows
            .iter()
            .filter(|l| l.contains("applies after restart"))
            .collect();
        assert_eq!(marked.len(), 1, "{rows:?}");
        assert!(marked[0].contains("refresh.alerts_secs"));
    }

    #[test]
    fn a_narrow_list_cuts_lines_instead_of_overflowing() {
        let d = tmp();
        let v = View::with_env(&d, none);
        for l in v.list_lines(100, 30) {
            assert!(text(&[l]).chars().count() <= 30);
        }
    }
}
