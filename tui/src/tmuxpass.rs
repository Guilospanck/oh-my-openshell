//! Host tmux keys for the shell in the sandbox. The host tmux takes its prefix and root-table
//! bindings (`M-h`, new tab, ...) before osh sees them, so they never reach the tmux in the
//! sandbox. `key-table` and `prefix` are per session, so switching them off would also take the
//! shortcuts from every other window of the host session. Instead each binding is wrapped as
//! "if this window has `@osh_focus`, send the key to the pane, else run the original", and the
//! flag is a window option set only on osh's window while a shell has focus. A wrapper with the
//! flag unset behaves like the original, so a crash leaves the host working.

use std::{
    io::Write,
    process::{Command, Stdio},
};

const FLAG: &str = "#{@osh_focus}";

/// A `list-keys` line: `bind-key [-r] -T table key command...`.
struct Bind<'a> {
    repeat: bool,
    key: &'a str,
    command: &'a str,
}

fn parse(line: &str) -> Option<Bind<'_>> {
    let mut rest = line.trim_start().strip_prefix("bind-key")?;
    let mut repeat = false;
    loop {
        rest = rest.trim_start();
        let (word, tail) = rest.split_once(char::is_whitespace)?;
        match word {
            "-r" => repeat = true,
            "-T" => {
                // The table name: the caller already asked for one table.
                let (_, tail) = tail.trim_start().split_once(char::is_whitespace)?;
                rest = tail;
                continue;
            }
            w if w.starts_with('-') => {}
            _ => break,
        }
        rest = tail;
    }
    let (key, command) = rest.split_once(char::is_whitespace)?;
    Some(Bind {
        repeat,
        key,
        command: command.trim(),
    })
}

fn is_mouse(key: &str) -> bool {
    ["Mouse", "Wheel", "Click"].iter().any(|m| key.contains(m))
}

fn flags(b: &Bind) -> &'static str {
    if b.repeat {
        "-r "
    } else {
        ""
    }
}

/// tmux commands that wrap every root binding and the prefix, given `list-keys -T root`,
/// `list-keys -T prefix`, the global `prefix` and the prefix already saved by an earlier install
/// (empty when none). Empty when there is nothing to do. Idempotent: wrapped bindings are kept.
pub fn install_script(root: &str, prefix_table: &str, prefix: &str, saved: &str) -> String {
    let real = if prefix == "None" { saved } else { prefix };
    let mut out = String::new();
    for b in root.lines().filter_map(parse) {
        if is_mouse(b.key) || b.command.contains("@osh_focus") {
            continue;
        }
        out.push_str(&format!(
            "bind-key {}-T root {k} if-shell -F \"{FLAG}\" {{ send-keys {k} }} {{ {c} }}\n",
            flags(&b),
            k = b.key,
            c = b.command
        ));
    }
    if real.is_empty() || real == "None" {
        return out;
    }
    if !root.lines().filter_map(parse).any(|b| b.key == real) {
        out.push_str(&format!(
            "bind-key -T root {real} if-shell -F \"{FLAG}\" {{ send-keys {real} }} {{ switch-client -T prefix }}\n"
        ));
    }
    // `send-prefix` sends the prefix option, which is None from here on.
    for b in prefix_table.lines().filter_map(parse) {
        if b.command == "send-prefix" {
            out.push_str(&format!(
                "bind-key {}-T prefix {k} send-keys {real}\n",
                flags(&b),
                k = b.key
            ));
        }
    }
    out.push_str(&format!("set-option -g @osh_prefix {real}\n"));
    // The wrapped root binding takes the prefix key, so tmux's own prefix handling (which runs
    // before root bindings) must be off for the flag to decide.
    out.push_str("set-option -g prefix None\n");
    out
}

