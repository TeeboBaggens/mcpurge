//! Scan orchestration: registry probe, content sweep, parse, then correlate
//! with running processes and on-disk artifacts.

pub mod discover;
pub mod parse;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::files;
use crate::model::{
    DetectedRuntime, Finding, OrphanProc, ScanResult, Scope, ServerEntry, UnknownConfig,
};
use crate::proc::{self, ProcSnapshot};
use crate::registry::Registry;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Run the content-based filesystem sweep as well as the registry probe.
    pub sweep: bool,
    /// Project directories to probe, on top of those harvested from configs.
    pub extra_project_roots: Vec<PathBuf>,
    /// Measure artifact sizes. Skipping this makes a rescan noticeably faster.
    pub measure_artifacts: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sweep: true,
            extra_project_roots: Vec::new(),
            measure_artifacts: true,
        }
    }
}

/// A config file to parse, with everything we know about where it came from.
#[derive(Debug, Clone)]
struct ConfigTarget {
    path: PathBuf,
    /// Registry client id, or `None` when nothing claimed it.
    client_id: Option<String>,
    scope: Scope,
}

/// Whether a directory is plausibly a project root rather than a home or
/// filesystem root. Configs record the directories an agent has been used in,
/// and that list routinely includes `$HOME`.
fn is_project_root_candidate(dir: &Path) -> bool {
    if dir.parent().is_none() {
        return false;
    }
    match dirs::home_dir() {
        Some(home) => dir != home,
        None => true,
    }
}

/// Decide the scope of a config path that the sweep turned up.
fn infer_scope(path: &Path, project_roots: &[PathBuf]) -> Scope {
    for root in project_roots {
        if path.starts_with(root) {
            return Scope::Project(root.clone());
        }
    }
    let home = dirs::home_dir();
    match home {
        Some(h) if path.starts_with(&h) => Scope::User,
        _ => Scope::System,
    }
}

