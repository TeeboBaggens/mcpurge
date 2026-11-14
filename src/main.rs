//! Entry point. Launches the GUI by default; `--list` and `--json` expose the
//! same scan headlessly, which makes the engine scriptable and testable
//! without a display.

use anyhow::Result;
use mcpurge::model::{human_bytes, human_duration};
use mcpurge::registry::{Registry, RegistrySource};
use mcpurge::scan::{self, ScanOptions};

const HELP: &str = "\
mcpurge - find, inspect, stop and purge MCP servers across every AI agent

USAGE:
    mcpurge [OPTIONS]

With no options, the GUI opens.

OPTIONS:
    -l, --list        Scan and print what was found, then exit
    -j, --json        Scan and print the results as JSON, then exit
        --no-sweep    Probe only known config paths; skip the content sweep
        --quick       Skip measuring artifact sizes on disk
    -v, --version     Print the version
    -h, --help        Print this help

mcpurge never changes anything without being asked. --list and --json are
read-only.
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flags: [&str; 2]| args.iter().any(|a| a == flags[0] || a == flags[1]);

    if has(["-h", "--help"]) {
        print!("{HELP}");
        return Ok(());
    }
    if has(["-v", "--version"]) {
        println!("mcpurge {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let opts = ScanOptions {
        sweep: !args.iter().any(|a| a == "--no-sweep"),
        extra_project_roots: Vec::new(),
        measure_artifacts: !args.iter().any(|a| a == "--quick"),
    };

    if has(["-l", "--list"]) {
        return list(&opts, false);
    }
    if has(["-j", "--json"]) {
        return list(&opts, true);
    }

    mcpurge::app::run()
}

fn list(opts: &ScanOptions, as_json: bool) -> Result<()> {
    let registry = Registry::load()?;
    let result = scan::scan(&registry, opts, |msg| {
        if !as_json {
            eprintln!("  .. {msg}");
        }
    });

    if as_json {
        println!("{}", render_json(&result));
        return Ok(());
    }

    let totals = scan::totals(&result);
    match &registry.source {
        RegistrySource::Builtin => {
            println!("\nregistry: built in ({} clients)", registry.clients.len())
        }
        RegistrySource::User(p) => println!(
            "\nregistry: {} ({} clients)",
            p.display(),
            registry.clients.len()
        ),
    }
    println!(
        "scanned {} config file(s) in {} ms\n",
        result.scanned_paths.len(),
        result.duration_ms
    );
    println!(
        "{} server(s) across {} client(s); {} running; {} reclaimable",
        totals.servers,
        totals.clients,
        totals.running,
        human_bytes(totals.bytes)
    );

    if !result.findings.is_empty() {
        println!(
            "\n{:<22} {:<22} {:<14} {:<9} STATE",
            "SERVER", "CLIENT", "SCOPE", "TRANSPORT"
        );
        println!("{}", "-".repeat(92));
        for f in &result.findings {
            let state = if f.procs.is_empty() {
                if f.entry.disabled {
                    "disabled".to_string()
                } else {
                    "stopped".to_string()
                }
            } else {
                let pids: Vec<String> = f.procs.iter().map(|p| p.pid.to_string()).collect();
                format!("RUNNING pid {}", pids.join(","))
            };
            println!(
                "{:<22} {:<22} {:<14} {:<9} {}",
                truncate(&f.entry.name, 21),
                truncate(&f.entry.client_name, 21),
                truncate(&f.entry.scope.to_string(), 13),
                f.entry.transport.label(),
                state
            );
        }
    }

    if !result.orphans.is_empty() {
        println!("\norphan processes (running, but matching no config):");
        for o in &result.orphans {
            println!(
                "  pid {:<7} {:<24} {}  [{}]",
                o.proc.pid,
                truncate(&o.proc.name, 23),
                human_duration(o.proc.run_time),
                o.likely_host.as_deref().unwrap_or("unknown host")
            );
            println!("      {}", truncate(&o.proc.cmdline, 100));
        }
    }

    if !result.unknown_configs.is_empty() {
        println!("\nMCP-shaped configs that match no known client:");
        for u in &result.unknown_configs {
            println!(
                "  {} ({} server(s), key(s): {})",
                u.path.display(),
                u.server_count,
                u.container_keys.join(", ")
            );
        }
    }

    if !result.runtimes.is_empty() {
        println!("\nlocal model runtimes detected (not MCP clients):");
        for note in scan::runtime_notes(&result.runtimes) {
            println!("  {note}");
        }
    }

    if !result.errors.is_empty() {
        println!("\nfiles that could not be read:");
        for e in result.errors.iter().take(10) {
            println!("  {e}");
        }
    }
    println!();
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}\u{2026}")
    }
}

/// Hand-rolled so the JSON shape is an explicit interface rather than an
/// accident of the internal types.
fn render_json(result: &mcpurge::model::ScanResult) -> String {
    let servers: Vec<serde_json::Value> = result
        .findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.entry.name,
                "client_id": f.entry.client_id,
                "client": f.entry.client_name,
                "scope": f.entry.scope.to_string(),
                "transport": f.entry.transport.label(),
                "command": f.entry.transport.command_line(),
                "disabled": f.entry.disabled,
                "config_path": f.entry.source.path,
                "config_pointer": f.entry.source.container_pointer,
                "env_keys": f.entry.env.keys().collect::<Vec<_>>(),
                "running": f.is_running(),
                "pids": f.procs.iter().map(|p| p.pid).collect::<Vec<_>>(),
                "artifacts": f.artifacts.iter().map(|a| serde_json::json!({
                    "path": a.path,
                    "bytes": a.size_bytes,
                    "kind": a.kind.label(),
                    "shared": a.shared,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let orphans: Vec<serde_json::Value> = result
        .orphans
        .iter()
        .map(|o| {
            serde_json::json!({
                "pid": o.proc.pid,
                "name": o.proc.name,
                "cmdline": o.proc.cmdline,
                "run_time_secs": o.proc.run_time,
                "likely_host": o.likely_host,
            })
        })
        .collect();

    serde_json::to_string_pretty(&serde_json::json!({
        "duration_ms": result.duration_ms,
        "scanned_paths": result.scanned_paths,
        "servers": servers,
        "orphans": orphans,
        "unknown_configs": result.unknown_configs.iter().map(|u| serde_json::json!({
            "path": u.path,
            "container_keys": u.container_keys,
            "server_count": u.server_count,
        })).collect::<Vec<_>>(),
        "runtimes": result.runtimes.iter().map(|r| serde_json::json!({
            "name": r.name, "pids": r.pids, "note": r.detail,
        })).collect::<Vec<_>>(),
        "errors": result.errors,
    }))
    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}
