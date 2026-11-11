//! Rendering.
use crate::app::{App, Modal, Tab, TABS};
use crate::cli::{self, Freshness};
use crate::ghrepo;
use crate::logview::LogView;
use crate::term::Session;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph,
        Row, Table, TableState,
    },
    Frame,
};

/// Semantic colour roles. Only the 16 ANSI colours, so every terminal theme (dark or light)
/// supplies the actual shades and the contrast stays the user's own.
mod role {
    use ratatui::style::{Color, Modifier, Style};
    pub const ACCENT: Color = Color::Cyan;
    pub const OK: Color = Color::Green;
    pub const WARN: Color = Color::Yellow;
    pub const ERR: Color = Color::Red;
    pub const BUSY: Color = Color::Indexed(208);
    pub fn dim() -> Style {
        Style::default().fg(Color::DarkGray)
    }
    pub fn bold() -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
    pub fn accent() -> Style {
        Style::default().fg(ACCENT)
    }
}

fn phase_style(phase: &str) -> Style {
    let c = match phase {
        "Ready" => role::OK,
        "Provisioning" | "Starting" | "Stopping" => role::WARN,
        "Error" => role::ERR,
        "Completed" => role::ACCENT,
        _ => Color::Gray,
    };
    Style::default().fg(c)
}

/// The status as spans: an orange spinner while work runs, red on a failure, `done` once finished.
fn status_spans(app: &App, text: &str, base: Style, done: Color) -> Vec<Span<'static>> {
    let bold = Modifier::BOLD;
    if let Some(frame) = app.spinner() {
        let c = role::BUSY;
        return vec![
            Span::styled(format!(" {frame} "), base.fg(c).add_modifier(bold)),
            Span::styled(format!("{text} "), base.fg(c)),
        ]
        .into_iter()
        .chain(
            app.busy_for()
                .map(|d| Span::styled(format!("({}s) ", d.as_secs()), base.fg(c))),
        )
        .collect();
    }
    let failed = ["Error", "Step failed", "Cancelled"]
        .iter()
        .any(|p| text.starts_with(p));
    let (mark, c) = if failed {
        ("✗", role::ERR)
    } else {
        ("✓", done)
    };
    vec![
        Span::styled(format!(" {mark} "), base.fg(c).add_modifier(bold)),
        Span::styled(format!("{text} "), base.fg(c)),
    ]
}

/// The selected row: an accent bar in the margin and bold text. No inverted block, which shouts
/// and hides the row's own colours.
fn highlight() -> Style {
    Style::default()
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::REVERSED)
}

fn row_highlight() -> Style {
    Style::default()
        .add_modifier(Modifier::BOLD)
        .fg(role::ACCENT)
}

/// A main panel: rounded, quiet border; the title bold and the row position (`3/12`) dim in the
/// top-right corner so the list always says where the cursor is.
fn panel(title: &str, at: Option<(usize, usize)>) -> Block<'static> {
    let mut b = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(role::dim())
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.to_string(), role::bold()),
            Span::raw(" "),
        ]));
    if let Some((i, n)) = at.filter(|(_, n)| *n > 0) {
        b = b.title(Line::styled(format!(" {}/{n} ", i + 1), role::dim()).right_aligned());
    }
    b
}

/// A table header row: dim, so the data is what the eye lands on.
fn head(cols: &[&str]) -> Row<'static> {
    Row::new(cols.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .style(role::dim().add_modifier(Modifier::BOLD))
        .bottom_margin(1)
}

/// Columns of each tab's table: the header names and widths, shared by the empty state so an
/// empty tab still shows which list it is (and where its rows will go).
type Cols = (&'static [&'static str], &'static [Constraint]);
const SANDBOX_COLS: Cols = (
    &["NAME", "PHASE", "CREATED"],
    &[
        Constraint::Min(14),
        Constraint::Length(15),
        Constraint::Length(19),
    ],
);
const PROVIDER_COLS: Cols = (
    &["NAME", "TYPE", "CREDENTIAL KEYS", "GITHUB TOKEN"],
    &[
        Constraint::Length(28),
        Constraint::Length(24),
        Constraint::Length(24),
        Constraint::Min(20),
    ],
);
const PROFILE_COLS: Cols = (
    &["PROFILE", "ON GATEWAY", "SOURCE"],
    &[
        Constraint::Length(24),
        Constraint::Length(16),
        Constraint::Min(14),
    ],
);
const IMAGE_COLS: Cols = (
    &["IMAGE", "STATE", "DOCKERFILE"],
    &[
        Constraint::Length(30),
        Constraint::Length(14),
        Constraint::Min(20),
    ],
);
const GATEWAY_COLS: Cols = (
    &["GATEWAY", "TYPE", "PORT", "STATE"],
    &[
        Constraint::Min(28),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(12),
    ],
);

/// A tab's table with `rows`, its column header and a panel titled `title`.
fn table<'a>(
    rows: Vec<Row<'a>>,
    (names, widths): Cols,
    title: &str,
    count: Option<(usize, usize)>,
) -> Table<'a> {
    Table::new(rows, widths.to_vec())
        .header(head(names))
        .row_highlight_style(row_highlight())
        .block(panel(title, count))
}

/// What a list says when it is empty: the table's header stays (the columns tell which list
/// this is), and below it a line centred in the panel points at the next step.
fn empty_state(f: &mut Frame, area: Rect, title: &str, cols: Cols, what: &str, hint: &str) {
    f.render_widget(table(Vec::new(), cols, title, None), area);
    let inner = area.inner(ratatui::layout::Margin::new(2, 1));
    let h = 3.min(inner.height);
    let at = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(h) / 2,
        inner.width,
        h,
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::styled(what.to_string(), role::bold()),
            Line::default(),
            Line::styled(hint.to_string(), role::dim()),
        ])
        .alignment(Alignment::Center),
        at,
    );
}

/// A coloured dot and a word, for a state.
fn dot(text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("● ", style),
        Span::styled(text.to_string(), style),
    ])
}

/// The newest alert in a box at the top-right, over whatever else is drawn. Red for high.
fn draw_toast(f: &mut Frame, app: &App) {
    let Some(a) = app.toast() else { return };
    let area = f.area();
    let w = 56.min(area.width.saturating_sub(2));
    if w < 12 || area.height < 5 {
        return;
    }
    let color = if a.sev == crate::alerts::Severity::High {
        role::ERR
    } else {
        role::WARN
    };
    let cols = text_cols(w);
    let mut lines = vec![Line::styled(a.title.clone(), role::bold())];
    lines.extend(wrap_text(&a.detail, cols));
    let h = (lines.len() as u16 + 2).min((area.height / 2).max(5));
    let rect = Rect::new(area.x + area.width - w - 1, area.y + 1, w, h);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(frame("Alert", "Esc dismiss · ! list", color)),
        rect,
    );
}

pub fn draw(f: &mut Frame, app: &App) {
    draw_screen(f, app);
    draw_toast(f, app);
}

