//! Rendering.
use crate::app::{App, Modal, Tab, TABS};
use crate::cli::Freshness;
use crate::logview::LogView;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap},
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

fn highlight() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

pub fn draw(f: &mut Frame, app: &App) {
    if let Some(lv) = &app.logview {
        draw_logview(f, lv);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(5), Constraint::Length(2)])
        .split(f.area());
    draw_tabs(f, app, rows[0]);
    match app.tab {
        Tab::Sandboxes => draw_sandboxes(f, app, rows[1]),
        Tab::Providers => draw_providers(f, app, rows[1]),
        Tab::Profiles => draw_profiles(f, app, rows[1]),
        Tab::Images => draw_images(f, app, rows[1]),
        Tab::Gateways => draw_gateways(f, app, rows[1]),
    }
    draw_footer(f, app, rows[2]);
    if let Some(m) = &app.modal {
        draw_modal(f, m);
    }
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" osh ", Style::default().add_modifier(Modifier::BOLD))];
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
        (format!("  {}{driver} connected  {}", app.snap.gateway, app.snap.version), Color::Green)
    } else {
        (format!("  {} unreachable", app.snap.gateway), Color::Red)
    };
    spans.push(Span::styled(txt, Style::default().fg(color)));
    let (link, link_color) = if app.linked {
        ("linked in PATH ", Color::Green)
    } else {
        ("not linked (just link) ", Color::Yellow)
    };
    if app.config_stale {
        spans.push(Span::styled("  gateway.toml out of date (just install)", Style::default().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    f.render_widget(
        Paragraph::new(Line::styled(link, Style::default().fg(link_color))).alignment(Alignment::Right),
        area,
    );
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
            Row::new(vec![s.name.clone(), s.phase.clone(), s.created.clone()])
                .style(phase_style(&s.phase))
        })
        .collect();
    let table = Table::new(
        rows,
        [Constraint::Min(14), Constraint::Length(13), Constraint::Length(19)],
    )
    .header(Row::new(vec!["NAME", "PHASE", "CREATED"]).style(Style::default().add_modifier(Modifier::BOLD)))
    .row_highlight_style(highlight())
    .block(Block::default().borders(Borders::ALL).title(" Sandboxes (live) "));
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
                let ok = if c.status == "True" { Color::Green } else { Color::Red };
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
    let tail = lines[lines.len().saturating_sub(height)..].join("\n");
    f.render_widget(
        Paragraph::new(tail).block(Block::default().borders(Borders::ALL).title(" Logs (tail; l: full view) ")),
        right[1],
    );
}

fn draw_providers(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .snap
        .providers
        .iter()
        .map(|p| Row::new(vec![p.name.clone(), p.ty.clone(), p.keys.join(", ")]))
        .collect();
    let table = Table::new(rows, [Constraint::Length(20), Constraint::Length(20), Constraint::Min(20)])
        .header(Row::new(vec!["NAME", "TYPE", "CREDENTIAL KEYS"]).style(Style::default().add_modifier(Modifier::BOLD)))
        .row_highlight_style(highlight())
        .block(Block::default().borders(Borders::ALL).title(" Providers "));
    f.render_stateful_widget(table, area, &mut table_state(app.cursor[1]));
}

fn draw_profiles(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .profile_rows()
        .into_iter()
        .map(|r| {
            let gw = r.gateway_version.map_or("not imported".to_string(), |v| format!("v{v}"));
            let src = if r.in_repo { "providers/" } else { "gateway only" };
            Row::new(vec![r.id, gw, src.to_string()])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(20), Constraint::Length(16), Constraint::Min(14)])
        .header(Row::new(vec!["PROFILE", "ON GATEWAY", "SOURCE"]).style(Style::default().add_modifier(Modifier::BOLD)))
        .row_highlight_style(highlight())
        .block(Block::default().borders(Borders::ALL).title(" Profiles (u: import or update from repo) "));
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
                i.file.file_name().map(|n| n.to_string_lossy().into()).unwrap_or_default(),
            ])
            .style(Style::default().fg(c))
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(28), Constraint::Length(12), Constraint::Min(20)])
        .header(Row::new(vec!["IMAGE", "STATE", "DOCKERFILE"]).style(Style::default().add_modifier(Modifier::BOLD)))
        .row_highlight_style(highlight())
        .block(Block::default().borders(Borders::ALL).title(" Images (b: build; outdated = Dockerfile changed since build) "));
    f.render_stateful_widget(table, area, &mut table_state(app.cursor[3]));
}

