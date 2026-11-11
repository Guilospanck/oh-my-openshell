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
fn head(cols: &[String]) -> Row<'static> {
    Row::new(cols.to_vec())
        .style(role::dim().add_modifier(Modifier::BOLD))
        .bottom_margin(1)
}

/// Columns of each tab's table: the header and the width below which the column is dropped rather
/// than squeezed (the last columns go first). Shared by the empty state so an empty tab still
/// shows which list it is.
type Cols = &'static [(&'static str, usize)];
const SANDBOX_COLS: Cols = &[("NAME", 8), ("PHASE", 9), ("CREATED", 19)];
const PROVIDER_COLS: Cols = &[
    ("NAME", 8),
    ("TYPE", 6),
    ("CREDENTIAL KEYS", 10),
    ("GITHUB TOKEN", 12),
];
const PROFILE_COLS: Cols = &[("PROFILE", 8), ("ON GATEWAY", 10), ("SOURCE", 10)];
const IMAGE_COLS: Cols = &[("IMAGE", 10), ("STATE", 9), ("DOCKERFILE", 10)];
const GATEWAY_COLS: Cols = &[("GATEWAY", 10), ("TYPE", 6), ("PORT", 5), ("STATE", 10)];

/// Column widths for `cells` in `avail` columns (one column of space between): each column gets
/// what its widest cell needs while there is room; short of room, later columns are dropped
/// before a column is squeezed under its minimum. Returns one width per column kept.
fn fit_widths(cols: Cols, cells: &[Vec<Line<'static>>], avail: usize) -> Vec<usize> {
    let content = |i: usize| {
        cells
            .iter()
            .filter_map(|r| r.get(i))
            .map(Line::width)
            .chain([text_width(cols[i].0)])
            .max()
            .unwrap_or(0)
    };
    let floor = |i: usize| cols[i].1.min(content(i));
    let mut keep = cols.len();
    while keep > 1 && (0..keep).map(floor).sum::<usize>() + keep - 1 > avail {
        keep -= 1;
    }
    let mut widths: Vec<usize> = (0..keep).map(floor).collect();
    let mut extra = avail.saturating_sub(widths.iter().sum::<usize>() + keep - 1);
    for (i, w) in widths.iter_mut().enumerate() {
        let add = extra.min(content(i).saturating_sub(*w));
        *w += add;
        extra -= add;
    }
    if let Some(last) = widths.last_mut() {
        *last += extra;
        if keep == 1 {
            *last = (*last).min(avail);
        }
    }
    widths
}

/// The selected row's every column in full, for the bottom of a table whose cells had to be cut
/// or whose columns were dropped: moving the cursor shows any row whole.
fn full_row(cols: Cols, row: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let label = cols.iter().map(|(n, _)| text_width(n)).max().unwrap_or(0) + 2;
    let mut out = vec![Line::styled(
        format!(
            "── selected row in full {}",
            "─".repeat(width.saturating_sub(25))
        ),
        role::dim(),
    )];
    for ((name, _), cell) in cols.iter().zip(row) {
        let mut spans = vec![Span::styled(format!("{name:<label$}"), role::dim())];
        spans.extend(cell.spans.iter().cloned());
        for (i, l) in wrap_line(&Line::from(spans), width).into_iter().enumerate() {
            out.push(if i == 0 {
                l
            } else {
                prefix(l, &" ".repeat(label))
            });
        }
    }
    out
}

/// A tab's table in a panel titled `title`. Cells too wide for their column end in `…`; then
/// (unless `elsewhere`, a details pane that already shows the selected row whole) the bottom of the
/// panel shows the selected row in full, so no value is out of reach.
fn draw_table(
    f: &mut Frame,
    area: Rect,
    title: &str,
    cols: Cols,
    cells: Vec<Vec<Line<'static>>>,
    cursor: usize,
    elsewhere: bool,
) {
    let n = cells.len();
    let block = panel(title, Some((cursor, n)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = usize::from(inner.width);
    let widths = fit_widths(cols, &cells, width);
    let keep = widths.len();
    let cut = cells.iter().any(|r| {
        r.iter().enumerate().any(|(i, c)| {
            if i < keep {
                c.width() > widths[i]
            } else {
                c.width() > 0
            }
        })
    });
    let mut strip = match cells.get(cursor) {
        Some(row) if cut && !elsewhere => full_row(cols, row, width),
        _ => Vec::new(),
    };
    // The table keeps at least its header and one row.
    let room = usize::from(inner.height).saturating_sub(3);
    if strip.len() > room {
        strip.truncate(room);
    }
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(strip.len() as u16)])
        .split(inner);
    let names: Vec<String> = cols[..keep]
        .iter()
        .zip(&widths)
        .map(|((name, _), w)| ellipsize(name, *w))
        .collect();
    let rows: Vec<Row> = cells
        .into_iter()
        .map(|r| {
            Row::new(
                r.into_iter()
                    .take(keep)
                    .zip(&widths)
                    .map(|(c, w)| Cell::from(fit_line(&c, *w).0))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let t = Table::new(rows, widths.iter().map(|w| Constraint::Length(*w as u16)))
        .column_spacing(1)
        .header(head(&names))
        .row_highlight_style(row_highlight());
    f.render_stateful_widget(t, parts[0], &mut table_state(cursor));
    f.render_widget(Paragraph::new(strip), parts[1]);
}

/// What a list says when it is empty: the table's header stays (the columns tell which list
/// this is), and below it the next step, centred and wrapped to the panel.
fn empty_state(f: &mut Frame, area: Rect, title: &str, cols: Cols, what: &str, hint: &str) {
    draw_table(f, area, title, cols, Vec::new(), 0, true);
    let inner = area.inner(ratatui::layout::Margin::new(2, 1));
    let w = usize::from(inner.width);
    let mut lines: Vec<Line> = wrap_text(what, w)
        .into_iter()
        .map(|l| l.style(role::bold()))
        .collect();
    lines.push(Line::default());
    lines.extend(wrap_text(hint, w).into_iter().map(|l| l.style(role::dim())));
    let h = (lines.len() as u16).min(inner.height);
    let at = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(h) / 2,
        inner.width,
        h,
    );
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), at);
}

/// A coloured dot and a word, for a state.
fn dot(text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("● ", style),
        Span::styled(text.to_string(), style),
    ])
}

