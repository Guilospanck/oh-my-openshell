//! osh: ratatui front end for OpenShell. Dashboard of sandboxes with live
//! status, embedded sandbox shells, plus creating, providers, profiles and images.
mod agents;
mod app;
mod assets;
mod cli;
mod clip;
mod gateway;
mod ghrepo;
mod gwmodel;
mod gwops;
mod logview;
mod platform;
mod setup;
mod term;
mod ui;
mod watch;
mod wizard;

use app::{App, Done, Failed, Step};
use ratatui::crossterm::{
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::supports_keyboard_enhancement,
};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

fn pause() {
    print!("\nPress Enter to return to osh...");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
}

/// How the host terminal can report modified keys such as Shift-Enter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HostKeys {
    Legacy,
    /// The kitty keyboard protocol.
    Kitty,
    /// tmux's extended keys, asked for with modifyOtherKeys; only in the `csi-u` format,
    /// since crossterm cannot parse the xterm one (`CSI 27;mod;key~`) and drops it.
    TmuxCsiU,
}

/// tmux does not answer the kitty query, but with `extended-keys on` (or `always`) and
/// `extended-keys-format csi-u` it reports Shift-Enter as `CSI 13;2u` once asked.
fn tmux_keys(extended: &str, format: &str) -> bool {
    matches!(extended.trim(), "on" | "always") && format.trim() == "csi-u"
}

fn host_keys(kitty: bool) -> HostKeys {
    if kitty {
        return HostKeys::Kitty;
    }
    if std::env::var_os("TMUX").is_none() {
        return HostKeys::Legacy;
    }
    let opt = |name: &str| {
        Command::new("tmux")
            .args(["show", "-gv", name])
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    };
    if tmux_keys(&opt("extended-keys"), &opt("extended-keys-format")) {
        HostKeys::TmuxCsiU
    } else {
        HostKeys::Legacy
    }
}

/// Mouse, paste and focus reports (and modified keys, when the host has them) are only
/// wanted while a shell has focus; the dashboard keeps the host terminal's own.
fn host_modes(on: bool, keys: HostKeys) {
    let mut out = io::stdout();
    if on {
        let _ = execute!(
            out,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        );
        match keys {
            HostKeys::Kitty => {
                let _ = execute!(
                    out,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    )
                );
            }
            // modifyOtherKeys level 1: only keys with no legacy form (Shift-Enter) change.
            HostKeys::TmuxCsiU => {
                let _ = out.write_all(b"\x1b[>4;1m").and_then(|_| out.flush());
            }
            HostKeys::Legacy => {}
        }
    } else {
        match keys {
            HostKeys::Kitty => {
                let _ = execute!(out, PopKeyboardEnhancementFlags);
            }
            HostKeys::TmuxCsiU => {
                let _ = out.write_all(b"\x1b[>4;0m").and_then(|_| out.flush());
            }
            HostKeys::Legacy => {}
        }
        let _ = execute!(
            out,
            DisableMouseCapture,
            DisableBracketedPaste,
            DisableFocusChange
        );
    }
}

/// Single-quote `s` for `sh -c`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The step run under script(1), which keeps the child on a terminal (prompts, progress
/// bars and colours work as before) while it records the output to `log`.
fn recorded(s: &Step, log: &Path) -> Command {
    let mut c = Command::new("script");
    if cfg!(target_os = "macos") {
        // BSD script: `script -q FILE CMD ARGS...`, exits with the command's status.
        c.arg("-q").arg(log).arg(&s.prog).args(&s.args);
    } else {
        // util-linux script: one shell command; -e returns its status.
        let line = std::iter::once(s.prog.as_str())
            .chain(s.args.iter().map(String::as_str))
            .map(sh_quote)
            .collect::<Vec<_>>()
            .join(" ");
        c.args(["-q", "-e", "-c", &line]).arg(log);
    }
    c
}

/// Hand the terminal to child processes, then take it back. Returns the step that failed;
/// with `capture`, the end of its output too.
fn run_steps(steps: Vec<Step>, capture: bool) -> Option<Failed> {
    ratatui::restore();
    let log = std::env::temp_dir().join(format!("osh-step-{}.log", std::process::id()));
    let capture = capture && cli::have("script");
    let mut failed = None;
    for (index, s) in steps.iter().enumerate() {
        println!("$ {} {}", s.prog, s.args.join(" "));
        let mut cmd = if capture {
            recorded(s, &log)
        } else {
            Command::new(&s.prog)
        };
        if !capture {
            cmd.args(&s.args);
        }
        let status = match cmd.status() {
            Ok(st) if st.success() => continue,
            Ok(st) => format!("exited with {st}"),
            Err(e) => e.to_string(),
        };
        println!("\n{} {status}", s.prog);
        let raw = if capture {
            std::fs::read(&log).unwrap_or_default()
        } else {
            Vec::new()
        };
        failed = Some(Failed {
            index,
            label: s.label(),
            status,
            tail: app::clean_tail(&String::from_utf8_lossy(&raw), 12),
        });
        break;
    }
    let _ = std::fs::remove_file(&log);
    pause();
    failed
}

