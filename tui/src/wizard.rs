//! The set-up wizard: which gateway types to create, what each needs, and the plan to get there.
#![allow(dead_code)] // drawn and run by Tasks 6 and 7 of the plan; remove when wired
//! This file holds the screens' state machine (pure, so it is unit-tested), their drawing, and the
//! runner that executes the plan.
use crate::gwmodel::{availability, Availability, GwKind, Pending, State};
use crate::platform::Facts;
use crate::setup::Step;
use ratatui::crossterm::event::KeyCode;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// First run: starts with what was found on the machine.
    First,
    /// From the Gateways tab: straight to the choice.
    Add,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Machine,
    Gateways,
    Review,
    Install,
    Done,
}

pub struct Item {
    pub kind: GwKind,
    pub avail: Availability,
    pub ticked: bool,
    /// Already set up (by osh or known to the CLI): shown dimmed, not tickable.
    pub existing: bool,
}

#[derive(Debug)]
pub enum Action {
    None,
    Quit,
    /// Run these steps to create these gateway types.
    Install(Vec<Step>, Vec<GwKind>),
    Finish,
}

/// Turns the ticked types into the plan; `Err` carries a reason to show.
pub type Planner = Box<dyn Fn(&[GwKind]) -> Result<Vec<Step>, String>>;

pub struct Wizard {
    pub mode: Mode,
    pub screen: Screen,
    pub items: Vec<Item>,
    pub cursor: usize,
    /// The order the types were ticked in: the first is the main gateway.
    tick_order: Vec<GwKind>,
    pub steps: Vec<Step>,
    pub facts: Facts,
    /// A one-line message for the current screen (why a key did nothing).
    pub note: String,
    has_main: bool,
    planner: Planner,
}

impl Wizard {
    pub fn new(
        mode: Mode,
        facts: Facts,
        existing: &[GwKind],
        has_main: bool,
        pending: Option<Pending>,
        planner: Planner,
    ) -> Wizard {
        let items = [GwKind::Docker, GwKind::Podman, GwKind::Vm]
            .into_iter()
            .map(|kind| Item {
                kind,
                avail: availability(&facts, kind),
                ticked: false,
                existing: existing.contains(&kind),
            })
            .collect();
        let mut w = Wizard {
            mode,
            screen: if mode == Mode::First {
                Screen::Machine
            } else {
                Screen::Gateways
            },
            items,
            cursor: 0,
            tick_order: Vec::new(),
            steps: Vec::new(),
            facts,
            note: String::new(),
            has_main,
            planner,
        };
        if let Some(p) = pending {
            for k in p.kinds {
                if w.items.iter().any(|i| {
                    i.kind == k && !i.existing && !matches!(i.avail.state, State::Unavailable(_))
                }) {
                    w.tick_order.push(k);
                }
            }
            w.sync_ticks();
            if !w.tick_order.is_empty() {
                if let Ok(steps) = (w.planner)(&w.tick_order) {
                    w.steps = steps;
                    w.screen = Screen::Review;
                }
            }
        }
        w
    }

    fn sync_ticks(&mut self) {
        for i in &mut self.items {
            i.ticked = self.tick_order.contains(&i.kind);
        }
    }

    /// The ticked types in the order they were ticked.
    pub fn wanted(&self) -> Vec<GwKind> {
        self.tick_order.clone()
    }

    /// The type that will become the main gateway: the first ticked, unless one already exists.
    pub fn main_kind(&self) -> Option<GwKind> {
        if self.has_main {
            None
        } else {
            self.tick_order.first().copied()
        }
    }

    /// The runner calls this when the install has ended (successfully or not).
    pub fn show_done(&mut self) {
        self.screen = Screen::Done;
    }

    pub fn on_key(&mut self, k: KeyCode) -> Action {
        self.note.clear();
        match self.screen {
            Screen::Machine => match k {
                KeyCode::Enter => self.screen = Screen::Gateways,
                KeyCode::Esc | KeyCode::Char('q') => return Action::Quit,
                _ => {}
            },
            Screen::Gateways => return self.gateways_key(k),
            Screen::Review => match k {
                KeyCode::Enter => {
                    self.screen = Screen::Install;
                    return Action::Install(self.steps.clone(), self.wanted());
                }
                KeyCode::Esc | KeyCode::Char('b') => self.screen = Screen::Gateways,
                KeyCode::Char('q') => return Action::Quit,
                _ => {}
            },
            Screen::Install => {}
            Screen::Done => {
                if matches!(k, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                    return Action::Finish;
                }
            }
        }
        Action::None
    }

