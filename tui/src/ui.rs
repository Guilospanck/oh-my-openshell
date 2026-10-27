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
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
    },
    Frame,
};

fn phase_style(phase: &str) -> Style {
    let c = match phase {
        "Ready" => Color::Green,
        "Provisioning" | "Starting" | "Stopping" => Color::Yellow,
        "Error" => Color::Red,
        "Completed" => Color::Cyan,
        _ => Color::Gray,
    };
    Style::default().fg(c)
}

/// The status as spans: an orange spinner while work runs, red on a failure, `done` once finished.
fn status_spans(app: &App, text: &str, base: Style, done: Color) -> Vec<Span<'static>> {
    let bold = Modifier::BOLD;
    if let Some(frame) = app.spinner() {
        let c = Color::Indexed(208);
        return vec![
            Span::styled(format!(" {frame} "), base.fg(c).add_modifier(bold)),
            Span::styled(format!("{text} "), base.fg(c)),
        ];
    }
    let failed = ["Error", "Step failed", "Cancelled"]
        .iter()
        .any(|p| text.starts_with(p));
    let (mark, c) = if failed {
        ("✗", Color::Red)
    } else {
        ("✓", done)
    };
    vec![
        Span::styled(format!(" {mark} "), base.fg(c).add_modifier(bold)),
        Span::styled(format!("{text} "), base.fg(c)),
    ]
}

fn highlight() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