fn draw_screen(f: &mut Frame, app: &App) {
    if let Some(Modal::ExtEdit { run, .. }) = &app.modal {
        draw_ext_edit(f, app, run);
        return;
    }
    if let Some(s) = app.session() {
        draw_session(f, app, s);
        if let Some(m) = &app.modal {
            draw_modal(f, m, app.tab);
        }
        return;
    }
    if let Some(lv) = &app.logview {
        draw_logview(f, lv);
        return;
    }
    let banner = app.expiry_banner();
    let help = footer_lines(app, f.area().width);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(u16::from(banner.is_some())),
            Constraint::Min(5),
            Constraint::Length(1 + help.len() as u16),
        ])
        .split(f.area());
    draw_tabs(f, app, rows[0]);
    if let Some(b) = banner {
        f.render_widget(
            Paragraph::new(Line::styled(
                format!(" ⚠ {b}"),
                Style::default().fg(role::WARN),
            )),
            rows[1],
        );
    }
    match app.tab {
        Tab::Sandboxes => draw_sandboxes(f, app, rows[2]),
        Tab::Providers => draw_providers(f, app, rows[2]),
        Tab::Profiles => draw_profiles(f, app, rows[2]),
        Tab::Images => draw_images(f, app, rows[2]),
        Tab::Gateways => draw_gateways(f, app, rows[2]),
    }
    draw_footer(f, app, rows[3], help);
    if let Some(m) = &app.modal {
        draw_modal(f, m, app.tab);
    }
}

/// The top bar: the app name, the tabs (the current one bold and underlined in the accent colour),
/// and on the right the gateway as one dot and its name.
fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" osh ", role::bold().fg(role::ACCENT))];
    for (i, (tab, name)) in TABS.iter().enumerate() {
        if *tab == app.tab {
            spans.push(Span::styled(
                format!(" {} {name} ", i + 1),
                role::accent()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled(format!(" {} {name} ", i + 1), role::dim()));
        }
    }
    let (txt, color) = if app.awaiting_snapshot {
        let gw = if app.snap.gateway.is_empty() {
            String::new()
        } else {
            format!(" {}", app.snap.gateway)
        };
        (format!("connecting{gw}..."), role::WARN)
    } else if app.snap.gateway_ok {
        let driver = if app.snap.driver.is_empty() {
            String::new()
        } else {
            format!(" [{}]", app.snap.driver)
        };
        (
            format!(
                "{}{driver} connected  {}",
                app.snap.gateway, app.snap.version
            ),
            role::OK,
        )
    } else {
        (format!("{} unreachable", app.snap.gateway), role::ERR)
    };
    let mut right = vec![
        Span::styled("● ", Style::default().fg(color)),
        Span::styled(format!("{txt} "), Style::default().fg(color)),
    ];
    if app.config_stale {
        right.insert(
            0,
            Span::styled(
                "gateway.toml out of date (R on Gateways installs it)  ",
                Style::default().fg(role::WARN),
            ),
        );
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    f.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        area,
    );
}

fn table_state(i: usize) -> TableState {
    TableState::default().with_selected(Some(i))
}

/// A label and its value, labels aligned in a dim column.
fn kv(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("{label:<12}"), role::dim())];
    spans.extend(value);
    Line::from(spans)
}

fn draw_sandboxes(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);
    let n = app.snap.sandboxes.len();
    if n == 0 {
        empty_state(
            f,
            cols[0],
            "Sandboxes",
            SANDBOX_COLS,
            "No sandboxes yet",
            "n  create one",
        );
    } else {
        let rows: Vec<Row> = app
            .snap
            .sandboxes
            .iter()
            .map(|s| {
                // A running shell is marked after the name, so every name starts in the header's column.
                let name = if app.has_session(&s.name) {
                    Line::from(vec![
                        Span::raw(s.name.clone()),
                        Span::styled(" ▸", role::accent()),
                    ])
                } else {
                    Line::from(s.name.clone())
                };
                Row::new(vec![
                    Cell::from(name),
                    Cell::from(dot(&s.phase, phase_style(&s.phase))),
                    Cell::from(Line::styled(s.created.clone(), role::dim())),
                ])
            })
            .collect();
        let t = table(rows, SANDBOX_COLS, "Sandboxes", Some((app.cursor[0], n)));
        f.render_stateful_widget(t, cols[0], &mut table_state(app.cursor[0]));
    }

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(cols[1]);
    let dcols = text_cols(right[0].width + 2).saturating_sub(12);
    let mut detail: Vec<Line> = Vec::new();
    match app.selected_sandbox() {
        Some(s) => {
            detail.push(kv("phase", dot(&s.phase, phase_style(&s.phase)).spans));
            if s.phase == "Ready" {
                match app.usage_of(&s.name, &app.snap.gateway) {
                    Some(u) => {
                        detail.push(kv("usage", usage_spans(u, dcols, Style::default())));
                        for l in wrap_text(totals_note(&app.snap.driver), dcols.max(8)) {
                            detail.push(prefix(l.style(role::dim()), &" ".repeat(12)));
                        }
                    }
                    None => detail.push(kv("usage", vec![Span::styled("reading…", role::dim())])),
                }
            }
            if let Some((_, l)) = app.snap.limits.as_ref().filter(|(n, _)| n == &s.name) {
                detail.push(kv("limits", vec![Span::raw(l.clone())]));
            }
            for c in &s.conditions {
                let (mark, color) = if c.status == "True" {
                    ("✓", role::OK)
                } else {
                    ("✗", role::ERR)
                };
                let mut line = kv(
                    &c.kind,
                    vec![Span::styled(format!("{mark} "), Style::default().fg(color))],
                );
                // The message wraps under its own column, not under the label.
                let msg = wrap_text(&c.message, dcols.max(8));
                for (i, m) in msg.into_iter().enumerate() {
                    if i == 0 {
                        line.spans.extend(m.spans);
                        detail.push(std::mem::take(&mut line));
                    } else {
                        detail.push(prefix(m, &" ".repeat(14)));
                    }
                }
                if !line.spans.is_empty() {
                    detail.push(line);
                }
            }
        }
        None => detail.push(Line::styled("Nothing selected.", role::dim())),
    }
    f.render_widget(
        Paragraph::new(detail).block(panel("Details", None)),
        right[0],
    );
    let log_text = app
        .snap
        .logs
        .as_ref()
        .filter(|(n, _)| app.selected_sandbox().is_some_and(|s| &s.name == n))
        .map(|(_, l)| l.as_str())
        .unwrap_or("");
    let height = right[1].height.saturating_sub(2) as usize;
    let lines: Vec<&str> = log_text.lines().collect();
    let tail: Vec<Line> = lines[lines.len().saturating_sub(height)..]
        .iter()
        .map(|l| log_line(&crate::logview::humanize(l)))
        .collect();
    f.render_widget(
        Paragraph::new(tail).block(
            panel("Logs", None)
                .title_bottom(Line::styled(" l full view ", role::dim()).right_aligned()),
        ),
        right[1],
    );
}

/// What the meter's totals are, since none of them is a limit of this one sandbox.
fn totals_note(driver: &str) -> &'static str {
    if driver == "vm" {
        "totals are the gateway's VM size, the same for every sandbox"
    } else {
        "totals are the machine's, shared by all sandboxes"
    }
}

/// The meter's figures that fit in `width` columns: `ok` for the normal ones, yellow from 80%,
/// red from 95%.
fn usage_spans(u: &crate::usage::Usage, width: usize, ok: Style) -> Vec<Span<'static>> {
    use crate::usage::{fit, parts, Level, GAP};
    let mut out = Vec::new();
    for (i, (text, level)) in fit(&parts(u), width).into_iter().enumerate() {
        if i > 0 {
            out.push(Span::styled(GAP, ok));
        }
        let style = match level {
            Level::Ok => ok,
            Level::Warn => ok.fg(role::WARN),
            Level::High => ok.fg(role::ERR).add_modifier(Modifier::BOLD),
        };
        out.push(Span::styled(text, style));
    }
    out
}

