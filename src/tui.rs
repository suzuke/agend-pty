//! agend-tui: connects to a named agent, raw terminal passthrough.
//!
//! Usage: agend-tui [agent-name]
//!   agend-tui           # connects to "shell" (default)
//!   agend-tui dev       # connects to agent "dev"
//!
//! Ctrl+D to detach (agent keeps running).

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

const TAG_DATA: u8 = 0;
const TAG_RESIZE: u8 = 1;

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
    // Read tag (always TAG_DATA from daemon)
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1_000_000 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
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

fn socket_path(name: &str) -> String {
    format!("/tmp/agend-{name}.sock")
}

fn main() {
    let agent = std::env::args().nth(1).unwrap_or_else(|| "shell".into());
    let sock = socket_path(&agent);

    let stream = UnixStream::connect(&sock).unwrap_or_else(|e| {
        eprintln!("Failed to connect to agent '{agent}' at {sock}: {e}");
        eprintln!("Available agents:");
        for entry in std::fs::read_dir("/tmp").into_iter().flatten() {
            if let Ok(e) = entry {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("agend-") && name.ends_with(".sock")
                    && !name.contains("mcp-") && !name.contains("ctrl")
                {
                    let agent_name = &name[6..name.len()-5];
                    eprintln!("  {agent_name}");
                }
            }
        }
        std::process::exit(1);
    });

    let mut write_stream = stream.try_clone().expect("clone");
    let mut read_stream = stream;

    terminal::enable_raw_mode().expect("enable raw mode");

    // Send initial terminal size
    let (cols, rows) = terminal::size().unwrap_or((120, 40));
    let _ = send_resize(&mut write_stream, cols, rows);

    // Output thread
    std::thread::Builder::new()
        .name("output".into())
        .spawn(move || {
            let mut stdout = std::io::stdout();
            loop {
                match read_frame(&mut read_stream) {
                    Ok(data) => {
                        stdout.write_all(&data).ok();
                        stdout.flush().ok();
                    }
                    Err(_) => break,
                }
            }
        })
        .unwrap();

    // Track size for resize detection
    let mut last_cols = cols;
    let mut last_rows = rows;

    // Input loop
    loop {
        if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(KeyEvent { code, modifiers, .. })) => {
                    if code == KeyCode::Char('d') && modifiers.contains(KeyModifiers::CONTROL) {
                        break;
                    }
                    let bytes = key_to_bytes(code, modifiers);
                    if !bytes.is_empty() {
                        if write_data(&mut write_stream, &bytes).is_err() { break; }
                    }
                }
                Ok(Event::Paste(text)) => {
                    if write_data(&mut write_stream, text.as_bytes()).is_err() { break; }
                }
                Ok(Event::Resize(cols, rows)) => {
                    if send_resize(&mut write_stream, cols, rows).is_err() { break; }
                    last_cols = cols;
                    last_rows = rows;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        } else {
            // Also check for resize outside of events (some terminals don't send resize events)
            if let Ok((c, r)) = terminal::size() {
                if c != last_cols || r != last_rows {
                    let _ = send_resize(&mut write_stream, c, r);
                    last_cols = c;
                    last_rows = r;
                }
            }
        }
    }

    terminal::disable_raw_mode().ok();
    eprintln!("\r\n[tui] detached from '{agent}'.");
}

fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Char(c) if ctrl => vec![(c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1)],
        KeyCode::Char(c) if alt => { let mut v = vec![0x1b]; let mut b = [0u8;4]; v.extend_from_slice(c.encode_utf8(&mut b).as_bytes()); v }
        KeyCode::Char(c) => { let mut b = [0u8;4]; c.encode_utf8(&mut b).as_bytes().to_vec() }
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
            1 => b"\x1bOP".to_vec(), 2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(), 4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(), 6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(), 8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(), 10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(), 12 => b"\x1b[24~".to_vec(),
            _ => vec![],
        },
        _ => vec![],
    }
}