pub fn draw(f: &mut Frame, app: &App) {
    if let Some(s) = app.session() {
        draw_session(f, app, s);
        if let Some(m) = &app.modal {
            draw_modal(f, m);
        }
        return;
    }
    if let Some(lv) = &app.logview {
        draw_logview(f, lv);
        return;
    }
    let banner = app.expiry_banner();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(u16::from(banner.is_some())),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(f.area());
    draw_tabs(f, app, rows[0]);
    if let Some(b) = banner {
        f.render_widget(
            Paragraph::new(Line::styled(
                format!(" ⚠ {b}"),
                Style::default().fg(Color::Yellow),
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
    draw_footer(f, app, rows[3]);
    if let Some(m) = &app.modal {
        draw_modal(f, m);
    }
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(
        " osh ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for (i, (tab, name)) in TABS.iter().enumerate() {
        let style = if *tab == app.tab {
            highlight()
        } else {
            Style::default()
        };
        spans.push(Span::styled(format!(" {} {name} ", i + 1), style));
    }
    let (txt, color) = if app.snap.gateway_ok {
        let driver = if app.snap.driver.is_empty() {
            String::new()
        } else {
            format!(" [{}]", app.snap.driver)
        };
        (
            format!(
                "  {}{driver} connected  {}",
                app.snap.gateway, app.snap.version
            ),
            Color::Green,
        )
    } else {
        (format!("  {} unreachable", app.snap.gateway), Color::Red)
    };
    spans.push(Span::styled(txt, Style::default().fg(color)));
    if app.config_stale {
        spans.push(Span::styled(
            "  gateway.toml out of date (osh setup)",
            Style::default().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn table_state(i: usize) -> TableState {
    TableState::default().with_selected(Some(i))
}

fn draw_sandboxes(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);
    let rows: Vec<Row> = app
        .snap
        .sandboxes
        .iter()
        .map(|s| {
            let name = if app.has_session(&s.name) {
                format!("● {}", s.name)
            } else {
                s.name.clone()
            };
            Row::new(vec![name, s.phase.clone(), s.created.clone()]).style(phase_style(&s.phase))
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Min(14),
            Constraint::Length(13),
            Constraint::Length(19),
        ],
    )
    .header(
        Row::new(vec!["NAME", "PHASE", "CREATED"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(highlight())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Sandboxes (live) "),
    );
    f.render_stateful_widget(table, cols[0], &mut table_state(app.cursor[0]));

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(cols[1]);
    let mut detail: Vec<Line> = Vec::new();
    match app.selected_sandbox() {
        Some(s) => {
            detail.push(Line::from(vec![
                Span::raw("phase: "),
                Span::styled(s.phase.clone(), phase_style(&s.phase)),
            ]));
            if let Some((_, l)) = app.snap.limits.as_ref().filter(|(n, _)| n == &s.name) {
                detail.push(Line::from(format!("limits: {l}")));
            }
            for c in &s.conditions {
                let ok = if c.status == "True" {
                    Color::Green
                } else {
                    Color::Red
                };
                detail.push(Line::from(vec![
                    Span::styled(format!("{:<18}", c.kind), Style::default().fg(ok)),
                    Span::raw(c.message.clone()),
                ]));
            }
        }
        None => detail.push(Line::from("No sandboxes. Press n to create one.")),
    }
    f.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" Details ")),
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
    let tail = lines[lines.len().saturating_sub(height)..]
        .iter()
        .map(|l| crate::logview::humanize(l))
        .collect::<Vec<_>>()
        .join("\n");
    f.render_widget(
        Paragraph::new(tail).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Logs (tail; l: full view) "),
        ),
        right[1],
    );
}

fn draw_providers(f: &mut Frame, app: &App, area: Rect) {
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
                        format!("{}: {}", t.repo, ghrepo::expiry_note(now, t.expiry_ms)),
                        ghrepo::days_left(now, t.expiry_ms) <= ghrepo::WARN_DAYS,
                    )
                });
            let (note, soon) = note.unwrap_or_default();
            let row = Row::new(vec![p.name.clone(), p.ty.clone(), p.keys.join(", "), note]);
            if soon {
                row.style(Style::default().fg(Color::Yellow))
            } else {
                row
            }
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(28),
            Constraint::Length(24),
            Constraint::Length(20),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec!["NAME", "TYPE", "CREDENTIAL KEYS", "GITHUB TOKEN"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(highlight())
    .block(Block::default().borders(Borders::ALL).title(" Providers "));
    f.render_stateful_widget(table, area, &mut table_state(app.cursor[1]));
}

fn draw_profiles(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .profile_rows()
        .into_iter()
        .map(|r| {
            let gw = r
                .gateway_version
                .map_or("not imported".to_string(), |v| format!("v{v}"));
            let src = if r.in_repo {
                "providers/"
            } else {
                "gateway only"
            };
            Row::new(vec![r.id, gw, src.to_string()])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(20),
            Constraint::Length(16),
            Constraint::Min(14),
        ],
    )
    .header(
        Row::new(vec!["PROFILE", "ON GATEWAY", "SOURCE"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(highlight())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Profiles (u: import or update from repo) "),
    );
    f.render_stateful_widget(table, area, &mut table_state(app.cursor[2]));
}

fn draw_images(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .snap
        .images
        .iter()
        .map(|i| {
            let (s, c) = match (i.built, i.freshness) {
                (false, _) => ("not built", Color::Yellow),
                (true, Freshness::UpToDate) => ("up to date", Color::Green),
                (true, Freshness::Outdated) => ("outdated", Color::Red),
                (true, Freshness::Unverified) => ("unverified", Color::Yellow),
            };
            Row::new(vec![
                i.tag.clone(),
                s.to_string(),
                i.file
                    .file_name()
                    .map(|n| n.to_string_lossy().into())
                    .unwrap_or_default(),
            ])
            .style(Style::default().fg(c))
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(28),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec!["IMAGE", "STATE", "DOCKERFILE"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(highlight())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Images (b: build; outdated = Dockerfile changed since build) "),
    );
    f.render_stateful_widget(table, area, &mut table_state(app.cursor[3]));
}

fn draw_gateways(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .snap
        .gateways
        .iter()
        .map(|g| {
            let cur = g.name == app.snap.gateway;
            let info = app.gateway_info(&g.name, &g.endpoint);
            let name = if info.main {
                format!("{}  main", g.name)
            } else {
                g.name.clone()
            };
            let row = Row::new(vec![
                name,
                info.kind,
                info.port,
                if cur {
                    "selected".to_string()
                } else {
                    String::new()
                },
            ]);
            if cur {
                row.style(Style::default().fg(Color::Green))
            } else {
                row
            }
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Min(28),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["GATEWAY", "TYPE", "PORT", "STATE"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(highlight())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Gateways (a: add  x: remove  d: wipe) "),
    );
    f.render_stateful_widget(table, area, &mut table_state(app.cursor[4]));
}

/// The per-tab key hints in the footer.
pub fn footer_help(tab: Tab) -> &'static str {
    match tab {
        Tab::Sandboxes => "Enter shell (● running)  l logs  p blocked  n new  d delete  s stop  S start  a/x attach/detach provider  K skills  t renew GitHub token",
        Tab::Providers => "n create  u update credentials (renews GitHub repo tokens)  d delete",
        Tab::Profiles => "u / Enter import or update from providers/",
        Tab::Images => "b / Enter build",
        Tab::Gateways => "Enter select  a add  d wipe (delete all data)  x remove",
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let help = footer_help(app.tab);
    let lines = vec![
        Line::from(status_spans(
            app,
            &app.status,
            Style::default(),
            Color::Green,
        )),
        Line::styled(
            format!("{help}  |  Tab/1-5 switch  j/k move  g gateway  r refresh  q quit"),
            Style::default().fg(Color::DarkGray),
        ),
    ];
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

/// A box that fits `text` (wrapped to `w` columns) when it can; the flag says it did not fit
/// and the text should be read in the viewer.
fn fit(screen: Rect, w: u16, text: &str) -> (Rect, bool) {
    let inner = w.min(screen.width).saturating_sub(2).max(1) as usize;
    let rows: usize = text
        .lines()
        .map(|l| l.chars().count().max(1).div_ceil(inner))
        .sum();
    let want = rows as u16 + 5;
    (centered(screen, w, want), want > screen.height)
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

fn draw_modal(f: &mut Frame, m: &Modal) {
    match m {
        Modal::Providers {
            sandbox,
            perms,
            scroll,
        } => {
            let area = centered(f.area(), 110, f.area().height.saturating_sub(2));
            f.render_widget(Clear, area);
            let lines = perm_lines(perms);
            let at = (*scroll as usize + 1).min(lines.len().max(1));
            f.render_widget(
                Paragraph::new(lines.clone()).scroll((*scroll, 0)).block(
                    Block::default().borders(Borders::ALL).title(format!(
                        " Providers of {sandbox} {at}/{}  j/k PgUp/PgDn g/G scroll, Esc close ",
                        lines.len()
                    )),
                ),
                area,
            );
        }
        Modal::Viewer {
            title,
            text,
            scroll,
            ..
        } => {
            let area = centered(f.area(), 100, f.area().height.saturating_sub(2));
            f.render_widget(Clear, area);
            let total = text.lines().count();
            let at = (*scroll as usize + 1).min(total.max(1));
            f.render_widget(
                Paragraph::new(text.as_str()).scroll((*scroll, 0)).block(
                    Block::default().borders(Borders::ALL).title(format!(
                        " {title} {at}/{total}  j/k PgUp/PgDn g/G scroll, Esc back "
                    )),
                ),
                area,
            );
        }
        Modal::Info(t) => {
            let (area, more) = fit(f.area(), 70, t);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!(
                    "{t}\n\n{}",
                    if more {
                        "v: scroll. Any other key closes."
                    } else {
                        "Press any key."
                    }
                ))
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title(" Info ")),
                area,
            );
        }
        Modal::Confirm { text, .. } => {
            let (area, more) = fit(f.area(), 90, text);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!(
                    "{text}\n(y/n{})",
                    if more { ", v: scroll" } else { "" }
                ))
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" Confirm ")),
                area,
            );
        }
        Modal::Input {
            title, buf, mask, ..
        } => {
            let area = centered(f.area(), 70.max(title.chars().count() as u16 + 4), 3);
            f.render_widget(Clear, area);
            let shown = if *mask {
                "•".repeat(buf.chars().count())
            } else {
                buf.clone()
            };
            f.render_widget(
                Paragraph::new(format!("{shown}█")).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {title} ")),
                ),
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
                .chain([title.chars().count() + 4])
                .max()
                .unwrap_or(0);
            let area = centered(
                f.area(),
                (widest as u16).clamp(60, 100),
                items.len() as u16 + 4,
            );
            f.render_widget(Clear, area);
            let li: Vec<ListItem> = items
                .iter()
                .zip(marked)
                .map(|(i, m)| {
                    let mark = if *multi {
                        if *m {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    } else {
                        ""
                    };
                    ListItem::new(format!("{mark}{i}"))
                })
                .collect();
            let list = List::new(li).highlight_style(highlight()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} ")),
            );
            f.render_stateful_widget(
                list,
                area,
                &mut ListState::default().with_selected(Some(*cursor)),
            );
        }
    }
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
    let base = Style::default().fg(Color::Black).bg(Color::Gray);
    let mut spans = vec![Span::styled(
        format!(" {} ", s.sandbox),
        base.add_modifier(Modifier::BOLD),
    )];
    let title = s.vt.title();
    if !title.is_empty() {
        spans.push(Span::styled(format!(" {title} "), base));
    }
    let back = s.vt.scrolled_back();
    if back > 0 {
        spans.push(Span::styled(
            format!(" [scrollback -{back}, type to return] "),
            base.fg(Color::Blue),
        ));
    }
    if let Some(m) = app.flash().filter(|m| !m.is_empty() && *m != "Ready") {
        spans.extend(status_spans(app, m, base, Color::Green));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(base), bar);
    f.render_widget(
        Paragraph::new("Ctrl-] menu ")
            .style(base)
            .alignment(Alignment::Right),
        bar,
    );
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
                Line::styled(l.clone(), style)
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
            Block::default().borders(Borders::ALL).title(format!(
                " Logs: {}  source: {}  [{mode}]  {}/{total} ",
                lv.name,
                lv.source_name(),
                (top + height).min(total)
            )),
        ),
        rows[0],
    );
    let bar = match &lv.typing {
        Some(t) => Line::from(format!("/{t}\u{2588}")),
        None => Line::styled(
            if lv.query.is_empty() {
                "/ search  s source  j/k PgUp/PgDn scroll  g/G top/bottom  f follow  h/l pan  q close".to_string()
            } else {
                format!("/{}  n/N next/prev  Esc clear  q close", lv.query)
            },
            Style::default().fg(Color::DarkGray),
        ),
    };
    f.render_widget(Paragraph::new(bar), rows[1]);
}