fn draw_providers(f: &mut Frame, app: &App, area: Rect) {
    let n = app.snap.providers.len();
    if n == 0 {
        empty_state(
            f,
            area,
            "Providers",
            PROVIDER_COLS,
            "No providers",
            "n  create one (credentials a sandbox may use)",
        );
        return;
    }
    let rows: Vec<Row> = app
        .snap
        .providers
        .iter()
        .map(|p| {
            let gw = cli::gateway();
            let note = app
                .tracked
                .iter()
                .find(|t| t.provider == p.name && t.gateway == gw)
                .map(|t| {
                    let now = ghrepo::now_secs();
                    (
                        format!("{}: {}", t.shown().1, ghrepo::expiry_note(now, t.expiry_ms)),
                        ghrepo::days_left(now, t.expiry_ms) <= ghrepo::WARN_DAYS,
                    )
                });
            let (note, soon) = note.unwrap_or_default();
            Row::new(vec![
                Cell::from(p.name.clone()),
                Cell::from(Line::styled(p.ty.clone(), role::dim())),
                Cell::from(p.keys.join(", ")),
                Cell::from(Line::styled(
                    note,
                    if soon {
                        Style::default().fg(role::WARN)
                    } else {
                        role::dim()
                    },
                )),
            ])
        })
        .collect();
    let t = table(rows, PROVIDER_COLS, "Providers", Some((app.cursor[1], n)));
    f.render_stateful_widget(t, area, &mut table_state(app.cursor[1]));
}

fn draw_profiles(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .profile_rows()
        .into_iter()
        .map(|r| {
            let (gw, gs) = r
                .gateway_version
                .map_or(("not imported".to_string(), role::dim()), |v| {
                    (format!("v{v}"), Style::default().fg(role::OK))
                });
            let src = if r.in_repo {
                "providers/"
            } else {
                "gateway only"
            };
            Row::new(vec![
                Cell::from(r.id),
                Cell::from(Line::styled(gw, gs)),
                Cell::from(Line::styled(src.to_string(), role::dim())),
            ])
        })
        .collect();
    let n = rows.len();
    if n == 0 {
        let hint = format!(
            "n creates one; the files live in {}",
            app.dir.join("providers").display()
        );
        empty_state(f, area, "Profiles", PROFILE_COLS, "No profiles", &hint);
        return;
    }
    let t = table(rows, PROFILE_COLS, "Profiles", Some((app.cursor[2], n)));
    f.render_stateful_widget(t, area, &mut table_state(app.cursor[2]));
}

fn draw_images(f: &mut Frame, app: &App, area: Rect) {
    let n = app.snap.images.len();
    if n == 0 {
        let hint = format!(
            "n creates a Dockerfile; they live in {}",
            app.dir.join("images").display()
        );
        empty_state(f, area, "Images", IMAGE_COLS, "No images", &hint);
        return;
    }
    let rows: Vec<Row> = app
        .snap
        .images
        .iter()
        .map(|i| {
            let (s, c) = match (i.built, i.freshness) {
                (false, _) => ("not built", role::WARN),
                (true, Freshness::UpToDate) => ("up to date", role::OK),
                (true, Freshness::Outdated) => ("outdated", role::ERR),
                (true, Freshness::Unverified) => ("unverified", role::WARN),
            };
            Row::new(vec![
                Cell::from(i.tag.clone()),
                Cell::from(dot(s, Style::default().fg(c))),
                Cell::from(Line::styled(
                    i.file
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    role::dim(),
                )),
            ])
        })
        .collect();
    let t = table(rows, IMAGE_COLS, "Images", Some((app.cursor[3], n)));
    f.render_stateful_widget(t, area, &mut table_state(app.cursor[3]));
}

fn draw_gateways(f: &mut Frame, app: &App, area: Rect) {
    let n = app.snap.gateways.len();
    if n == 0 {
        empty_state(
            f,
            area,
            "Gateways",
            GATEWAY_COLS,
            "No gateways",
            "a  add one",
        );
        return;
    }
    let rows: Vec<Row> = app
        .snap
        .gateways
        .iter()
        .map(|g| {
            let cur = g.name == app.snap.gateway;
            let info = app.gateway_info(&g.name, &g.endpoint);
            let mut tags = Vec::new();
            if info.main {
                tags.push("main");
            }
            if g.active {
                tags.push("default");
            }
            let mut name = vec![Span::raw(g.name.clone())];
            for t in tags {
                name.push(Span::styled(format!("  {t}"), role::dim()));
            }
            Row::new(vec![
                Cell::from(Line::from(name)),
                Cell::from(Line::styled(info.kind, role::dim())),
                Cell::from(Line::styled(info.port, role::dim())),
                Cell::from(if cur {
                    dot("selected", Style::default().fg(role::OK))
                } else {
                    Line::default()
                }),
            ])
        })
        .collect();
    let t = table(rows, GATEWAY_COLS, "Gateways", Some((app.cursor[4], n)));
    f.render_stateful_widget(t, area, &mut table_state(app.cursor[4]));
}

/// The keys every tab has, last in the footer.
pub const GLOBAL_HELP: &str =
    "Tab/1-5 switch  j/k move  g gateway  r refresh  ! alerts  ? help  : commands  q quit";

/// The per-tab key hints in the footer.
pub fn footer_help(tab: Tab) -> &'static str {
    match tab {
        Tab::Sandboxes => "Enter shell (● running)  l logs  i settings  p blocked  n new  d delete  s stop  S start  a/x attach/detach provider  K skills  T transfer state  t renew repo token",
        Tab::Providers => "n create  u update credentials  d delete",
        Tab::Profiles => "u / Enter import or update from providers/  e file  n new",
        Tab::Images => "b / Enter build  e Dockerfile  n new",
        Tab::Gateways => "Enter select  s make default  a add  R start/restart  l log  e config  V VM size  d wipe (delete all data)  x remove",
    }
}

/// Key hints as spans: the key of each entry in bold accent, its words dim, entries apart.
/// An entry is `key words` and entries are separated by two spaces, as in the help strings.
pub fn hint_spans(help: &str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (i, entry) in help.split("  ").filter(|e| !e.is_empty()).enumerate() {
        if i > 0 {
            out.push(Span::styled("  ", role::dim()));
        }
        let (key, rest) = entry.split_once(' ').unwrap_or((entry, ""));
        out.push(Span::styled(
            key.to_string(),
            role::accent().add_modifier(Modifier::BOLD),
        ));
        if !rest.is_empty() {
            out.push(Span::styled(format!(" {rest}"), role::dim()));
        }
    }
    out
}