/// The newest alert in a box at the top-right, over whatever else is drawn. Red for high. A
/// detail too long for the box ends in `…`; `!` lists the alerts in full.
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
    let mut lines: Vec<Line> = wrap_text(&a.title, cols)
        .into_iter()
        .map(|l| l.style(role::bold()))
        .collect();
    lines.extend(wrap_text(&a.detail, cols));
    let max = usize::from((area.height / 2).max(5)) - 2;
    if lines.len() > max {
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            *last = Line::styled("… ! lists it in full", role::dim());
        }
    }
    let h = lines.len() as u16 + 2;
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

/// The warnings above the tabs' content, each wrapped to the screen.
fn banner_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut warn = |text: String| {
        for l in wrap_text(&text, width.saturating_sub(3)) {
            let first = out.is_empty();
            out.push(prefix(
                l.style(Style::default().fg(role::WARN)),
                if first { " ⚠ " } else { "   " },
            ));
        }
    };
    if let Some(b) = app.expiry_banner() {
        warn(b);
    }
    if app.config_stale {
        warn("gateway.toml out of date (R on Gateways installs it)".into());
    }
    out
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
    let width = f.area().width;
    let banner = banner_lines(app, usize::from(width));
    let help = footer_lines(app, width);
    let status = status_lines(app, usize::from(width));
    // Every key stays on screen: a terminal too small for the keys and a few rows of content
    // says so instead of hiding any of them.
    let need = 1 + banner.len() + 5 + status.len() + help.len();
    if usize::from(f.area().height) < need
        || !hints_fit(&footer_entries(app.tab), usize::from(width))
    {
        draw_too_small(f, f.area());
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(banner.len() as u16),
            Constraint::Min(5),
            Constraint::Length((status.len() + help.len()) as u16),
        ])
        .split(f.area());
    draw_tabs(f, app, rows[0]);
    f.render_widget(Paragraph::new(banner), rows[1]);
    match app.tab {
        Tab::Sandboxes => draw_sandboxes(f, app, rows[2]),
        Tab::Providers => draw_providers(f, app, rows[2]),
        Tab::Profiles => draw_profiles(f, app, rows[2]),
        Tab::Images => draw_images(f, app, rows[2]),
        Tab::Gateways => draw_gateways(f, app, rows[2]),
    }
    let mut lines = status;
    lines.extend(help);
    f.render_widget(Paragraph::new(lines), rows[3]);
    if let Some(m) = &app.modal {
        draw_modal(f, m, app.tab);
    }
}

/// The status line wrapped to the screen: at most four rows, a longer one ends in an explicit `…`.
fn status_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let line = Line::from(status_spans(app, &app.status, Style::default(), role::OK));
    let mut rows = wrap_line(&line, width.max(4));
    if rows.len() > 4 {
        rows.truncate(4);
        let last = rows.pop().unwrap_or_default();
        let (l, cut) = fit_line(&last, width.saturating_sub(1));
        let mut spans = l.spans;
        if !cut {
            spans.push(Span::raw("…"));
        }
        rows.push(Line::from(spans));
    }
    rows
}

