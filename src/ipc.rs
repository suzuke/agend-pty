//! Cross-platform IPC over TCP loopback.
//!
//! The daemon and each agent bind an OS-assigned port on 127.0.0.1 and write
//! it to `{run_dir}/{name}.port` (`api.port` / `ctrl.port` for the daemon's
//! control sockets, `<agent>.port` for each agent's TUI socket). Clients
//! discover ports by reading those files.
//!
//! Rationale: Unix domain sockets are not available on stable Rust for
//! Windows; named pipes would require a separate code path. TCP loopback is
//! portable and keeps a single code path across platforms. Binding is
//! restricted to 127.0.0.1 so the ports are never reachable off-host.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// Port-file name for the daemon's JSON-RPC API socket.
pub const API_NAME: &str = "api";

/// Port-file name for the daemon's control socket.
pub const CTRL_NAME: &str = "ctrl";

/// Bind a TCP listener on 127.0.0.1 with an OS-assigned port.
pub fn bind_loopback() -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((LOOPBACK, 0)))
}

/// Return the port a listener is bound to, or 0 if the addr is unavailable.
pub fn local_port(listener: &TcpListener) -> u16 {
    listener.local_addr().map(|a| a.port()).unwrap_or(0)
}

/// Path for a named port file inside `run_dir` (e.g. `run_dir/api.port`).
pub fn port_path(run_dir: &Path, name: &str) -> PathBuf {
    run_dir.join(format!("{name}.port"))
}

/// Write `port` to `{run_dir}/{name}.port` atomically (tmp + rename).
pub fn write_port(run_dir: &Path, name: &str, port: u16) -> io::Result<()> {
    let final_path = port_path(run_dir, name);
    let tmp = run_dir.join(format!(".{name}.port.tmp"));
    std::fs::write(&tmp, port.to_string())?;
    std::fs::rename(&tmp, &final_path)
}

/// Read a port from `{run_dir}/{name}.port`. Returns None if missing/malformed.
pub fn read_port(run_dir: &Path, name: &str) -> Option<u16> {
    std::fs::read_to_string(port_path(run_dir, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Best-effort removal of a port file.
pub fn remove_port(run_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(port_path(run_dir, name));
}

/// Connect to `127.0.0.1:port` with `TCP_NODELAY` enabled.
pub fn connect_port(port: u16) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(SocketAddr::from((LOOPBACK, port)))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Connect by looking up `{run_dir}/{name}.port` and dialing it.
pub fn connect_named(run_dir: &Path, name: &str) -> io::Result<TcpStream> {
    let port = read_port(run_dir, name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{name}.port missing or invalid"),
        )
    })?;
    connect_port(port)
}

/// Probe whether a named port file's listener is reachable (for `doctor`).
pub fn probe_named(run_dir: &Path, name: &str, timeout: Duration) -> bool {
    match read_port(run_dir, name) {
        Some(port) => {
            TcpStream::connect_timeout(&SocketAddr::from((LOOPBACK, port)), timeout).is_ok()
        }
        None => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-ipc-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bind_loopback_assigns_port() {
        let listener = bind_loopback().unwrap();
        let port = local_port(&listener);
        assert!(port > 0);
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn write_and_read_port_roundtrip() {
        let dir = tmp_dir("roundtrip");
        write_port(&dir, "api", 12345).unwrap();
        assert_eq!(read_port(&dir, "api"), Some(12345));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_port_missing_returns_none() {
        let dir = tmp_dir("missing");
        assert_eq!(read_port(&dir, "nope"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_port_malformed_returns_none() {
        let dir = tmp_dir("malformed");
        std::fs::write(dir.join("x.port"), "not-a-port").unwrap();
        assert_eq!(read_port(&dir, "x"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_port_is_best_effort() {
        let dir = tmp_dir("remove");
        remove_port(&dir, "absent"); // must not panic
        write_port(&dir, "a", 1).unwrap();
        remove_port(&dir, "a");
        assert_eq!(read_port(&dir, "a"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn connect_named_missing_returns_notfound() {
        let dir = tmp_dir("connect-missing");
        let err = connect_named(&dir, "absent").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_named_connects_then_fails_when_missing() {
        let dir = tmp_dir("probe");
        let listener = bind_loopback().unwrap();
        let port = local_port(&listener);
        write_port(&dir, "dev", port).unwrap();

        // Accept once in the background so connect_timeout succeeds cleanly.
        let handle = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        assert!(probe_named(&dir, "dev", Duration::from_millis(200)));
        handle.join().ok();

        remove_port(&dir, "dev");
        assert!(!probe_named(&dir, "dev", Duration::from_millis(200)));

        std::fs::remove_dir_all(&dir).ok();
    }
}
