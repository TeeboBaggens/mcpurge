//! Finding, attributing and stopping MCP server processes.
//!
//! Matching a config entry to a live process is guesswork, so it is scored
//! rather than assumed, and the UI shows the reason. The shape that matters in
//! practice is the wrapper chain: a config saying
//! `npx -y rea-agents@6.7.0 mcp` shows up as `npm exec rea-agents@6.7.0 mcp`
//! with a `node .../rea mcp` child doing the actual work. Matching only the
//! parent and killing only its PID leaves the real server running, so every
//! kill walks the subtree.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, Signal, System};

use crate::model::{MatchConfidence, OrphanProc, RunningProc, ServerEntry, Transport};
use crate::registry::Registry;

/// A snapshot of the process table, plus the parent/child index we need.
pub struct ProcSnapshot {
    sys: System,
    children: HashMap<Pid, Vec<Pid>>,
    /// Lower-cased full command line per pid, built once.
    cmdlines: HashMap<Pid, String>,
}

impl ProcSnapshot {
    /// Read the process table. Fails only if the platform refuses to give it
    /// to us, which the caller reports rather than treating as "nothing runs".
    pub fn capture() -> Result<Self> {
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
        )
        .context("reading the process table")?;
        sys.refresh_processes(ProcessesToUpdate::All, true);
        Ok(Self::index(sys))
    }

    fn index(sys: System) -> Self {
        let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
        let mut cmdlines: HashMap<Pid, String> = HashMap::new();
        for (pid, proc) in sys.processes() {
            if let Some(parent) = proc.parent() {
                children.entry(parent).or_default().push(*pid);
            }
            cmdlines.insert(*pid, full_cmdline(proc).to_ascii_lowercase());
        }
        Self {
            sys,
            children,
            cmdlines,
        }
    }

    pub fn refresh(&mut self) {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
        let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
        let mut cmdlines: HashMap<Pid, String> = HashMap::new();
        for (pid, proc) in self.sys.processes() {
            if let Some(parent) = proc.parent() {
                children.entry(parent).or_default().push(*pid);
            }
            cmdlines.insert(*pid, full_cmdline(proc).to_ascii_lowercase());
        }
        self.children = children;
        self.cmdlines = cmdlines;
    }

    pub fn exists(&self, pid: u32) -> bool {
        self.sys.process(Pid::from_u32(pid)).is_some()
    }

    /// Every descendant of `pid`, parents before children.
    pub fn descendants(&self, pid: Pid) -> Vec<Pid> {
        let mut out = Vec::new();
        let mut queue = vec![pid];
        // Depth guard: a cycle in the table would otherwise hang the sweep.
        let mut guard = 0;
        while let Some(current) = queue.pop() {
            guard += 1;
            if guard > 10_000 {
                break;
            }
            if let Some(kids) = self.children.get(&current) {
                for kid in kids {
                    if !out.contains(kid) {
                        out.push(*kid);
                        queue.push(*kid);
                    }
                }
            }
        }
        out
    }

    fn to_running(
        &self,
        pid: Pid,
        reason: String,
        confidence: MatchConfidence,
    ) -> Option<RunningProc> {
        let proc = self.sys.process(pid)?;
        Some(RunningProc {
            pid: pid.as_u32(),
            parent: proc.parent().map(|p| p.as_u32()),
            name: proc.name().to_string_lossy().into_owned(),
            cmdline: full_cmdline(proc),
            exe: proc.exe().map(Path::to_path_buf),
            run_time: proc.run_time(),
            memory_bytes: proc.memory(),
            descendants: self
                .descendants(pid)
                .into_iter()
                .map(|p| p.as_u32())
                .collect(),
            match_reason: reason,
            confidence,
        })
    }

    /// Our own pid plus every ancestor. The shell and terminal that launched
    /// mcpurge are not MCP servers, and a build command line mentioning this
    /// project would otherwise look like one.
    fn self_and_ancestors(&self) -> Vec<Pid> {
        let mut out = vec![Pid::from_u32(std::process::id())];
        let mut current = self
            .sys
            .process(Pid::from_u32(std::process::id()))
            .and_then(Process::parent);
        for _ in 0..32 {
            let Some(pid) = current else { break };
            out.push(pid);
            current = self.sys.process(pid).and_then(Process::parent);
        }
        out
    }

    /// Walk up the tree looking for a process that matches a client's hints,
    /// so an orphan can be reported as "spawned by Cursor" rather than bare.
    fn likely_host(&self, pid: Pid, reg: &Registry) -> Option<String> {
        let mut current = self.sys.process(pid)?.parent();
        for _ in 0..12 {
            let parent_pid = current?;
            let proc = self.sys.process(parent_pid)?;
            let name = proc.name().to_string_lossy().to_ascii_lowercase();
            for client in &reg.clients {
                for hint in &client.process_hints {
                    if name.contains(&hint.to_ascii_lowercase()) {
                        return Some(client.name.clone());
                    }
                }
            }
            current = proc.parent();
        }
        None
    }
}