/// The top bar: the app name, the tabs (the current one bold and underlined in the accent colour),
/// and on the right the gateway as one dot and its name. On a narrow screen the gateway line drops
/// its version, then its driver; then the other tabs show only their number (Tab and 1-5 still
/// reach them, `?` names them), so nothing overlaps or is cut.
fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let tabs = |short: bool, name: bool| {
        let mut spans = Vec::new();
        if name {
            spans.push(Span::styled(" osh ", role::bold().fg(role::ACCENT)));
        }
        for (i, (tab, title)) in TABS.iter().enumerate() {
            if *tab == app.tab {
                spans.push(Span::styled(
                    format!(" {} {title} ", i + 1),
                    role::accent()
                        .add_modifier(Modifier::BOLD)
                        .add_modifier(Modifier::UNDERLINED),
                ));
            } else if short {
                spans.push(Span::styled(format!(" {} ", i + 1), role::dim()));
            } else {
                spans.push(Span::styled(format!(" {} {title} ", i + 1), role::dim()));
            }
        }
        Line::from(spans)
    };
    let gw = &app.snap.gateway;
    let (variants, color): (Vec<String>, Color) = if app.awaiting_snapshot {
        let g = if gw.is_empty() {
            String::new()
        } else {
            format!(" {gw}")
        };
        (
            vec![format!("connecting{g}..."), "connecting...".into()],
            role::WARN,
        )
    } else if app.snap.gateway_ok {
        let driver = if app.snap.driver.is_empty() {
            String::new()
        } else {
            format!(" [{}]", app.snap.driver)
        };
        (
            vec![
                format!("{gw}{driver} connected  {}", app.snap.version),
                format!("{gw}{driver} connected"),
                format!("{gw} connected"),
                gw.clone(),
            ],
            role::OK,
        )
    } else {
        (vec![format!("{gw} unreachable"), gw.clone()], role::ERR)
    };
    let right_of = |txt: &str| {
        Line::from(vec![
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(format!("{txt} "), Style::default().fg(color)),
        ])
    };
    let w = usize::from(area.width);
    let mut chosen = None;
    // The gateway's name outranks the tab names, which outrank the version and driver.
    let named = |v: &String| gw.is_empty() || v.contains(gw.as_str());
    'pick: for (short, name, any) in [
        (false, true, false),
        (true, true, false),
        (true, false, false),
        (true, false, true),
    ] {
        let left = tabs(short, name);
        for v in variants.iter().filter(|v| any || named(v)) {
            let right = right_of(v);
            if left.width() + 1 + right.width() <= w {
                chosen = Some((left, right));
                break 'pick;
            }
        }
    }
    let (left, right) = chosen.unwrap_or_else(|| (tabs(true, false), right_of("")));
    let rw = (right.width() as u16).min(area.width);
    f.render_widget(Paragraph::new(left), area);
    f.render_widget(
        Paragraph::new(right),
        Rect::new(area.right() - rw, area.y, rw, area.height),
    );
}

fn table_state(i: usize) -> TableState {
    TableState::default().with_selected(Some(i))
}

/// A label and its value, labels aligned in a dim column; a value too long for `width` columns
/// wraps under its own column.
fn kv(label: &str, value: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    wrap_line(&Line::from(value), width.max(8))
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let lab = if i == 0 { label } else { "" };
            let mut spans = vec![Span::styled(format!("{lab:<12}"), role::dim())];
            spans.extend(l.spans);
            Line::from(spans)
        })
        .collect()
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
        let cells: Vec<Vec<Line<'static>>> = app
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
                vec![
                    name,
                    dot(&s.phase, phase_style(&s.phase)),
                    Line::styled(s.created.clone(), role::dim()),
                ]
            })
            .collect();
        // The details pane shows the selected sandbox's name, phase and creation time in full.
        draw_table(
            f,
            cols[0],
            "Sandboxes",
            SANDBOX_COLS,
            cells,
            app.cursor[0],
            true,
        );
    }

    let dcols = usize::from(cols[1].width).saturating_sub(4 + 12).max(8);
    let mut detail: Vec<Line> = Vec::new();
    match app.selected_sandbox() {
        Some(s) => {
            detail.extend(kv("name", vec![Span::raw(s.name.clone())], dcols));
            detail.extend(kv(
                "phase",
                dot(&s.phase, phase_style(&s.phase)).spans,
                dcols,
            ));
            if !s.created.is_empty() {
                detail.extend(kv("created", vec![Span::raw(s.created.clone())], dcols));
            }
            if s.phase == "Ready" {
                match app.usage_of(&s.name, &app.snap.gateway) {
                    Some(u) => {
                        detail.extend(kv("usage", usage_spans(u, dcols, Style::default()), dcols));
                        for l in wrap_text(totals_note(&app.snap.driver), dcols) {
                            detail.push(prefix(l.style(role::dim()), &" ".repeat(12)));
                        }
                    }
                    None => detail.extend(kv(
                        "usage",
                        vec![Span::styled("reading…", role::dim())],
                        dcols,
                    )),
                }
            }
            if let Some((_, l)) = app.snap.limits.as_ref().filter(|(n, _)| n == &s.name) {
                detail.extend(kv("limits", vec![Span::raw(l.clone())], dcols));
            }
            for c in &s.conditions {
                let (mark, color) = if c.status == "True" {
                    ("✓", role::OK)
                } else {
                    ("✗", role::ERR)
                };
                // The message wraps under its own column, not under the label.
                let msg = wrap_text(&c.message, dcols.saturating_sub(2).max(8));
                let label = ellipsize(&c.kind, 11);
                for (i, m) in msg.into_iter().enumerate() {
                    let mut spans = if i == 0 {
                        vec![
                            Span::styled(format!("{label:<12}"), role::dim()),
                            Span::styled(format!("{mark} "), Style::default().fg(color)),
                        ]
                    } else {
                        vec![Span::raw(" ".repeat(14))]
                    };
                    spans.extend(m.spans);
                    detail.push(Line::from(spans));
                }
                if c.message.is_empty() {
                    detail.push(Line::from(vec![
                        Span::styled(format!("{label:<12}"), role::dim()),
                        Span::styled(mark, Style::default().fg(color)),
                    ]));
                }
            }
        }
        None => detail.push(Line::styled("Nothing selected.", role::dim())),
    }
    // The details take the rows they need; the logs get the rest when that is at least four
    // rows, else the logs pane gives way (`l` shows the logs whole). Rows past the pane on a
    // tiny screen are counted in its last row, never dropped without a word.
    let total = cols[1].height;
    let floor = total * 35 / 100;
    let want = (detail.len() as u16 + 2).max(floor);
    let dh = if want + 4 <= total { want } else { total };
    let shown = usize::from(dh.saturating_sub(2));
    if detail.len() > shown && shown > 0 {
        let more = detail.len() - shown + 1;
        detail.truncate(shown - 1);
        detail.push(Line::styled(
            format!("… {more} more rows: a taller window shows them"),
            role::dim(),
        ));
    }
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(dh), Constraint::Min(0)])
        .split(cols[1]);
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
    // Long log lines wrap; `l` opens the full view with the history.
    let height = right[1].height.saturating_sub(2) as usize;
    let lw = usize::from(right[1].width.saturating_sub(4));
    let lines: Vec<&str> = log_text.lines().collect();
    let wrapped: Vec<Line> = lines[lines.len().saturating_sub(height)..]
        .iter()
        .flat_map(|l| wrap_line(&log_line(&crate::logview::humanize(l)), lw))
        .collect();
    let tail: Vec<Line> = wrapped[wrapped.len().saturating_sub(height)..].to_vec();
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
    let cells: Vec<Vec<Line<'static>>> = app
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
            vec![
                Line::from(p.name.clone()),
                Line::styled(p.ty.clone(), role::dim()),
                Line::from(p.keys.join(", ")),
                Line::styled(
                    note,
                    if soon {
                        Style::default().fg(role::WARN)
                    } else {
                        role::dim()
                    },
                ),
            ]
        })
        .collect();
    draw_table(
        f,
        area,
        "Providers",
        PROVIDER_COLS,
        cells,
        app.cursor[1],
        false,
    );
}