/// The key help in rows that break between entries (never between a key and its words), at most
/// three rows. The per-tab keys come first, the global keys after a dim dot.
fn footer_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let room = usize::from(width).saturating_sub(2).max(20);
    let entries: Vec<&str> = footer_help(app.tab)
        .split("  ")
        .chain(["·"])
        .chain(GLOBAL_HELP.split("  "))
        .filter(|e| !e.is_empty())
        .collect();
    let mut rows: Vec<String> = vec![String::new()];
    for e in entries {
        let cur = rows.last_mut().expect("one row");
        if !cur.is_empty() && cur.chars().count() + 2 + e.chars().count() > room {
            rows.push(e.to_string());
        } else {
            if !cur.is_empty() {
                cur.push_str("  ");
            }
            cur.push_str(e);
        }
    }
    rows.truncate(3);
    rows.iter()
        .map(|r| prefix(Line::from(hint_spans(r)), " "))
        .collect()
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect, help: Vec<Line<'static>>) {
    let mut lines = vec![Line::from(status_spans(
        app,
        &app.status,
        Style::default(),
        role::OK,
    ))];
    lines.extend(help);
    f.render_widget(Paragraph::new(lines), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// Columns of text a popup of outer width `w` shows: borders and one column of padding each side.
fn text_cols(w: u16) -> usize {
    usize::from(w).saturating_sub(4).max(1)
}

/// Outer width of a popup that wants `pref` columns: never wider than the screen minus a margin.
fn popup_width(screen_w: u16, pref: u16) -> u16 {
    pref.min(screen_w.saturating_sub(4)).max(screen_w.min(24))
}

/// Outer width of the full-height popups (viewer, providers).
pub fn tall_width(screen_w: u16) -> u16 {
    popup_width(screen_w, 120)
}

/// Columns the scrolling popups wrap to on a terminal `screen_w` wide. The key handler and the
/// drawing code both use it, so the scroll limit counts the rows that are drawn.
pub fn tall_cols(screen_w: u16) -> usize {
    text_cols(tall_width(screen_w))
}

type Glyph = (char, Style);

fn cells_line(cells: &[Glyph], pad: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    let mut i = 0;
    while i < cells.len() {
        let st = cells[i].1;
        let mut s = String::new();
        while i < cells.len() && cells[i].1 == st {
            s.push(cells[i].0);
            i += 1;
        }
        spans.push(Span::styled(s, st));
    }
    Line::from(spans)
}

/// Word-wraps one styled line to `width` columns. Continuation rows keep the line's indent, so a
/// wrapped list entry stays visibly one entry. A word longer than the row is cut at the edge.
pub fn wrap_line(line: &Line, width: usize) -> Vec<Line<'static>> {
    let width = width.max(4);
    let cells: Vec<Glyph> = line
        .spans
        .iter()
        .flat_map(|s| {
            let st = line.style.patch(s.style);
            s.content.chars().map(move |c| (c, st))
        })
        .collect();
    if cells.len() <= width {
        return vec![cells_line(&cells, 0)];
    }
    let indent = cells.iter().take_while(|(c, _)| *c == ' ').count();
    let hang = indent.min(width / 2);
    let mut out = Vec::new();
    let (mut start, mut first) = (0, true);
    while start < cells.len() {
        let pad = if first { 0 } else { hang };
        let room = width - pad;
        if cells.len() - start <= room {
            out.push(cells_line(&cells[start..], pad));
            break;
        }
        let end = start + room;
        let floor = start + if first { indent } else { 0 };
        let (stop, next) = match (floor + 1..=end).rev().find(|&i| cells[i].0 == ' ') {
            Some(i) => (i, i + 1),
            None => (end, end),
        };
        out.push(cells_line(&cells[start..stop], pad));
        start = next;
        while start < cells.len() && cells[start].0 == ' ' {
            start += 1;
        }
        first = false;
    }
    out
}

/// Wraps every line; an empty line stays one empty row.
pub fn wrap_lines(lines: &[Line], width: usize) -> Vec<Line<'static>> {
    lines.iter().flat_map(|l| wrap_line(l, width)).collect()
}

/// Plain text as wrapped lines, ready for the viewer.
pub fn wrap_text(text: &str, width: usize) -> Vec<Line<'static>> {
    let lines: Vec<Line> = text.lines().map(|l| Line::raw(l.to_string())).collect();
    wrap_lines(&lines, width)
}

/// The shared popup frame: rounded border in the accent colour, a bold title, and a dim key hint
/// along the bottom edge, so every popup tells where it is and what its keys do.
fn frame(title: &str, hint: &str, accent: Color) -> Block<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut b = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                title.to_string(),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]));
    if !hint.is_empty() {
        b = b.title_bottom(Line::styled(format!(" {hint} "), dim).right_aligned());
    }
    b
}

/// A scroll position for a popup's top-right corner, e.g. `3/40`.
fn position(scroll: u16, rows: usize, shown: usize) -> Line<'static> {
    let end = (usize::from(scroll) + shown).min(rows);
    Line::styled(
        format!(" {end}/{rows} "),
        Style::default().fg(Color::DarkGray),
    )
    .right_aligned()
}

/// Wrapped lines for a short popup, and whether they had to be cut to fit the screen.
fn fit_lines(screen: Rect, w: u16, lines: Vec<Line<'static>>) -> (Rect, Vec<Line<'static>>, bool) {
    let max_rows = usize::from(screen.height.saturating_sub(4)).max(1);
    let cut = lines.len() > max_rows;
    let mut lines = lines;
    if cut {
        lines.truncate(max_rows);
        if let Some(last) = lines.last_mut() {
            *last = Line::styled("…", Style::default().fg(Color::DarkGray));
        }
    }
    let area = centered(screen, w, lines.len() as u16 + 2);
    (area, lines, cut)
}

/// A scrolling popup: wrapped lines inside the frame, with the position in the corner.
fn draw_scroller(f: &mut Frame, title: &str, lines: Vec<Line<'static>>, scroll: u16, hint: &str) {
    let area = centered(
        f.area(),
        tall_width(f.area().width),
        f.area().height.saturating_sub(2),
    );
    f.render_widget(Clear, area);
    let shown = usize::from(area.height.saturating_sub(2));
    let rows = lines.len();
    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .block(frame(title, hint, Color::Cyan).title(position(scroll, rows, shown))),
        area,
    );
}

/// Styled lines for the providers popup: one block per provider.
pub fn perm_lines(perms: &[cli::ProviderPerm]) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let head = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().add_modifier(Modifier::BOLD);
    let mut out: Vec<Line> = Vec::new();
    if perms.is_empty() {
        out.push(Line::styled("No providers are attached.", dim));
    }
    for (i, p) in perms.iter().enumerate() {
        if i > 0 {
            out.push(Line::default());
        }
        let mut title = vec![Span::styled(format!("\u{25a0} {}", p.name), head)];
        if p.name != p.ty {
            title.push(Span::styled(format!("  type {}", p.ty), dim));
        }
        if !p.category.is_empty() {
            title.push(Span::styled(format!("  [{}]", p.category), dim));
        }
        out.push(Line::from(title));
        if let Some(e) = &p.error {
            out.push(Line::styled(
                format!("  {e}"),
                Style::default().fg(Color::Red),
            ));
            continue;
        }
        if !p.description.is_empty() {
            out.push(Line::styled(format!("  {}", p.description), dim));
        }
        out.push(Line::from(Span::styled("  Credentials", label)));
        if p.credentials.is_empty() {
            out.push(Line::styled("    none", dim));
        }
        for c in &p.credentials {
            out.push(Line::from(format!("    {c}")));
        }
        out.push(Line::from(Span::styled("  Network", label)));
        if p.endpoints.is_empty() {
            out.push(Line::styled("    none", dim));
        }
        for e in &p.endpoints {
            let (access, color) = match e.access.as_str() {
                "read-only" => ("read-only", Color::Green),
                "read-write" => ("read-write", Color::Yellow),
                "full" => ("full", Color::Red),
                "" => ("custom rules", Color::Yellow),
                other => (other, Color::Reset),
            };
            out.push(Line::from(vec![
                Span::raw(format!("    {:<34}", format!("{}:{}", e.host, e.port))),
                Span::styled(format!("{access:<13}"), Style::default().fg(color)),
                Span::styled(format!("{} {}", e.protocol, e.enforcement), dim),
            ]));
            for r in &e.rules {
                let writes =
                    !(r.starts_with("GET ") || r.starts_with("HEAD ") || r.starts_with("OPTIONS "));
                let color = if writes { Color::Yellow } else { Color::Green };
                out.push(Line::styled(
                    format!("        {r}"),
                    Style::default().fg(color),
                ));
            }
        }
        out.push(Line::from(Span::styled("  Binaries", label)));
        if p.binaries.is_empty() {
            out.push(Line::styled("    any", dim));
        }
        for b in &p.binaries {
            out.push(Line::from(format!("    {b}")));
        }
    }
    out
}

