//! Demo mode — spawns mock agents to showcase core features without API keys.

use std::path::PathBuf;

fn demo_fleet_yaml(tmp: &std::path::Path) -> String {
    let echo_cmd =
        r#"bash -c "echo 'Type your question'; while read line; do echo \"Echo: $line\"; done""#;
    format!(
        "defaults:\n  backend: bash\n  worktree: false\n\ninstances:\n  alice:\n    command: \"{echo_cmd}\"\n    working_directory: {tmp}\n  bob:\n    command: \"{echo_cmd}\"\n    working_directory: {tmp}\n",
        tmp = tmp.display()
    )
}

pub fn run() {
    println!("AgEnD-PTY Demo\n");
    println!("Launching 2 echo agents (no API key needed)...\n");

    let tmp = std::env::temp_dir().join("agend-demo");
    std::fs::create_dir_all(&tmp).ok();
    let fleet_path = tmp.join("fleet.yaml");
    std::fs::write(&fleet_path, demo_fleet_yaml(&tmp)).ok();

    println!("1. Starting daemon with demo fleet...");
    println!("   fleet.yaml: {}\n", fleet_path.display());

    // Exec the daemon binary with --config pointing to our demo fleet
    let daemon = exe_dir().join("agend-daemon");
    let status = std::process::Command::new(&daemon)
        .args(["--config", &fleet_path.display().to_string()])
        .status();

    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("Failed to start daemon: {e}");
            eprintln!("Build first: cargo build");
            std::process::exit(1);
        }
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|par| par.to_path_buf()))
        .unwrap_or_default()
}
