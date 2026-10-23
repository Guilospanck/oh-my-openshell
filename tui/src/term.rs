//! Embedded terminal: a sandbox shell on a PTY, parsed by libghostty-vt (Ghostty's
//! terminal engine) and drawn into the ratatui buffer. The VT handles are !Send, so
//! everything here lives on the UI thread; only raw bytes cross threads.
use crate::cli;
use libghostty_vt::key::{self, Action as KAction, Encoder as KeyEncoder, Event as KeyEv, Key, Mods};
use libghostty_vt::mouse::{self, Encoder as MouseEncoder, EncoderSize, Event as MouseEv};
use libghostty_vt::render::{CellIterator, RenderState, RowIterator};
use libghostty_vt::screen::{CellWide, Screen};
use libghostty_vt::style::{StyleColor, Underline};
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode, Options,
    PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes, Terminal,
    TertiaryDeviceAttributes,
};
use libghostty_vt::{focus, paste};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
};
use std::{
    cell::RefCell,
    io::{Read, Write},
    sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError},
    thread,
    time::{Duration, Instant},
};

const SCROLLBACK: usize = 10_000;
/// PTY output chunks queued before the reader blocks, which throttles the shell.
const QUEUE: usize = 64;
/// Output bytes parsed per session per loop iteration, so input stays responsive.
const BUDGET: usize = 1 << 20;

/// Ctrl-]: legacy terminals send 0x1d, which crossterm reports as Ctrl+5.
pub fn is_chord(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char(']' | '5'))
}

/// Logical key and unshifted codepoint for a character on a US layout.
fn key_from_char(c: char) -> (Key, char) {
    const LETTERS: [Key; 26] = [
        Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I, Key::J, Key::K, Key::L, Key::M,
        Key::N, Key::O, Key::P, Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V, Key::W, Key::X, Key::Y, Key::Z,
    ];
    let lc = c.to_ascii_lowercase();
    match lc {
        'a'..='z' => (LETTERS[(lc as u8 - b'a') as usize], lc),
        '0' | ')' => (Key::Digit0, '0'),
        '1' | '!' => (Key::Digit1, '1'),
        '2' | '@' => (Key::Digit2, '2'),
        '3' | '#' => (Key::Digit3, '3'),
        '4' | '$' => (Key::Digit4, '4'),
        '5' | '%' => (Key::Digit5, '5'),
        '6' | '^' => (Key::Digit6, '6'),
        '7' | '&' => (Key::Digit7, '7'),
        '8' | '*' => (Key::Digit8, '8'),
        '9' | '(' => (Key::Digit9, '9'),
        '`' | '~' => (Key::Backquote, '`'),
        '\\' | '|' => (Key::Backslash, '\\'),
        '[' | '{' => (Key::BracketLeft, '['),
        ']' | '}' => (Key::BracketRight, ']'),
        ',' | '<' => (Key::Comma, ','),
        '=' | '+' => (Key::Equal, '='),
        '-' | '_' => (Key::Minus, '-'),
        '.' | '>' => (Key::Period, '.'),
        '\'' | '"' => (Key::Quote, '\''),
        ';' | ':' => (Key::Semicolon, ';'),
        '/' | '?' => (Key::Slash, '/'),
        ' ' => (Key::Space, ' '),
        _ => (Key::Unidentified, c),
    }
}

fn map_mods(m: KeyModifiers) -> Mods {
    let mut o = Mods::empty();
    for (from, to) in [
        (KeyModifiers::SHIFT, Mods::SHIFT),
        (KeyModifiers::ALT, Mods::ALT),
        (KeyModifiers::CONTROL, Mods::CTRL),
        (KeyModifiers::SUPER, Mods::SUPER),
    ] {
        if m.contains(from) {
            o |= to;
        }
    }
    o
}

