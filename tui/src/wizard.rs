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
}