    fn gateways_key(&mut self, k: KeyCode) -> Action {
        match k {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.items.len() - 1)
            }
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Enter => {
                if self.tick_order.is_empty() {
                    self.note = "Pick at least one gateway with space.".into();
                } else {
                    match (self.planner)(&self.tick_order) {
                        Ok(steps) => {
                            self.steps = steps;
                            self.screen = Screen::Review;
                        }
                        Err(e) => self.note = e,
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('b') => {
                if self.mode == Mode::First {
                    self.screen = Screen::Machine;
                } else {
                    return Action::Quit;
                }
            }
            KeyCode::Char('q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    fn toggle(&mut self) {
        let item = &self.items[self.cursor];
        let kind = item.kind;
        if item.existing {
            self.note = format!("{} is already set up.", kind.label());
        } else if let State::Unavailable(why) = &item.avail.state {
            self.note = format!("{} cannot be used here: {why}", kind.label());
        } else if let Some(pos) = self.tick_order.iter().position(|k| *k == kind) {
            self.tick_order.remove(pos);
        } else {
            self.tick_order.push(kind);
        }
        self.sync_ticks();
    }
}

// ---- drawing ------------------------------------------------------------------------------

use crate::gwmodel::Need;
use crate::platform::{Kvm, Os, Runtime};
use crate::setup::StepKind;
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Padding, Paragraph, Wrap},
    Frame,
};
use std::time::Duration;

/// How far the install has got, for the Install and Done screens. The runner updates it.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// One per step of the plan, in order.
    pub rows: Vec<RowState>,
    /// The latest lines of output of the step being run (the command first).
    pub now: Vec<String>,
    pub finished: bool,
    /// Every required step succeeded.
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    Pending,
    Running,
    Done(Duration),
    /// A command failed; the text says how.
    Failed(String),
    /// Something only the user can do.
    Manual(String),
}

const ACCENT: Color = Color::Cyan;
const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const DIM: Color = Color::DarkGray;

fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn dim(t: impl Into<String>) -> Span<'static> {
    Span::styled(t.into(), fg(DIM))
}

fn plain(t: impl Into<String>) -> Span<'static> {
    Span::raw(t.into())
}

/// Lines from rows of cells with every column padded to its widest cell, so columns line up in
/// every row. Columns in `right` are right-aligned; the last column is not padded.
fn table(rows: Vec<Vec<Span<'static>>>, gap: usize, right: &[usize]) -> Vec<Line<'static>> {
    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..n)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(Span::width)
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.into_iter()
        .map(|r| {
            let last = r.len().saturating_sub(1);
            let mut spans = Vec::new();
            for (i, cell) in r.into_iter().enumerate() {
                let pad = widths[i] - cell.width();
                if right.contains(&i) {
                    spans.push(plain(" ".repeat(pad)));
                    spans.push(cell);
                } else {
                    spans.push(cell);
                    if i != last {
                        spans.push(plain(" ".repeat(pad)));
                    }
                }
                if i != last {
                    spans.push(plain(" ".repeat(gap)));
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn mark(ok: bool) -> Span<'static> {
    if ok {
        Span::styled("✓", fg(OK))
    } else {
        Span::styled("✗", fg(WARN))
    }
}

fn boxed(title: &str) -> Block<'static> {
    Block::bordered()
        .title(format!(" {title} "))
        .padding(Padding::horizontal(1))
}

fn step_number(w: &Wizard) -> usize {
    match w.screen {
        Screen::Machine => 1,
        Screen::Gateways => 2,
        Screen::Review => 3,
        Screen::Install | Screen::Done => 4,
    }
}

fn header(w: &Wizard) -> Line<'static> {
    let n = step_number(w);
    let mut spans = vec![Span::styled(" oh-my-openshell ", bold()), dim("set up    ")];
    for (i, name) in ["Machine", "Gateways", "Review", "Install"]
        .iter()
        .enumerate()
    {
        let (dot, style) = if i + 1 < n {
            ("●", fg(ACCENT))
        } else if i + 1 == n {
            ("●", fg(ACCENT).add_modifier(Modifier::BOLD))
        } else {
            ("○", fg(DIM))
        };
        spans.push(Span::styled(format!("{dot} "), style));
        let label = if i + 1 == n {
            Span::styled(*name, bold())
        } else if i + 1 < n {
            plain(*name)
        } else {
            dim(*name)
        };
        spans.push(label);
        spans.push(plain("  "));
    }
    Line::from(spans)
}

fn hints(w: &Wizard, p: &Progress) -> &'static str {
    match w.screen {
        Screen::Machine => "enter  continue    q  quit",
        Screen::Gateways => "↑↓ move   space tick   enter continue   esc back   q quit",
        Screen::Review => "Commands are shown as they run.   enter  Install   b  back   q  quit",
        Screen::Install if p.finished => "enter  continue",
        Screen::Install => "working… the command being run is shown below",
        Screen::Done => "enter  open the dashboard",
    }
}

pub fn draw(f: &mut Frame, w: &Wizard, p: &Progress) {
    let area = f.area().inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    f.render_widget(Paragraph::new(header(w)), rows[0]);
    match w.screen {
        Screen::Machine => draw_machine(f, w, rows[1]),
        Screen::Gateways => draw_gateways(f, w, rows[1]),
        Screen::Review => draw_review(f, w, rows[1]),
        Screen::Install => draw_install(f, w, p, rows[1]),
        Screen::Done => draw_done(f, w, p, rows[1]),
    }
    if !w.note.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(w.note.clone(), fg(WARN))),
            rows[2],
        );
    }
    f.render_widget(Paragraph::new(dim(hints(w, p))), rows[3]);
}

fn runtime_line(f: &Facts, rt: Runtime) -> Vec<Span<'static>> {
    let name = if rt == Runtime::Docker {
        "Docker"
    } else {
        "Podman"
    };
    if f.runtimes_ready.contains(&rt) {
        vec![mark(true), plain(name), dim("running")]
    } else if f.runtimes_installed.contains(&rt) {
        vec![mark(false), plain(name), dim("installed, not answering")]
    } else {
        vec![mark(false), plain(name), dim("not installed")]
    }
}

