//! agend-tui: multi-tab client for the agend daemon.
//!
//! Two modes:
//!   agend-tui          — connect to every running agent, tabbed chrome
//!   agend-tui <name>   — single agent, raw full-screen (legacy behavior)
//!
//! In multi-tab mode only the active tab is rendered to the real screen;
//! inactive tabs keep feeding their own `VTerm` model so switching back
//! shows current state. In single-agent mode there is no chrome — the
//! PTY output flows straight through with no row offset.
//!
//! Keybindings (Ctrl+B is the prefix, tmux-style):
//!   Ctrl+B n        — next tab            (multi-tab only)
//!   Ctrl+B p        — prev tab            (multi-tab only)
//!   Ctrl+B 1..9     — jump to tab N       (multi-tab only)
//!   Ctrl+B d        — detach
//!   Ctrl+B <other>  — pass Ctrl+B + key through to the active agent
//!                      (including n/p/digits when only one agent is attached)

use agend_pty_poc::vterm::VTerm;
use agend_pty_poc::{ipc, paths};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const TAG_DATA: u8 = 0;
const TAG_RESIZE: u8 = 1;
const TAB_BAR_ROWS: u16 = 1;
const STATUS_BAR_ROWS: u16 = 1;
const CHROME_ROWS: u16 = TAB_BAR_ROWS + STATUS_BAR_ROWS;

// ── Wire protocol ────────────────────────────────────────────────────────

fn write_tagged(w: &mut impl Write, tag: u8, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&[tag])?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)?;
    w.flush()
}

fn write_data(w: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    write_tagged(w, TAG_DATA, data)
}