/// One help row: the key in a fixed bold column, then what it does.
fn key_row(key: &str, what: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {key:<12}"),
            role::accent().add_modifier(Modifier::BOLD),
        ),
        Span::raw(what.to_string()),
    ])
}

fn section(title: &str) -> Line<'static> {
    Line::styled(title.to_string(), role::bold())
}

/// The help screen: the current tab's keys first, then the ones that work everywhere, then the
/// popups, the shell menu and the palette.
pub fn help_lines(tab: Tab) -> Vec<Line<'static>> {
    use crate::palette::{Scope, ACTIONS};
    let name = TABS.iter().find(|(t, _)| *t == tab).map_or("", |(_, n)| *n);
    let mut out = vec![section(&format!("{name} tab"))];
    out.extend(
        ACTIONS
            .iter()
            .filter(|x| x.scope == Scope::Tab(tab))
            .map(|x| key_row(x.key, x.title)),
    );
    out.push(Line::default());
    out.push(section("Everywhere on the dashboard"));
    out.push(key_row("Tab", "Next tab (Shift-Tab goes back)"));
    out.push(key_row("j / k", "Move down / up (arrow keys too)"));
    out.extend(
        ACTIONS
            .iter()
            .filter(|x| {
                x.scope == Scope::Global
                    && !x.key.is_empty()
                    && !matches!(x.key, "1" | "2" | "3" | "4" | "5")
            })
            .map(|x| key_row(x.key, x.title)),
    );
    out.push(key_row("1-5", "Go to a tab"));
    out.push(key_row(
        ": or Ctrl-P",
        "Command palette: type to find any action",
    ));
    out.push(Line::default());
    out.push(section(
        "Only in the command palette (press : and type part of the name)",
    ));
    out.extend(
        ACTIONS
            .iter()
            .filter(|x| x.scope == Scope::Global && x.key.is_empty())
            .map(|x| key_row("", x.title)),
    );
    out.push(Line::default());
    out.push(section("In a shell"));
    out.push(key_row(
        "Ctrl-]",
        "Session menu: providers, blocked requests, toolchain hosts, port forwards, alerts, close",
    ));
    out.push(Line::default());
    out.push(section("In lists and popups"));
    out.push(key_row("j / k", "Move; Enter chooses, Esc cancels"));
    out.push(key_row("Space", "Mark an entry in a multiple-choice list"));
    out.push(key_row(
        "y / n",
        "Answer a confirmation; v shows all of a long one",
    ));
    out.push(key_row("g / G", "Top / bottom of a scrolling popup"));
    out.push(Line::default());
    out.push(section("In the log view"));
    out.push(key_row("/", "Search; n / N next and previous match"));
    out.push(key_row("f", "Follow the end of the log"));
    out.push(key_row("s", "Change the log source"));
    out.push(key_row("h / l", "Pan sideways; q closes"));
    out.push(Line::default());
    out.push(section("In the editor"));
    for (k, what) in crate::editor::KEYS {
        out.push(key_row(k, what));
    }
    out
}