fn draw_gateways(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .snap
        .gateways
        .iter()
        .map(|g| {
            let cur = g.name == app.snap.gateway;
            let row = Row::new(vec![
                g.name.clone(),
                g.endpoint.clone(),
                if cur { "selected".to_string() } else { String::new() },
            ]);
            if cur { row.style(Style::default().fg(Color::Green)) } else { row }
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(20), Constraint::Min(30), Constraint::Length(10)])
        .header(Row::new(vec!["GATEWAY", "ENDPOINT", "STATE"]).style(Style::default().add_modifier(Modifier::BOLD)))
        .row_highlight_style(highlight())
        .block(Block::default().borders(Borders::ALL).title(" Gateways (d: wipe) "));
    f.render_stateful_widget(table, area, &mut table_state(app.cursor[4]));
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let help = match app.tab {
        Tab::Sandboxes => "Enter attach  l logs  n new  d delete  s stop  S start  a/x attach/detach provider",
        Tab::Providers => "n create  u update credentials  d delete",
        Tab::Profiles => "u / Enter import or update from providers/",
        Tab::Images => "b / Enter build",
        Tab::Gateways => "Enter select  d wipe (delete all data)",
    };
    let lines = vec![
        Line::from(app.status.clone()),
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
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn draw_modal(f: &mut Frame, m: &Modal) {
    match m {
        Modal::Info(t) => {
            let area = centered(f.area(), 70, 7);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!("{t}\n\nPress any key."))
                    .wrap(Wrap { trim: true })
                    .block(Block::default().borders(Borders::ALL).title(" Info ")),
                area,
            );
        }
        Modal::Confirm { text, .. } => {
            let h = text.lines().count() as u16 + 4;
            let area = centered(f.area(), 90, h);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!("{text}\n(y/n)"))
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title(" Confirm ")),
                area,
            );
        }
        Modal::Input { title, buf, mask, .. } => {
            let area = centered(f.area(), 70.max(title.chars().count() as u16 + 4), 3);
            f.render_widget(Clear, area);
            let shown = if *mask { "•".repeat(buf.chars().count()) } else { buf.clone() };
            f.render_widget(
                Paragraph::new(format!("{shown}█"))
                    .block(Block::default().borders(Borders::ALL).title(format!(" {title} "))),
                area,
            );
        }
        Modal::Select { title, items, cursor, marked, multi, .. } => {
            let area = centered(f.area(), 60, items.len() as u16 + 4);
            f.render_widget(Clear, area);
            let li: Vec<ListItem> = items
                .iter()
                .zip(marked)
                .map(|(i, m)| {
                    let mark = if *multi { if *m { "[x] " } else { "[ ] " } } else { "" };
                    ListItem::new(format!("{mark}{i}"))
                })
                .collect();
            let list = List::new(li)
                .highlight_style(highlight())
                .block(Block::default().borders(Borders::ALL).title(format!(" {title} ")));
            f.render_stateful_widget(list, area, &mut ListState::default().with_selected(Some(*cursor)));
        }
    }
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
        let top = if lv.follow { total.saturating_sub(height) } else { lv.top.min(total.saturating_sub(height)) };
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
    let top = if lv.follow { total.saturating_sub(height) } else { lv.top.min(total.saturating_sub(height)) };
    let mode = if lv.follow { "following" } else { "paused" };
    f.render_widget(
        Paragraph::new(lines).scroll((0, lv.left)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Logs: {}  source: {}  [{mode}]  {}/{total} ", lv.name, lv.source_name(), (top + height).min(total))),
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
