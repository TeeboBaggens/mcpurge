//! The content-based sweep.
//!
//! The registry says where configs usually live. This module exists because
//! that is not good enough: vendors move their files, forks invent new paths,
//! and tools appear faster than any hardcoded table is updated. So we also
//! walk a bounded set of roots, pick out files whose *names* could plausibly
//! be agent config, and let [`super::parse`] decide by content shape whether
//! each one actually declares MCP servers.
//!
//! Everything here is bounded on purpose: depth limits, a prune list, and a
//! file-size cap, so a sweep stays in the low hundreds of milliseconds rather
//! than turning into a full-disk crawl.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// A directory to sweep, with how deep to go from there.
#[derive(Debug, Clone)]
pub struct SweepRoot {
    pub path: PathBuf,
    pub max_depth: usize,
}

impl SweepRoot {
    pub fn new(path: impl Into<PathBuf>, max_depth: usize) -> Self {
        Self {
            path: path.into(),
            max_depth,
        }
    }
}

/// Config files larger than this are not agent config; reading them would just
/// stall the sweep.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Directory names never worth descending into. Caches and dependency trees
/// contain thousands of `config.json` files and no agent configuration, and
/// cloud-sync folders would pull files over the network.
const PRUNE_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".svn",
    ".hg",
    "target",
    "build",
    "dist",
    "out",
    ".next",
    ".nuxt",
    ".cache",
    "Caches",
    "CachedData",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "Service Worker",
    "IndexedDB",
    "Local Storage",
    "Session Storage",
    "blob_storage",
    "logs",
    "Logs",
    "Crashpad",
    "CrashReporter",
    "site-packages",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
    ".m2",
    ".rustup",
    ".cargo",
    ".Trash",
    "Trash",
    // Cloud-sync and media libraries: slow, and nothing of ours lives there.
    "CloudStorage",
    "Photos Library.photoslibrary",
    "Music",
    "Movies",
    "Pictures",
    ".DocumentRevisions-V100",
    ".Spotlight-V100",
    ".fseventsd",
];

/// npx and uv caches hold real server code, but thousands of files each. They
/// are inspected deliberately by `files::artifacts_for`, never swept.
const PRUNE_PATH_FRAGMENTS: &[&str] = &["_npx", "/.npm/", "/.bun/install/", "/.pnpm-store/"];

fn should_prune(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    if PRUNE_DIRS.iter().any(|d| *d == name) {
        return true;
    }
    let full = entry.path().to_string_lossy().replace('\\', "/");
    PRUNE_PATH_FRAGMENTS.iter().any(|f| full.contains(f))
}

/// Could this filename hold agent configuration? Intentionally generous: the
/// shape test downstream is what actually decides, and a wasted parse of a
/// small JSON file is cheap.
pub fn candidate_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();

    // Anything with "mcp" in the name, in any supported format.
    let has_ext = |exts: &[&str]| exts.iter().any(|e| lower.ends_with(e));
    if lower.contains("mcp") && has_ext(&[".json", ".jsonc", ".yaml", ".yml", ".toml"]) {
        return true;
    }

    // Known config filenames that carry MCP blocks among other settings.
    const EXACT: &[&str] = &[
        "settings.json",
        "settings.local.json",
        "config.json",
        "config.jsonc",
        "config.toml",
        "config.yaml",
        "config.yml",
        "claude_desktop_config.json",
        "opencode.json",
        "opencode.jsonc",
        "crush.json",
        "librechat.yaml",
        "librechat.yml",
        "default.json",
        "manifest.json",
    ];
    if EXACT.contains(&lower.as_str()) {
        return true;
    }

    // Hidden per-tool configs: .claude.json, .cursor.json, .crush.json, ...
    lower.starts_with('.') && has_ext(&[".json", ".jsonc"])
}

/// Default roots for the host OS. Depths are tuned so that the places configs
/// actually live are reachable without crawling whole home directories.
pub fn default_roots() -> Vec<SweepRoot> {
    let mut roots = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return roots;
    };

    // Dotfiles and dotdirs directly under home: ~/.cursor, ~/.gemini, ~/.kiro,
    // ~/.codex, ~/.pi, ~/.lmstudio, ~/.claude, ~/.aws/amazonq, ...
    roots.push(SweepRoot::new(&home, 4));

    // Per-app config/data dirs. On macOS this is ~/Library/Application Support,
    // which is wide but shallow for our purposes; depth 5 reaches
    // Code/User/globalStorage/<ext>/settings/x.json.
    if let Some(cfg) = dirs::config_dir()
        && cfg != home
    {
        roots.push(SweepRoot::new(cfg, 6));
    }
    if let Some(local) = dirs::config_local_dir()
        && !roots.iter().any(|r| r.path == local)
    {
        roots.push(SweepRoot::new(local, 6));
    }
    if let Some(data) = dirs::data_dir()
        && !roots.iter().any(|r| r.path == data)
    {
        roots.push(SweepRoot::new(data, 6));
    }

    roots
}