fn full_cmdline(proc: &Process) -> String {
    let parts: Vec<String> = proc
        .cmd()
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        proc.name().to_string_lossy().into_owned()
    } else {
        parts.join(" ")
    }
}

fn basename(cmd: &str) -> String {
    Path::new(cmd)
        .file_name()
        .map(OsStr::to_string_lossy)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| cmd.to_ascii_lowercase())
}

/// Launchers that tell us nothing on their own: matching `npx` alone would tie
/// every npx-based server to every other one, so a distinctive argument is
/// required before such a match counts.
const GENERIC_LAUNCHERS: &[&str] = &[
    "npx",
    "npm",
    "node",
    "bun",
    "bunx",
    "deno",
    "uv",
    "uvx",
    "pipx",
    "python",
    "python3",
    "sh",
    "bash",
    "zsh",
    "cmd",
    "cmd.exe",
    "powershell",
    "pwsh",
    "docker",
    "pnpm",
    "yarn",
    "cargo",
    "go",
];

/// Arguments distinctive enough to identify a specific server: package specs,
/// module paths, script paths. Flags and bare numbers are not.
fn distinctive_args(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| {
            let a = a.trim();
            if a.len() < 4 || a.starts_with('-') {
                return false;
            }
            // Dates, ports and other bare values are not identifying.
            if a.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            // A package spec, a scoped package, a path, or a dotted module.
            a.contains('@')
                || a.contains('/')
                || a.contains('\\')
                || a.contains('-')
                || a.contains('_')
                || a.contains('.')
        })
        .map(|a| a.to_ascii_lowercase())
        .collect()
}

/// Score how well a process matches a stdio server definition.
fn score(cmdline: &str, command: &str, args: &[String]) -> Option<(MatchConfidence, String)> {
    let base = basename(command);
    let generic = GENERIC_LAUNCHERS.contains(&base.as_str());
    let command_hit = !base.is_empty() && cmdline.contains(&base);
    // An absolute command path is itself distinctive.
    let full_path_hit = command.contains('/') && cmdline.contains(&command.to_ascii_lowercase());

    let distinct = distinctive_args(args);
    let arg_hits: Vec<&String> = distinct.iter().filter(|a| cmdline.contains(*a)).collect();

    // An absolute path to a non-generic binary identifies one specific server
    // on its own: no two servers share it. `/opt/homebrew/bin/npx` is excluded
    // by the `generic` test, since every npx server would otherwise match.
    if full_path_hit && !generic {
        return Some((
            MatchConfidence::Strong,
            format!("exact command path {command}"),
        ));
    }
    if (full_path_hit || command_hit) && !arg_hits.is_empty() {
        return Some((
            MatchConfidence::Strong,
            format!("command {base} and argument {}", arg_hits[0]),
        ));
    }
    // The wrapper case: `npx -y pkg mcp` is re-exec'd as `npm exec pkg mcp`,
    // so the launcher basename is gone from the command line but every
    // identifying argument survives. That is still conclusive.
    if !distinct.is_empty() && arg_hits.len() == distinct.len() {
        return Some((
            MatchConfidence::Strong,
            format!("all identifying arguments present ({})", arg_hits[0]),
        ));
    }
    if command_hit && !generic {
        return Some((MatchConfidence::Probable, format!("command {base}")));
    }
    if !arg_hits.is_empty() && arg_hits.len() * 2 >= distinct.len() {
        return Some((
            MatchConfidence::Probable,
            format!("argument {} present", arg_hits[0]),
        ));
    }
    None
}

/// Tokens that mark a process as MCP-related when we have no config to match.
fn looks_mcp_ish(cmdline: &str) -> bool {
    if cmdline.contains("modelcontextprotocol")
        || cmdline.contains("mcp-server")
        || cmdline.contains("mcp_server")
        || cmdline.contains("server-mcp")
    {
        return true;
    }
    // A bare `mcp` subcommand or flag, rather than the substring appearing
    // inside an unrelated word or a long prompt.
    cmdline.split_whitespace().any(|tok| {
        let t = tok.trim_matches(|c: char| !c.is_alphanumeric());
        t == "mcp" || t == "--mcp" || t.ends_with("/mcp") || t.starts_with("mcp-")
    })
}