/// Fill `ev` from a crossterm key; false when the key has no terminal encoding.
fn to_ghostty_key(ev: &mut KeyEv, k: &KeyEvent) -> bool {
    let mut text = None;
    let mut unshifted = None;
    let key = match k.code {
        KeyCode::Char(c) => {
            let (key, u) = key_from_char(c);
            unshifted = Some(u);
            // Ctrl chords carry no text: the encoder derives the C0 byte.
            if !k.modifiers.contains(KeyModifiers::CONTROL) && !c.is_control() {
                text = Some(c.to_string());
            }
            key
        }
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab | KeyCode::BackTab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Esc => Key::Escape,
        KeyCode::Up => Key::ArrowUp,
        KeyCode::Down => Key::ArrowDown,
        KeyCode::Left => Key::ArrowLeft,
        KeyCode::Right => Key::ArrowRight,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Insert => Key::Insert,
        KeyCode::Delete => Key::Delete,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::F(n @ 1..=12) => [
            Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12,
        ][n as usize - 1],
        _ => return false,
    };
    let mut mods = map_mods(k.modifiers);
    if k.code == KeyCode::BackTab {
        mods |= Mods::SHIFT;
    }
    // Shift is consumed when it already produced the text, e.g. 'A' or '!'.
    let consumed = if text.is_some() && mods.contains(Mods::SHIFT) { Mods::SHIFT } else { Mods::empty() };
    ev.set_action(match k.kind {
        KeyEventKind::Press => KAction::Press,
        KeyEventKind::Repeat => KAction::Repeat,
        KeyEventKind::Release => KAction::Release,
    })
    .set_key(key)
    .set_mods(mods)
    .set_utf8(text)
    .set_consumed_mods(consumed);
    if let Some(u) = unshifted {
        ev.set_unshifted_codepoint(u);
    }
    true
}

fn map_color(c: StyleColor) -> Color {
    match c {
        StyleColor::None => Color::Reset,
        StyleColor::Palette(i) => Color::Indexed(i.0),
        StyleColor::Rgb(r) => Color::Rgb(r.r, r.g, r.b),
    }
}