fn read_frame(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1_000_000 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn send_resize(w: &mut impl Write, cols: u16, rows: u16) -> std::io::Result<()> {
    let mut data = [0u8; 4];
    data[0..2].copy_from_slice(&cols.to_be_bytes());
    data[2..4].copy_from_slice(&rows.to_be_bytes());
    write_tagged(w, TAG_RESIZE, &data)
}

// ── Shared app state ─────────────────────────────────────────────────────

struct Tab {
    name: String,
    write: Mutex<TcpStream>,
    vterm: Mutex<VTerm>,
    alive: AtomicBool,
}

struct App {
    tabs: Vec<Arc<Tab>>,
    active: AtomicUsize,
    needs_render: AtomicBool,
    show_chrome: bool,
}

impl App {
    fn active_idx(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    fn set_active(&self, idx: usize) {
        if idx < self.tabs.len() {
            self.active.store(idx, Ordering::Release);
            self.needs_render.store(true, Ordering::Release);
        }
    }

    fn mark_render(&self) {
        self.needs_render.store(true, Ordering::Release);
    }

    fn take_render(&self) -> bool {
        self.needs_render.swap(false, Ordering::AcqRel)
    }

    fn render_pending(&self) -> bool {
        self.needs_render.load(Ordering::Acquire)
    }

    fn content_offset(&self) -> u16 {
        if self.show_chrome {
            TAB_BAR_ROWS
        } else {
            0
        }
    }
}

// ── Terminal lifecycle ───────────────────────────────────────────────────

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Self {
        terminal::enable_raw_mode().expect("enable raw mode");
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?1049h\x1b[2J\x1b[H");
        let _ = out.flush();
        Self
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?1049l");
        let _ = out.flush();
        terminal::disable_raw_mode().ok();
    }
}

fn content_rows(total_rows: u16, show_chrome: bool) -> u16 {
    let reserved = if show_chrome { CHROME_ROWS } else { 0 };
    total_rows.saturating_sub(reserved).max(1)
}

// ── Dump post-processing ─────────────────────────────────────────────────
//
// VTerm::dump_screen emits CUP escapes (`ESC [ <row> ; <col> H`) using its
// own 1-based coordinates. In multi-tab mode we render the tab bar on row 1,
// so content must start on row 2 — add 1 to every CUP row. In single-agent
// mode the offset is 0 (no-op rewrite, just copies bytes through).

fn rewrite_cursor(dump: &[u8], offset: u16) -> Vec<u8> {
    if offset == 0 {
        return dump.to_vec();
    }
    let mut out = Vec::with_capacity(dump.len() + 64);
    let mut i = 0;
    while i < dump.len() {
        if dump[i] == 0x1b && i + 1 < dump.len() && dump[i + 1] == b'[' {
            let params_start = i + 2;
            let mut j = params_start;
            while j < dump.len() && (0x20..=0x3F).contains(&dump[j]) {
                j += 1;
            }
            if j < dump.len() {
                let term = dump[j];
                if term == b'H' || term == b'f' {
                    let (row, col) = parse_cup(&dump[params_start..j]);
                    let shifted = format!("\x1b[{};{}H", row.saturating_add(offset), col);
                    out.extend_from_slice(shifted.as_bytes());
                } else {
                    out.extend_from_slice(&dump[i..=j]);
                }
                i = j + 1;
                continue;
            }
        }
        out.push(dump[i]);
        i += 1;
    }
    out
}

fn parse_cup(params: &[u8]) -> (u16, u16) {
    let s = std::str::from_utf8(params).unwrap_or("");
    let mut parts = s.split(';').map(|p| {
        if p.is_empty() {
            1u16
        } else {
            p.parse::<u16>().unwrap_or(1)
        }
    });
    let row = parts.next().unwrap_or(1).max(1);
    let col = parts.next().unwrap_or(1).max(1);
    (row, col)
}

// ── Tab bar + status bar rendering ───────────────────────────────────────

fn render_tab_bar(tabs: &[Arc<Tab>], active: usize, cols: u16) -> Vec<u8> {
    let mut visible = String::new();
    for (i, t) in tabs.iter().enumerate() {
        let marker = if t.alive.load(Ordering::Relaxed) {
            ""
        } else {
            "!"
        };
        if i == active {
            visible.push_str(&format!("\x1b[7m[{}{}]\x1b[0m ", t.name, marker));
        } else {
            visible.push_str(&format!("[{}{}] ", t.name, marker));
        }
    }
    let truncated = truncate_ansi(&visible, cols as usize);
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[1;1H\x1b[2K");
    out.extend_from_slice(truncated.as_bytes());
    out.extend_from_slice(b"\x1b[0m");
    out
}

fn render_status_bar(active: &Tab, tab_count: usize, rows: u16, cols: u16) -> Vec<u8> {
    let state = if active.alive.load(Ordering::Relaxed) {
        "Running"
    } else {
        "Disconnected"
    };
    let text = format!(
        " {}:{} │ {} agent(s) │ Ctrl+B: n=next p=prev 1-9=jump d=detach",
        active.name, state, tab_count
    );
    let truncated = truncate_ansi(&text, cols as usize);
    let mut out = Vec::new();
    out.extend_from_slice(format!("\x1b[{};1H\x1b[2K\x1b[7m", rows).as_bytes());
    out.extend_from_slice(truncated.as_bytes());
    out.extend_from_slice(b"\x1b[0m");
    out
}

/// Truncate a string that may contain ANSI CSI escapes so its visible content
/// fits within `max_cols` terminal columns. Escape sequences are copied
/// verbatim (zero visible width). Character width honors East Asian wide / CJK
/// / emoji via `unicode-width` (wide chars = 2 cols).
fn truncate_ansi(s: &str, max_cols: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut visible = 0usize;
    let mut iter = s.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if ch == '\x1b' {
            if let Some(&(_, '[')) = iter.peek() {
                // consume '['
                iter.next();
                let start = idx;
                let mut end_excl = idx + 2; // bytes consumed so far: ESC + [
                while let Some(&(j, c)) = iter.peek() {
                    end_excl = j + c.len_utf8();
                    iter.next();
                    let byte = c as u32;
                    if !(0x20u32..=0x3F).contains(&byte) {
                        // this char is the CSI terminator (or outside range)
                        break;
                    }
                }
                out.push_str(&s[start..end_excl]);
                continue;
            }
            // Lone ESC: treat as 1 visible (shouldn't really happen in our UI).
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w > 0 && visible + w > max_cols {
            break;
        }
        out.push(ch);
        visible += w;
    }
    out
}

// ── Render orchestration ─────────────────────────────────────────────────

fn render(app: &App, cols: u16, rows: u16, stdout: &mut std::io::Stdout) -> std::io::Result<()> {
    let idx = app.active_idx();
    if idx >= app.tabs.len() {
        return Ok(());
    }
    let tab = &app.tabs[idx];
    let dump = {
        let vt = tab.vterm.lock().unwrap_or_else(|e| e.into_inner());
        vt.dump_screen()
    };
    let shifted = rewrite_cursor(&dump, app.content_offset());

    let mut buf = Vec::with_capacity(shifted.len() + 512);
    buf.extend_from_slice(&shifted);
    if app.show_chrome {
        // Overlay tab bar + status bar with DEC save/restore cursor so the
        // content cursor position (from the shifted dump) survives the draw.
        buf.extend_from_slice(b"\x1b7");
        buf.extend_from_slice(&render_tab_bar(&app.tabs, idx, cols));
        buf.extend_from_slice(&render_status_bar(tab, app.tabs.len(), rows, cols));
        buf.extend_from_slice(b"\x1b8");
    }
    stdout.write_all(&buf)?;
    stdout.flush()
}

fn broadcast_resize(app: &App, cols: u16, rows: u16) {
    let content = content_rows(rows, app.show_chrome);
    for tab in &app.tabs {
        if let Ok(mut w) = tab.write.lock() {
            if send_resize(&mut *w, cols, content).is_err() {
                tab.alive.store(false, Ordering::Relaxed);
            }
        }
        if let Ok(mut vt) = tab.vterm.lock() {
            vt.resize(cols, content);
        }
    }
    app.mark_render();
}

fn forward_to_active(app: &App, bytes: &[u8]) {
    let idx = app.active_idx();
    if idx >= app.tabs.len() {
        return;
    }
    let tab = &app.tabs[idx];
    if !tab.alive.load(Ordering::Relaxed) {
        return;
    }
    let mut w = match tab.write.lock() {
        Ok(w) => w,
        Err(e) => e.into_inner(),
    };
    if write_data(&mut *w, bytes).is_err() {
        tab.alive.store(false, Ordering::Relaxed);
        app.mark_render();
    }
}

// ── Connection setup ─────────────────────────────────────────────────────

struct PendingTab {
    tab: Arc<Tab>,
    read_stream: TcpStream,
}

fn connect_agent(name: &str, cols: u16, content: u16) -> Option<PendingTab> {
    let agent_dir = paths::find_agent_dir(name)?;
    let stream = ipc::connect_named(&agent_dir, "tui").ok()?;
    let write_stream = stream.try_clone().ok()?;
    let read_stream = stream;

    let tab = Arc::new(Tab {
        name: name.to_owned(),
        write: Mutex::new(write_stream),
        vterm: Mutex::new(VTerm::new(cols, content)),
        alive: AtomicBool::new(true),
    });

    if let Ok(mut w) = tab.write.lock() {
        let _ = send_resize(&mut *w, cols, content);
    }

    Some(PendingTab { tab, read_stream })
}

fn spawn_output_thread(app: Arc<App>, index: usize, tab: Arc<Tab>, mut read_stream: TcpStream) {
    std::thread::Builder::new()
        .name(format!("tui-out-{}", tab.name))
        .spawn(move || {
            while let Ok(data) = read_frame(&mut read_stream) {
                {
                    let mut vt = tab.vterm.lock().unwrap_or_else(|e| e.into_inner());
                    vt.process(&data);
                }
                if app.active_idx() == index {
                    app.mark_render();
                }
            }
            tab.alive.store(false, Ordering::Relaxed);
            app.mark_render();
        })
        .expect("spawn output thread");
}

// ── Input loop ───────────────────────────────────────────────────────────

enum InputAction {
    Continue,
    Detach,
}

fn handle_key(
    app: &App,
    ctrl_b_pressed: &mut bool,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> InputAction {
    if *ctrl_b_pressed {
        *ctrl_b_pressed = false;
        // Detach always; tab navigation only when more than one tab is
        // attached. Otherwise fall through to pass-through so single-agent
        // users can still literally send Ctrl+B + {n,p,digit} to the agent.
        let multi = app.tabs.len() > 1;
        match code {
            KeyCode::Char('d') => return InputAction::Detach,
            KeyCode::Char('n') if multi => {
                let n = app.tabs.len();
                app.set_active((app.active_idx() + 1) % n);
                return InputAction::Continue;
            }
            KeyCode::Char('p') if multi => {
                let n = app.tabs.len();
                app.set_active((app.active_idx() + n - 1) % n);
                return InputAction::Continue;
            }
            KeyCode::Char(c) if multi && ('1'..='9').contains(&c) => {
                let target = (c as u8 - b'1') as usize;
                if target < app.tabs.len() {
                    app.set_active(target);
                }
                return InputAction::Continue;
            }
            _ => {
                // Pass-through: buffered Ctrl+B (0x02) followed by this key.
                let mut bytes = vec![0x02];
                bytes.extend_from_slice(&key_to_bytes(code, modifiers));
                forward_to_active(app, &bytes);
                return InputAction::Continue;
            }
        }
    }
    if code == KeyCode::Char('b') && modifiers.contains(KeyModifiers::CONTROL) {
        *ctrl_b_pressed = true;
        return InputAction::Continue;
    }
    let bytes = key_to_bytes(code, modifiers);
    if !bytes.is_empty() {
        forward_to_active(app, &bytes);
    }
    InputAction::Continue
}

// ── Entry point ──────────────────────────────────────────────────────────

enum Mode {
    MultiTab(Vec<String>),
    SingleAgent(String),
}

fn parse_mode() -> Result<Mode, i32> {
    let mut args = std::env::args().skip(1);
    match args.next() {
        Some(name) => {
            if paths::find_agent_dir(&name).is_none() {
                eprintln!("Agent '{name}' not found.");
                let avail = paths::list_agents();
                if avail.is_empty() {
                    eprintln!("No daemon running. Start with: agend-pty daemon");
                } else {
                    eprintln!("Available agents:");
                    for a in avail {
                        eprintln!("  {a}");
                    }
                }
                return Err(1);
            }
            Ok(Mode::SingleAgent(name))
        }
        None => {
            let mut all = paths::list_agents();
            if all.is_empty() {
                eprintln!("No agents available.");
                eprintln!("Is the daemon running? Try: agend-pty daemon");
                return Err(1);
            }
            // Sort for deterministic tab ordering — the daemon's agents
            // directory returns entries in filesystem traversal order.
            all.sort();
            Ok(Mode::MultiTab(all))
        }
    }
}

fn main() {
    let mode = match parse_mode() {
        Ok(m) => m,
        Err(code) => std::process::exit(code),
    };

    let (agents, show_chrome) = match mode {
        Mode::MultiTab(list) => (list, true),
        Mode::SingleAgent(name) => (vec![name], false),
    };

    let (cols, rows) = terminal::size().unwrap_or((120, 40));
    let content = content_rows(rows, show_chrome);

    let mut pending: Vec<PendingTab> = Vec::new();
    for name in &agents {
        match connect_agent(name, cols, content) {
            Some(p) => pending.push(p),
            None => eprintln!("warn: could not connect to agent '{name}', skipping"),
        }
    }
    if pending.is_empty() {
        eprintln!("No reachable agents.");
        std::process::exit(1);
    }

    let all_tabs: Vec<Arc<Tab>> = pending.iter().map(|p| Arc::clone(&p.tab)).collect();
    let app = Arc::new(App {
        tabs: all_tabs,
        active: AtomicUsize::new(0),
        needs_render: AtomicBool::new(true),
        show_chrome,
    });

    let _guard = RawModeGuard::enter();

    for (i, p) in pending.into_iter().enumerate() {
        spawn_output_thread(Arc::clone(&app), i, Arc::clone(&p.tab), p.read_stream);
    }

    let mut last_cols = cols;
    let mut last_rows = rows;
    let mut ctrl_b_pressed = false;
    let mut stdout = std::io::stdout();
    let mut last_activity = Instant::now();

    let _ = render(&app, last_cols, last_rows, &mut stdout);

    'main: loop {
        // Adaptive poll cadence: short when activity is recent (smooth PTY
        // output), long when idle (keeps CPU near zero during quiet periods).
        let timeout =
            if app.render_pending() || last_activity.elapsed() < Duration::from_millis(500) {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(200)
            };
        let polled = event::poll(timeout).unwrap_or(false);
        if polled {
            last_activity = Instant::now();
            match event::read() {
                Ok(Event::Key(KeyEvent {
                    code, modifiers, ..
                })) => match handle_key(&app, &mut ctrl_b_pressed, code, modifiers) {
                    InputAction::Detach => break 'main,
                    InputAction::Continue => {}
                },
                Ok(Event::Paste(text)) => {
                    forward_to_active(&app, text.as_bytes());
                }
                Ok(Event::Resize(c, r)) => {
                    broadcast_resize(&app, c, r);
                    last_cols = c;
                    last_rows = r;
                }
                Ok(_) => {}
                Err(_) => break 'main,
            }
        } else if let Ok((c, r)) = terminal::size() {
            if c != last_cols || r != last_rows {
                broadcast_resize(&app, c, r);
                last_cols = c;
                last_rows = r;
                last_activity = Instant::now();
            }
        }

        if app.take_render() {
            let _ = render(&app, last_cols, last_rows, &mut stdout);
            last_activity = Instant::now();
        }
    }

    drop(_guard);
    eprintln!("\r\n[tui] detached. (Ctrl+B d)");
}

// ── Keycode translation ──────────────────────────────────────────────────

fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            vec![(c.to_ascii_lowercase() as u8) - b'a' + 1]
        }
        KeyCode::Char(_) if ctrl => vec![],
        KeyCode::Char(c) if alt => {
            let mut v = vec![0x1b];
            let mut b = [0u8; 4];
            v.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            v
        }
        KeyCode::Char(c) => {
            let mut b = [0u8; 4];
            c.encode_utf8(&mut b).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => vec![],
        },
        _ => vec![],
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_cursor_adds_offset_to_cup() {
        let src = b"\x1b[H\x1b[2J hello \x1b[5;10Hx";
        let out = rewrite_cursor(src, 1);
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("\x1b[2;1H"),
            "ESC[H should become ESC[2;1H: {s:?}"
        );
        assert!(s.contains("\x1b[6;10H"), "ESC[5;10H should shift: {s:?}");
        assert!(s.contains("\x1b[2J"), "clear should survive: {s:?}");
    }

    #[test]
    fn rewrite_cursor_preserves_non_cup() {
        let src = b"\x1b[7mhello\x1b[0m\x1b[?25h";
        let out = rewrite_cursor(src, 3);
        assert_eq!(out, src);
    }

    #[test]
    fn rewrite_cursor_handles_cup_without_col() {
        let src = b"\x1b[7H";
        let out = rewrite_cursor(src, 2);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b[9;1H");
    }

    #[test]
    fn rewrite_cursor_zero_offset_is_identity() {
        // Single-agent mode: no rewrite, pure passthrough.
        let src = b"\x1b[H\x1b[3;7Hxxx\x1b[0m";
        let out = rewrite_cursor(src, 0);
        assert_eq!(out, src);
    }

    #[test]
    fn parse_cup_defaults() {
        assert_eq!(parse_cup(b""), (1, 1));
        assert_eq!(parse_cup(b"7"), (7, 1));
        assert_eq!(parse_cup(b"7;5"), (7, 5));
        assert_eq!(parse_cup(b";5"), (1, 5));
    }

    #[test]
    fn truncate_ansi_preserves_escapes_and_caps_visible() {
        let s = "\x1b[7mhello world\x1b[0m";
        let t = truncate_ansi(s, 5);
        assert!(t.starts_with("\x1b[7m"));
        assert!(t.contains("hello"));
        assert!(!t.contains("world"));
    }

    #[test]
    fn content_rows_reserves_space_with_chrome() {
        assert_eq!(content_rows(40, true), 38);
        assert_eq!(content_rows(3, true), 1);
        assert_eq!(content_rows(2, true), 1); // saturating
        assert_eq!(content_rows(0, true), 1);
    }

    #[test]
    fn content_rows_no_reservation_without_chrome() {
        assert_eq!(content_rows(40, false), 40);
        assert_eq!(content_rows(1, false), 1);
        assert_eq!(content_rows(0, false), 1);
    }

    #[test]
    fn truncate_ansi_zero_cols() {
        let out = truncate_ansi("\x1b[1mhi\x1b[0m", 0);
        assert!(!out.contains('h'));
    }

    #[test]
    fn truncate_ansi_cjk_counted_as_two_cols() {
        // Each CJK char is 2 terminal columns. With max_cols=2, exactly one
        // character fits.
        let t = truncate_ansi("中文abc", 2);
        assert!(t.contains("中"), "first CJK should fit: {t:?}");
        assert!(
            !t.contains("文"),
            "second CJK would exceed 2-col budget: {t:?}"
        );
        assert!(!t.contains("a"));
    }

    #[test]
    fn truncate_ansi_cjk_across_wider_budget() {
        // 6 cols = exactly 3 CJK chars
        let t = truncate_ansi("中文字abc", 6);
        assert!(t.contains("中") && t.contains("文") && t.contains("字"));
        assert!(!t.contains("a"));
    }

    #[test]
    fn truncate_ansi_mixed_ascii_and_cjk() {
        // "ab中" = 1 + 1 + 2 = 4 cols; with budget 3 → "ab" only (3-col
        // threshold reached before CJK; CJK alone is 2 and 2+2>3)
        let t = truncate_ansi("ab中cd", 3);
        assert_eq!(t, "ab");
    }

    #[test]
    fn truncate_ansi_escape_passthrough_doesnt_count_cols() {
        // A string that's mostly escapes + 1 visible char should render that
        // char even with max_cols=1.
        let t = truncate_ansi("\x1b[1;31;4mX\x1b[0m", 1);
        assert!(t.contains('X'));
        assert!(t.starts_with("\x1b[1;31;4m"));
        assert!(t.ends_with("\x1b[0m"));
    }
}