fn draw_profiles(f: &mut Frame, app: &App, area: Rect) {
    let cells: Vec<Vec<Line<'static>>> = app
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
            vec![
                Line::from(r.id),
                Line::styled(gw, gs),
                Line::styled(src.to_string(), role::dim()),
            ]
        })
        .collect();
    if cells.is_empty() {
        let hint = format!(
            "n creates one; the files live in {}",
            app.dir.join("providers").display()
        );
        empty_state(f, area, "Profiles", PROFILE_COLS, "No profiles", &hint);
        return;
    }
    draw_table(
        f,
        area,
        "Profiles",
        PROFILE_COLS,
        cells,
        app.cursor[2],
        false,
    );
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
    let cells: Vec<Vec<Line<'static>>> = app
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
            vec![
                Line::from(i.tag.clone()),
                dot(s, Style::default().fg(c)),
                Line::styled(
                    i.file
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    role::dim(),
                ),
            ]
        })
        .collect();
    draw_table(f, area, "Images", IMAGE_COLS, cells, app.cursor[3], false);
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
    let cells: Vec<Vec<Line<'static>>> = app
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
            vec![
                Line::from(name),
                Line::styled(info.kind, role::dim()),
                Line::styled(info.port, role::dim()),
                if cur {
                    dot("selected", Style::default().fg(role::OK))
                } else {
                    Line::default()
                },
            ]
        })
        .collect();
    draw_table(
        f,
        area,
        "Gateways",
        GATEWAY_COLS,
        cells,
        app.cursor[4],
        false,
    );
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

/// The entries of a help string (`key words` apart by two spaces).
pub fn hint_entries(help: &str) -> Vec<&str> {
    help.split("  ")
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .collect()
}

/// Display width of a string, as the terminal draws it.
pub fn text_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Key hint `entries` packed into as many rows of `width` columns as they need. A row breaks
/// only between entries, never inside one, so every key is shown whole with its words; the bar
/// grows instead of hiding a key. The first row starts `lead` columns in (a prompt before the
/// hints). An entry wider than a whole row gets a row to itself (only on screens narrower than
/// any entry, which the callers replace with a "terminal too small" line).
pub fn pack_hints(entries: &[&str], width: usize, lead: usize) -> Vec<String> {
    let mut rows: Vec<String> = vec![String::new()];
    let mut used = lead;
    for e in entries {
        let ew = text_width(e);
        let empty = rows.last().is_some_and(String::is_empty);
        let gap = if empty { 0 } else { 2 };
        if (empty && used == 0) || used + gap + ew <= width {
            let cur = rows.last_mut().expect("a row");
            if gap > 0 {
                cur.push_str("  ");
            }
            cur.push_str(e);
            used += gap + ew;
        } else {
            rows.push(e.to_string());
            used = ew;
        }
    }
    rows
}

/// Key hint rows as styled lines, each after a one-column margin.
pub fn hint_lines(entries: &[&str], width: usize) -> Vec<Line<'static>> {
    pack_hints(entries, width.saturating_sub(1), 0)
        .iter()
        .map(|r| prefix(Line::from(hint_spans(r)), " "))
        .collect()
}

/// Whether every entry fits on a row of `width` columns (after the one-column margin); narrower
/// than that, a screen says it is too small rather than cutting a key.
pub fn hints_fit(entries: &[&str], width: usize) -> bool {
    entries.iter().all(|e| text_width(e) < width)
}