/// Processes belonging to the host apps themselves. A client is not a server:
/// killing Cursor because its command line mentions MCP would be a disaster.
fn is_host_process(cmdline: &str, name: &str, reg: &Registry) -> bool {
    let name = name.to_ascii_lowercase();
    for client in &reg.clients {
        for hint in &client.process_hints {
            let hint = hint.to_ascii_lowercase();
            if hint.len() < 3 {
                continue;
            }
            if name == hint || name.starts_with(&format!("{hint} ")) {
                // The host binary itself, unless it was launched *as* a server
                // via an `mcp` subcommand (several CLIs can do both).
                let serves = cmdline
                    .split_whitespace()
                    .any(|t| t == "mcp" || t == "serve-mcp" || t == "mcp-server");
                return !serves;
            }
        }
    }
    false
}

/// Result of attributing processes to config entries.
pub struct ProcessMatch {
    /// Processes per entry key.
    pub by_entry: HashMap<String, Vec<RunningProc>>,
    pub orphans: Vec<OrphanProc>,
    pub runtimes: Vec<crate::model::DetectedRuntime>,
}

/// Attribute running processes to the given entries, and report what is left.
pub fn attribute(snapshot: &ProcSnapshot, entries: &[ServerEntry], reg: &Registry) -> ProcessMatch {
    let mut by_entry: HashMap<String, Vec<RunningProc>> = HashMap::new();
    let mut claimed: Vec<Pid> = Vec::new();
    let self_pid = std::process::id();

    // De-duplicate entries that describe the same command: the same server
    // registered in several clients must not each claim a different copy.
    for entry in entries {
        let Transport::Stdio { command, args } = &entry.transport else {
            continue;
        };
        let mut hits: Vec<RunningProc> = Vec::new();
        for pid in snapshot.sys.processes().keys() {
            if pid.as_u32() == self_pid {
                continue;
            }
            let Some(cmdline) = snapshot.cmdlines.get(pid) else {
                continue;
            };
            if let Some((confidence, reason)) = score(cmdline, command, args) {
                hits.push(match snapshot.to_running(*pid, reason, confidence) {
                    Some(r) => r,
                    None => continue,
                });
            }
        }

        // Drop any hit that is a descendant of another hit: the ancestor owns
        // the subtree, and the UI should show one row per server, not one per
        // wrapper process.
        let hit_pids: Vec<u32> = hits.iter().map(|h| h.pid).collect();
        hits.retain(|h| {
            !hit_pids.iter().any(|other| {
                *other != h.pid
                    && snapshot
                        .descendants(Pid::from_u32(*other))
                        .iter()
                        .any(|d| d.as_u32() == h.pid)
            })
        });
        hits.sort_by(|a, b| b.confidence.cmp(&a.confidence).then(a.pid.cmp(&b.pid)));

        for h in &hits {
            claimed.push(Pid::from_u32(h.pid));
            for d in &h.descendants {
                claimed.push(Pid::from_u32(*d));
            }
        }
        if !hits.is_empty() {
            by_entry.insert(entry.key(), hits);
        }
    }

    // Anything MCP-shaped and unclaimed is an orphan: a server still running
    // after its config was edited away, or one from a client we do not know.
    let mut orphans = Vec::new();
    let mut orphan_claimed: Vec<Pid> = Vec::new();
    let own_tree = snapshot.self_and_ancestors();
    for (pid, proc) in snapshot.sys.processes() {
        if pid.as_u32() == self_pid
            || own_tree.contains(pid)
            || claimed.contains(pid)
            || orphan_claimed.contains(pid)
        {
            continue;
        }
        let Some(cmdline) = snapshot.cmdlines.get(pid) else {
            continue;
        };
        if !looks_mcp_ish(cmdline) {
            continue;
        }
        let name = proc.name().to_string_lossy();
        if is_host_process(cmdline, &name, reg) {
            continue;
        }
        // Do not report both a wrapper and its child as separate orphans.
        let descendants = snapshot.descendants(*pid);
        if descendants
            .iter()
            .any(|d| orphan_claimed.contains(d) || claimed.contains(d))
        {
            continue;
        }
        if let Some(running) = snapshot.to_running(
            *pid,
            "matches no known config entry".to_string(),
            MatchConfidence::Loose,
        ) {
            orphan_claimed.push(*pid);
            orphan_claimed.extend(descendants);
            orphans.push(OrphanProc {
                likely_host: snapshot.likely_host(*pid, reg),
                proc: running,
            });
        }
    }
    orphans.sort_by_key(|o| o.proc.pid);

    // Local inference runtimes: not MCP, but worth saying we looked.
    let mut runtimes = Vec::new();
    for rt in &reg.runtimes {
        let mut pids = Vec::new();
        for (pid, proc) in snapshot.sys.processes() {
            let name = proc.name().to_string_lossy().to_ascii_lowercase();
            if rt
                .process_hints
                .iter()
                .any(|h| name == h.to_ascii_lowercase())
            {
                pids.push(pid.as_u32());
            }
        }
        if !pids.is_empty() {
            pids.sort_unstable();
            runtimes.push(crate::model::DetectedRuntime {
                name: rt.name.clone(),
                detail: rt.notes.clone(),
                pids,
            });
        }
    }

    ProcessMatch {
        by_entry,
        orphans,
        runtimes,
    }
}