/// Undo the prefix part of `install_script` (the root wrappers stay: they are inert without
/// the flag): the saved prefix back, its root binding gone, `send-prefix` bindings restored.
pub fn release_script(prefix_table: &str, prefix: &str, saved: &str) -> String {
    if prefix != "None" || saved.is_empty() {
        return String::new();
    }
    let mut out = format!("unbind-key -T root {saved}\n");
    for b in prefix_table.lines().filter_map(parse) {
        if b.command == format!("send-keys {saved}") {
            out.push_str(&format!(
                "bind-key {}-T prefix {} send-prefix\n",
                flags(&b),
                b.key
            ));
        }
    }
    out.push_str(&format!("set-option -g prefix {saved}\n"));
    out.push_str("set-option -g -u @osh_prefix\n");
    out
}

fn tmux(args: &[&str], stdin: Option<&str>) -> String {
    let Ok(mut child) = Command::new("tmux")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(text.as_bytes());
    }
    child
        .wait_with_output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Switched off from the command palette; kept as a marker file in the data dir across runs.
static OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const OFF_FILE: &str = ".tmux-passthrough-off";

/// Read the palette's choice (call once at start).
pub fn load(dir: &std::path::Path) {
    OFF.store(
        dir.join(OFF_FILE).exists(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Flip the palette's choice and keep it; returns whether passing keys is now on.
pub fn toggle(dir: &std::path::Path) -> std::io::Result<bool> {
    let off = !OFF.load(std::sync::atomic::Ordering::Relaxed);
    if off {
        std::fs::write(dir.join(OFF_FILE), "")?;
    } else {
        match std::fs::remove_file(dir.join(OFF_FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    OFF.store(off, std::sync::atomic::Ordering::Relaxed);
    Ok(!off)
}

fn enabled() -> bool {
    std::env::var_os("TMUX").is_some()
        && std::env::var("OSH_TMUX_PASSTHROUGH").as_deref() != Ok("0")
        && !OFF.load(std::sync::atomic::Ordering::Relaxed)
}

/// At start: an osh that crashed with a shell focused left its window flagged and the host
/// prefix handed over. Clear this window's flag, then give the prefix back the way quitting does
/// (still kept when another osh window has a focused shell).
pub fn recover() {
    if std::env::var_os("TMUX").is_none() {
        return;
    }
    if let Ok(pane) = std::env::var("TMUX_PANE") {
        tmux(&["set-option", "-w", "-t", &pane, "-u", "@osh_focus"], None);
    }
    let (_, table, prefix, saved) = state();
    if saved.is_empty() {
        return;
    }
    if tmux(&["list-windows", "-a", "-F", "#{@osh_focus}"], None)
        .lines()
        .any(|l| l == "1")
    {
        return;
    }
    let script = release_script(&table, &prefix, &saved);
    if !script.is_empty() {
        tmux(&["source-file", "-"], Some(&script));
    }
}

fn state() -> (String, String, String, String) {
    let one = |args: &[&str]| tmux(args, None).trim().to_string();
    (
        tmux(&["list-keys", "-T", "root"], None),
        tmux(&["list-keys", "-T", "prefix"], None),
        one(&["show-options", "-gv", "prefix"]),
        one(&["show-options", "-gqv", "@osh_prefix"]),
    )
}

/// A shell takes focus (`on`) or the dashboard gets it back: the host's keys go to this
/// window's pane only while `on`. The palette (or `OSH_TMUX_PASSTHROUGH=0`) turns it off.
pub fn focus(on: bool) {
    let (true, Ok(pane)) = (enabled(), std::env::var("TMUX_PANE")) else {
        return;
    };
    if on {
        let (root, table, prefix, saved) = state();
        let script = install_script(&root, &table, &prefix, &saved);
        if !script.is_empty() {
            tmux(&["source-file", "-"], Some(&script));
        }
        tmux(&["set-option", "-w", "-t", &pane, "@osh_focus", "1"], None);
    } else {
        tmux(&["set-option", "-w", "-t", &pane, "-u", "@osh_focus"], None);
    }
}

/// osh quits: give the prefix back unless another window still has a shell focused.
pub fn release() {
    if !enabled() {
        return;
    }
    if tmux(&["list-windows", "-a", "-F", "#{@osh_focus}"], None)
        .lines()
        .any(|l| l == "1")
    {
        return;
    }
    let (_, table, prefix, saved) = state();
    let script = release_script(&table, &prefix, &saved);
    if !script.is_empty() {
        tmux(&["source-file", "-"], Some(&script));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_palette_switch_is_kept_in_the_data_dir() {
        let d = std::env::temp_dir().join(format!("osh-tp-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        super::load(&d);
        assert!(!super::toggle(&d).unwrap(), "first press turns it off");
        assert!(d.join(super::OFF_FILE).exists());
        super::load(&d);
        assert!(super::OFF.load(std::sync::atomic::Ordering::Relaxed));
        assert!(super::toggle(&d).unwrap(), "and back on");
        assert!(!d.join(super::OFF_FILE).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    use super::*;

    const ROOT: &str = "bind-key    -T root         M-h                if-shell -F \"#{pane_at_left}\" \"previous-window\" \"select-pane -L\"\n\
bind-key -r -T root         M-Up               resize-pane -U 2\n\
bind-key    -T root         MouseDown1Status   select-window -t =\n";
    const TABLE: &str = "bind-key    -T prefix       C-a                send-prefix\n\
bind-key    -T prefix       c                  new-window\n";

    #[test]
    fn wraps_root_bindings_per_window_and_leaves_mouse_alone() {
        let s = install_script(ROOT, TABLE, "C-a", "");
        assert!(s.contains(
            "bind-key -T root M-h if-shell -F \"#{@osh_focus}\" { send-keys M-h } { if-shell -F \"#{pane_at_left}\" \"previous-window\" \"select-pane -L\" }"
        ));
        assert!(s.contains("bind-key -r -T root M-Up if-shell -F \"#{@osh_focus}\" { send-keys M-Up } { resize-pane -U 2 }"));
        assert!(
            !s.contains("MouseDown1Status"),
            "tab clicks stay the host's"
        );
        assert!(s.contains("bind-key -T root C-a if-shell -F \"#{@osh_focus}\" { send-keys C-a } { switch-client -T prefix }"));
        assert!(s.contains("bind-key -T prefix C-a send-keys C-a"));
        assert!(
            !s.contains("prefix c"),
            "other prefix bindings are untouched"
        );
        assert!(s.ends_with("set-option -g @osh_prefix C-a\nset-option -g prefix None\n"));
    }

    #[test]
    fn installing_twice_changes_nothing_more() {
        let first = install_script(ROOT, TABLE, "C-a", "");
        let root2 = first
            .lines()
            .filter(|l| l.contains("-T root"))
            .map(|l| format!("{l}\n"))
            .collect::<String>();
        let table2 = "bind-key -T prefix C-a send-keys C-a\n";
        let again = install_script(&root2, table2, "None", "C-a");
        assert!(!again.contains("-T root M-h"), "already wrapped: {again}");
        assert!(
            !again.contains("-T root C-a"),
            "prefix already wrapped: {again}"
        );
        assert!(again.ends_with("set-option -g prefix None\n"));
    }

    #[test]
    fn release_restores_the_prefix_and_send_prefix() {
        let s = release_script("bind-key -T prefix C-a send-keys C-a\n", "None", "C-a");
        assert_eq!(
            s,
            "unbind-key -T root C-a\nbind-key -T prefix C-a send-prefix\nset-option -g prefix C-a\nset-option -g -u @osh_prefix\n"
        );
        assert_eq!(release_script("", "C-a", ""), "", "nothing was installed");
    }

    #[test]
    fn a_host_without_a_prefix_gets_only_the_root_wrappers() {
        let s = install_script(ROOT, TABLE, "None", "");
        assert!(s.contains("-T root M-h"));
        assert!(!s.contains("prefix None"));
    }
}
