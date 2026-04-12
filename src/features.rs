//! Features — dry-run, snapshot/restore, dependency graph.

use crate::{config, paths};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ── B1: Dry-run (config validation only) ────────────────────────────────

pub fn dry_run(cfg: &config::FleetConfig) {
    let mut errors = 0u32;
    println!("=== Dry-run: {} instance(s) ===\n", cfg.instances.len());
    for (name, ic) in &cfg.instances {
        let cmd = ic.build_command(&cfg.defaults);
        let wd = ic.effective_working_dir(&cfg.defaults, name);
        let bin = cmd.split_whitespace().next().unwrap_or(&cmd);
        let bin_ok = which(bin);
        let wd_ok = wd.exists();
        if !bin_ok {
            errors += 1;
        }
        let deps = if ic.depends_on.is_empty() {
            "(none)".into()
        } else {
            ic.depends_on.join(", ")
        };
        println!(
            "  {name}: {} {} | wd: {} {} | deps: {deps}",
            ic.backend_or(&cfg.defaults),
            if bin_ok { "✅" } else { "❌" },
            wd.display(),
            if wd_ok { "✅" } else { "⚠️" }
        );
        for dep in &ic.depends_on {
            if !cfg.instances.contains_key(dep) {
                errors += 1;
                println!("    ❌ dep '{dep}' not found");
            }
        }
        // dry_run: validate only, no side effects (no dir creation, no file generation)
    }
    match resolve_order(cfg) {
        Ok(order) => println!("\nOrder: {}", order.join(" → ")),
        Err(e) => {
            errors += 1;
            println!("\n❌ {e}");
        }
    }
    if let Some(ch) = &cfg.channel {
        let env = ch.bot_token_env.as_deref().unwrap_or("TELEGRAM_BOT_TOKEN");
        println!(
            "Channel: telegram ({})",
            if std::env::var(env).is_ok() {
                "✅"
            } else {
                "⚠️ no token"
            }
        );
    }
    if errors > 0 {
        println!("\n❌ {errors} error(s)");
        std::process::exit(1);
    } else {
        println!("\n✅ Dry-run passed.");
    }
}

fn which(name: &str) -> bool {
    crate::paths::which(name).is_some()
}

// ── B2: Snapshot/Restore ────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct FleetSnapshot {
    pub timestamp: u64,
    pub fleet_yaml: String,
    pub agents: Vec<AgentSnapshot>,
    #[serde(default)]
    pub topic_mappings: HashMap<String, i64>,
}

#[derive(Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub name: String,
    pub command: String,
    pub working_dir: String,
}

pub fn snapshot(config_path: Option<&Path>, output: &Path) -> Result<(), String> {
    let cfg_path = config_path
        .map(|p| p.to_path_buf())
        .or_else(|| {
            ["fleet.yaml", "fleet.yml"]
                .iter()
                .map(std::path::PathBuf::from)
                .chain(std::iter::once(crate::paths::home().join("fleet.yaml")))
                .find(|p| p.exists())
        })
        .ok_or("fleet.yaml not found (checked ./fleet.yaml, ~/.agend/fleet.yaml)")?;
    let fleet_yaml = std::fs::read_to_string(&cfg_path).map_err(|e| format!("read: {e}"))?;
    let cfg: config::FleetConfig =
        serde_yml::from_str(&fleet_yaml).map_err(|e| format!("parse: {e}"))?;
    let agents = cfg
        .instances
        .iter()
        .map(|(name, ic)| AgentSnapshot {
            name: name.clone(),
            command: ic.build_command(&cfg.defaults),
            working_dir: ic
                .effective_working_dir(&cfg.defaults, name)
                .display()
                .to_string(),
        })
        .collect();
    // Load topic mappings if they exist
    let topic_mappings: HashMap<String, i64> =
        std::fs::read_to_string(paths::home().join("topics.json"))
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default();
    let snap = FleetSnapshot {
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        fleet_yaml,
        agents,
        topic_mappings,
    };
    std::fs::write(
        output,
        serde_json::to_string_pretty(&snap).unwrap_or_default(),
    )
    .map_err(|e| format!("write: {e}"))?;
    println!(
        "Snapshot saved to {} ({} agents, {} topics)",
        output.display(),
        snap.agents.len(),
        snap.topic_mappings.len()
    );
    Ok(())
}

pub fn restore(input: &Path) -> Result<(), String> {
    let snap: FleetSnapshot =
        serde_json::from_str(&std::fs::read_to_string(input).map_err(|e| format!("read: {e}"))?)
            .map_err(|e| format!("parse: {e}"))?;
    let fleet_path = std::path::PathBuf::from("fleet.yaml");
    if !fleet_path.exists() {
        std::fs::write(&fleet_path, &snap.fleet_yaml).map_err(|e| format!("write: {e}"))?;
        println!("Restored fleet.yaml ({} agents)", snap.agents.len());
    } else {
        println!("fleet.yaml exists, skipping");
    }
    if !snap.topic_mappings.is_empty() {
        std::fs::create_dir_all(paths::home()).ok();
        std::fs::write(
            paths::home().join("topics.json"),
            serde_json::to_string_pretty(&snap.topic_mappings).unwrap_or_default(),
        )
        .map_err(|e| format!("write topics: {e}"))?;
        println!("Restored {} topic mappings", snap.topic_mappings.len());
    }
    for a in &snap.agents {
        if !Path::new(&a.working_dir).exists() {
            std::fs::create_dir_all(&a.working_dir).ok();
        }
        println!("  {} ({})", a.name, a.command);
    }
    println!("Run `agend-pty daemon` to start (fresh health state).");
    Ok(())
}