/// What happened to one process during a kill.
#[derive(Debug, Clone)]
pub struct KillOutcome {
    pub pid: u32,
    pub name: String,
    pub signalled: bool,
    pub exited: bool,
    pub escalated: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct KillReport {
    pub outcomes: Vec<KillOutcome>,
}

impl KillReport {
    pub fn all_exited(&self) -> bool {
        self.outcomes.iter().all(|o| o.exited)
    }

    pub fn summary(&self) -> String {
        let total = self.outcomes.len();
        let gone = self.outcomes.iter().filter(|o| o.exited).count();
        let escalated = self.outcomes.iter().filter(|o| o.escalated).count();
        let mut s = format!("{gone}/{total} process(es) exited");
        if escalated > 0 {
            s.push_str(&format!(", {escalated} needed SIGKILL"));
        }
        s
    }
}

/// How long to wait for a graceful exit before escalating.
const TERM_GRACE: Duration = Duration::from_millis(1500);

/// Stop a process and everything below it.
///
/// Children are signalled before their parents so a supervisor cannot notice
/// the loss and respawn them, and SIGTERM is tried before SIGKILL so servers
/// get the chance to flush and release resources. Windows has no SIGTERM, so
/// `kill_with` returns `None` there and we go straight to the hard kill.
pub fn kill_tree(pid: u32, allow_sigkill: bool) -> KillReport {
    let mut snapshot = match ProcSnapshot::capture() {
        Ok(s) => s,
        Err(e) => {
            return KillReport {
                outcomes: vec![KillOutcome {
                    pid,
                    name: String::new(),
                    signalled: false,
                    exited: false,
                    escalated: false,
                    detail: format!("could not read the process table: {e}"),
                }],
            };
        }
    };
    let root = Pid::from_u32(pid);

    let mut targets = snapshot.descendants(root);
    // Deepest first: reverse the breadth-first order.
    targets.reverse();
    targets.push(root);

    let mut report = KillReport::default();

    // Pass one: polite.
    for target in &targets {
        let Some(proc) = snapshot.sys.process(*target) else {
            continue;
        };
        let name = proc.name().to_string_lossy().into_owned();
        let signalled = match proc.kill_with(Signal::Term) {
            Some(ok) => ok,
            // Signal unsupported on this platform (Windows): hard kill now.
            None => proc.kill(),
        };
        report.outcomes.push(KillOutcome {
            pid: target.as_u32(),
            name,
            signalled,
            exited: false,
            escalated: false,
            detail: if signalled {
                "SIGTERM sent".into()
            } else {
                "signal failed".into()
            },
        });
    }

    // Give them a moment, then see who is left.
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        snapshot.refresh();
        if targets.iter().all(|t| !snapshot.exists(t.as_u32())) {
            break;
        }
    }
    snapshot.refresh();

    // Pass two: escalate on whatever survived.
    for outcome in &mut report.outcomes {
        if !snapshot.exists(outcome.pid) {
            outcome.exited = true;
            continue;
        }
        if !allow_sigkill {
            outcome.detail = "still running; SIGKILL not permitted".into();
            continue;
        }
        if let Some(proc) = snapshot.sys.process(Pid::from_u32(outcome.pid)) {
            let killed = proc.kill();
            outcome.escalated = true;
            outcome.detail = if killed {
                "SIGTERM ignored, SIGKILL sent".into()
            } else {
                "SIGKILL failed (permission denied?)".into()
            };
        }
    }

    if allow_sigkill {
        let deadline = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(80));
            snapshot.refresh();
            if report
                .outcomes
                .iter()
                .all(|o| o.exited || !snapshot.exists(o.pid))
            {
                break;
            }
        }
        snapshot.refresh();
        for outcome in &mut report.outcomes {
            if !snapshot.exists(outcome.pid) {
                outcome.exited = true;
            }
        }
    }

    report
}