fn draw_machine(f: &mut Frame, w: &Wizard, area: Rect) {
    let facts = &w.facts;
    let os = match facts.os {
        Os::MacOs => "macOS",
        Os::Linux => "Linux",
    };
    let mut rows = vec![vec![
        mark(true),
        plain(os),
        plain(format!("package manager {:?}", facts.pkg).to_lowercase()),
    ]];
    if facts.os == Os::Linux {
        let sudo = if facts.is_root || facts.has_sudo {
            (true, "you will be asked for your password")
        } else {
            (
                false,
                "not available: steps needing root are shown for you to run",
            )
        };
        rows.push(vec![mark(sudo.0), plain("sudo"), dim(sudo.1)]);
    }
    rows.push(runtime_line(facts, Runtime::Docker));
    rows.push(runtime_line(facts, Runtime::Podman));
    let kvm = match (facts.os, facts.kvm) {
        (Os::MacOs, _) => (true, "Hypervisor.framework", "VMs"),
        (_, Kvm::Usable) => (true, "usable", "KVM (/dev/kvm)"),
        (_, Kvm::NoAccess) => (
            false,
            "present, but your user cannot use it yet",
            "KVM (/dev/kvm)",
        ),
        (_, Kvm::Absent) => (
            false,
            "virtualization is off or unsupported",
            "KVM (/dev/kvm)",
        ),
    };
    rows.push(vec![mark(kvm.0), plain(kvm.2), dim(kvm.1)]);
    rows.push(if facts.has_openshell {
        vec![mark(true), plain("OpenShell"), dim("installed")]
    } else {
        vec![
            mark(false),
            plain("OpenShell"),
            dim("not installed: osh will install it"),
        ]
    });
    let lines = table(rows, 2, &[]);
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(lines.len() as u16 + 2),
            Constraint::Min(0),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new("Here is what I found on this machine. Nothing is changed yet."),
        v[0],
    );
    f.render_widget(Paragraph::new(lines).block(boxed("What I found")), v[1]);
}

fn status_word(i: &Item, f: &Facts) -> Span<'static> {
    if i.existing {
        return dim("added");
    }
    match &i.avail.state {
        State::Ready => Span::styled("ready", fg(OK)),
        State::NeedsInstall => Span::styled("needs install", fg(WARN)),
        State::Unavailable(_) if i.kind == GwKind::Vm && f.kvm == Kvm::Absent => dim("no KVM"),
        State::Unavailable(_) => dim("unavailable"),
    }
}

fn need_rows(needs: &[Need]) -> Vec<Vec<Span<'static>>> {
    needs
        .iter()
        .map(|n| vec![mark(n.ok), plain(n.text.clone()), dim(n.action.clone())])
        .collect()
}

fn draw_gateways(f: &mut Frame, w: &Wizard, area: Rect) {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let intro = if w.mode == Mode::First {
        "Pick the gateways you want. The first one you tick becomes your main one."
    } else {
        "Pick the gateways to add."
    };
    f.render_widget(Paragraph::new(intro), v[0]);
    let wide = area.width >= 70;
    // Build the list and the details first, so the boxes can be sized to what they hold.
    let rows: Vec<Vec<Span<'static>>> = w
        .items
        .iter()
        .map(|i| {
            let off = i.existing || matches!(i.avail.state, State::Unavailable(_));
            let tick = if off {
                "[-]"
            } else if i.ticked {
                "[x]"
            } else {
                "[ ]"
            };
            let name = if i.kind == GwKind::Vm {
                "VM"
            } else {
                i.kind.label()
            };
            let status = status_word(i, &w.facts);
            if off {
                vec![dim(format!("{tick} {name}")), status]
            } else if i.ticked {
                vec![Span::styled(format!("{tick} {name}"), fg(OK)), status]
            } else {
                vec![plain(format!("{tick} {name}")), status]
            }
        })
        .collect();
    let mut lines = table(rows, 2, &[]);
    // The highlighted row is coloured across the whole box, not only under its text.
    let inner = 30usize.saturating_sub(4);
    for (idx, line) in lines.iter_mut().enumerate() {
        if idx == w.cursor {
            let pad = inner.saturating_sub(line.width());
            line.spans.push(plain(" ".repeat(pad)));
            *line = line
                .clone()
                .patch_style(Style::default().bg(Color::Blue).fg(Color::White));
        }
    }
    if w.mode == Mode::First || w.main_kind().is_some() {
        lines.push(Line::raw(""));
        lines.push(Line::from(dim("1st ticked = main")));
    }
    let item = &w.items[w.cursor];
    let mut d: Vec<Line<'static>> = vec![Line::raw(item.kind.describe()), Line::raw("")];
    if item.existing {
        d.push(Line::from(dim("Already set up on this machine.")));
    } else if let State::Unavailable(why) = &item.avail.state {
        d.push(Line::from(Span::styled(why.clone(), fg(WARN))));
    } else {
        d.push(Line::from(Span::styled("What it needs", bold())));
        d.extend(table(need_rows(&item.avail.needs), 2, &[]));
    }
    let want = (lines.len().max(d.len() + 1) + 2).max(9) as u16;
    let body = Rect {
        height: want.min(v[1].height),
        ..v[1]
    };
    let panes = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(0)])
            .split(body)
    } else {
        Layout::default()
            .constraints([Constraint::Min(0)])
            .split(body)
    };
    f.render_widget(Paragraph::new(lines).block(boxed("Gateways")), panes[0]);
    if wide {
        f.render_widget(
            Paragraph::new(d)
                .wrap(Wrap { trim: false })
                .block(boxed(item.kind.label())),
            panes[1],
        );
    }
}