// ── B3: Dependency Graph ────────────────────────────────────────────────

pub fn resolve_order(cfg: &config::FleetConfig) -> Result<Vec<String>, String> {
    let names: HashSet<&str> = cfg.instances.keys().map(|s| s.as_str()).collect();
    for (name, ic) in &cfg.instances {
        for dep in &ic.depends_on {
            if !names.contains(dep.as_str()) {
                return Err(format!("'{name}' depends on '{dep}' which doesn't exist"));
            }
        }
    }
    let mut in_deg: HashMap<&str, usize> = names.iter().map(|&n| (n, 0)).collect();
    let mut fwd: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, ic) in &cfg.instances {
        if let Some(d) = in_deg.get_mut(name.as_str()) {
            *d = ic.depends_on.len();
        }
        for dep in &ic.depends_on {
            fwd.entry(dep.as_str()).or_default().push(name.as_str());
        }
    }
    let mut queue: Vec<&str> = in_deg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&n, _)| n)
        .collect();
    queue.sort();
    let mut order = Vec::new();
    while let Some(node) = queue.pop() {
        order.push(node.to_owned());
        for &dep in fwd.get(node).unwrap_or(&vec![]) {
            if let Some(d) = in_deg.get_mut(dep) {
                *d -= 1;
                if *d == 0 {
                    queue.push(dep);
                    queue.sort();
                }
            }
        }
    }
    if order.len() != names.len() {
        let stuck: Vec<_> = names
            .iter()
            .filter(|n| !order.contains(&n.to_string()))
            .collect();
        return Err(format!("circular dependency: {:?}", stuck));
    }
    Ok(order)
}

pub fn dependency_layers(cfg: &config::FleetConfig) -> Result<Vec<Vec<String>>, String> {
    let order = resolve_order(cfg)?;
    let mut layers: Vec<Vec<String>> = Vec::new();
    for name in &order {
        let ic = &cfg.instances[name];
        let layer = if ic.depends_on.is_empty() {
            0
        } else {
            ic.depends_on
                .iter()
                .filter_map(|dep| layers.iter().position(|l| l.contains(dep)))
                .max()
                .unwrap_or(0)
                + 1
        };
        while layers.len() <= layer {
            layers.push(Vec::new());
        }
        layers[layer].push(name.clone());
    }
    Ok(layers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_cfg(yaml: &str) -> config::FleetConfig {
        serde_yml::from_str(yaml).unwrap()
    }

    #[test]
    fn no_dependencies() {
        let cfg = parse_cfg("instances:\n  a: {}\n  b: {}\n");
        assert_eq!(resolve_order(&cfg).unwrap().len(), 2);
    }

    #[test]
    fn linear_dependency() {
        let cfg = parse_cfg(
            "instances:\n  a: {}\n  b:\n    depends_on: [a]\n  c:\n    depends_on: [b]\n",
        );
        assert_eq!(resolve_order(&cfg).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn diamond_dependency() {
        let cfg = parse_cfg("instances:\n  a: {}\n  b:\n    depends_on: [a]\n  c:\n    depends_on: [a]\n  d:\n    depends_on: [b, c]\n");
        let order = resolve_order(&cfg).unwrap();
        assert_eq!(order[0], "a");
        assert_eq!(*order.last().unwrap(), "d");
    }

    #[test]
    fn circular_detected() {
        let cfg = parse_cfg("instances:\n  a:\n    depends_on: [b]\n  b:\n    depends_on: [a]\n");
        assert!(resolve_order(&cfg).unwrap_err().contains("circular"));
    }

    #[test]
    fn missing_dep_detected() {
        let cfg = parse_cfg("instances:\n  a:\n    depends_on: [x]\n");
        assert!(resolve_order(&cfg).unwrap_err().contains("doesn't exist"));
    }

    #[test]
    fn layers_parallel() {
        let cfg = parse_cfg("instances:\n  coord: {}\n  w1:\n    depends_on: [coord]\n  w2:\n    depends_on: [coord]\n");
        let layers = dependency_layers(&cfg).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0], vec!["coord"]);
        assert!(layers[1].contains(&"w1".to_string()) && layers[1].contains(&"w2".to_string()));
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut topics = HashMap::new();
        topics.insert("alice".into(), 12345i64);
        let snap = FleetSnapshot {
            timestamp: 12345,
            fleet_yaml: "instances:\n  test: {}\n".into(),
            agents: vec![AgentSnapshot {
                name: "test".into(),
                command: "claude".into(),
                working_dir: "/tmp".into(),
            }],
            topic_mappings: topics,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let restored: FleetSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.agents[0].name, "test");
        assert_eq!(restored.topic_mappings["alice"], 12345);
    }

    #[test]
    fn which_finds_common_binaries() {
        assert!(which("ls"));
        assert!(!which("nonexistent_binary_xyz"));
    }
}