/// Resolve a command to the binary that would run, for the detail pane.
pub fn which(command: &str) -> Option<PathBuf> {
    let p = Path::new(command);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(command);
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_command_and_arg_is_a_strong_match() {
        let (c, _) = score(
            "/applications/solidcraft.app/contents/macos/solidcraft-cli mcp --allow-path /x",
            "/Applications/SolidCraft.app/Contents/MacOS/solidcraft-cli",
            &["mcp".into(), "--allow-path".into(), "/x".into()],
        )
        .expect("should match");
        assert_eq!(c, MatchConfidence::Strong);
    }

    #[test]
    fn npx_wrapper_chain_matches_on_the_package_spec() {
        // Config: npx -y rea-agents@6.7.0 mcp
        // Reality: npm exec rea-agents@6.7.0 mcp
        let (c, reason) = score(
            "npm exec rea-agents@6.7.0 mcp",
            "npx",
            &["-y".into(), "rea-agents@6.7.0".into(), "mcp".into()],
        )
        .expect("should match the wrapper");
        assert_eq!(c, MatchConfidence::Strong);
        assert!(reason.contains("rea-agents@6.7.0"), "got {reason}");
    }

    #[test]
    fn a_generic_launcher_alone_never_matches() {
        // Two different uvx servers must not be confused for each other.
        assert!(
            score("uvx mcp-server-time", "uvx", &[]).is_none(),
            "bare uvx must not match"
        );
        assert!(
            score("uvx mcp-server-time", "uvx", &["mcp-server-fetch".into()]).is_none(),
            "a different package must not match"
        );
    }

    #[test]
    fn distinctive_args_exclude_flags_and_numbers() {
        let d = distinctive_args(&[
            "-y".into(),
            "8001".into(),
            "rea-agents@6.7.0".into(),
            "--port".into(),
            "mcp".into(),
        ]);
        assert_eq!(d, vec!["rea-agents@6.7.0".to_string()]);
    }

    #[test]
    fn mcp_ish_detection_requires_a_real_token() {
        assert!(looks_mcp_ish("node /x/.bin/rea mcp"));
        assert!(looks_mcp_ish("uvx mcp-server-time"));
        assert!(looks_mcp_ish("python -m modelcontextprotocol.server"));
        // A long prompt mentioning the word must not count.
        assert!(!looks_mcp_ish(
            "claude -p --append-system-prompt you can use mcpurge to inspect things"
        ));
        assert!(!looks_mcp_ish("/usr/bin/some-daemon --config /etc/x.conf"));
    }

    #[test]
    fn host_apps_are_not_treated_as_servers() {
        let reg = Registry::builtin().unwrap();
        assert!(is_host_process("cursor --type=renderer", "Cursor", &reg));
        // ...but a CLI invoked as a server is a server.
        assert!(!is_host_process(
            "craft-anything-cli mcp --project /x",
            "craft-anything-cli",
            &reg
        ));
    }

    #[test]
    fn snapshot_sees_this_process_and_its_ancestry() {
        let snap = ProcSnapshot::capture().unwrap();
        assert!(snap.exists(std::process::id()), "own pid must be visible");
    }

    #[test]
    fn descendants_terminate_and_exclude_self() {
        let snap = ProcSnapshot::capture().unwrap();
        let me = Pid::from_u32(std::process::id());
        let kids = snap.descendants(me);
        assert!(!kids.contains(&me), "a process is not its own descendant");
    }

    #[test]
    fn our_own_ancestry_is_never_reported_as_an_orphan() {
        // The shell that launched mcpurge has this project's path on its
        // command line, which previously made it look like a server.
        let snap = ProcSnapshot::capture().unwrap();
        let own = snap.self_and_ancestors();
        assert!(own.contains(&Pid::from_u32(std::process::id())));

        let reg = Registry::builtin().unwrap();
        let matched = attribute(&snap, &[], &reg);
        for o in &matched.orphans {
            assert!(
                !own.contains(&Pid::from_u32(o.proc.pid)),
                "pid {} is our own ancestry but was reported as an orphan: {}",
                o.proc.pid,
                o.proc.cmdline
            );
        }
    }

    #[test]
    fn which_finds_a_binary_on_path() {
        // Every supported platform has a shell or cmd on PATH.
        let found = which(if cfg!(windows) { "cmd.exe" } else { "sh" });
        assert!(found.is_some(), "expected to resolve a standard binary");
    }
}
