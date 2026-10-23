//! osh: ratatui front end for OpenShell. Dashboard of sandboxes with live
//! status, embedded sandbox shells, plus creating, providers, profiles and images.
mod app;
mod cli;
mod logview;
mod term;
mod ui;
mod watch;

use app::{App, Done, Step};
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
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("OSH_DIR") {
        return PathBuf::from(d);
    }
    // Fall back to the symlinked config directory.
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("openshell")
}

fn pause() {
    print!("\nPress Enter to return to osh...");
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
}

/// Mouse, paste and focus reports (and kitty keys, when the host has them) are only
/// wanted while a shell has focus; the dashboard keeps the host terminal's own.
fn host_modes(on: bool, kitty: bool) {
    let mut out = io::stdout();
    if on {
        let _ = execute!(out, EnableMouseCapture, EnableBracketedPaste, EnableFocusChange);
        if kitty {
            let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        }
    } else {
        if kitty {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste, DisableFocusChange);
    }
}

/// Hand the terminal to child processes, then take it back.
fn run_steps(steps: Vec<Step>) {
    ratatui::restore();
    for s in steps {
        println!("$ {} {}", s.prog, s.args.join(" "));
        match Command::new(&s.prog).args(&s.args).status() {
            Ok(st) if st.success() => {}
            Ok(st) => {
                println!("\n{} exited with {st}", s.prog);
                break;
            }
            Err(e) => {
                println!("\n{}: {e}", s.prog);
                break;
            }
        }
    }
    pause();
}

/// Offer to install OpenShell before the dashboard opens. Returns false to quit.
fn ensure_installed(dir: &Path) -> bool {
    while !cli::have("openshell") {
        print!("openshell is not installed. Install it now? [Y/n] ");
        let _ = io::stdout().flush();
        let mut a = String::new();
        let _ = io::stdin().read_line(&mut a);
        if matches!(a.trim().to_lowercase().as_str(), "n" | "no") {
            return false;
        }
        if !cli::have("just") {
            println!("`just` is required to run the installer (brew install just).");
            return false;
        }
        let st = Command::new("just")
            .arg("--justfile")
            .arg(dir.join(".justfile"))
            .arg("install")
            .status();
        if !st.map(|s| s.success()).unwrap_or(false) {
            println!("Install failed; see the output above.");
        }
    }
    true
}

