//! Full-screen, live, scrollable and searchable view of one sandbox's logs.
use crate::cli;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{
    cell::Cell,
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    thread,
};

/// Oldest lines are dropped beyond this, so a chatty sandbox cannot grow memory forever.
const MAX_LINES: usize = 50_000;
const BACKLOG: &str = "2000";

#[derive(Default)]
pub struct Buf {
    pub lines: Vec<String>,
    /// Lines discarded from the front so far; keeps `top` stable when trimming.
    pub dropped: usize,
}

/// Values accepted by `openshell logs --source`; cycled with `s`.
const SOURCES: [&str; 3] = ["all", "gateway", "sandbox"];

pub struct LogView {
    pub name: String,
    /// Index into `SOURCES`.
    pub source: usize,
    pub buf: Arc<Mutex<Buf>>,
    child: Option<Child>,
    /// Index of the first visible line.
    pub top: usize,
    pub left: u16,
    pub follow: bool,
    /// Search text being typed, if the prompt is open.
    pub typing: Option<String>,
    pub query: String,
    /// Line of the current match.
    pub current: Option<usize>,
    /// Visible rows, set by the renderer.
    pub height: Cell<usize>,
    seen_dropped: usize,
}

/// Local time zone offset from UTC in seconds, read once from `date +%z`.
fn utc_offset() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        let z = cli::run("date", &["+%z".to_string()], &[]).unwrap_or_default();
        let z = z.trim();
        if z.len() != 5 || !z[1..].bytes().all(|b| b.is_ascii_digit()) {
            return 0;
        }
        let secs =
            z[1..3].parse::<i64>().unwrap_or(0) * 3600 + z[3..5].parse::<i64>().unwrap_or(0) * 60;
        if z.starts_with('-') {
            -secs
        } else {
            secs
        }
    })
}

/// `YYYY-MM-DD HH:MM:SS` for epoch seconds shifted by `offset` (proleptic Gregorian).
fn format_time(epoch: i64, offset: i64) -> String {
    let t = epoch + offset;
    let (days, rem) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Rewrite the leading `[1793039362.195]` epoch stamp as local `YYYY-MM-DD HH:MM:SS`.
pub fn humanize(line: &str) -> String {
    let Some(rest) = line.strip_prefix('[') else {
        return line.to_string();
    };
    let Some((stamp, tail)) = rest.split_once(']') else {
        return line.to_string();
    };
    let secs = stamp.split_once('.').map_or(stamp, |(s, _)| s);
    match secs.parse::<i64>() {
        Ok(e) if !secs.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit() || b == b'.') => {
            format!("{}{tail}", format_time(e, utc_offset()))
        }
        _ => line.to_string(),
    }
}

fn pump(r: impl Read + Send + 'static, buf: Arc<Mutex<Buf>>) {
    thread::spawn(move || {
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { break };
            let Ok(mut b) = buf.lock() else { break };
            b.lines.push(humanize(&line));
            if b.lines.len() > MAX_LINES {
                let n = b.lines.len() - MAX_LINES;
                b.lines.drain(..n);
                b.dropped += n;
            }
        }
    });
}

impl LogView {
    pub fn open(name: String) -> Result<Self, String> {
        let buf = Arc::new(Mutex::new(Buf::default()));
        let child = Self::spawn(&name, SOURCES[0], &buf)?;
        Ok(LogView {
            name,
            source: 0,
            buf,
            child: Some(child),
            top: 0,
            left: 0,
            follow: true,
            typing: None,
            query: String::new(),
            current: None,
            height: Cell::new(20),
            seen_dropped: 0,
        })
    }