type Render = (RenderState<'static>, RowIterator<'static>, CellIterator<'static>);

/// Terminal state plus the encoders that turn host input into bytes for the shell.
pub struct Vt {
    term: Terminal<'static, 'static>,
    render: RefCell<Render>,
    keys: KeyEncoder<'static>,
    kev: KeyEv<'static>,
    mice: MouseEncoder<'static>,
    mev: MouseEv<'static>,
    pub cols: u16,
    pub rows: u16,
}

fn err(e: impl std::fmt::Debug) -> String {
    format!("terminal: {e:?}")
}

impl Vt {
    /// Replies to terminal queries (device attributes, cursor reports) go to `reply`.
    pub fn new(cols: u16, rows: u16, reply: Sender<Vec<u8>>) -> Result<Self, String> {
        let (cols, rows) = (cols.max(2), rows.max(2));
        let mut term = Terminal::new(Options { cols, rows, max_scrollback: SCROLLBACK }).map_err(err)?;
        term.on_pty_write(move |_, data| {
            let _ = reply.send(data.to_vec());
        })
        .map_err(err)?;
        term.on_device_attributes(|_| {
            Some(DeviceAttributes {
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    &[DeviceAttributeFeature::COLUMNS_132, DeviceAttributeFeature::ANSI_COLOR],
                ),
                secondary: SecondaryDeviceAttributes { device_type: DeviceType::VT220, firmware_version: 1, rom_cartridge: 0 },
                tertiary: TertiaryDeviceAttributes::default(),
            })
        })
        .map_err(err)?;
        term.on_xtversion(|_| Some("osh")).map_err(err)?;
        Ok(Vt {
            term,
            render: RefCell::new((
                RenderState::new().map_err(err)?,
                RowIterator::new().map_err(err)?,
                CellIterator::new().map_err(err)?,
            )),
            keys: KeyEncoder::new().map_err(err)?,
            kev: KeyEv::new().map_err(err)?,
            mice: MouseEncoder::new().map_err(err)?,
            mev: MouseEv::new().map_err(err)?,
            cols,
            rows,
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.term.vt_write(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let (cols, rows) = (cols.max(2), rows.max(2));
        if (cols, rows) == (self.cols, self.rows) {
            return false;
        }
        (self.cols, self.rows) = (cols, rows);
        let _ = self.term.resize(cols, rows, 8, 16);
        true
    }

    pub fn key(&mut self, k: &KeyEvent) -> Vec<u8> {
        let mut out = Vec::new();
        self.keys.set_options_from_terminal(&self.term).set_macos_option_as_alt(key::OptionAsAlt::True);
        if to_ghostty_key(&mut self.kev, k) {
            let _ = self.keys.encode_to_vec(&self.kev, &mut out);
        }
        out
    }

    /// Mouse report for a position in a pane starting at the top-left corner.
    pub fn mouse(&mut self, me: &MouseEvent) -> Vec<u8> {
        let mut out = Vec::new();
        if me.column >= self.cols || me.row >= self.rows {
            return out;
        }
        let btn = |b| match b {
            MouseButton::Left => mouse::Button::Left,
            MouseButton::Right => mouse::Button::Right,
            MouseButton::Middle => mouse::Button::Middle,
        };
        let (action, button) = match me.kind {
            MouseEventKind::Down(b) => (mouse::Action::Press, Some(btn(b))),
            MouseEventKind::Up(b) => (mouse::Action::Release, Some(btn(b))),
            MouseEventKind::Drag(b) => (mouse::Action::Motion, Some(btn(b))),
            MouseEventKind::Moved => (mouse::Action::Motion, None),
            MouseEventKind::ScrollUp => (mouse::Action::Press, Some(mouse::Button::Four)),
            MouseEventKind::ScrollDown => (mouse::Action::Press, Some(mouse::Button::Five)),
            _ => return out,
        };
        // One pixel per cell; the middle of the cell avoids rounding into a neighbour.
        self.mice.set_options_from_terminal(&self.term).set_size(EncoderSize {
            screen_width: self.cols as u32,
            screen_height: self.rows as u32,
            cell_width: 1,
            cell_height: 1,
            padding_top: 0,
            padding_bottom: 0,
            padding_left: 0,
            padding_right: 0,
        });
        self.mev
            .set_action(action)
            .set_button(button)
            .set_mods(map_mods(me.modifiers))
            .set_position(mouse::Position { x: me.column as f32 + 0.5, y: me.row as f32 + 0.5 });
        let _ = self.mice.encode_to_vec(&self.mev, &mut out);
        out
    }

    pub fn paste(&self, text: String) -> Vec<u8> {
        let bracketed = self.term.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
        let mut data = text.into_bytes();
        let mut out = vec![0u8; data.len() + 32];
        match paste::encode(&mut data, bracketed, &mut out) {
            Ok(n) => {
                out.truncate(n);
                out
            }
            Err(_) => Vec::new(),
        }
    }

    pub fn focus(&self, gained: bool) -> Vec<u8> {
        if !self.term.mode(Mode::FOCUS_EVENT).unwrap_or(false) {
            return Vec::new();
        }
        let ev = if gained { focus::Event::Gained } else { focus::Event::Lost };
        let mut buf = [0u8; 8];
        ev.encode(&mut buf).map(|n| buf[..n].to_vec()).unwrap_or_default()
    }

    pub fn mouse_tracking(&self) -> bool {
        self.term.is_mouse_tracking().unwrap_or(false)
    }

    pub fn primary_screen(&self) -> bool {
        matches!(self.term.active_screen(), Ok(Screen::Primary))
    }

    /// Scroll the viewport through scrollback; negative is up.
    pub fn scroll(&mut self, rows: isize) {
        self.term.scroll_viewport(ScrollViewport::Delta(rows));
    }

    pub fn bottom(&mut self) {
        self.term.scroll_viewport(ScrollViewport::Bottom);
    }

    /// Rows the viewport sits above the live screen.
    pub fn scrolled_back(&self) -> u64 {
        self.term.scrollbar().map(|s| s.total.saturating_sub(s.offset + s.len)).unwrap_or(0)
    }

    pub fn title(&self) -> String {
        self.term.title().unwrap_or_default().to_string()
    }

    /// Last non-blank line of the viewport, for saying why a shell ended.
    pub fn last_line(&self) -> String {
        let area = Rect::new(0, 0, self.cols, self.rows);
        let mut buf = Buffer::empty(area);
        self.render(area, &mut buf);
        (0..self.rows)
            .rev()
            .map(|y| (0..self.cols).map(|x| buf[(x, y)].symbol()).collect::<String>().trim().to_string())
            .find(|l| !l.is_empty())
            .unwrap_or_default()
    }

    /// Draw the viewport into `area`; returns where the cursor is, if visible.
    pub fn render(&self, area: Rect, buf: &mut Buffer) -> Option<Position> {
        let mut r = self.render.borrow_mut();
        let (rs, rows, cells) = &mut *r;
        let snap = rs.update(&self.term).ok()?;
        let mut ri = rows.update(&snap).ok()?;
        let mut g = String::new();
        let mut y = 0;
        while let Some(row) = ri.next() {
            if y >= area.height {
                break;
            }
            let mut ci = cells.update(row).ok()?;
            let mut x = 0;
            while let Some(c) = ci.next() {
                if x >= area.width {
                    break;
                }
                let wide = c.raw_cell().ok().and_then(|r| r.wide().ok());
                if matches!(wide, Some(CellWide::SpacerTail)) {
                    x += 1;
                    continue;
                }
                g.clear();
                let _ = c.graphemes_utf8(&mut g);
                let mut s = Style::default();
                if let Ok(st) = c.style() {
                    let fg = map_color(st.fg_color);
                    let mut bg = map_color(st.bg_color);
                    if bg == Color::Reset {
                        if let Ok(Some(b)) = c.bg_color() {
                            bg = Color::Rgb(b.r, b.g, b.b);
                        }
                    }
                    let mut m = Modifier::empty();
                    for (on, flag) in [
                        (st.bold, Modifier::BOLD),
                        (st.faint, Modifier::DIM),
                        (st.italic, Modifier::ITALIC),
                        (st.inverse, Modifier::REVERSED),
                        (st.strikethrough, Modifier::CROSSED_OUT),
                        (st.invisible, Modifier::HIDDEN),
                        (st.blink, Modifier::SLOW_BLINK),
                        (st.underline != Underline::None, Modifier::UNDERLINED),
                    ] {
                        if on {
                            m |= flag;
                        }
                    }
                    s = s.fg(fg).bg(bg).add_modifier(m);
                }
                if let Some(bc) = buf.cell_mut((area.x + x, area.y + y)) {
                    bc.set_symbol(if g.is_empty() { " " } else { &g }).set_style(s);
                }
                x += 1;
            }
            y += 1;
        }
        if !snap.cursor_visible().unwrap_or(false) {
            return None;
        }
        let cv = snap.cursor_viewport().ok().flatten()?;
        (cv.x < area.width && cv.y < area.height).then(|| Position::new(area.x + cv.x, area.y + cv.y))
    }
}

/// A shell in a sandbox, running on a local PTY.
pub struct Session {
    pub sandbox: String,
    pub gateway: String,
    pub vt: Vt,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Box<dyn MasterPty + Send>,
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    /// When the child was seen gone; output still queued gets a moment to arrive.
    reaped: Option<Instant>,
    pub exited: bool,
}

impl Session {
    /// `bash` in the sandbox with `~/.local/bin` on PATH, like the other in-sandbox commands.
    pub fn open(sandbox: String, cols: u16, rows: u16) -> Result<Self, String> {
        let args = cli::with_gateway(vec![
            "sandbox".into(),
            "exec".into(),
            "-n".into(),
            sandbox.clone(),
            "--tty".into(),
            "--".into(),
            "sh".into(),
            "-c".into(),
            "export PATH=\"$HOME/.local/bin:$PATH\"; exec bash".into(),
        ]);
        Self::spawn(sandbox, cli::gateway(), "openshell", &args, cols, rows)
    }

    pub fn spawn(sandbox: String, gateway: String, prog: &str, args: &[String], cols: u16, rows: u16) -> Result<Self, String> {
        let (cols, rows) = (cols.max(2), rows.max(2));
        let pty = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        let mut cmd = CommandBuilder::new(prog);
        cmd.args(args);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if let Ok(d) = std::env::current_dir() {
            cmd.cwd(d);
        }
        let child = pty.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().map_err(|e| e.to_string())?;
        let mut writer = pty.master.take_writer().map_err(|e| e.to_string())?;

        let (tx, wrx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            for b in wrx {
                if writer.write_all(&b).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        });
        let (btx, rx): (SyncSender<Vec<u8>>, _) = mpsc::sync_channel(QUEUE);
        thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if btx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Session {
            sandbox,
            gateway,
            vt: Vt::new(cols, rows, tx.clone())?,
            child: Some(child),
            master: pty.master,
            tx,
            rx,
            reaped: None,
            exited: false,
        })
    }

    /// Parse queued output (up to the budget) and notice when the shell has ended.
    pub fn pump(&mut self) {
        let mut n = 0;
        while n < BUDGET {
            match self.rx.try_recv() {
                Ok(b) => {
                    n += b.len();
                    self.vt.feed(&b);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.exited = true;
                    break;
                }
            }
        }
        if self.reaped.is_none() {
            if let Some(c) = &mut self.child {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    self.reaped = Some(Instant::now());
                }
            }
        }
        if self.reaped.is_some_and(|t| t.elapsed() > Duration::from_millis(500)) {
            self.exited = true;
        }
    }

    fn send(&self, b: Vec<u8>) {
        if !b.is_empty() {
            let _ = self.tx.send(b);
        }
    }

    /// Window size goes to the PTY first so the shell and the parser agree.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (c, r) = (cols.max(2), rows.max(2));
        if (c, r) != (self.vt.cols, self.vt.rows) {
            let _ = self.master.resize(PtySize { rows: r, cols: c, pixel_width: 0, pixel_height: 0 });
            self.vt.resize(c, r);
        }
    }

    pub fn on_key(&mut self, k: &KeyEvent) {
        if k.modifiers.contains(KeyModifiers::SHIFT) && matches!(k.code, KeyCode::PageUp | KeyCode::PageDown) {
            let h = (self.vt.rows as isize / 2).max(1);
            self.vt.scroll(if k.code == KeyCode::PageUp { -h } else { h });
            return;
        }
        self.vt.bottom();
        let b = self.vt.key(k);
        self.send(b);
    }

    pub fn on_mouse(&mut self, me: &MouseEvent) {
        if self.vt.mouse_tracking() {
            let b = self.vt.mouse(me);
            self.send(b);
        } else if self.vt.primary_screen() {
            match me.kind {
                MouseEventKind::ScrollUp => self.vt.scroll(-3),
                MouseEventKind::ScrollDown => self.vt.scroll(3),
                _ => {}
            }
        }
    }

    pub fn on_paste(&mut self, text: String) {
        self.vt.bottom();
        let b = self.vt.paste(text);
        self.send(b);
    }

    pub fn on_focus(&mut self, gained: bool) {
        let b = self.vt.focus(gained);
        self.send(b);
    }
}