fn main() -> io::Result<()> {
    let dir = config_dir();
    if !ensure_installed(&dir) {
        return Ok(());
    }
    let (snap_tx, snap_rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel::<Done>();
    let selected = Arc::new(Mutex::new(None));
    cli::spawn_poller(dir.clone(), snap_tx, selected.clone(), wake_rx);

    let mut app = App::new(dir, selected, wake_tx, done_tx);
    let mut terminal = ratatui::init();
    let kitty = supports_keyboard_enhancement().unwrap_or(false);
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
                host_modes(focused, kitty);
                captured = focused;
            }
            if focused {
                let size = terminal.size()?;
                app.fit_sessions(size.width, size.height);
            }
            terminal.draw(|f| ui::draw(f, &app))?;
            // A focused shell wants frames at display rate; the dashboard does not.
            let wait = if focused { 16 } else { 150 };
            if event::poll(Duration::from_millis(wait))? {
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                        Event::Key(k) if k.kind == KeyEventKind::Repeat && app.active.is_some() => app.on_key(k),
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
                        "Gateway unreachable: brew services start openshell (vm) or just gateway-docker".into()
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
                    host_modes(false, kitty);
                    captured = false;
                }
                run_steps(steps);
                terminal = ratatui::init();
                terminal.clear()?;
                app.finish_job();
                app.refresh();
            }
        }
        Ok(())
    })();
    if captured {
        host_modes(false, kitty);
    }
    ratatui::restore();
    app.cleanup_sync_stage();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cond, Image, Profile, Provider, Sandbox, Snapshot};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{backend::TestBackend, Terminal};

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
                conditions: vec![Cond { kind: "Ready".into(), status: "True".into(), message: "ok".into() }],
            }],
            providers: vec![Provider { name: "p".into(), ty: "p".into(), keys: vec!["K".into()] }],
            profiles: vec![Profile { id: "p".into(), version: 1 }],
            images: vec![Image { tag: "x:latest".into(), file: "Dockerfile-x".into(), built: false, freshness: Default::default() }],
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
        let s = term::Session::spawn("sb".into(), app.snap.gateway.clone(), "cat", &[], 60, 19).unwrap();
        app.sessions.push(s);
        app.active = Some(0);
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| ui::draw(f, &app)).unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit, "Ctrl-C belongs to the shell");
        app.on_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL));
        assert!(app.modal.is_some());
        term.draw(|f| ui::draw(f, &app)).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.active.is_none() && app.sessions.len() == 1 && app.has_session("sb"));
        term.draw(|f| ui::draw(f, &app)).unwrap();
        app.active = Some(0);
        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        for _ in 0..2 {
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
            app.sessions.push(term::Session::spawn(name.into(), String::new(), "cat", &[], 80, 23).unwrap());
        }
        app.active = Some(0);
        app.fit_sessions(120, 40);
        assert!(app.sessions.iter().all(|s| (s.vt.cols, s.vt.rows) == (120, 39)));
        assert_eq!(app::pane(120, 40), (120, 39));
    }

    #[test]
    fn blocked_prompt_queues_and_reviews() {
        let (wake, _w) = mpsc::channel();
        let (done, _d) = mpsc::channel();
        let mut app = App::new(PathBuf::from("."), Arc::new(Mutex::new(None)), wake, done);
        app.on_snapshot(Snapshot {
            sandboxes: vec![Sandbox { name: "sb".into(), phase: "Ready".into(), ..Default::default() }],
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
                assert_eq!(title, "curl was blocked from reaching example.com:443 in sb (2x)");
                assert_eq!(items[0], "Allow read-only (GET/HEAD/OPTIONS) for curl");
                assert_eq!(items.len(), 4);
            }
            _ => panic!("no prompt"),
        }
        // Esc ignores this time only; p lists it and Enter asks again.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(app.watch.for_sandbox("sb")[0].state, watch::State::IgnoredOnce);
        app.on_key(KeyEvent::from(KeyCode::Char('p')));
        assert!(matches!(&app.modal, Some(app::Modal::Select { items, .. }) if items[0].starts_with("ignored once")));
        term.draw(|f| ui::draw(f, &app)).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(&app.modal, Some(app::Modal::Select { title, .. }) if title.starts_with("curl was blocked")));
        assert!(app.prompt_at.is_some());
    }

    #[test]
    fn profile_keys_follow_aliases() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        assert_eq!(cli::profile_env_keys(&dir, "github"), vec!["GITHUB_TOKEN"]);
        assert_eq!(cli::profile_env_keys(&dir, "forgejo"), vec!["FORGEJO_HOST", "FORGEJO_TOKEN"]);
        assert_eq!(cli::dockerfile_tag(&PathBuf::from("images/Dockerfile-claude_code")), "claude-code:latest");
    }

    const MANIFEST: &str = r#"{"agents":["claude-code","pi"],"stage_dir":"/tmp/stage","secret_findings":[],"items":[
      {"id":"npx-skills:caveman","group":"npx-skills","name":"caveman","default":true,"kind":"run",
       "agents":{"claude-code":["npx skills add a/b -g -a claude-code -s caveman -y --copy"],"pi":["npx skills add a/b -g -a pi -s caveman -y --copy"]}},
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
        assert!(install.last().unwrap().starts_with("export PATH=\"$HOME/.local/bin:$PATH\"; "));
        // A directory upload targets the parent: the CLI nests it under its own name.
        assert_eq!(pi[1].args[1..], ["upload", "sb", "/tmp/stage/skills/bro", "/workspace/.agents/skills", "--no-git-ignore"]);
        // Only chosen ids run; unchosen items never do.
        assert!(app::sync_steps(&plan("claude-code", &["plugins:swift-lsp"])).len() == 1);
        assert!(app::sync_steps(&plan("pi", &["plugins:swift-lsp"])).is_empty());
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
        assert_eq!(c.missing, vec![("github.com".to_string(), "/usr/bin/git".to_string())]);
        assert_eq!(c.fixes, vec!["claude-marketplace"]);
        assert_eq!(c.unfixed, vec![("x.com".to_string(), "/usr/bin/git".to_string())]);
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
        };
        app.modal = Some(app::Modal::Confirm {
            text: "short summary".into(),
            kind: app::ConfKind::SyncGen { sync, name: "n".into(), yaml },
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