/// Relative paths worth probing inside a project directory. Built from the
/// registry's `project_paths` plus the portable spellings.
pub fn project_candidates(registry_project_paths: &[String]) -> Vec<String> {
    let mut set: BTreeSet<String> = registry_project_paths.iter().cloned().collect();
    for extra in [
        ".mcp.json",
        "mcp.json",
        ".vscode/mcp.json",
        ".cursor/mcp.json",
    ] {
        set.insert(extra.to_string());
    }
    set.into_iter().collect()
}

/// Walk the roots and return candidate config files, de-duplicated.
pub fn sweep(roots: &[SweepRoot], mut progress: impl FnMut(&str)) -> Vec<PathBuf> {
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    for root in roots {
        if !root.path.is_dir() {
            continue;
        }
        progress(&format!("sweeping {}", root.path.display()));
        let walker = WalkDir::new(&root.path)
            .max_depth(root.max_depth)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !should_prune(e));

        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if !candidate_filename(&name) {
                continue;
            }
            if entry
                .metadata()
                .map(|m| m.len() > MAX_FILE_BYTES)
                .unwrap_or(true)
            {
                continue;
            }
            found.insert(entry.path().to_path_buf());
        }
    }
    found.into_iter().collect()
}

/// Probe a project directory for the relative candidates. Cheap: a stat per
/// candidate rather than a walk.
pub fn probe_project(root: &Path, candidates: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in candidates {
        if rel.contains('*') {
            let pattern = root.join(rel);
            if let Ok(paths) = glob::glob(&pattern.to_string_lossy()) {
                out.extend(paths.filter_map(Result::ok).filter(|p| p.is_file()));
            }
            continue;
        }
        let p = root.join(rel);
        if p.is_file()
            && std::fs::metadata(&p)
                .map(|m| m.len() <= MAX_FILE_BYTES)
                .unwrap_or(false)
        {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_filenames_are_candidates() {
        for name in [
            "mcp.json",
            "mcp_config.json",
            "cline_mcp_settings.json",
            "mcp_settings.json",
            ".mcp.json",
            "mcp.jsonc",
            "anythingllm_mcp_servers.json",
        ] {
            assert!(candidate_filename(name), "{name} should be a candidate");
        }
    }

    #[test]
    fn shared_config_filenames_are_candidates() {
        for name in [
            "settings.json",
            "config.yaml",
            "config.toml",
            "claude_desktop_config.json",
            "opencode.jsonc",
            ".claude.json",
            "librechat.yaml",
        ] {
            assert!(candidate_filename(name), "{name} should be a candidate");
        }
    }

    #[test]
    fn unrelated_files_are_skipped() {
        for name in [
            "package.json",
            "Cargo.toml",
            "index.js",
            "README.md",
            "photo.png",
            "yarn.lock",
            "tsconfig.json",
        ] {
            assert!(!candidate_filename(name), "{name} should be skipped");
        }
    }

    #[test]
    fn prune_list_covers_the_expensive_directories() {
        for d in [
            "node_modules",
            "Caches",
            "site-packages",
            "CloudStorage",
            ".git",
        ] {
            assert!(PRUNE_DIRS.contains(&d), "{d} should be pruned");
        }
    }

    #[test]
    fn default_roots_are_directories_that_exist() {
        let roots = default_roots();
        assert!(!roots.is_empty());
        assert!(roots.iter().all(|r| r.max_depth > 0));
    }

    #[test]
    fn probe_project_finds_a_planted_config() {
        let dir = std::env::temp_dir().join(format!("mcpurge-probe-{}", std::process::id()));
        let nested = dir.join(".cursor");
        std::fs::create_dir_all(&nested).unwrap();
        let cfg = nested.join("mcp.json");
        std::fs::write(&cfg, r#"{"mcpServers":{}}"#).unwrap();

        let found = probe_project(
            &dir,
            &[".cursor/mcp.json".to_string(), "absent.json".into()],
        );
        assert_eq!(found, vec![cfg]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn project_candidates_include_portable_spellings() {
        let c = project_candidates(&[".kiro/settings/mcp.json".to_string()]);
        assert!(c.iter().any(|x| x == ".mcp.json"));
        assert!(c.iter().any(|x| x == ".kiro/settings/mcp.json"));
    }
}