impl Drop for Session {
    /// Hang up now; reap off the UI thread, since a stubborn child takes a grace period.
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.clone_killer().kill();
            thread::spawn(move || {
                let _ = c.kill();
                let _ = c.wait();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vt() -> Vt {
        Vt::new(20, 5, mpsc::channel().0).unwrap()
    }

    fn enc(v: &mut Vt, code: KeyCode, m: KeyModifiers) -> String {
        v.key(&KeyEvent::new(code, m)).escape_ascii().to_string()
    }

    #[test]
    fn keys_encode_like_a_terminal() {
        let mut v = vt();
        let none = KeyModifiers::NONE;
        assert_eq!(enc(&mut v, KeyCode::Char('a'), none), "a");
        assert_eq!(enc(&mut v, KeyCode::Char('A'), KeyModifiers::SHIFT), "A");
        assert_eq!(enc(&mut v, KeyCode::Char('c'), KeyModifiers::CONTROL), "\\x03");
        assert_eq!(enc(&mut v, KeyCode::Up, none), "\\x1b[A");
        assert_eq!(enc(&mut v, KeyCode::Enter, none), "\\r");
        assert_eq!(enc(&mut v, KeyCode::Tab, none), "\\t");
        assert_eq!(enc(&mut v, KeyCode::Esc, none), "\\x1b");
        assert_eq!(enc(&mut v, KeyCode::F(1), none), "\\x1bOP");
        assert_eq!(enc(&mut v, KeyCode::Char('x'), KeyModifiers::ALT), "\\x1bx");
        // Ghostty's legacy form for Shift+Enter, then the kitty one once the app asks for it.
        assert_eq!(enc(&mut v, KeyCode::Enter, KeyModifiers::SHIFT), "\\x1b[27;2;13~");
        v.feed(b"\x1b[>1u");
        assert_eq!(enc(&mut v, KeyCode::Enter, KeyModifiers::SHIFT), "\\x1b[13;2u");
        assert_eq!(enc(&mut v, KeyCode::Enter, none), "\\r");
    }

    #[test]
    fn chord_accepts_both_forms() {
        assert!(is_chord(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(is_chord(&KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL)));
        assert!(!is_chord(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert!(!is_chord(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn output_renders_into_a_buffer() {
        let mut v = vt();
        v.feed("\x1b[31mred\x1b[0m 世x\r\nnext".as_bytes());
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        let cur = v.render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "r");
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(1));
        assert_eq!(buf[(3, 0)].fg, Color::Reset);
        assert_eq!(buf[(4, 0)].symbol(), "世");
        assert_eq!(buf[(6, 0)].symbol(), "x");
        assert_eq!(buf[(0, 1)].symbol(), "n");
        assert_eq!(cur, Some(Position::new(4, 1)));
    }

    #[test]
    fn session_runs_a_command() {
        let args = vec!["-c".to_string(), "echo hi".to_string()];
        let mut s = Session::spawn("t".into(), String::new(), "bash", &args, 20, 5).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while !s.exited && Instant::now() < end {
            s.pump();
            thread::sleep(Duration::from_millis(20));
        }
        assert!(s.exited);
        assert_eq!(s.vt.last_line(), "hi");
    }
}