/// The one line drawn instead of a screen too small to show its text and every key whole.
pub const TOO_SMALL: &str = "Terminal too small: enlarge it to see every key";

/// `TOO_SMALL` (wrapped) over the whole of `area`.
pub fn draw_too_small(f: &mut Frame, area: Rect) {
    f.render_widget(Clear, area);
    let lines: Vec<Line> = wrap_text(TOO_SMALL, usize::from(area.width))
        .into_iter()
        .map(|l| l.style(Style::default().fg(role::WARN).add_modifier(Modifier::BOLD)))
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

/// `line` cut to `width` columns with an explicit `…` at the end when it is longer, and whether it
/// was cut. Callers show the full text elsewhere (a details pane, the selected row in full).
pub fn fit_line(line: &Line<'static>, width: usize) -> (Line<'static>, bool) {
    if line.width() <= width {
        return (line.clone(), false);
    }
    let room = width.saturating_sub(1);
    let mut used = 0;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last = line.style;
    'outer: for s in &line.spans {
        let mut out = String::new();
        last = line.style.patch(s.style);
        for c in s.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + cw > room {
                spans.push(Span::styled(out, last));
                break 'outer;
            }
            used += cw;
            out.push(c);
        }
        spans.push(Span::styled(out, last));
    }
    if width > 0 {
        spans.push(Span::styled("…", last));
    }
    (Line::from(spans), true)
}

/// A plain string cut with `…` to `width` columns.
pub fn ellipsize(s: &str, width: usize) -> String {
    plain_text(&fit_line(&Line::raw(s.to_string()), width).0)
}

/// Test check for a drawn screen (`rows` of cell symbols): every key hint entry (`key words`) of
/// `entries` is drawn whole on one row, unless the screen says it is too small. Nothing else is
/// accepted: no overflow entry, no cut piece at a row's end (the user's `... copy/cut/paste  Es`).
#[cfg(test)]
pub fn assert_hints_whole(rows: &[String], entries: &[&str]) {
    let screen = rows.join("\n");
    let flat = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.contains(TOO_SMALL) {
        return;
    }
    for e in entries {
        assert!(
            rows.iter().any(|r| r.contains(e)),
            "hint {e:?} is not drawn whole:\n{screen}"
        );
    }
}

/// The key help in as many rows as it needs, breaking between entries (never between a key and
/// its words). The per-tab keys come first, the global keys after a dim dot.
fn footer_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    hint_lines(&footer_entries(app.tab), usize::from(width))
}

/// Every entry of the dashboard footer of `tab`.
pub fn footer_entries(tab: Tab) -> Vec<&'static str> {
    hint_entries(footer_help(tab))
        .into_iter()
        .chain(["·"])
        .chain(hint_entries(GLOBAL_HELP))
        .collect()
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
    wrap_hang(line, width, None)
}