fn draw_modal(f: &mut Frame, m: &Modal, tab: Tab) {
    let screen = f.area();
    match m {
        Modal::Help { scroll } => {
            let lines = wrap_lines(&help_lines(tab), tall_cols(screen.width));
            draw_scroller(f, "Keys", lines, *scroll, "j/k scroll · ? or Esc close");
        }
        Modal::Palette { query, cursor } => {
            let hits = crate::palette::filter(tab, query);
            let w = popup_width(screen.width, 76);
            let cols = text_cols(w);
            let show = hits
                .len()
                .min(usize::from(screen.height.saturating_sub(8)).clamp(1, 10));
            let top = cursor.saturating_sub(show.saturating_sub(1));
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("› ", role::accent().add_modifier(Modifier::BOLD)),
                    Span::raw(query.clone()),
                    Span::styled("█", role::accent()),
                ]),
                Line::styled("─".repeat(cols), role::dim()),
            ];
            if hits.is_empty() {
                lines.push(Line::styled("No action matches", role::dim()));
            }
            for (i, (x, at)) in hits.iter().enumerate().skip(top).take(show) {
                let sel = i == *cursor;
                let key = format!(" {} ", x.key);
                let title_room = cols.saturating_sub(key.chars().count() + 4);
                let mut spans = vec![Span::styled(if sel { "▌ " } else { "  " }, role::accent())];
                for (n, c) in x.title.chars().take(title_room).enumerate() {
                    let mut st = Style::default();
                    if at.contains(&n) {
                        st = st.fg(role::ACCENT).add_modifier(Modifier::BOLD);
                    } else if sel {
                        st = st.add_modifier(Modifier::BOLD);
                    }
                    spans.push(Span::styled(c.to_string(), st));
                }
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                spans.push(Span::raw(
                    " ".repeat(cols.saturating_sub(used + key.chars().count())),
                ));
                spans.push(Span::styled(key, role::dim()));
                lines.push(Line::from(spans));
            }
            let area = centered(screen, w, lines.len() as u16 + 2);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(lines).block(frame(
                    "Commands",
                    "↑↓ move · Enter run · Esc close",
                    role::ACCENT,
                )),
                area,
            );
        }
        Modal::Task(t) => {
            let Ok(t) = t.lock() else { return };
            let elapsed = t.started.elapsed();
            // A job that ends at once never flashes a popup.
            if t.total == 0 && elapsed < std::time::Duration::from_millis(400) {
                return;
            }
            let w = popup_width(screen.width, 64);
            let cols = text_cols(w);
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let ms = elapsed.as_millis();
            let spin = FRAMES[(ms / 80) as usize % FRAMES.len()];
            let accent = Style::default().fg(Color::Cyan);
            let dim = Style::default().fg(Color::DarkGray);
            let mut head = vec![
                Span::styled(spin, Style::default().fg(Color::Yellow)),
                Span::raw(" Working"),
            ];
            head.push(Span::styled(format!(" · {}s", elapsed.as_secs()), dim));
            let mut lines = vec![Line::from(head), Line::default()];
            lines.extend(wrap_text(&t.title, cols).into_iter().map(|l| {
                Line::from(
                    l.spans
                        .into_iter()
                        .map(|s| {
                            Span::styled(s.content, Style::default().add_modifier(Modifier::BOLD))
                        })
                        .collect::<Vec<_>>(),
                )
            }));
            lines.push(Line::default());
            if t.total == 0 {
                // No known end: a short bar sweeps back and forth.
                let seg = (cols / 5).max(3);
                let span = cols.saturating_sub(seg).max(1);
                let step = (ms / 40) as usize % (span * 2);
                let at = if step < span { step } else { span * 2 - step };
                lines.push(Line::from(vec![
                    Span::styled("─".repeat(at), dim),
                    Span::styled("━".repeat(seg), accent),
                    Span::styled("─".repeat(cols.saturating_sub(at + seg)), dim),
                ]));
            } else {
                let count = format!("{}/{}", t.done, t.total);
                let bar_w = cols.saturating_sub(count.len() + 2).max(4);
                let filled = (bar_w * t.done.min(t.total))
                    .checked_div(t.total)
                    .unwrap_or(0);
                lines.push(Line::from(vec![
                    Span::styled("━".repeat(filled), accent),
                    Span::styled("─".repeat(bar_w - filled), dim),
                    Span::raw(format!("  {count}")),
                ]));
                lines.push(Line::styled(
                    wrap_text(&t.current, cols)
                        .first()
                        .map(plain_text)
                        .unwrap_or_default(),
                    dim,
                ));
            }
            let area = centered(screen, w, lines.len() as u16 + 2);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(lines).block(frame(
                    "Working",
                    "Esc hides, the work goes on",
                    Color::Cyan,
                )),
                area,
            );
        }
        Modal::Providers {
            sandbox,
            perms,
            scroll,
        } => {
            let lines = wrap_lines(&perm_lines(perms), tall_cols(screen.width));
            draw_scroller(
                f,
                &format!("Providers of {sandbox}"),
                lines,
                *scroll,
                "j/k PgUp/PgDn g/G scroll · Esc close",
            );
        }
        Modal::Editor { ed, .. } => crate::editor::draw(f, screen, ed),
        // Drawn by `draw_ext_edit`, over the whole screen.
        Modal::ExtEdit { .. } => {}
        Modal::Viewer {
            title,
            text,
            scroll,
            ..
        } => {
            let lines = wrap_text(text, tall_cols(screen.width));
            draw_scroller(
                f,
                title,
                lines,
                *scroll,
                "j/k PgUp/PgDn g/G scroll · Esc back",
            );
        }
        Modal::Info(t) => {
            let w = popup_width(screen.width, 76);
            let (area, lines, cut) = fit_lines(screen, w, mark_report(wrap_text(t, text_cols(w))));
            f.render_widget(Clear, area);
            let hint = if cut {
                "v read all · any other key closes"
            } else {
                "any key closes"
            };
            f.render_widget(
                Paragraph::new(lines).block(frame("Info", hint, Color::Cyan)),
                area,
            );
        }
        Modal::Confirm { text, .. } => {
            let w = popup_width(screen.width, 84);
            let (area, lines, cut) = fit_lines(screen, w, wrap_text(text, text_cols(w)));
            f.render_widget(Clear, area);
            let hint = if cut {
                "y yes · n no · v read all"
            } else {
                "y yes · n no · v details"
            };
            f.render_widget(
                Paragraph::new(lines).block(frame("Confirm", hint, Color::Yellow)),
                area,
            );
        }
        Modal::Input {
            title, buf, mask, ..
        } => {
            let w = popup_width(screen.width, 72.max(title.chars().count() as u16 + 6));
            let area = centered(screen, w, 3);
            f.render_widget(Clear, area);
            let shown = if *mask {
                "•".repeat(buf.chars().count())
            } else {
                buf.clone()
            };
            // Keep the cursor in view: show the tail of a line longer than the box.
            let cols = text_cols(w).saturating_sub(1);
            let n = shown.chars().count();
            let tail: String = shown.chars().skip(n.saturating_sub(cols)).collect();
            f.render_widget(
                Paragraph::new(format!("{tail}█")).block(frame(
                    title,
                    "Enter ok · Esc cancel",
                    Color::Cyan,
                )),
                area,
            );
        }
        Modal::Select {
            title,
            items,
            cursor,
            marked,
            multi,
            ..
        } => {
            let widest = items
                .iter()
                .map(|i| i.chars().count() + 8)
                .chain([title.chars().count() + 6])
                .max()
                .unwrap_or(0);
            let w = popup_width(screen.width, (widest as u16).clamp(48, 100));
            // "› " marker column, then the item; wrapped rows hang under the first character.
            let cols = text_cols(w).saturating_sub(2);
            let li: Vec<ListItem> = items
                .iter()
                .zip(marked)
                .map(|(i, m)| {
                    let mark = match (*multi, *m) {
                        (true, true) => "[x] ",
                        (true, false) => "[ ] ",
                        _ => "",
                    };
                    let hang = " ".repeat(mark.len());
                    let lines: Vec<Line> = wrap_text(&format!("{mark}{i}"), cols)
                        .into_iter()
                        .enumerate()
                        .map(|(n, l)| if n == 0 { l } else { prefix(l, &hang) })
                        .collect();
                    ListItem::new(lines)
                })
                .collect();
            let rows: usize = li.iter().map(|i| i.height()).sum();
            let area = centered(screen, w, (rows as u16 + 2).min(screen.height));
            f.render_widget(Clear, area);
            let hint = if *multi {
                "↑↓ move · Space mark · Enter ok · Esc cancel"
            } else {
                "↑↓ move · Enter choose · Esc cancel"
            };
            let list = List::new(li)
                .highlight_style(highlight())
                .highlight_symbol("› ")
                .block(frame(title, hint, Color::Cyan));
            f.render_stateful_widget(
                list,
                area,
                &mut ListState::default().with_selected(Some(*cursor)),
            );
        }
    }
}

