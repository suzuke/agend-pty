//! agend-mcp-bridge: bridges stdio (Claude Code) ↔ Unix socket (daemon MCP).
//!
//! Claude sends NDJSON (one JSON per line) on stdin.
//! Daemon expects Content-Length framed JSON-RPC.
//! This bridge translates between the two.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

fn mcp_socket_path(name: &str) -> String {
    format!("/tmp/agend-mcp-{name}.sock")
}

fn main() {
    let agent = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: agend-mcp-bridge <agent-name>");
        std::process::exit(1);
    });

    let sock_path = mcp_socket_path(&agent);

    // Retry connection
    let stream = {
        let mut attempts = 0;
        loop {
            match UnixStream::connect(&sock_path) {
                Ok(s) => break s,
                Err(e) => {
                    attempts += 1;
                    if attempts > 30 {
                        eprintln!("[bridge] failed to connect after 30 attempts: {e}");
                        std::process::exit(1);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    };

    let mut sock_writer = stream.try_clone().expect("clone");
    let sock_reader = stream;

    // Thread: daemon (Content-Length framed) → stdout (NDJSON)
    let mut stdout = std::io::stdout();
    std::thread::Builder::new()
        .name("sock_to_stdout".into())
        .spawn(move || {
            let mut reader = BufReader::new(sock_reader);
            loop {
                // Read Content-Length header from daemon
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 { return; }
                    headers.push_str(&line);
                    if line.trim().is_empty() { break; }
                }
                let cl = headers.lines()
                    .find_map(|l| l.strip_prefix("Content-Length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                    .unwrap_or(0);
                if cl == 0 { continue; }

                let mut body = vec![0u8; cl];
                if reader.read_exact(&mut body).is_err() { return; }

                // Write as NDJSON to stdout (one JSON per line)
                if stdout.write_all(&body).is_err() { return; }
                if stdout.write_all(b"\n").is_err() { return; }
                let _ = stdout.flush();
            }
        })
        .unwrap();

    // Main thread: stdin (NDJSON from Claude) → daemon (Content-Length framed)
    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        // Forward as Content-Length framed to daemon
        let header = format!("Content-Length: {}\r\n\r\n", trimmed.len());
        if sock_writer.write_all(header.as_bytes()).is_err() { break; }
        if sock_writer.write_all(trimmed.as_bytes()).is_err() { break; }
        let _ = sock_writer.flush();
    }
}