/// Word-wraps like `wrap_line`, with continuation rows indented `hang` columns (under a table's
/// last column, say) instead of the line's own indent.
pub fn wrap_hang(line: &Line, width: usize, hang: Option<usize>) -> Vec<Line<'static>> {
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
    let hang = match hang {
        Some(h) => h.min(width.saturating_sub(8)),
        None => indent.min(width / 2),
    };
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
        .padding(Padding::horizontal(1));
    if !title.is_empty() {
        b = b.title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                title.to_string(),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]));
    }
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
    let pos = position(scroll, rows, shown);
    let w = usize::from(area.width);
    // The title keeps the top edge; the position moves to the bottom-left when both do not fit,
    // and only a title wider than the whole edge ends in `…`.
    let (title, top) = if text_width(title) + pos.width() + 4 <= w {
        (title.to_string(), true)
    } else {
        (ellipsize(title, w.saturating_sub(4)), false)
    };
    let pos = if top { pos } else { pos.left_aligned() };
    let block = frame(&title, hint, Color::Cyan);
    let block = if top {
        block.title(pos)
    } else {
        block.title_bottom(pos)
    };
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), area);
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
            // The selected action's title wraps in full; the others end in `…` when too long
            // (moving the cursor onto one shows it whole).
            let sel_rows = hits.get(*cursor).map_or(1, |(x, _)| {
                let key = text_width(x.key) + 2;
                wrap_text(x.title, cols.saturating_sub(key + 3).max(8)).len()
            });
            let show = hits.len().min(
                usize::from(screen.height.saturating_sub(8))
                    .saturating_sub(sel_rows.saturating_sub(1))
                    .clamp(1, 10),
            );
            let top = cursor.saturating_sub(show.saturating_sub(1));
            let q = query.clone() + "█";
            let qw = text_width(&q);
            let shown_q: String = q
                .chars()
                .skip(qw.saturating_sub(cols.saturating_sub(2)))
                .collect();
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("› ", role::accent().add_modifier(Modifier::BOLD)),
                    Span::raw(shown_q),
                ]),
                Line::styled("─".repeat(cols), role::dim()),
            ];
            if hits.is_empty() {
                lines.push(Line::styled("No action matches", role::dim()));
            }
            for (i, (x, at)) in hits.iter().enumerate().skip(top).take(show) {
                let sel = i == *cursor;
                let key = format!(" {} ", x.key);
                let room = cols.saturating_sub(text_width(&key) + 3).max(8);
                let mut title: Vec<Span<'static>> = Vec::new();
                for (n, c) in x.title.chars().enumerate() {
                    let mut st = Style::default();
                    if at.contains(&n) {
                        st = st.fg(role::ACCENT).add_modifier(Modifier::BOLD);
                    } else if sel {
                        st = st.add_modifier(Modifier::BOLD);
                    }
                    title.push(Span::styled(c.to_string(), st));
                }
                let title = Line::from(title);
                let rows = if sel {
                    wrap_line(&title, room)
                } else {
                    vec![fit_line(&title, room).0]
                };
                for (r, row) in rows.into_iter().enumerate() {
                    let bar = if sel { "▌ " } else { "  " };
                    let mut spans = vec![Span::styled(bar, role::accent())];
                    let used = 2 + row.width();
                    spans.extend(row.spans);
                    if r == 0 {
                        spans.push(Span::raw(
                            " ".repeat(cols.saturating_sub(used + text_width(&key))),
                        ));
                        spans.push(Span::styled(key.clone(), role::dim()));
                    }
                    lines.push(Line::from(spans));
                }
            }
            let area = centered(screen, w, lines.len() as u16 + 2);
            f.render_widget(Clear, area);
            let mut block = frame("Commands", "↑↓ move · Enter run · Esc close", role::ACCENT);
            if hits.len() > show {
                // Not every match fits: say where the cursor is among them.
                block = block.title(
                    Line::styled(format!(" {}/{} ", cursor + 1, hits.len()), role::dim())
                        .right_aligned(),
                );
            }
            f.render_widget(Paragraph::new(lines).block(block), area);
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
                lines.extend(
                    wrap_text(&t.current, cols)
                        .into_iter()
                        .map(|l| l.style(dim)),
                );
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
            let w = popup_width(screen.width, 72.max(text_width(title) as u16 + 6));
            let (name, head) = title_fit(title, w, 0);
            let area = centered(screen, w, 3 + head.len() as u16);
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
            let mut lines = head;
            lines.push(Line::raw(format!("{tail}█")));
            f.render_widget(
                Paragraph::new(lines).block(frame(&name, "Enter ok · Esc cancel", Color::Cyan)),
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
                .map(|i| text_width(i) + 8)
                .chain([text_width(title) + 6])
                .max()
                .unwrap_or(0);
            let w = popup_width(screen.width, (widest as u16).clamp(48, 100));
            // The position in the top-right corner, when the list scrolls, takes room from the
            // title.
            let total: usize = items.iter().map(|i| wrap_text(i, text_cols(w)).len()).sum();
            let reserve = if total + 2 > usize::from(screen.height) {
                format!(" {}/{} ", cursor + 1, items.len()).len() + 1
            } else {
                0
            };
            let (name, head) = title_fit(title, w, reserve);
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
            let hh = head.len() as u16;
            let area = centered(screen, w, (rows as u16 + 2 + hh).min(screen.height));
            f.render_widget(Clear, area);
            let hint = if *multi {
                "↑↓ move · Space mark · Enter ok · Esc cancel"
            } else {
                "↑↓ move · Enter choose · Esc cancel"
            };
            let mut block = frame(&name, hint, Color::Cyan);
            if rows + usize::from(hh) + 2 > usize::from(area.height) {
                // The list scrolls: say where the cursor is among the entries.
                block = block.title(
                    Line::styled(format!(" {}/{} ", cursor + 1, items.len()), role::dim())
                        .right_aligned(),
                );
            }
            let inner = block.inner(area);
            f.render_widget(block, area);
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(hh), Constraint::Min(0)])
                .split(inner);
            f.render_widget(Paragraph::new(head), parts[0]);
            let list = List::new(li)
                .highlight_style(highlight())
                .highlight_symbol("› ");
            f.render_stateful_widget(
                list,
                parts[1],
                &mut ListState::default().with_selected(Some(*cursor)),
            );
        }
    }
}

/// A popup's title when it fits in its frame `w` wide; otherwise no frame title and the title as
/// bold wrapped lines for the top of the body, so a long question is never cut.
fn title_fit(title: &str, w: u16, reserve: usize) -> (String, Vec<Line<'static>>) {
    if text_width(title) + 4 + reserve <= usize::from(w) {
        return (title.to_string(), Vec::new());
    }
    let head = wrap_text(title, text_cols(w))
        .into_iter()
        .map(|l| l.style(role::bold()))
        .collect();
    (String::new(), head)
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
    let flash = app
        .flash()
        .filter(|m| !m.is_empty() && *m != "Ready")
        .map(|m| status_spans(app, m, base, Color::Green));
    let hint = "save and quit the editor: osh checks, then writes ";
    // The name and a status first, then the hint (shorter on a narrow screen), then the program.
    let short = "save and quit: osh checks ";
    let hw = if text_width(hint)
        + text_width(&run.name)
        + 2
        + flash.as_ref().map_or(0, |f| Line::from(f.clone()).width())
        <= usize::from(bar.width)
    {
        hint
    } else {
        short
    };
    let w = (text_width(hw) as u16).min(bar.width);
    let parts = vec![
        BarPart::new(
            0,
            vec![Span::styled(
                format!(" {} ", run.name),
                base.fg(role::ACCENT).add_modifier(Modifier::BOLD),
            )],
            true,
        ),
        BarPart::new(
            2,
            vec![Span::styled(
                format!(" {prog} ({}) ", run.editor.from),
                base,
            )],
            true,
        ),
        BarPart::new(1, flash.unwrap_or_default(), true),
    ];
    let spans = fit_bar(parts, usize::from(bar.width - w));
    f.render_widget(Paragraph::new(Line::from(spans)).style(base), bar);
    f.render_widget(
        Paragraph::new(hw).style(base.fg(Color::Indexed(244))),
        Rect::new(bar.right() - w, bar.y, w, bar.height),
    );
}