    fn spawn(name: &str, source: &str, buf: &Arc<Mutex<Buf>>) -> Result<Child, String> {
        let mut args = vec![
            "logs".into(),
            name.to_string(),
            "--tail".into(),
            "-n".into(),
            BACKLOG.into(),
        ];
        if source != "all" {
            args.extend(["--source".into(), source.to_string()]);
        }
        let mut child = Command::new("openshell")
            .args(cli::with_gateway(args))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("openshell logs: {e}"))?;
        if let Some(o) = child.stdout.take() {
            pump(o, buf.clone());
        }
        if let Some(e) = child.stderr.take() {
            pump(e, buf.clone());
        }
        Ok(child)
    }

    pub fn source_name(&self) -> &'static str {
        SOURCES[self.source]
    }

    /// Switch to the next log source: restart the stream with a fresh buffer.
    fn cycle_source(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.source = (self.source + 1) % SOURCES.len();
        // A new buffer, so the old pump threads (still draining the dead pipes) write nowhere visible.
        self.buf = Arc::new(Mutex::new(Buf::default()));
        self.child = Self::spawn(&self.name, SOURCES[self.source], &self.buf).ok();
        if self.child.is_none() {
            self.buf
                .lock()
                .unwrap()
                .lines
                .push("failed to restart openshell logs".into());
        }
        self.seen_dropped = 0;
        self.top = 0;
        self.current = None;
        self.follow = true;
    }

    /// Keep `top` pointing at the same line after the buffer dropped old ones.
    pub fn sync(&mut self) {
        let Ok(b) = self.buf.lock() else { return };
        let d = b.dropped - self.seen_dropped;
        self.seen_dropped = b.dropped;
        self.top = self.top.saturating_sub(d);
        self.current = self.current.and_then(|c| c.checked_sub(d));
    }

    fn len(&self) -> usize {
        self.buf.lock().map(|b| b.lines.len()).unwrap_or(0)
    }

    fn max_top(&self) -> usize {
        self.len().saturating_sub(self.height.get().max(1))
    }

    pub fn matches(&self, line: &str) -> bool {
        !self.query.is_empty() && line.to_lowercase().contains(&self.query.to_lowercase())
    }

    fn scroll(&mut self, delta: isize) {
        let max = self.max_top();
        if self.follow {
            self.top = max;
        }
        self.top = self.top.min(max).saturating_add_signed(delta).min(max);
        self.follow = self.top >= max;
    }

    fn jump(&mut self, forward: bool) {
        let Ok(b) = self.buf.lock() else { return };
        let n = b.lines.len();
        if n == 0 || self.query.is_empty() {
            return;
        }
        let start = self.current.unwrap_or(if forward {
            self.top.saturating_sub(1)
        } else {
            self.top + 1
        });
        let found = if forward {
            (start + 1..n)
                .chain(0..=start.min(n - 1))
                .find(|&i| self.matches(&b.lines[i]))
        } else {
            (0..start.min(n))
                .rev()
                .chain((start..n).rev())
                .find(|&i| self.matches(&b.lines[i]))
        };
        drop(b);
        if let Some(i) = found {
            self.current = Some(i);
            self.follow = false;
            // Put the match about a third of the way down the view.
            self.top = i.saturating_sub(self.height.get() / 3).min(self.max_top());
        }
    }

    /// Returns false when the viewer should close.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if let Some(buf) = &mut self.typing {
            match key.code {
                KeyCode::Esc => self.typing = None,
                KeyCode::Enter => {
                    self.query = std::mem::take(buf);
                    self.typing = None;
                    self.current = None;
                    self.jump(true);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return true;
        }
        let page = self.height.get().max(2) as isize - 1;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Esc if self.query.is_empty() => return false,
            KeyCode::Esc => {
                self.query.clear();
                self.current = None;
            }
            KeyCode::Char('s') => self.cycle_source(),
            KeyCode::Char('/') => self.typing = Some(String::new()),
            KeyCode::Char('n') => self.jump(true),
            KeyCode::Char('N') => self.jump(false),
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                if self.follow {
                    self.top = self.max_top();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll(-1),
            KeyCode::PageDown => self.scroll(page),
            KeyCode::PageUp => self.scroll(-page),
            KeyCode::Char('d') if ctrl => self.scroll(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll(-page / 2),
            KeyCode::Char(' ') => self.scroll(page),
            KeyCode::Char('g') | KeyCode::Home => {
                self.top = 0;
                self.follow = false;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.top = self.max_top();
                self.follow = true;
            }
            KeyCode::Char('l') | KeyCode::Right => self.left = self.left.saturating_add(8),
            KeyCode::Char('h') | KeyCode::Left => self.left = self.left.saturating_sub(8),
            KeyCode::Char('0') => self.left = 0,
            _ => {}
        }
        true
    }
}

impl Drop for LogView {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(lines: usize) -> LogView {
        let buf = Buf {
            lines: (0..lines).map(|i| format!("line {i}")).collect(),
            dropped: 0,
        };
        LogView {
            name: "a".into(),
            source: 0,
            buf: Arc::new(Mutex::new(buf)),
            child: None,
            top: 0,
            left: 0,
            follow: true,
            typing: None,
            query: String::new(),
            current: None,
            height: Cell::new(10),
            seen_dropped: 0,
        }
    }

    #[test]
    fn formats_epoch_stamps() {
        assert_eq!(format_time(1_793_039_362, 0), "2026-10-26 18:29:22");
        assert_eq!(format_time(0, 3600), "1970-01-01 01:00:00");
        assert_eq!(format_time(951_782_400, 0), "2000-02-29 00:00:00");
        assert!(humanize("[1793039362.195] [sandbox] x").ends_with(" [sandbox] x"));
        assert!(!humanize("[1793039362.195] a").contains("1793039362"));
        assert_eq!(humanize("[sandbox] x"), "[sandbox] x");
        assert_eq!(humanize("plain"), "plain");
    }

    #[test]
    fn scrolling_up_leaves_follow_and_search_finds_line() {
        let mut v = view(100);
        v.on_key(KeyEvent::from(KeyCode::Char('k')));
        assert!(!v.follow);
        assert_eq!(v.top, 89);
        for c in "/line 42".chars() {
            v.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        v.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(v.current, Some(42));
        v.on_key(KeyEvent::from(KeyCode::Char('G')));
        assert!(v.follow);
    }
}