fn plain_text(l: &Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Colours the leading mark of a report line: ✓ green, ✗ red, • yellow.
fn mark_report(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|l| {
            let text = plain_text(&l);
            let color = match text.chars().next() {
                Some('\u{2713}') => Color::Green,
                Some('\u{2717}') => Color::Red,
                Some('\u{2022}') => Color::Yellow,
                _ => return l,
            };
            let rest: String = text.chars().skip(1).collect();
            Line::from(vec![
                Span::styled(
                    text.chars().take(1).collect::<String>(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(rest),
            ])
        })
        .collect()
}

fn prefix(line: Line<'static>, pad: &str) -> Line<'static> {
    let mut spans = vec![Span::raw(pad.to_string())];
    spans.extend(line.spans);
    Line::from(spans)
}

/// The user's editor fills the screen above a status bar naming the file; after a failed check
/// a box says why and offers to reopen the editor or discard the edits.
fn draw_ext_edit(f: &mut Frame, app: &App, run: &crate::exted::Run) {
    let area = f.area();
    let pane = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let bar = Rect::new(area.x, pane.bottom(), area.width, area.height - pane.height);
    let prog = run.editor.program();
    if let Some(s) = &run.session {
        if let Some(p) = s.vt.render(pane, f.buffer_mut()) {
            f.set_cursor_position(p);
        }
    } else {
        let w = popup_width(area.width, 76);
        let mut lines = wrap_text(
            run.error.as_deref().unwrap_or("The check failed"),
            text_cols(w),
        );
        lines.push(Line::raw(""));
        lines.extend(wrap_text(
            &format!("Your edits are kept. Enter or r opens them in {prog} again; d discards them and leaves {} as it was.", run.name),
            text_cols(w),
        ));
        let lines: Vec<Line> = lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    l.style(Style::default().fg(role::ERR))
                } else {
                    l
                }
            })
            .collect();
        let r = centered(area, w, lines.len() as u16 + 2);
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(lines).block(frame(
                &format!("{} is not valid: not saved", run.name),
                "Enter/r reopen · d discard",
                role::ERR,
            )),
            r,
        );
    }
    let base = Style::default()
        .fg(Color::Indexed(250))
        .bg(Color::Indexed(236));
    let mut spans = vec![
        Span::styled(
            format!(" {} ", run.name),
            base.fg(role::ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {prog} ({}) ", run.editor.from), base),
    ];
    if let Some(m) = app.flash().filter(|m| !m.is_empty() && *m != "Ready") {
        spans.extend(status_spans(app, m, base, Color::Green));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(base), bar);
    let hint = "save and quit the editor: osh checks, then writes ";
    let w = (hint.chars().count() as u16).min(bar.width);
    f.render_widget(
        Paragraph::new(hint).style(base.fg(Color::Indexed(244))),
        Rect::new(bar.right() - w, bar.y, w, bar.height),
    );
}

/// The shell fills the screen above a one-row status bar; the cursor is shown only with focus.
fn draw_session(f: &mut Frame, app: &App, s: &Session) {
    let focus = app.modal.is_none();
    let area = f.area();
    let pane = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let bar = Rect::new(area.x, pane.bottom(), area.width, area.height - pane.height);
    if let Some(p) = s.vt.render(pane, f.buffer_mut()) {
        if focus {
            f.set_cursor_position(p);
        }
    }
    let base = Style::default()
        .fg(Color::Indexed(250))
        .bg(Color::Indexed(236));
    let mut spans = vec![Span::styled(
        format!(" {} ", s.sandbox),
        base.fg(role::ACCENT).add_modifier(Modifier::BOLD),
    )];
    let title = s.vt.title();
    if let Some(branch) = crate::term::branch_of(&title) {
        spans.push(Span::styled(
            format!(" {branch} "),
            base.fg(Color::Indexed(176)).add_modifier(Modifier::BOLD),
        ));
    }
    let title = crate::term::shown_title(&title);
    if !title.is_empty() {
        spans.push(Span::styled(format!(" {title} "), base));
    }
    let back = s.vt.scrolled_back();
    if back > 0 {
        spans.push(Span::styled(
            format!(" [scrollback -{back}, type to return] "),
            base.fg(Color::Indexed(111)),
        ));
    }
    if let Some(m) = app.flash().filter(|m| !m.is_empty() && *m != "Ready") {
        spans.extend(status_spans(app, m, base, Color::Green));
    }
    f.render_widget(Paragraph::new(Line::from(spans.clone())).style(base), bar);
    // Its own rectangle: a style on a paragraph repaints the whole area, which would wipe the
    // colours of the name and branch drawn on the left.
    let hint = "Ctrl-] menu ";
    let w = (hint.chars().count() as u16).min(bar.width);
    f.render_widget(
        Paragraph::new(hint).style(base.fg(Color::Indexed(244))),
        Rect::new(bar.right() - w, bar.y, w, bar.height),
    );
    // The sandbox's CPU, memory and disk use, right-aligned before the hint, in what the left
    // side leaves free (a long status hides it rather than being overwritten).
    if let Some(u) = app.usage_of(&s.sandbox, &s.gateway) {
        let left: usize = spans.iter().map(|x| x.width()).sum();
        let free = usize::from(bar.width).saturating_sub(left + usize::from(w) + 2);
        let shown = usage_spans(u, free, base);
        let uw: u16 = shown.iter().map(|x| x.width() as u16).sum();
        if uw > 0 {
            f.render_widget(
                Paragraph::new(Line::from(shown)),
                Rect::new(bar.right() - w - uw - 1, bar.y, uw, bar.height),
            );
        }
    }
}

/// A log line with its classes coloured: the time dim, the source tag by who wrote it (sandbox
/// orange, gateway violet, anything else cyan), the level by severity, other tags dim, and the
/// message red or yellow on an error or a warning.
pub fn log_line(l: &str) -> Line<'static> {
    let dim = role::dim();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = l;
    // A leading `YYYY-MM-DD HH:MM:SS` stamp.
    if rest.len() >= 19
        && rest.is_char_boundary(19)
        && rest.as_bytes()[4] == b'-'
        && rest.as_bytes()[10] == b' '
    {
        spans.push(Span::styled(rest[..19].to_string(), dim));
        rest = &rest[19..];
    }
    let mut msg = Style::default();
    let mut first_tag = true;
    loop {
        let t = rest.trim_start();
        let lead = &rest[..rest.len() - t.len()];
        let Some(inner) = t.strip_prefix('[').and_then(|x| x.split_once(']')) else {
            spans.push(Span::raw(rest.to_string()));
            break;
        };
        let (name, after) = inner;
        let tag = format!("[{name}]");
        let word = name.trim();
        let style = match word {
            "ERROR" | "FATAL" => {
                msg = Style::default().fg(role::ERR);
                Style::default().fg(role::ERR).add_modifier(Modifier::BOLD)
            }
            "WARN" | "WARNING" => {
                msg = Style::default().fg(role::WARN);
                Style::default().fg(role::WARN).add_modifier(Modifier::BOLD)
            }
            "INFO" | "DEBUG" | "TRACE" => dim,
            _ if first_tag => match word {
                "sandbox" => Style::default().fg(role::BUSY),
                "gateway" => Style::default().fg(Color::Indexed(141)),
                _ => Style::default().fg(role::ACCENT),
            },
            _ => dim,
        };
        first_tag = false;
        spans.push(Span::raw(lead.to_string()));
        spans.push(Span::styled(tag, style));
        rest = after;
        if !after.trim_start().starts_with('[') {
            spans.push(Span::styled(after.to_string(), msg));
            break;
        }
    }
    Line::from(spans)
}