fn privileged(s: &Step) -> bool {
    matches!(&s.kind, StepKind::Cmd { prog, .. } if prog == "sudo")
}

fn draw_review(f: &mut Frame, w: &Wizard, area: Rect) {
    let kinds = w.wanted();
    let main = w.main_kind();
    let names: Vec<String> = kinds
        .iter()
        .map(|k| {
            if Some(*k) == main {
                format!("{} (main)", k.name())
            } else {
                k.name().to_string()
            }
        })
        .collect();
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(area);
    let plan_area = Rect {
        height: (w.steps.len() as u16 + 2).min(v[1].height),
        ..v[1]
    };
    let n = kinds.len();
    f.render_widget(
        Paragraph::new(vec![
            Line::raw(format!(
                "You will get {n} gateway{}: {}",
                if n == 1 { "" } else { "s" },
                names.join(", ")
            )),
            Line::raw(format!("I will run these {} steps:", w.steps.len())),
        ]),
        v[0],
    );
    let room = (v[1].height as usize).saturating_sub(2).max(1);
    let shown = w.steps.len().min(room);
    let mut rows: Vec<Vec<Span<'static>>> = w
        .steps
        .iter()
        .take(if shown < w.steps.len() {
            shown.saturating_sub(1)
        } else {
            shown
        })
        .enumerate()
        .map(|(i, s)| {
            vec![
                Span::styled((i + 1).to_string(), bold()),
                plain(s.title.clone()),
                dim(if privileged(s) { "sudo" } else { "" }),
            ]
        })
        .collect();
    if shown < w.steps.len() {
        rows.push(vec![
            plain(""),
            dim(format!("… and {} more", w.steps.len() - rows.len())),
            plain(""),
        ]);
    }
    f.render_widget(
        Paragraph::new(table(rows, 2, &[0])).block(boxed("Plan")),
        plan_area,
    );
}

fn draw_install(f: &mut Frame, w: &Wizard, p: &Progress, area: Rect) {
    let done = p
        .rows
        .iter()
        .filter(|r| matches!(r, RowState::Done(_)))
        .count();
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(w.steps.len() as u16 + 2),
            Constraint::Min(0),
            Constraint::Length(6),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            plain("Working… "),
            dim(format!("({done} of {} done)", w.steps.len())),
        ])),
        v[0],
    );
    let rows: Vec<Vec<Span<'static>>> = w
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let state = p.rows.get(i).cloned().unwrap_or(RowState::Pending);
            let (icon, num, title, time) = match state {
                RowState::Done(d) => (
                    mark(true),
                    bold(),
                    plain(s.title.clone()),
                    dim(format!("{}s", d.as_secs())),
                ),
                RowState::Running => (
                    Span::styled("⠹", fg(ACCENT)),
                    bold(),
                    plain(s.title.clone()),
                    plain(""),
                ),
                RowState::Failed(_) => (
                    Span::styled("✗", fg(BAD)),
                    bold(),
                    Span::styled(s.title.clone(), fg(BAD)),
                    plain(""),
                ),
                RowState::Manual(_) => (
                    Span::styled("!", fg(WARN)),
                    bold(),
                    Span::styled(s.title.clone(), fg(WARN)),
                    plain(""),
                ),
                RowState::Pending => (dim("·"), fg(DIM), dim(s.title.clone()), plain("")),
            };
            vec![icon, Span::styled((i + 1).to_string(), num), title, time]
        })
        .collect();
    f.render_widget(
        Paragraph::new(table(rows, 2, &[3])).block(boxed("Progress")),
        v[1],
    );
    let now: Vec<Line<'static>> = p
        .now
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|l| Line::from(dim(l.clone())))
        .collect();
    f.render_widget(Paragraph::new(now).block(boxed("Now running")), v[3]);
}