/// One piece of a one-row bar: its priority (0 is kept first), its spans, and whether it may be
/// shortened with `…` rather than left out.
struct BarPart {
    prio: u8,
    spans: Vec<Span<'static>>,
    shrink: bool,
}

impl BarPart {
    fn new(prio: u8, spans: Vec<Span<'static>>, shrink: bool) -> Self {
        BarPart {
            prio,
            spans,
            shrink,
        }
    }
}

/// The parts of a one-row bar that fit in `width` columns, in their own order: parts are taken by
/// priority while whole ones fit; a shrinkable part takes what is left with an explicit `…`.
/// Nothing is drawn over anything else, so no part is cut silently.
fn fit_bar(parts: Vec<BarPart>, width: usize) -> Vec<Span<'static>> {
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&i| parts[i].prio);
    let mut room = width;
    let mut shown: Vec<Option<Line<'static>>> = vec![None; parts.len()];
    for i in order {
        let line = Line::from(parts[i].spans.clone());
        let lw = line.width();
        if lw == 0 {
            continue;
        }
        if lw <= room {
            room -= lw;
            shown[i] = Some(line);
        } else if parts[i].shrink && room >= 6 {
            let (l, _) = fit_line(&line, room);
            room = room.saturating_sub(l.width());
            shown[i] = Some(l);
        }
    }
    shown.into_iter().flatten().flat_map(|l| l.spans).collect()
}

/// The shell fills the screen above a one-row status bar; the cursor is shown only with focus.
/// The bar keeps `Ctrl-] menu`, the sandbox's name and a status whole, then the scrollback mark,
/// the branch, the window title (shortened with `…`) and the usage meter, as room allows; the
/// dashboard shows the status and the usage in full.
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
    let mut parts = vec![BarPart::new(
        0,
        vec![Span::styled(
            format!(" {} ", s.sandbox),
            base.fg(role::ACCENT).add_modifier(Modifier::BOLD),
        )],
        true,
    )];
    let title = s.vt.title();
    if let Some(branch) = crate::term::branch_of(&title) {
        parts.push(BarPart::new(
            3,
            vec![Span::styled(
                format!(" {branch} "),
                base.fg(Color::Indexed(176)).add_modifier(Modifier::BOLD),
            )],
            true,
        ));
    }
    let title = crate::term::shown_title(&title);
    if !title.is_empty() {
        parts.push(BarPart::new(
            4,
            vec![Span::styled(format!(" {title} "), base)],
            true,
        ));
    }
    let back = s.vt.scrolled_back();
    if back > 0 {
        parts.push(BarPart::new(
            2,
            vec![Span::styled(
                format!(" [scrollback -{back}, type to return] "),
                base.fg(Color::Indexed(111)),
            )],
            true,
        ));
    }
    if let Some(m) = app.flash().filter(|m| !m.is_empty() && *m != "Ready") {
        parts.push(BarPart::new(
            1,
            status_spans(app, m, base, Color::Green),
            true,
        ));
    }
    // Its own rectangle: a style on a paragraph repaints the whole area, which would wipe the
    // colours of the name and branch drawn on the left.
    let hint = "Ctrl-] menu ";
    let w = (hint.chars().count() as u16).min(bar.width);
    let spans = fit_bar(parts, usize::from(bar.width - w));
    f.render_widget(Paragraph::new(Line::from(spans.clone())).style(base), bar);
    f.render_widget(
        Paragraph::new(hint).style(base.fg(Color::Indexed(244))),
        Rect::new(bar.right() - w, bar.y, w, bar.height),
    );
    // The sandbox's CPU, memory and disk use, right-aligned before the hint, in what the left
    // side leaves free (the details pane on the dashboard shows it whole).
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

/// The log view's keys.
pub const LOGVIEW_HELP: &str =
    "/ search  s source  j/k PgUp/PgDn scroll  g/G top/bottom  f follow  h/l pan  q close";