/// Run a full scan. `progress` is called with short status lines for the UI.
pub fn scan(reg: &Registry, opts: &ScanOptions, mut progress: impl FnMut(&str)) -> ScanResult {
    let started = Instant::now();
    let mut result = ScanResult::default();

    // ---- 1. Registry-known config files -----------------------------------
    progress("probing known config locations");
    let mut targets: BTreeMap<PathBuf, ConfigTarget> = BTreeMap::new();
    for (path, client_ids) in crate::registry::registry_config_files(reg) {
        targets.insert(
            path.clone(),
            ConfigTarget {
                path,
                // A file read by several clients (.mcp.json) is attributed to
                // the first that claims it; the entries still show the file.
                client_id: client_ids.first().cloned(),
                scope: Scope::User,
            },
        );
    }

    // ---- 2. Project roots -------------------------------------------------
    // Harvested from configs that record the projects they have been used in
    // (~/.claude.json), which beats guessing where the user keeps code.
    progress("collecting project roots");
    let mut project_roots: BTreeSet<PathBuf> = opts.extra_project_roots.iter().cloned().collect();
    for client in &reg.clients {
        if client.harvest_project_keys.is_empty() {
            continue;
        }
        for pattern in client.patterns_for_host() {
            for path in crate::registry::expand_existing(&pattern) {
                if let Ok(root) = parse::read_as_json(&path) {
                    for dir in parse::harvest_project_roots(&root, &client.harvest_project_keys) {
                        // The home directory is not a project. Treating it as
                        // one makes every user-scope config look
                        // project-scoped, because patterns like
                        // `.kiro/agents/*.json` match relative to it.
                        if is_project_root_candidate(&dir) {
                            project_roots.insert(dir);
                        }
                    }
                }
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir()
        && is_project_root_candidate(&cwd)
    {
        project_roots.insert(cwd);
    }

    let mut all_project_patterns: Vec<String> = Vec::new();
    for client in &reg.clients {
        all_project_patterns.extend(client.project_paths.iter().cloned());
    }
    let project_candidates = discover::project_candidates(&all_project_patterns);

    let project_roots: Vec<PathBuf> = project_roots.into_iter().collect();
    progress(&format!("probing {} project root(s)", project_roots.len()));
    for root in &project_roots {
        for path in discover::probe_project(root, &project_candidates) {
            // A file already claimed as user-scope config keeps that scope: a
            // project probe must not relabel it just because the pattern also
            // matches from a project root.
            if targets.contains_key(&path) {
                continue;
            }
            let client_id = reg.attribute_path(&path).map(|c| c.id.clone());
            targets.insert(
                path.clone(),
                ConfigTarget {
                    path,
                    client_id,
                    scope: Scope::Project(root.clone()),
                },
            );
        }
    }

    // ---- 3. Content sweep -------------------------------------------------
    // This is what finds clients the registry has never heard of.
    if opts.sweep {
        let roots = discover::default_roots();
        let swept = discover::sweep(&roots, |m| progress(m));
        progress(&format!("shape-testing {} candidate file(s)", swept.len()));
        for path in swept {
            if targets.contains_key(&path) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let format = crate::model::ConfigFormat::from_path(&path);
            match parse::sniff_mcp_shaped(&text, format, &path) {
                Ok(keys) if !keys.is_empty() => {
                    let client = reg.attribute_path(&path);
                    if client.is_none() {
                        // Coverage gap: worth telling the user about.
                        let count = parse::read_as_json(&path)
                            .map(|v| {
                                parse::find_containers(&v, &reg.all_container_keys())
                                    .iter()
                                    .map(|c| c.servers.len())
                                    .sum::<usize>()
                            })
                            .unwrap_or(0);
                        result.unknown_configs.push(UnknownConfig {
                            path: path.clone(),
                            container_keys: keys,
                            server_count: count,
                        });
                    }
                    let scope = infer_scope(&path, &project_roots);
                    targets.insert(
                        path.clone(),
                        ConfigTarget {
                            path,
                            client_id: client.map(|c| c.id.clone()),
                            scope,
                        },
                    );
                }
                Ok(_) => {}
                // Unparseable candidates are normal: most `settings.json`
                // files on a machine are not agent config at all.
                Err(_) => {}
            }
        }
    }

    // ---- 4. Parse ---------------------------------------------------------
    progress(&format!("parsing {} config file(s)", targets.len()));
    let all_keys = reg.all_container_keys();
    let mut entries: Vec<ServerEntry> = Vec::new();
    for target in targets.values() {
        let (client_id, client_name, keys) = match target.client_id.as_deref() {
            Some(id) => {
                let client = reg.client(id);
                let name = client
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| id.to_string());
                // Use the client's keys plus the universal set, so an entry
                // under an unexpected key is still found.
                (id.to_string(), name, all_keys.clone())
            }
            None => (
                "unknown".to_string(),
                "unidentified client".to_string(),
                all_keys.clone(),
            ),
        };
        match parse::entries_from_file(&target.path, &keys, &client_id, &client_name, &target.scope)
        {
            Ok(found) => {
                result.scanned_paths.push(target.path.clone());
                entries.extend(found);
            }
            Err(e) => result
                .errors
                .push(format!("{}: {e}", target.path.display())),
        }
    }

    // De-duplicate: the same file can be reached by more than one route.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    entries.retain(|e| seen.insert(e.key()));

    // ---- 5. Processes -----------------------------------------------------
    progress("inspecting running processes");
    let matched = match ProcSnapshot::capture() {
        Ok(snapshot) => proc::attribute(&snapshot, &entries, reg),
        Err(e) => {
            // Without the process table we can still report configs, so say so
            // rather than implying nothing is running.
            result
                .errors
                .push(format!("could not inspect processes: {e}"));
            proc::ProcessMatch {
                by_entry: Default::default(),
                orphans: Vec::new(),
                runtimes: Vec::new(),
            }
        }
    };

    // ---- 6. Artifacts -----------------------------------------------------
    progress("locating server files on disk");
    for entry in entries {
        let procs = matched
            .by_entry
            .get(&entry.key())
            .cloned()
            .unwrap_or_default();
        let artifacts = if opts.measure_artifacts {
            files::artifacts_for(&entry.transport)
        } else {
            Vec::new()
        };
        result.findings.push(Finding {
            entry,
            procs,
            artifacts,
        });
    }

    result.orphans = matched.orphans;
    result.runtimes = matched.runtimes;
    sort_findings(&mut result);
    result.duration_ms = started.elapsed().as_millis();
    result
}

fn sort_findings(result: &mut ScanResult) {
    // Running first, then by name, then by client: the rows a user is looking
    // for are almost always the live ones.
    result.findings.sort_by(|a, b| {
        b.is_running()
            .cmp(&a.is_running())
            .then_with(|| {
                a.entry
                    .name
                    .to_ascii_lowercase()
                    .cmp(&b.entry.name.to_ascii_lowercase())
            })
            .then_with(|| a.entry.client_name.cmp(&b.entry.client_name))
    });
    result.unknown_configs.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Summary counts for the header bar.
pub struct Totals {
    pub servers: usize,
    pub running: usize,
    pub clients: usize,
    pub orphans: usize,
    /// Bytes that could actually be reclaimed, counting each path once.
    pub bytes: u64,
}

pub fn totals(result: &ScanResult) -> Totals {
    let clients: BTreeSet<&str> = result
        .findings
        .iter()
        .map(|f| f.entry.client_id.as_str())
        .collect();

    // De-duplicate by path. One npx cache directory commonly backs the same
    // server registered in a dozen different agent configs, and summing per
    // finding would report a dozen times the space that deleting it frees.
    let mut unique: BTreeMap<&std::path::Path, u64> = BTreeMap::new();
    for f in &result.findings {
        for a in f.deletable_artifacts() {
            unique.insert(a.path.as_path(), a.size_bytes);
        }
    }

    Totals {
        servers: result.findings.len(),
        running: result.findings.iter().filter(|f| f.is_running()).count(),
        clients: clients.len(),
        orphans: result.orphans.len(),
        bytes: unique.values().sum(),
    }
}

/// Orphans grouped by their likely host, for the UI's orphan panel.
pub fn orphans_by_host(orphans: &[OrphanProc]) -> BTreeMap<String, Vec<&OrphanProc>> {
    let mut out: BTreeMap<String, Vec<&OrphanProc>> = BTreeMap::new();
    for o in orphans {
        let key = o
            .likely_host
            .clone()
            .unwrap_or_else(|| "unknown host".to_string());
        out.entry(key).or_default().push(o);
    }
    out
}

/// Runtimes that have no MCP support, to explain their absence from the table.
pub fn runtime_notes(runtimes: &[DetectedRuntime]) -> Vec<String> {
    runtimes
        .iter()
        .map(|r| format!("{} ({} pid(s)): {}", r.name, r.pids.len(), r.detail))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_scope_prefers_a_containing_project_root() {
        let root = PathBuf::from("/Users/me/proj");
        let inside = PathBuf::from("/Users/me/proj/.cursor/mcp.json");
        assert_eq!(
            infer_scope(&inside, std::slice::from_ref(&root)),
            Scope::Project(root)
        );
    }

    #[test]
    fn home_directory_is_not_a_project_root() {
        // Regression: ~/.claude.json records $HOME among its projects, which
        // made every ~/.kiro/agents/*.json file report as project-scoped.
        let home = dirs::home_dir().expect("home dir");
        assert!(!is_project_root_candidate(&home));
        assert!(!is_project_root_candidate(Path::new("/")));
        assert!(is_project_root_candidate(&home.join("development/apps/x")));
    }

    #[test]
    fn user_scope_configs_are_not_relabelled_as_project_scope() {
        // The user-scope Kiro config must stay user-scope even though
        // `.kiro/settings/mcp.json` also matches as a project-relative path.
        let reg = Registry::builtin().unwrap();
        let result = scan(
            &reg,
            &ScanOptions {
                sweep: false,
                extra_project_roots: vec![],
                measure_artifacts: false,
            },
            |_| {},
        );
        let home = dirs::home_dir().unwrap();
        for f in &result.findings {
            if f.entry.source.path.starts_with(home.join(".kiro"))
                || f.entry.source.path == home.join(".claude.json")
            {
                assert_eq!(
                    f.entry.scope,
                    Scope::User,
                    "{} should be user scope, got {}",
                    f.entry.source.path.display(),
                    f.entry.scope
                );
            }
        }
    }

    #[test]
    fn infer_scope_falls_back_to_user_for_home_paths() {
        let home = dirs::home_dir().expect("home dir");
        let p = home.join(".cursor/mcp.json");
        assert_eq!(infer_scope(&p, &[]), Scope::User);
    }

    #[test]
    fn a_real_scan_completes_and_stays_consistent() {
        // Runs against the real machine: it must not panic, and its own
        // invariants must hold whatever it finds.
        let reg = Registry::builtin().unwrap();
        let opts = ScanOptions {
            sweep: false, // keep the unit test fast; the sweep is exercised below
            extra_project_roots: vec![],
            measure_artifacts: false,
        };
        let result = scan(&reg, &opts, |_| {});

        for f in &result.findings {
            assert!(!f.entry.name.is_empty(), "a server must have a name");
            assert!(
                f.entry.source.path.is_absolute(),
                "source path should be absolute: {}",
                f.entry.source.path.display()
            );
            assert!(
                f.entry.source.container_pointer.starts_with('/'),
                "pointer should be rooted: {}",
                f.entry.source.container_pointer
            );
        }

        // Keys must be unique, or the UI would show duplicate rows and the
        // purge would target the wrong slot.
        let mut keys: Vec<String> = result.findings.iter().map(|f| f.entry.key()).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "duplicate finding keys");
    }

    #[test]
    fn totals_agree_with_the_findings() {
        let reg = Registry::builtin().unwrap();
        let result = scan(
            &reg,
            &ScanOptions {
                sweep: false,
                extra_project_roots: vec![],
                measure_artifacts: false,
            },
            |_| {},
        );
        let t = totals(&result);
        assert_eq!(t.servers, result.findings.len());
        assert_eq!(
            t.running,
            result.findings.iter().filter(|f| f.is_running()).count()
        );
        assert!(t.running <= t.servers);
    }

    #[test]
    fn scanning_a_planted_tree_finds_the_servers() {
        // A self-contained project tree, so the assertion does not depend on
        // what happens to be installed on the host.
        let base = std::env::temp_dir().join(format!("mcpurge-scan-{}", std::process::id()));
        let proj = base.join("myproj");
        std::fs::create_dir_all(proj.join(".cursor")).unwrap();
        std::fs::write(
            proj.join(".cursor/mcp.json"),
            r#"{"mcpServers":{"planted":{"command":"uvx","args":["mcp-server-time"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            proj.join(".mcp.json"),
            r#"{"mcpServers":{"shared":{"url":"https://example.test/mcp"}}}"#,
        )
        .unwrap();

        let reg = Registry::builtin().unwrap();
        let result = scan(
            &reg,
            &ScanOptions {
                sweep: false,
                extra_project_roots: vec![proj.clone()],
                measure_artifacts: false,
            },
            |_| {},
        );

        let names: Vec<&str> = result
            .findings
            .iter()
            .filter(|f| f.entry.source.path.starts_with(&proj))
            .map(|f| f.entry.name.as_str())
            .collect();
        assert!(names.contains(&"planted"), "got {names:?}");
        assert!(names.contains(&"shared"), "got {names:?}");

        // The .cursor/mcp.json file should be attributed to Cursor.
        let planted = result
            .findings
            .iter()
            .find(|f| f.entry.name == "planted")
            .unwrap();
        assert_eq!(planted.entry.client_id, "cursor");
        assert_eq!(planted.entry.scope, Scope::Project(proj.clone()));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn reclaimable_bytes_count_each_path_once() {
        // Regression: the headline figure summed artifacts per finding, so one
        // npx cache shared by 42 server entries was reported 42 times over
        // (223 MB became "1.0 GB reclaimable").
        use crate::model::{
            Artifact, ArtifactKind, ConfigFormat, Finding, ServerEntry, SourceLocation, Transport,
        };

        let shared_cache = Artifact {
            path: PathBuf::from("/tmp/mcpurge-test/_npx/abc"),
            size_bytes: 100,
            kind: ArtifactKind::NpmCache,
            shared: false,
            note: String::new(),
        };
        let make = |name: &str| Finding {
            entry: ServerEntry {
                name: name.to_string(),
                client_id: "a".into(),
                client_name: "A".into(),
                scope: Scope::User,
                transport: Transport::Stdio {
                    command: "npx".into(),
                    args: vec![],
                },
                env: Default::default(),
                disabled: false,
                source: SourceLocation {
                    path: PathBuf::from(format!("/tmp/{name}.json")),
                    container_pointer: "/mcpServers".into(),
                    container_key: "mcpServers".into(),
                    format: ConfigFormat::Json,
                },
                raw: serde_json::Value::Null,
            },
            procs: vec![],
            artifacts: vec![shared_cache.clone()],
        };

        let result = ScanResult {
            findings: vec![make("one"), make("two"), make("three")],
            ..Default::default()
        };
        let t = totals(&result);
        assert_eq!(t.servers, 3);
        assert_eq!(
            t.bytes, 100,
            "three entries sharing one 100-byte cache reclaim 100 bytes, not 300"
        );
    }

    #[test]
    fn runtime_notes_are_rendered_for_display() {
        let rts = vec![DetectedRuntime {
            name: "Ollama".into(),
            detail: "no native MCP support".into(),
            pids: vec![1, 2],
        }];
        let notes = runtime_notes(&rts);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("Ollama") && notes[0].contains("2 pid"));
    }
}