fn draw_done(f: &mut Frame, w: &Wizard, p: &Progress, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if p.ok {
        lines.push(Line::from(vec![
            Span::styled("All set.", fg(OK)),
            plain(" Your gateways:"),
        ]));
        lines.push(Line::raw(""));
        let main = w.main_kind();
        let rows: Vec<Vec<Span<'static>>> = w
            .wanted()
            .iter()
            .map(|k| {
                vec![
                    Span::styled("●", fg(OK)),
                    Span::styled(k.name(), bold()),
                    dim(if Some(*k) == main { "main" } else { "" }),
                    plain(k.driver()),
                    Span::styled("running", fg(OK)),
                ]
            })
            .collect();
        let rows = table(rows, 3, &[]);
        let h = rows.len() as u16 + 2;
        let v = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(h),
                Constraint::Min(0),
            ])
            .split(area);
        f.render_widget(Paragraph::new(lines), v[0]);
        f.render_widget(Paragraph::new(rows).block(boxed("Gateways")), v[1]);
        f.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::from(vec![
                    plain("Next: create a sandbox with "),
                    Span::styled("n", bold()),
                    plain(" on the Sandboxes tab."),
                ]),
                Line::from(vec![
                    plain("Add more gateways any time on the Gateways tab with "),
                    Span::styled("a", bold()),
                    plain("."),
                ]),
            ]),
            v[2],
        );
    } else {
        lines.push(Line::from(Span::styled(
            "Finished, with something left to do:",
            fg(WARN),
        )));
        lines.push(Line::raw(""));
        for (i, s) in w.steps.iter().enumerate() {
            match p.rows.get(i) {
                Some(RowState::Failed(e)) => lines.push(Line::from(vec![
                    Span::styled("✗ ", fg(BAD)),
                    plain(format!("{}: {e}", s.title)),
                ])),
                Some(RowState::Manual(t)) => lines.push(Line::from(vec![
                    Span::styled("! ", fg(WARN)),
                    plain(format!("{}: {t}", s.title)),
                ])),
                _ => {}
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(dim(
            "Do that, then run osh setup again: it carries on where it stopped.",
        )));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::testing::sample_facts;
    use crate::platform::{Kvm, Os, Runtime};
    use crate::setup::{Step, StepKind};
    use ratatui::crossterm::event::KeyCode;

    fn planner() -> Planner {
        Box::new(|kinds: &[GwKind]| {
            Ok(kinds
                .iter()
                .map(|k| Step {
                    id: "x",
                    title: format!("Set up {}", k.label()),
                    kind: StepKind::Manual("m".into()),
                    optional: false,
                    after: None,
                })
                .collect())
        })
    }

    fn linux_docker_ready() -> crate::platform::Facts {
        let mut f = sample_facts(Os::Linux);
        f.runtimes_installed = vec![Runtime::Docker];
        f.runtimes_ready = vec![Runtime::Docker];
        f
    }

    fn first(f: crate::platform::Facts) -> Wizard {
        Wizard::new(Mode::First, f, &[], false, None, planner())
    }

    fn press(w: &mut Wizard, keys: &[KeyCode]) -> Action {
        let mut last = Action::None;
        for k in keys {
            last = w.on_key(*k);
        }
        last
    }

    fn at_gateways(f: crate::platform::Facts) -> Wizard {
        let mut w = first(f);
        press(&mut w, &[KeyCode::Enter]);
        assert_eq!(w.screen, Screen::Gateways);
        w
    }

    fn cursor_to(w: &mut Wizard, k: GwKind) {
        w.cursor = w.items.iter().position(|i| i.kind == k).unwrap();
    }

    #[test]
    fn first_run_starts_at_the_machine_screen_and_add_mode_at_the_gateways() {
        assert_eq!(first(linux_docker_ready()).screen, Screen::Machine);
        let w = Wizard::new(Mode::Add, linux_docker_ready(), &[], false, None, planner());
        assert_eq!(w.screen, Screen::Gateways);
    }

    #[test]
    fn items_are_listed_docker_podman_vm() {
        let w = first(linux_docker_ready());
        let kinds: Vec<_> = w.items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, [GwKind::Docker, GwKind::Podman, GwKind::Vm]);
    }

    #[test]
    fn first_tick_becomes_main() {
        let mut w = at_gateways(linux_docker_ready());
        cursor_to(&mut w, GwKind::Podman);
        press(&mut w, &[KeyCode::Char(' ')]);
        cursor_to(&mut w, GwKind::Docker);
        press(&mut w, &[KeyCode::Char(' ')]);
        assert_eq!(w.wanted(), [GwKind::Podman, GwKind::Docker]);
        assert_eq!(w.main_kind(), Some(GwKind::Podman));
    }

    #[test]
    fn unticking_the_main_promotes_the_next() {
        let mut w = at_gateways(linux_docker_ready());
        cursor_to(&mut w, GwKind::Docker);
        press(&mut w, &[KeyCode::Char(' ')]);
        cursor_to(&mut w, GwKind::Podman);
        press(&mut w, &[KeyCode::Char(' ')]);
        cursor_to(&mut w, GwKind::Docker);
        press(&mut w, &[KeyCode::Char(' ')]);
        assert_eq!(w.wanted(), [GwKind::Podman]);
        assert_eq!(w.main_kind(), Some(GwKind::Podman));
    }

    #[test]
    fn unavailable_types_cannot_be_ticked_and_say_why() {
        let mut w = at_gateways(linux_docker_ready());
        cursor_to(&mut w, GwKind::Vm);
        press(&mut w, &[KeyCode::Char(' ')]);
        assert!(w.wanted().is_empty());
        assert!(w.note.contains("/dev/kvm"), "{}", w.note);
        let mut f = linux_docker_ready();
        f.kvm = Kvm::Usable;
        f.has_e2fsprogs = true;
        let mut w = at_gateways(f);
        cursor_to(&mut w, GwKind::Vm);
        press(&mut w, &[KeyCode::Char(' ')]);
        assert_eq!(w.wanted(), [GwKind::Vm]);
    }

    #[test]
    fn existing_types_are_dimmed_and_not_tickable_in_add_mode() {
        let mut w = Wizard::new(
            Mode::Add,
            linux_docker_ready(),
            &[GwKind::Docker],
            true,
            None,
            planner(),
        );
        assert!(w.items[0].existing && w.items[0].kind == GwKind::Docker);
        press(&mut w, &[KeyCode::Char(' ')]);
        assert!(
            w.wanted().is_empty() && w.note.contains("already"),
            "{}",
            w.note
        );
        assert_eq!(
            w.main_kind(),
            None,
            "a main already exists, so nothing new is main"
        );
    }

    #[test]
    fn enter_needs_at_least_one_tick() {
        let mut w = at_gateways(linux_docker_ready());
        press(&mut w, &[KeyCode::Enter]);
        assert_eq!(w.screen, Screen::Gateways);
        assert!(w.note.contains("at least one"), "{}", w.note);
    }

    #[test]
    fn back_and_quit_keys() {
        let mut w = first(linux_docker_ready());
        assert!(matches!(w.on_key(KeyCode::Esc), Action::Quit));
        let mut w = at_gateways(linux_docker_ready());
        w.on_key(KeyCode::Esc);
        assert_eq!(w.screen, Screen::Machine);
        let mut w = at_gateways(linux_docker_ready());
        assert!(matches!(w.on_key(KeyCode::Char('q')), Action::Quit));
        let mut add = Wizard::new(Mode::Add, linux_docker_ready(), &[], false, None, planner());
        assert!(matches!(add.on_key(KeyCode::Esc), Action::Quit));
        let mut w = at_gateways(linux_docker_ready());
        cursor_to(&mut w, GwKind::Docker);
        press(&mut w, &[KeyCode::Char(' '), KeyCode::Enter]);
        assert_eq!(w.screen, Screen::Review);
        w.on_key(KeyCode::Char('b'));
        assert_eq!(w.screen, Screen::Gateways);
    }

    #[test]
    fn review_lists_the_plan_for_the_ticked_types() {
        let mut w = at_gateways(linux_docker_ready());
        cursor_to(&mut w, GwKind::Docker);
        press(&mut w, &[KeyCode::Char(' '), KeyCode::Enter]);
        assert_eq!(w.screen, Screen::Review);
        assert_eq!(w.steps.len(), 1);
        assert_eq!(w.steps[0].title, "Set up Docker");
    }

    #[test]
    fn a_planner_error_keeps_the_user_on_the_gateways_screen_with_the_reason() {
        let bad: Planner = Box::new(|_| Err("Podman: nope".into()));
        let mut w = Wizard::new(Mode::Add, linux_docker_ready(), &[], false, None, bad);
        cursor_to(&mut w, GwKind::Docker);
        press(&mut w, &[KeyCode::Char(' '), KeyCode::Enter]);
        assert_eq!(w.screen, Screen::Gateways);
        assert!(w.note.contains("nope"));
    }

    #[test]
    fn pending_selection_opens_at_review_with_ticks() {
        let pending = Pending {
            kinds: vec![GwKind::Podman, GwKind::Docker],
        };
        let w = Wizard::new(
            Mode::First,
            linux_docker_ready(),
            &[],
            false,
            Some(pending),
            planner(),
        );
        assert_eq!(w.screen, Screen::Review);
        assert_eq!(w.wanted(), [GwKind::Podman, GwKind::Docker]);
        assert_eq!(w.steps.len(), 2);
    }

    #[test]
    fn install_action_carries_steps_and_kinds_and_finish_comes_after() {
        let mut w = at_gateways(linux_docker_ready());
        cursor_to(&mut w, GwKind::Docker);
        press(&mut w, &[KeyCode::Char(' '), KeyCode::Enter]);
        match w.on_key(KeyCode::Enter) {
            Action::Install(steps, kinds) => {
                assert_eq!(steps.len(), 1);
                assert_eq!(kinds, [GwKind::Docker]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(w.screen, Screen::Install);
        assert!(
            matches!(w.on_key(KeyCode::Enter), Action::None),
            "keys do nothing while installing"
        );
        w.show_done();
        assert_eq!(w.screen, Screen::Done);
        assert!(matches!(w.on_key(KeyCode::Enter), Action::Finish));
    }

    #[test]
    fn cursor_stays_inside_the_list() {
        let mut w = at_gateways(linux_docker_ready());
        for _ in 0..10 {
            w.on_key(KeyCode::Down);
        }
        assert_eq!(w.cursor, 2);
        for _ in 0..10 {
            w.on_key(KeyCode::Up);
        }
        assert_eq!(w.cursor, 0);
    }

    // ---- drawing --------------------------------------------------------------------------

    use ratatui::{backend::TestBackend, style::Color, Terminal};
    use std::time::Duration;

    fn render(
        w: &Wizard,
        p: &Progress,
        width: u16,
        height: u16,
    ) -> (Vec<String>, ratatui::buffer::Buffer) {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| draw(f, w, p)).unwrap();
        let buf = term.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        (rows, buf)
    }

    fn progress() -> Progress {
        Progress {
            rows: vec![
                RowState::Done(Duration::from_secs(12)),
                RowState::Done(Duration::from_secs(9)),
                RowState::Running,
                RowState::Pending,
            ],
            now: vec![
                "$ systemctl --user enable --now podman.socket".into(),
                "Created symlink".into(),
            ],
            finished: false,
            ok: false,
        }
    }

    fn named_step(title: &str) -> Step {
        Step {
            id: "s",
            title: title.into(),
            kind: StepKind::Manual("m".into()),
            optional: false,
            after: None,
        }
    }

    fn wizard_with(screen: Screen) -> Wizard {
        let mut w = Wizard::new(
            Mode::First,
            linux_docker_ready(),
            &[],
            false,
            None,
            planner(),
        );
        cursor_to(&mut w, GwKind::Docker);
        press(&mut w, &[KeyCode::Enter, KeyCode::Char(' ')]);
        cursor_to(&mut w, GwKind::Podman);
        press(&mut w, &[KeyCode::Char(' ')]);
        w.steps = [
            "Install OpenShell",
            "Install podman",
            "Enable the podman socket",
            "Check both answer",
        ]
        .iter()
        .map(|t| named_step(t))
        .collect();
        w.screen = screen;
        w
    }

    /// Every box's left and right edges run straight down: rows between a top-left corner and its
    /// bottom-left corner have a vertical bar at the corner's column, and likewise on the right.
    fn assert_boxes_aligned(rows: &[String]) {
        let grid: Vec<Vec<char>> = rows.iter().map(|r| r.chars().collect()).collect();
        for (y, row) in grid.iter().enumerate() {
            for (x, c) in row.iter().enumerate() {
                if *c != '┌' {
                    continue;
                }
                let right = row[x..]
                    .iter()
                    .position(|c| *c == '┐')
                    .map(|i| i + x)
                    .expect("top edge ends with ┐");
                let mut yy = y + 1;
                loop {
                    let r = grid.get(yy).unwrap_or_else(|| {
                        panic!("box at ({x},{y}) never closes:\n{}", rows.join("\n"))
                    });
                    match r.get(x) {
                        Some('└') => {
                            assert_eq!(
                                r.get(right),
                                Some(&'┘'),
                                "bottom-right corner off:\n{}",
                                rows.join("\n")
                            );
                            break;
                        }
                        Some('│') => assert_eq!(
                            r.get(right),
                            Some(&'│'),
                            "right edge off at row {yy}:\n{}",
                            rows.join("\n")
                        ),
                        other => {
                            panic!("left edge off at row {yy}: {other:?}\n{}", rows.join("\n"))
                        }
                    }
                    yy += 1;
                }
            }
        }
    }

    fn all_text(rows: &[String]) -> String {
        rows.join("\n")
    }

    #[test]
    fn machine_screen_lists_what_was_found() {
        let w = wizard_with(Screen::Machine);
        let (rows, _) = render(&w, &progress(), 80, 24);
        let t = all_text(&rows);
        assert!(
            t.contains("What I found") && t.contains("apt") && t.contains("sudo"),
            "{t}"
        );
        assert!(
            t.contains("Docker")
                && t.contains("Podman")
                && t.contains("KVM")
                && t.contains("OpenShell"),
            "{t}"
        );
        assert!(t.contains("continue"), "key hints: {t}");
        assert_boxes_aligned(&rows);
    }

    #[test]
    fn gateways_screen_shows_list_and_details_for_the_highlighted_type() {
        let mut w = wizard_with(Screen::Gateways);
        cursor_to(&mut w, GwKind::Podman);
        let (rows, _) = render(&w, &progress(), 80, 24);
        let t = all_text(&rows);
        assert!(
            t.contains("[x] Docker") && t.contains("[x] Podman") && t.contains("[-] VM"),
            "{t}"
        );
        assert!(
            t.contains("What it needs") && t.contains("podman") && t.contains("will be installed"),
            "{t}"
        );
        assert!(t.contains("1st ticked = main"), "{t}");
        assert_boxes_aligned(&rows);
    }

    #[test]
    fn every_box_row_has_equal_width_at_80_columns() {
        for screen in [
            Screen::Machine,
            Screen::Gateways,
            Screen::Review,
            Screen::Install,
            Screen::Done,
        ] {
            let mut w = wizard_with(screen);
            w.screen = screen;
            let mut p = progress();
            p.finished = screen == Screen::Done;
            p.ok = true;
            let (rows, _) = render(&w, &p, 80, 24);
            assert_boxes_aligned(&rows);
            let widths: std::collections::BTreeSet<usize> = rows
                .iter()
                .map(|r| r.trim_start())
                .filter(|r| r.starts_with('┌') || r.starts_with('│') || r.starts_with('└'))
                .map(|r| r.trim_end().chars().count())
                .collect();
            assert!(
                widths.len() <= 2,
                "{screen:?}: full-width boxes differ in width {widths:?}\n{}",
                rows.join("\n")
            );
        }
    }

    #[test]
    fn columns_line_up_in_the_review_and_install_screens() {
        let w = wizard_with(Screen::Install);
        let (rows, _) = render(&w, &progress(), 80, 24);
        let twelve = rows.iter().find(|r| r.contains("12s")).unwrap();
        let nine = rows.iter().find(|r| r.contains(" 9s")).unwrap();
        let end = |r: &str, needle: &str| r.find(needle).unwrap() + needle.len();
        assert_eq!(
            end(twelve, "12s"),
            end(nine, "9s"),
            "times are right-aligned:\n{}",
            all_text(&rows)
        );
        let titles = |needle: &str| {
            rows.iter()
                .find(|r| r.contains(needle))
                .unwrap()
                .find(needle)
                .unwrap()
        };
        assert_eq!(titles("Install OpenShell"), titles("Install podman"));
        let w = wizard_with(Screen::Review);
        let (rows, _) = render(&w, &progress(), 80, 24);
        let pos = |needle: &str| {
            rows.iter()
                .find(|r| r.contains(needle))
                .unwrap()
                .find(needle)
                .unwrap()
        };
        assert_eq!(
            pos("Install OpenShell"),
            pos("Install podman"),
            "{}",
            all_text(&rows)
        );
    }

    #[test]
    fn install_screen_shows_progress_and_the_command_being_run() {
        let w = wizard_with(Screen::Install);
        let (rows, _) = render(&w, &progress(), 80, 24);
        let t = all_text(&rows);
        assert!(t.contains("Progress") && t.contains("Now running"), "{t}");
        assert!(
            t.contains("systemctl --user enable --now podman.socket"),
            "{t}"
        );
        assert!(t.contains("✓") && t.contains("·"), "{t}");
    }

    #[test]
    fn unavailable_row_is_dim_and_shows_the_reason() {
        let mut w = wizard_with(Screen::Gateways);
        cursor_to(&mut w, GwKind::Vm);
        let (rows, buf) = render(&w, &progress(), 80, 24);
        let y = rows.iter().position(|r| r.contains("[-] VM")).unwrap();
        let x = rows[y].find("[-] VM").unwrap();
        assert_eq!(buf[(x as u16, y as u16)].fg, Color::DarkGray);
        let t = all_text(&rows);
        assert!(t.contains("no KVM") && t.contains("/dev/kvm"), "{t}");
    }

    #[test]
    fn stacked_layout_under_70_columns_has_no_details_pane() {
        let w = wizard_with(Screen::Gateways);
        let (rows, _) = render(&w, &progress(), 60, 20);
        let t = all_text(&rows);
        assert!(!t.contains("What it needs"), "{t}");
        assert!(t.contains("Docker") && t.contains("Podman"), "{t}");
        assert_boxes_aligned(&rows);
    }

    #[test]
    fn nothing_panics_on_any_screen_at_small_sizes() {
        for screen in [
            Screen::Machine,
            Screen::Gateways,
            Screen::Review,
            Screen::Install,
            Screen::Done,
        ] {
            for (w_, h_) in [(80, 24), (60, 20), (40, 12), (20, 6), (120, 40)] {
                let mut w = wizard_with(screen);
                w.note = "a note that is long enough to need wrapping or truncating at small sizes"
                    .into();
                let mut p = progress();
                p.finished = true;
                render(&w, &p, w_, h_);
                p.ok = true;
                render(&w, &p, w_, h_);
            }
        }
    }

    #[test]
    fn done_screen_lists_gateways_and_what_is_left_to_do() {
        let w = wizard_with(Screen::Done);
        let mut p = progress();
        p.finished = true;
        p.ok = true;
        p.rows = vec![RowState::Done(Duration::from_secs(1)); 4];
        let (rows, _) = render(&w, &p, 80, 24);
        let t = all_text(&rows);
        assert!(
            t.contains("All set")
                && t.contains("openshell-docker")
                && t.contains("openshell-podman")
                && t.contains("main"),
            "{t}"
        );
        assert!(t.contains("open the dashboard"), "{t}");
        let mut p = progress();
        p.finished = true;
        p.ok = false;
        p.rows = vec![
            RowState::Done(Duration::from_secs(1)),
            RowState::Manual("log out and back in, then run osh setup again".into()),
            RowState::Failed("apt-get exited with 100".into()),
            RowState::Pending,
        ];
        let (rows, _) = render(&w, &p, 80, 24);
        let t = all_text(&rows);
        assert!(
            t.contains("log out and back in") && t.contains("apt-get exited"),
            "{t}"
        );
        assert!(!t.contains("All set"), "{t}");
    }

    #[test]
    fn review_marks_privileged_steps_and_names_the_main_gateway() {
        let mut w = wizard_with(Screen::Review);
        w.steps[0].kind = StepKind::Cmd {
            prog: "sudo".into(),
            args: vec!["apt-get".into()],
        };
        let (rows, _) = render(&w, &progress(), 80, 24);
        let t = all_text(&rows);
        assert!(t.contains("sudo"), "{t}");
        assert!(t.contains("openshell-docker") && t.contains("main"), "{t}");
        assert!(t.contains("Install") && t.contains("enter"), "{t}");
    }

    /// `OSH_PRINT_WIZARD=1 cargo test print_screens -- --nocapture` shows every screen as text.
    #[test]
    fn print_screens_when_asked() {
        if std::env::var_os("OSH_PRINT_WIZARD").is_none() {
            return;
        }
        for screen in [
            Screen::Machine,
            Screen::Gateways,
            Screen::Review,
            Screen::Install,
            Screen::Done,
        ] {
            let mut w = wizard_with(screen);
            cursor_to(&mut w, GwKind::Podman);
            let mut p = progress();
            if screen == Screen::Done {
                p.finished = true;
                p.ok = true;
            }
            let (rows, _) = render(&w, &p, 80, 24);
            println!("===== {screen:?}\n{}", rows.join("\n"));
        }
    }
}