/// Opens the set-up wizard before the dashboard when this looks like a first run (or a stopped
/// install to resume). Returns false when osh should exit instead of opening the dashboard.
fn first_run(data: &Path) -> bool {
    let reg = gwmodel::Registry::load(data);
    let have = cli::have("openshell");
    let facts = platform::gather(&platform::RealHost);
    // If the CLI cannot say which gateways exist, do not guess: the user's own gateway could be
    // among them. Open the dashboard as it is.
    let known = match setup::known_gateways(&facts, &platform::RealHost) {
        Ok(k) => k,
        Err(e) => {
            if have {
                eprintln!("osh: {e}");
            }
            return have;
        }
    };
    // A remembered selection whose gateways all exist now has nothing to resume.
    if let Some(p) = gwmodel::Pending::load(data) {
        if setup::pending_is_stale(&p, &reg, &known) {
            gwmodel::Pending::clear(data);
        }
    }
    let pending = gwmodel::Pending::load(data).is_some();
    if !wizard::needs_wizard(have, known.len(), reg.gateways.is_empty(), pending) {
        return true;
    }
    if !io::stdin().is_terminal() {
        if !have {
            eprintln!("osh: OpenShell is not installed. Run `osh setup --gateways docker` (or podman, vm) to set it up.");
        }
        return have;
    }
    match wizard::run_modal(wizard::Mode::First, data) {
        Ok(wizard::Outcome::Installed) => {}
        Ok(wizard::Outcome::Cancelled) => return have,
        Ok(wizard::Outcome::NeedsAction(text)) => {
            println!("osh: {text}");
            // Something is left to do, but if OpenShell and a gateway work, the dashboard still helps.
            return have && !known.is_empty();
        }
        Err(e) => {
            eprintln!("osh: the set-up screen failed: {e}");
            return false;
        }
    }
    if !cli::have("openshell") {
        println!("OpenShell is installed but not on PATH yet: open a new terminal (or add its folder to PATH) and run osh again.");
        return false;
    }
    true
}

/// `osh doctor` and `osh setup [--yes] [--dry-run] [--runtime docker|podman]`; None for the dashboard.
fn subcommand(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str)? {
        "doctor" => {
            let (text, ok) = setup::doctor(&platform::gather(&platform::RealHost));
            print!("{text}");
            Some(if ok { 0 } else { 1 })
        }
        "setup" => {
            materialize_data();
            let mut o = setup::Options::default();
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--yes" | "-y" => o.yes = true,
                    "--dry-run" => o.dry_run = true,
                    "--gateways" => match it.next().map(|l| setup::parse_gateways(l)) {
                        Some(Ok(k)) => o.gateways = k,
                        Some(Err(e)) => {
                            eprintln!("osh setup: {e}");
                            return Some(2);
                        }
                        None => {
                            eprintln!("--gateways takes a list such as docker,podman");
                            return Some(2);
                        }
                    },
                    // The older flag: one runtime means one gateway of that type.
                    "--runtime" => match it.next().and_then(|r| gwmodel::GwKind::parse(r)) {
                        Some(k) if k != gwmodel::GwKind::Vm => o.gateways = vec![k],
                        _ => {
                            eprintln!("--runtime takes docker or podman");
                            return Some(2);
                        }
                    },
                    other => {
                        eprintln!("osh setup: unknown option {other}");
                        return Some(2);
                    }
                }
            }
            if !o.gateways.is_empty() {
                return Some(setup::run_gateways(&o));
            }
            // Without flags and with a terminal, `osh setup` is the guided wizard.
            if !o.yes && !o.dry_run && io::stdin().is_terminal() {
                let data = assets::data_dir();
                return Some(match wizard::run_modal(wizard::Mode::First, &data) {
                    Ok(wizard::Outcome::Installed | wizard::Outcome::Cancelled) => 0,
                    Ok(wizard::Outcome::NeedsAction(t)) => {
                        println!("osh: {t}");
                        1
                    }
                    Err(e) => {
                        eprintln!("osh: the set-up screen failed: {e}");
                        1
                    }
                });
            }
            Some(if setup::run(&o, false) { 0 } else { 1 })
        }
        "gateway-docker" => {
            materialize_data();
            let o = setup::Options {
                gateways: vec![gwmodel::GwKind::Docker],
                ..Default::default()
            };
            Some(setup::run_gateways(&o))
        }
        "gateway" => Some(gateway_add_remove(&args[1..])),
        "sync-agents" => Some(agents::cli_main(&args[1..], &assets::data_dir())),
        "upgrade" | "uninstall" | "gateway-reset" | "gateway-service" => {
            Some(gateway_command(args))
        }
        _ => None,
    }
}

