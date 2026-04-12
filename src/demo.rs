//! Demo mode — spawns mock agents to showcase core features without API keys.

fn demo_fleet_yaml(tmp: &std::path::Path) -> String {
    let echo_cmd =
        r#"bash -c "echo 'Type your question'; while read line; do echo \"Echo: $line\"; done""#;
    format!(
        "defaults:\n  backend: bash\n  worktree: false\n\ninstances:\n  alice:\n    command: \"{echo_cmd}\"\n    working_directory: {tmp}\n  bob:\n    command: \"{echo_cmd}\"\n    working_directory: {tmp}\n",
        tmp = tmp.display()
    )
}

pub fn run() {
    println!("=== AgEnD-PTY Demo ===");
    println!("Starting 2 echo agents (no API key needed)...");
    println!();
    println!("Once agents are ready, try in another terminal:");
    println!("  agend-pty attach alice    # Connect to agent TUI");
    println!("  agend-pty attach bob      # Connect to another agent");
    println!("  agend-pty status          # Show fleet status");
    println!("  agend-pty inject alice \"hello world\"  # Send message");
    println!();
    println!("Press Ctrl+C to stop the demo.");
    println!("================================================");
    println!();

    let tmp = std::env::temp_dir().join("agend-demo");
    std::fs::create_dir_all(&tmp).ok();
    let fleet_path = tmp.join("fleet.yaml");
    std::fs::write(&fleet_path, demo_fleet_yaml(&tmp)).ok();

    let daemon = crate::paths::exe_sibling("agend-daemon");
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