/// The log view: the lines (h/l pan sideways; the title says when lines go on past the right
/// edge) and the keys below, on as many rows as they need.
fn draw_logview(f: &mut Frame, lv: &LogView) {
    let width = usize::from(f.area().width);
    let bar: Vec<Line<'static>> = match &lv.typing {
        Some(t) => wrap_text(&format!("/{t}\u{2588}"), width),
        None => {
            let help = if lv.query.is_empty() {
                LOGVIEW_HELP.to_string()
            } else {
                format!("/{} search  n/N next/prev  Esc clear  q close", lv.query)
            };
            let entries = hint_entries(&help);
            if !hints_fit(&entries, width) {
                draw_too_small(f, f.area());
                return;
            }
            hint_lines(&entries, width)
        }
    };
    if usize::from(f.area().height) < bar.len() + 3 {
        draw_too_small(f, f.area());
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(bar.len() as u16)])
        .split(f.area());
    let height = rows[0].height.saturating_sub(2) as usize;
    let view = usize::from(rows[0].width.saturating_sub(4));
    lv.height.set(height);
    let (lines, total, widest): (Vec<Line>, usize, usize) = {
        let Ok(b) = lv.buf.lock() else { return };
        let total = b.lines.len();
        let top = if lv.follow {
            total.saturating_sub(height)
        } else {
            lv.top.min(total.saturating_sub(height))
        };
        let shown: Vec<&String> = b.lines.iter().skip(top).take(height).collect();
        let widest = shown.iter().map(|l| text_width(l)).max().unwrap_or(0);
        let lines = shown
            .into_iter()
            .enumerate()
            .map(|(n, l)| {
                let i = top + n;
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
        (lines, total, widest)
    };
    let top = if lv.follow {
        total.saturating_sub(height)
    } else {
        lv.top.min(total.saturating_sub(height))
    };
    let mode = if lv.follow { "following" } else { "paused" };
    let left = usize::from(lv.left);
    // Columns hidden on either side of the view, so a long line never ends silently at the edge.
    let pan = match (left > 0, widest > left + view) {
        (false, false) => String::new(),
        (true, false) => "← h ".to_string(),
        (false, true) => "l → ".to_string(),
        (true, true) => "← h · l → ".to_string(),
    };
    let mut block = panel(&format!("Logs · {}", lv.name), None).title(
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
    );
    if !pan.is_empty() {
        block = block.title_bottom(
            Line::styled(format!(" more columns {pan}"), role::accent()).right_aligned(),
        );
    }
    f.render_widget(
        Paragraph::new(lines).scroll((0, lv.left)).block(block),
        rows[0],
    );
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
    fn hints_wrap_between_entries_and_never_lose_one() {
        let entries = hint_entries(footer_help(Tab::Sandboxes));
        for w in [30usize, 40, 60, 80, 200] {
            let rows = pack_hints(&entries, w, 0);
            let joined = rows.join("  ");
            assert_eq!(hint_entries(&joined), entries, "width {w}");
            assert!(
                rows.iter().all(|r| text_width(r) <= w),
                "width {w}: {rows:?}"
            );
        }
        // A lead (a prompt) shortens only the first row.
        let rows = pack_hints(&["aa bb", "cc dd"], 12, 6);
        assert_eq!(rows, ["aa bb", "cc dd"]);
    }

    #[test]
    fn columns_are_dropped_last_first_and_cut_cells_show_in_full_below() {
        let cells = vec![vec![
            Line::raw("a-long-provider-name"),
            Line::raw("github"),
            Line::raw("KEY_ONE, KEY_TWO"),
            Line::raw("token expires in 3 days"),
        ]];
        let w = fit_widths(PROVIDER_COLS, &cells, 30);
        assert!(w.len() < 4, "a column gives way: {w:?}");
        assert!(w.iter().sum::<usize>() + w.len() - 1 <= 30);
        assert_eq!(fit_widths(PROVIDER_COLS, &cells, 200).len(), 4);
        let mut app = usage_app("docker");
        app.snap.providers = vec![cli::Provider {
            name: "a-long-provider-name".into(),
            ty: "github".into(),
            keys: vec!["KEY_ONE".into(), "KEY_TWO_THAT_IS_LONG".into()],
        }];
        app.tab = Tab::Providers;
        let (all, _) = screen(&app, 60, 24);
        assert!(all.contains("selected row in full"), "{all}");
        assert!(all.contains("KEY_TWO_THAT_IS_LONG"), "{all}");
        let (all, _) = screen(&app, 160, 24);
        assert!(
            !all.contains("selected row in full"),
            "nothing cut, no strip"
        );
    }

    #[test]
    fn the_tab_bar_never_overlaps_and_the_gateway_stays() {
        let mut app = usage_app("docker");
        app.snap.gateway = "openshell-docker".into();
        app.snap.gateway_ok = true;
        app.snap.version = "openshell 0.0.36".into();
        app.config_stale = true;
        for w in [60u16, 80, 100, 160] {
            let (_, buf) = screen(&app, w, 30);
            let top: String = (0..w).map(|x| buf[(x, 0)].symbol()).collect();
            assert!(top.contains("1 Sandboxes"), "{w}: {top}");
            assert!(top.contains("openshell-docker"), "{w}: {top}");
            let all: String = buf.content().iter().map(|c| c.symbol()).collect();
            assert!(
                all.contains("R on Gateways installs it"),
                "the warning wraps in a banner"
            );
        }
    }

    #[test]
    fn a_bar_keeps_parts_by_priority_and_marks_a_shortened_one() {
        let parts = vec![
            BarPart::new(0, vec![Span::raw(" name ")], true),
            BarPart::new(2, vec![Span::raw(" a very long window title here ")], true),
            BarPart::new(1, vec![Span::raw(" ✓ Saved ")], true),
        ];
        let text: String = fit_bar(parts, 25)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.starts_with(" name "), "{text}");
        assert!(text.ends_with(" ✓ Saved "), "{text}");
        assert!(text.contains('…'), "the title is shortened visibly: {text}");
        assert!(text_width(&text) <= 25);
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