/// Host-changing gateway commands. Each shows its steps and asks before running them.
fn gateway_command(args: &[String]) -> i32 {
    use platform::Host as _;
    let h = platform::RealHost;
    let f = platform::gather(&h);
    let p = gateway::Paths::detect();
    let o = setup::Options::default();
    let interactive = io::stdin().is_terminal();
    let steps = match args[0].as_str() {
        "upgrade" => Ok(gateway::upgrade_steps(&f, &o, &h, &p)),
        "gateway-service" if args.get(1).map(String::as_str) == Some("docker") => {
            let e = gateway::exec_docker_service(&p, &assets::data_dir());
            eprintln!("osh: cannot start the gateway: {e}");
            return 1;
        }
        "gateway-service" => match args.get(1) {
            Some(name) => {
                let e = gwops::service_exec(name, &p);
                eprintln!("osh: cannot start the gateway {name}: {e}");
                return 1;
            }
            None => Err("usage: osh gateway-service <name>".to_string()),
        },
        "uninstall" => {
            let steps = gateway::uninstall_steps(&f, &p);
            println!(
                "This will remove OpenShell, its services and these folders (sandboxes are lost):"
            );
            println!(
                "  {}\n  {}\n  {}",
                p.config_home.display(),
                p.state.display(),
                p.share.display()
            );
            if !confirmed("uninstall") {
                eprintln!("aborted");
                return 1;
            }
            Ok(steps)
        }
        _ => {
            let Some(name) = args.get(1) else {
                eprintln!("usage: osh gateway-reset <gateway>");
                return 2;
            };
            // The dashboard asks its own question first and passes --confirmed.
            if !pre_confirmed(args) {
                println!("This deletes every sandbox and provider on gateway {name}.");
                if !confirmed("reset") {
                    eprintln!("aborted");
                    return 1;
                }
            }
            let reg = gwmodel::Registry::load(&p.data);
            let shared = gwops::shares_runtime(&reg, name);
            if let Some(spec) = reg.get(name) {
                Ok(gwops::reset_steps_for(&f, &p, spec, shared))
            } else {
                // Sweep with the runtime the installed gateway drives, not whichever answers first.
                let driver = h
                    .read(&p.config_home.join("gateway.toml"))
                    .and_then(|t| gateway::driver_runtime(&t));
                gateway::reset_steps(&f, &p, name, driver.or(f.runtime()), shared)
            }
        }
    };
    match steps {
        Ok(steps) => {
            // Uninstall and reset were confirmed by typing; upgrade and gateway-docker ask per step.
            let typed = matches!(args[0].as_str(), "uninstall" | "gateway-reset");
            let o = setup::Options { yes: typed, ..o };
            i32::from(!setup::run_steps(&h, steps, &o, interactive))
        }
        Err(e) => {
            eprintln!("osh: {e}");
            2
        }
    }
}

/// `osh gateway add <vm|docker|podman> [--yes] [--dry-run]` and
/// `osh gateway remove <name> [--delete-data]`.
fn gateway_add_remove(args: &[String]) -> i32 {
    let usage = "usage: osh gateway add <vm|docker|podman> [--yes] [--dry-run] | osh gateway remove <name> [--delete-data]";
    match args.first().map(String::as_str) {
        Some("add") => {
            let Some(kind) = args.get(1).and_then(|k| gwmodel::GwKind::parse(k)) else {
                eprintln!("{usage}");
                return 2;
            };
            materialize_data();
            let o = setup::Options {
                gateways: vec![kind],
                yes: args.iter().any(|a| a == "--yes" || a == "-y"),
                dry_run: args.iter().any(|a| a == "--dry-run"),
                ..Default::default()
            };
            setup::run_gateways(&o)
        }
        Some("remove") => {
            let Some(name) = args.get(1).filter(|n| !n.starts_with("--")) else {
                eprintln!("{usage}");
                return 2;
            };
            let delete = args.iter().any(|a| a == "--delete-data");
            let h = platform::RealHost;
            let f = platform::gather(&h);
            let p = gateway::Paths::detect();
            let reg = gwmodel::Registry::load(&p.data);
            let steps = match gwops::remove_gateway_steps(&f, &p, &reg, name, delete) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("osh: {e}");
                    return 2;
                }
            };
            let word = if delete {
                "remove and delete data"
            } else {
                "remove"
            };
            println!(
                "This removes the gateway {name}{}.",
                if delete {
                    " and its data"
                } else {
                    " (its data stays)"
                }
            );
            // The dashboard asks its own question first and passes --confirmed.
            if !pre_confirmed(args) && !confirmed(word) {
                eprintln!("aborted");
                return 1;
            }
            let o = setup::Options {
                yes: true,
                ..Default::default()
            };
            i32::from(!setup::run_steps(&h, steps, &o, io::stdin().is_terminal()))
        }
        _ => {
            eprintln!("{usage}");
            2
        }
    }
}