fn draw_logview(f: &mut Frame, lv: &LogView) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());
    let height = rows[0].height.saturating_sub(2) as usize;
    lv.height.set(height);
    let (lines, total): (Vec<Line>, usize) = {
        let Ok(b) = lv.buf.lock() else { return };
        let total = b.lines.len();
        let top = if lv.follow {
            total.saturating_sub(height)
        } else {
            lv.top.min(total.saturating_sub(height))
        };
        let lines = b
            .lines
            .iter()
            .enumerate()
            .skip(top)
            .take(height)
            .map(|(i, l)| {
                let style = if lv.current == Some(i) {
                    highlight()
                } else if lv.matches(l) {
                    Style::default().fg(Color::Black).bg(Color::Yellow)
                } else {
                    Style::default()
                };
                if style == Style::default() {
                    log_line(l)
                } else {
                    Line::styled(l.clone(), style)
                }
            })
            .collect();
        (lines, total)
    };
    let top = if lv.follow {
        total.saturating_sub(height)
    } else {
        lv.top.min(total.saturating_sub(height))
    };
    let mode = if lv.follow { "following" } else { "paused" };
    f.render_widget(
        Paragraph::new(lines).scroll((0, lv.left)).block(
            panel(&format!("Logs · {}", lv.name), None).title(
                Line::from(vec![
                    Span::styled(
                        format!(" {mode} "),
                        Style::default().fg(if lv.follow { role::OK } else { role::WARN }),
                    ),
                    Span::styled(
                        format!(
                            "{} · {}/{total} ",
                            lv.source_name(),
                            (top + height).min(total)
                        ),
                        role::dim(),
                    ),
                ])
                .right_aligned(),
            ),
        ),
        rows[0],
    );
    let bar = match &lv.typing {
        Some(t) => Line::from(format!("/{t}\u{2588}")),
        None => Line::from(hint_spans(&if lv.query.is_empty() {
            " /  search  s source  j/k PgUp/PgDn scroll  g/G top/bottom  f follow  h/l pan  q close"
                .to_string()
        } else {
            format!(" /{}  n/N next/prev  Esc clear  q close", lv.query)
        })),
    };
    f.render_widget(Paragraph::new(bar), rows[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn plain(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrapping_breaks_at_words_and_keeps_the_indent() {
        let out = wrap_line(&Line::raw("    alpha beta gamma delta"), 16);
        let rows: Vec<String> = out.iter().map(plain).collect();
        assert_eq!(rows, ["    alpha beta", "    gamma delta"]);
        assert!(rows.iter().all(|r| r.chars().count() <= 16));
    }

    #[test]
    fn a_word_longer_than_the_row_is_cut_and_nothing_is_lost() {
        let out = wrap_line(&Line::raw("x".repeat(25)), 10);
        let joined: String = out.iter().map(plain).collect();
        assert_eq!(joined, "x".repeat(25));
        assert!(out.iter().all(|l| plain(l).chars().count() <= 10));
    }

    #[test]
    fn empty_lines_stay_and_short_lines_are_untouched() {
        let rows = wrap_text("one\n\ntwo", 40);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn the_progress_popup_shows_count_and_current_host() {
        let mut task = crate::app::Task::new("Opening", 4);
        task.done = 1;
        task.current = "pypi.org".into();
        let t = std::sync::Arc::new(std::sync::Mutex::new(task));
        let m = Modal::Task(t);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| draw_modal(f, &m, Tab::Sandboxes)).unwrap();
        let all: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(all.contains("1/4") && all.contains("pypi.org") && all.contains("Opening"));
    }

    #[test]
    fn log_lines_colour_by_source_and_level_and_keep_every_char() {
        let l = "2026-11-10 13:53:55 [sandbox] [OCSF ] [ocsf] EVENT [INFO] ssh relay open";
        let line = log_line(l);
        assert_eq!(plain(&line), l);
        let fg = |needle: &str| {
            line.spans
                .iter()
                .find(|s| s.content.contains(needle))
                .and_then(|s| s.style.fg)
        };
        assert_eq!(fg("[sandbox]"), Some(role::BUSY));
        let g = log_line("2026-11-10 13:53:58 [gateway] [WARN ] [x] failed");
        let gw = g.spans.iter().find(|s| s.content == "[gateway]").unwrap();
        assert_eq!(gw.style.fg, Some(Color::Indexed(141)));
        let w = g.spans.iter().find(|s| s.content == "[WARN ]").unwrap();
        assert_eq!(w.style.fg, Some(role::WARN));
        assert_eq!(plain(&log_line("no tags here")), "no tags here");
        assert_eq!(plain(&log_line("é[x")), "é[x");
    }

    fn usage_app(driver: &str) -> App {
        let (wake, _w) = std::sync::mpsc::channel();
        let (done, _d) = std::sync::mpsc::channel();
        let mut app = App::new(
            std::path::PathBuf::from("."),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
            wake,
            done,
        );
        app.snap.gateway = "gw".into();
        app.snap.driver = driver.into();
        app.snap.sandboxes = vec![cli::Sandbox {
            name: "sb".into(),
            phase: "Ready".into(),
            ..Default::default()
        }];
        app
    }

    fn reading(mem_avail_kb: u64) -> crate::usage::Usage {
        crate::usage::Usage {
            cpu: Some(12.0),
            mem: Some(crate::usage::Mem {
                total_kb: 2 * 1024 * 1024,
                avail_kb: mem_avail_kb,
            }),
            disks: vec![],
        }
    }

    fn screen(app: &App, w: u16, h: u16) -> (String, ratatui::buffer::Buffer) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        let all = buf.content().iter().map(|c| c.symbol()).collect();
        (all, buf)
    }

    #[test]
    fn the_details_show_the_selected_sandboxs_usage_and_what_the_totals_are() {
        let mut app = usage_app("vm");
        let (all, _) = screen(&app, 120, 30);
        assert!(all.contains("reading…"), "before the first reading");
        // 1.8 of 2.0 GiB used: 90%, warned in yellow.
        app.usage = Some(("sb".into(), "gw".into(), reading(200 * 1024)));
        let (all, buf) = screen(&app, 120, 30);
        assert!(
            all.contains("cpu 12%") && all.contains("mem 1.8/2.0 GiB"),
            "{all}"
        );
        assert!(all.contains("gateway's VM size"));
        let m = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width - 4).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buf[(x, y)].symbol() == "m"
                    && buf[(x + 1, y)].symbol() == "e"
                    && buf[(x + 4, y)].symbol() == "1"
            })
            .unwrap();
        assert_eq!(buf[m].fg, role::WARN);
        // Another gateway's sandbox of the same name shows no figures.
        app.usage = Some(("sb".into(), "other".into(), reading(0)));
        assert!(!screen(&app, 120, 30).0.contains("cpu 12%"));
        // Tiny screens draw without panicking.
        app.usage = Some(("sb".into(), "gw".into(), reading(0)));
        for (w, h) in [(20, 8), (40, 10), (1, 1)] {
            screen(&app, w, h);
        }
        let docker = usage_app("docker");
        let mut d = docker;
        d.usage = Some(("sb".into(), "gw".into(), reading(0)));
        assert!(screen(&d, 120, 30).0.contains("shared by all sandboxes"));
    }

    #[test]
    fn an_empty_tab_keeps_its_column_header_above_the_next_step() {
        let mut app = usage_app("docker");
        app.snap.sandboxes.clear();
        app.snap.gateways.clear();
        let cases = [
            (Tab::Sandboxes, "PHASE", "No sandboxes yet"),
            (Tab::Providers, "CREDENTIAL KEYS", "No providers"),
            (Tab::Profiles, "ON GATEWAY", "No profiles"),
            (Tab::Images, "DOCKERFILE", "No images"),
            (Tab::Gateways, "GATEWAY", "No gateways"),
        ];
        for (tab, header, what) in cases {
            app.tab = tab;
            let (_, buf) = screen(&app, 140, 30);
            let rows: Vec<String> = (0..buf.area.height)
                .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
                .collect();
            let h = rows.iter().position(|r| r.contains(header));
            let m = rows.iter().position(|r| r.contains(what));
            assert!(
                matches!((h, m), (Some(h), Some(m)) if h < m),
                "{header} above {what}\n{}",
                rows.join("\n")
            );
        }
        // Small screens draw without panicking.
        for (w, h) in [(20, 8), (1, 1)] {
            screen(&app, w, h);
        }
    }

    #[test]
    fn usage_spans_colour_by_level_and_never_exceed_the_width() {
        let u = reading(0);
        let s = usage_spans(&u, 80, Style::default());
        let red = s.iter().find(|x| x.content.starts_with("mem")).unwrap();
        assert_eq!(red.style.fg, Some(role::ERR));
        for w in 0..40 {
            let n: usize = usage_spans(&u, w, Style::default())
                .iter()
                .map(|x| x.width())
                .sum();
            assert!(n <= w);
        }
    }

    #[test]
    fn long_popups_fit_every_width_and_show_all_the_text() {
        let text = "word ".repeat(80);
        for w in [30u16, 60, 92, 140] {
            let app_modal = Modal::Info(text.clone());
            let mut term = Terminal::new(TestBackend::new(w, 24)).unwrap();
            term.draw(|f| draw_modal(f, &app_modal, Tab::Sandboxes))
                .unwrap();
            let buf = term.backend().buffer().clone();
            let shown: usize = buf.content().iter().filter(|c| c.symbol() == "w").count();
            assert!(shown > 0, "width {w}");
            // The popup stays inside the margin: first and last columns are blank.
            for y in 0..24 {
                assert_eq!(buf[(0, y)].symbol(), " ", "width {w} row {y}");
                assert_eq!(buf[(w - 1, y)].symbol(), " ", "width {w} row {y}");
            }
        }
    }
}

#[cfg(test)]
mod help_tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn palette_only_actions_are_listed_without_a_key_and_none_shows_a_colon() {
        let rows = text(&help_lines(Tab::Sandboxes));
        let at = rows
            .iter()
            .position(|r| r.starts_with("Only in the command palette"))
            .expect("a palette-only section");
        let row = rows
            .iter()
            .skip(at + 1)
            .find(|r| r.contains("toolchains.toml"))
            .expect("the toolchains action");
        assert_eq!(
            row.trim_start().chars().next(),
            Some('E'),
            "no key column text: {row}"
        );
        assert!(
            !rows.iter().any(|r| r.starts_with("  :  ")),
            "no row may claim ':' is the key of an action"
        );
    }
}