/// True when the caller (the dashboard) already asked the user and passed `--confirmed`.
fn pre_confirmed(args: &[String]) -> bool {
    args.iter().any(|a| a == "--confirmed")
}

/// Asks the user to type `word`; `--yes` does not skip it, since these steps destroy data.
fn confirmed(word: &str) -> bool {
    print!("Type '{word}' to continue: ");
    let _ = io::stdout().flush();
    let mut a = String::new();
    let _ = io::stdin().read_line(&mut a);
    if word == "reset" {
        gateway::confirm_reset(&a)
    } else {
        a.trim() == word
    }
}

/// Writes the embedded providers, Dockerfiles and configs to the data dir, and returns it.
fn materialize_data() -> PathBuf {
    let dir = assets::data_dir();
    // OSH_DIR is a checkout in development: its files are the source, not a copy to refresh.
    if std::env::var_os("OSH_DIR").is_none() {
        match assets::materialize(&dir) {
            Ok(r) => {
                for p in r.kept_user_edits {
                    eprintln!("osh: keeping your edited {}", p.display());
                }
            }
            Err(e) => eprintln!("osh: cannot write {}: {e}", dir.display()),
        }
    }
    dir
}

fn main() -> io::Result<()> {
    if platform::valid_home(std::env::var_os("HOME")).is_none() {
        eprintln!("osh: HOME is not set to an absolute path; refusing to run");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = subcommand(&args) {
        std::process::exit(code);
    }
    let dir = materialize_data();
    if !first_run(&dir) {
        return Ok(());
    }
    let (snap_tx, snap_rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel::<Done>();
    let selected = Arc::new(Mutex::new(None));
    cli::spawn_poller(dir.clone(), snap_tx, selected.clone(), wake_rx);

    let mut app = App::new(dir, selected, wake_tx, done_tx);
    if !std::env::args().any(|a| a == "--ui") {
        app.jump_to(cli::find_shared());
    }
    let mut terminal = ratatui::init();
    let keys = host_keys(supports_keyboard_enhancement().unwrap_or(false));
    let mut captured = false;
    let result = (|| -> io::Result<()> {
        while !app.quit {
            if let Some(lv) = &mut app.logview {
                lv.sync();
            }
            app.pump_sessions();
            app.pump_watch();
            app.tick();
            let focused = app.active.is_some();
            if focused != captured {
                host_modes(focused, keys);
                captured = focused;
            }
            if focused {
                let size = terminal.size()?;
                app.fit_sessions(size.width, size.height);
            }
            terminal.draw(|f| ui::draw(f, &app))?;
            // A focused shell wants frames at display rate; the dashboard does not, bar a spinner.
            let wait = if focused {
                16
            } else if app.busy() {
                80
            } else {
                150
            };
            if event::poll(Duration::from_millis(wait))? {
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                        Event::Key(k) if k.kind == KeyEventKind::Repeat && app.active.is_some() => {
                            app.on_key(k)
                        }
                        Event::Mouse(m) => app.on_mouse(m),
                        Event::Paste(p) => app.on_paste(p),
                        Event::FocusGained => app.on_focus(true),
                        Event::FocusLost => app.on_focus(false),
                        Event::Resize(w, h) => app.fit_sessions(w, h),
                        _ => {}
                    }
                    if app.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            while let Ok(s) = snap_rx.try_recv() {
                if app.status == "Loading..." || app.status == "Refreshing..." {
                    app.status = if s.gateway_ok {
                        "Ready".into()
                    } else {
                        if cfg!(target_os = "macos") {
                            "Gateway unreachable: brew services start openshell (vm) or osh gateway-docker".into()
                        } else {
                            "Gateway unreachable: systemctl --user start openshell-gateway".into()
                        }
                    };
                }
                if let Some(e) = &s.error {
                    app.status = e.clone();
                }
                app.on_snapshot(s);
            }
            while let Ok(d) = done_rx.try_recv() {
                app.on_done(d);
            }
            if let Some(steps) = app.job.take() {
                if captured {
                    host_modes(false, keys);
                    captured = false;
                }
                let failed = run_steps(steps, app.capture);
                terminal = ratatui::init();
                terminal.clear()?;
                app.finish_job(failed);
                app.reload_gateways();
                app.refresh();
            }
            if let Some(mode) = app.wizard_request.take() {
                if captured {
                    host_modes(false, keys);
                    captured = false;
                }
                let data = app.dir.clone();
                match wizard::run(&mut terminal, wizard::open(mode, &data), &data) {
                    Ok(wizard::Outcome::NeedsAction(t)) => {
                        app.status = format!("Gateway setup stopped: {t}")
                    }
                    Ok(wizard::Outcome::Installed) => app.status = "Gateways set up".into(),
                    _ => {}
                }
                terminal.clear()?;
                app.reload_gateways();
                app.refresh();
            }
        }
        Ok(())
    })();
    if captured {
        host_modes(false, keys);
    }
    ratatui::restore();
    app.cleanup_sync_stage();
    result
}

#[cfg(test)]
mod tests {

    #[test]
    fn confirmed_flag_skips_the_second_question() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(pre_confirmed(&a(&[
            "remove",
            "openshell-docker",
            "--confirmed"
        ])));
        assert!(!pre_confirmed(&a(&["remove", "openshell-docker"])));
    }

    #[test]
    fn sandbox_paths_follow_the_current_folder_inside_the_share() {
        assert_eq!(
            cli::sandbox_paths("/h/proj", "/h/proj/src/a"),
            vec!["/sandbox/proj/src/a", "/sandbox/proj"]
        );
        assert_eq!(
            cli::sandbox_paths("/h/proj", "/h/proj"),
            vec!["/sandbox/proj"]
        );
        assert_eq!(
            cli::sandbox_paths("/h/proj", "/elsewhere"),
            vec!["/sandbox/proj"]
        );
    }

    use super::*;
    use crate::cli::{Cond, Image, Profile, Provider, Sandbox, Snapshot};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn share_keys_are_stable_and_label_safe() {
        assert_eq!(cli::share_key("/a/b"), cli::share_key("/a/b"));
        assert_ne!(cli::share_key("/a/b"), cli::share_key("/a/c"));
        let key = cli::share_key("/Users/x/my repo");
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn jump_offers_a_pick_across_gateways_and_waits_for_the_chosen_gateway() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        app.jump_to(vec![]);
        assert!(app.modal.is_none());
        assert!(app.status.starts_with("No ready sandbox shares"));

        let hit = |gw: &str, name: &str| (gw.to_string(), name.to_string());
        app.jump_to(vec![hit("openshell", "a"), hit("openshell-docker", "b")]);
        assert!(matches!(
            app.modal,
            Some(app::Modal::Select {
                kind: app::SelKind::OpenShared(_),
                ..
            })
        ));

        // One hit on the current gateway is pending until that gateway's snapshot loads.
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        app.jump_to(vec![hit(&cli::gateway(), "a")]);
        app.on_snapshot(Snapshot {
            gateway_ok: false,
            gateway: cli::gateway(),
            ..Default::default()
        });
        assert!(app.sessions.is_empty());
    }

    #[test]
    fn every_tab_and_modal_renders() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        app.on_snapshot(Snapshot {
            gateway_ok: true,
            version: "openshell 0".into(),
            sandboxes: vec![Sandbox {
                name: "a".into(),
                phase: "Ready".into(),
                created: "now".into(),
                conditions: vec![Cond {
                    kind: "Ready".into(),
                    status: "True".into(),
                    message: "ok".into(),
                }],
            }],
            providers: vec![Provider {
                name: "p".into(),
                ty: "p".into(),
                keys: vec!["K".into()],
            }],
            profiles: vec![Profile {
                id: "p".into(),
                version: 1,
            }],
            images: vec![Image {
                tag: "x:latest".into(),
                file: "Dockerfile-x".into(),
                built: false,
                freshness: Default::default(),
            }],
            logs: Some(("a".into(), "line1\nline2".into())),
            ..Default::default()
        });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        for tab in 0..4u8 {
            app.on_key(KeyEvent::from(KeyCode::Char((b'1' + tab) as char)));
            term.draw(|f| ui::draw(f, &app)).unwrap();
        }
        app.on_key(KeyEvent::from(KeyCode::Char('1')));
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.modal.is_some());
        term.draw(|f| ui::draw(f, &app)).unwrap();
    }

    #[test]
    fn session_view_routes_keys_and_draws_its_menu() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        let s = term::Session::spawn("sb".into(), app.snap.gateway.clone(), "cat", &[], 60, 19)
            .unwrap();
        app.sessions.push(s);
        app.active = Some(0);
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| ui::draw(f, &app)).unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit, "Ctrl-C belongs to the shell");
        app.on_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL));
        assert!(app.modal.is_some());
        term.draw(|f| ui::draw(f, &app)).unwrap();
        for _ in 0..2 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.active.is_none() && app.sessions.len() == 1 && app.has_session("sb"));
        term.draw(|f| ui::draw(f, &app)).unwrap();
        app.active = Some(0);
        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        for _ in 0..3 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.active.is_none() && app.sessions.is_empty());
    }

    #[test]
    fn resize_reaches_background_sessions() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        for name in ["a", "b"] {
            app.sessions.push(
                term::Session::spawn(name.into(), String::new(), "cat", &[], 80, 23).unwrap(),
            );
        }
        app.active = Some(0);
        app.fit_sessions(120, 40);
        assert!(app
            .sessions
            .iter()
            .all(|s| (s.vt.cols, s.vt.rows) == (120, 39)));
        assert_eq!(app::pane(120, 40), (120, 39));
    }

    #[test]
    fn blocked_prompt_queues_and_reviews() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        app.on_snapshot(Snapshot {
            sandboxes: vec![Sandbox {
                name: "sb".into(),
                phase: "Ready".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let line = "[1] [sandbox] [OCSF ] [ocsf] NET:OPEN [MED] DENIED /usr/bin/curl(0) -> example.com:443 [reason:transparent_tcp_policy_denied]";
        for _ in 0..2 {
            app.watch.push("sb", "gw", watch::parse(line).unwrap());
        }
        app.pump_watch();
        // Keys typed for the shell as the prompt opens do not answer it.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(app.modal, Some(app::Modal::Select { .. })));
        app.prompt_at = None;
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| ui::draw(f, &app)).unwrap();
        match &app.modal {
            Some(app::Modal::Select { title, items, .. }) => {
                assert_eq!(
                    title,
                    "curl was blocked from reaching example.com:443 in sb (2x)"
                );
                assert_eq!(items[0], "Allow read-only (GET/HEAD/OPTIONS) for curl");
                assert_eq!(items.len(), 5);
            }
            _ => panic!("no prompt"),
        }
        // Esc ignores this time only; p lists it and Enter asks again.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(
            app.watch.for_sandbox("sb")[0].state,
            watch::State::IgnoredOnce
        );
        app.on_key(KeyEvent::from(KeyCode::Char('p')));
        assert!(
            matches!(&app.modal, Some(app::Modal::Select { items, .. }) if items[0].starts_with("ignored once"))
        );
        term.draw(|f| ui::draw(f, &app)).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(&app.modal, Some(app::Modal::Select { title, .. }) if title.starts_with("curl was blocked"))
        );
        assert!(app.prompt_at.is_some());
    }

    #[test]
    fn tmux_extended_keys_only_in_csi_u() {
        assert!(tmux_keys("on\n", "csi-u\n"));
        assert!(tmux_keys("always", "csi-u"));
        assert!(!tmux_keys("on", "xterm"), "crossterm drops CSI 27;2;13~");
        assert!(!tmux_keys("off", "csi-u"));
        assert!(
            !tmux_keys("", ""),
            "tmux older than 3.5 has no format option"
        );
    }

    #[test]
    fn profile_keys_follow_aliases() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        assert_eq!(cli::profile_env_keys(&dir, "github"), vec!["GITHUB_TOKEN"]);
        assert_eq!(
            cli::profile_env_keys(&dir, "forgejo"),
            vec!["FORGEJO_HOST", "FORGEJO_TOKEN"]
        );
        assert_eq!(
            cli::dockerfile_tag(&PathBuf::from("images/Dockerfile-claude_code")),
            "claude-code:latest"
        );
    }

    const MANIFEST: &str = r#"{"agents":["claude-code","pi"],"stage_dir":"/tmp/stage","secret_findings":[],"items":[
      {"id":"npx-skills:caveman","group":"npx-skills","name":"caveman","default":true,"kind":"run",
       "agents":{"claude-code":["npx --yes skills@1.7.4 add a/b -g -a claude-code -s caveman -y --copy"],"pi":["npx --yes skills@1.7.4 add a/b -g -a pi -s caveman -y --copy"]}},
      {"id":"own-skills:bro","group":"own-skills","name":"bro","default":true,"kind":"upload",
       "agents":{"claude-code":{"src":"/tmp/stage/skills/bro","dest":"/workspace/.claude/skills","result":"/workspace/.claude/skills/bro"},
                 "pi":{"src":"/tmp/stage/skills/bro","dest":"/workspace/.agents/skills","result":"/workspace/.agents/skills/bro"}}},
      {"id":"plugins:swift-lsp","group":"plugins","name":"swift-lsp","default":false,"kind":"host-only",
       "agents":{"claude-code":["claude plugin install swift-lsp@m"]}}]}"#;

    fn plan(agent: &str, ids: &[&str]) -> app::SyncPlan {
        app::SyncPlan {
            sandbox: "sb".into(),
            agent: agent.into(),
            manifest: cli::parse_manifest(MANIFEST).unwrap(),
            ids: ids.iter().map(|s| s.to_string()).collect(),
            skip: 0,
        }
    }

    #[test]
    fn manifest_parses_defaults_and_findings() {
        let m = cli::parse_manifest(MANIFEST).unwrap();
        assert_eq!(m.stage_dir, "/tmp/stage");
        assert_eq!(m.findings, Some(0));
        assert_eq!(m.items.len(), 3);
        assert!(m.items[0].default && !m.items[2].default);
        assert_eq!(m.items[2].kind, "host-only");
    }

    #[test]
    fn sync_steps_follow_agent_and_selection() {
        let both = app::sync_steps(&plan("both", &["npx-skills:caveman", "own-skills:bro"]));
        assert_eq!(both.len(), 4);
        let pi = app::sync_steps(&plan("pi", &["npx-skills:caveman", "own-skills:bro"]));
        assert_eq!(pi.len(), 2);
        let install = &pi[0].args;
        assert!(install.contains(&"exec".to_string()) && install.last().unwrap().contains("-a pi"));
        assert!(install
            .last()
            .unwrap()
            .starts_with("export PATH=\"$HOME/.local/bin:$PATH\"; npx --yes skills@1.7.4 add "));
        // A directory upload targets the parent: the CLI nests it under its own name.
        assert_eq!(
            pi[1].args[1..],
            [
                "upload",
                "sb",
                "/tmp/stage/skills/bro",
                "/workspace/.agents/skills",
                "--no-git-ignore"
            ]
        );
        // Only chosen ids run; unchosen items never do.
        assert!(app::sync_steps(&plan("claude-code", &["plugins:swift-lsp"])).len() == 1);
        assert!(app::sync_steps(&plan("pi", &["plugins:swift-lsp"])).is_empty());
    }

    #[test]
    fn failed_steps_explain_themselves() {
        let rec = "^D\x1b[1m$ npx\x1b[0m\r\nCloning into 'x'...\r\nremote: Repository not found.\r\nfatal: repository 'https://github.com/me/private/' not found\r\n\r\n";
        let tail = app::clean_tail(rec, 2);
        assert_eq!(
            tail,
            vec![
                "remote: Repository not found.",
                "fatal: repository 'https://github.com/me/private/' not found"
            ]
        );
        assert!(app::auth_failure(&tail));
        assert_eq!(app::clean_tail("50%\r100%\n", 5), vec!["100%"]);
        for l in [
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
            "remote: Invalid username or token. Password authentication is not supported.\nfatal: Authentication failed for 'https://github.com/a/b/'",
            "error: The requested URL returned error: 403",
        ] {
            assert!(app::auth_failure(&app::clean_tail(l, 5)), "{l}");
        }
        assert!(!app::auth_failure(&[
            "curl: (56) CONNECT tunnel failed, response 403".into()
        ]));
        assert!(!app::auth_failure(&["npm error code E404".into()]));
        // Labels drop the PATH prefix; a retry skips the steps already done.
        let p = plan("pi", &["npx-skills:caveman", "own-skills:bro"]);
        let steps = app::sync_steps(&p);
        assert!(steps[0]
            .label()
            .starts_with("npx --yes skills@1.7.4 add a/b"));
        assert!(steps[1].label().starts_with("openshell "));
        let retry = app::sync_steps(&app::SyncPlan { skip: 1, ..p });
        assert_eq!(retry.len(), 1);
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn a_failed_sync_names_the_step_and_offers_github_for_auth_errors() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        let run = |app: &mut App, tail: &[&str]| {
            app.modal = Some(app::Modal::Confirm {
                text: "run?".into(),
                kind: app::ConfKind::Sync(plan("pi", &["npx-skills:caveman", "own-skills:bro"])),
            });
            app.on_key(KeyEvent::from(KeyCode::Char('y')));
            assert!(app.capture && app.job.take().is_some_and(|j| j.len() == 2));
            let tail = tail.iter().map(|s| s.to_string()).collect();
            app.finish_job(Some(Failed {
                index: 1,
                label: "upload bro".into(),
                status: "exited with 1".into(),
                tail,
            }));
            assert!(!app.capture);
            assert_eq!(app.status, "Step failed: upload bro (exited with 1)");
        };
        run(
            &mut app,
            &["fatal: Authentication failed for 'https://github.com/me/x/'"],
        );
        match &app.modal {
            Some(app::Modal::Confirm {
                kind: app::ConfKind::SyncSecret { sync, profile },
                text,
            }) => {
                assert_eq!((profile.as_str(), sync.skip), ("github", 1));
                assert!(text.contains("Authentication failed"));
            }
            _ => panic!("no github offer"),
        }
        run(&mut app, &["npm error code E404"]);
        assert!(
            matches!(&app.modal, Some(app::Modal::Info(t)) if t.contains("upload bro") && t.contains("E404"))
        );
    }

    #[test]
    fn blocked_title_names_each_host_once() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        let pair = |h: &str, b: &str| (h.to_string(), b.to_string());
        let check = cli::SyncCheck {
            missing: vec![
                pair("github.com", "/usr/bin/git"),
                pair("github.com", "/usr/local/bin/claude"),
                pair("pypi.org", "/usr/local/bin/uv"),
            ],
            ..Default::default()
        };
        let sync = Some(app::SyncStage::Check {
            sync: plan("claude-code", &["npx-skills:caveman"]),
            check,
            offered: 0,
        });
        app.on_done(Done {
            msg: "Policy checked".into(),
            next: None,
            sync,
            gh: None,
        });
        assert!(
            matches!(&app.modal, Some(app::Modal::Select { title, .. }) if title == "sb blocks installs; missing: github.com, pypi.org")
        );
    }

    #[test]
    fn attach_conflict_offers_to_detach_another_provider() {
        let attached = vec!["pypi".to_string(), "github".to_string(), "npm".to_string()];
        let items = app::conflict_items(&attached, "github");
        assert_eq!(items[0], "Detach pypi, then attach github and check again");
        assert_eq!(items.len(), 3);
        assert_eq!(app::conflict_pick(&items[1]), Some("npm"));
        assert_eq!(app::conflict_pick("Cancel"), None);
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        let sync = Some(app::SyncStage::Conflict {
            sync: plan("pi", &[]),
            profile: "github".into(),
            attached,
            round: 2,
        });
        app.on_done(Done {
            msg: "Could not attach github to sb".into(),
            next: None,
            sync,
            gh: None,
        });
        match &app.modal {
            Some(app::Modal::Select { title, items, .. }) => {
                assert_eq!(title, "github cannot be attached next to what sb has");
                assert_eq!(items.last().map(String::as_str), Some("Cancel"));
            }
            _ => panic!("no conflict choice"),
        }
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.modal.is_none(), "Cancel closes it");
    }

    #[test]
    fn check_reports_blocked_items_and_missing_access() {
        let c = cli::parse_check(
            r#"{"ok":false,"items":{"a":[["github.com","/usr/bin/git"]],"b":[]},
                "missing":[{"host":"github.com","binary":"/usr/bin/git"}],
                "fixes":["claude-marketplace"],"unfixed":[{"host":"x.com","binary":"/usr/bin/git"}]}"#,
        )
        .unwrap();
        assert!(!c.ok);
        assert_eq!(c.blocked, vec!["a"]);
        assert_eq!(
            c.missing,
            vec![("github.com".to_string(), "/usr/bin/git".to_string())]
        );
        assert_eq!(c.fixes, vec!["claude-marketplace"]);
        assert_eq!(
            c.unfixed,
            vec![("x.com".to_string(), "/usr/bin/git".to_string())]
        );
        assert!(cli::parse_check("not json").is_err());
    }

    #[test]
    fn long_text_opens_in_a_scrollable_viewer() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        let yaml: String = (0..60).map(|i| format!("line {i}\n")).collect();
        let sync = app::SyncPlan {
            sandbox: "sb".into(),
            agent: "pi".into(),
            manifest: cli::parse_manifest(MANIFEST).unwrap(),
            ids: vec![],
            skip: 0,
        };
        app.modal = Some(app::Modal::Confirm {
            text: "short summary".into(),
            kind: app::ConfKind::SyncGen {
                sync,
                name: "n".into(),
                yaml,
            },
        });
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Char('v')));
        for _ in 0..3 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }
        match &app.modal {
            Some(app::Modal::Viewer { scroll, text, .. }) => {
                assert_eq!(*scroll, 3);
                assert!(text.starts_with("line 0"));
            }
            _ => panic!("viewer not open"),
        }
        term.draw(|f| ui::draw(f, &app)).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Char('G')));
        match &app.modal {
            Some(app::Modal::Viewer { scroll, .. }) => assert!(*scroll > 3 && *scroll < 60),
            _ => panic!("viewer closed"),
        }
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(app.modal, Some(app::Modal::Confirm { .. })));
        term.draw(|f| ui::draw(f, &app)).unwrap();
    }
}
