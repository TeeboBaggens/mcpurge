//! Locating the files on disk that back a server.
//!
//! The safety rule here matters more than the completeness: an artifact is
//! marked `shared` whenever deleting it would break something other than the
//! server being purged. A server launched from
//! `/Applications/SolidCraft.app/...` must never offer to delete the app, and
//! `~/.cache/uv` backs every uv-based tool on the machine, not just this one.
//! Shared artifacts are shown with their size but excluded from deletion.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::model::{Artifact, ArtifactKind, Transport};

/// Directory trees that belong to the system or a package manager. Nothing
/// under these is ever offered for deletion.
fn is_system_path(path: &Path) -> bool {
    let p = path.to_string_lossy().replace('\\', "/");
    const ROOTS: &[&str] = &[
        "/Applications/",
        "/System/",
        "/Library/",
        "/usr/",
        "/bin/",
        "/sbin/",
        "/opt/homebrew/",
        "/opt/local/",
        "/opt/",
        "/etc/",
        "C:/Program Files",
        "C:/Windows",
    ];
    if ROOTS.iter().any(|r| p.starts_with(r)) {
        return true;
    }
    // Version-manager and toolchain installs are shared infrastructure.
    const FRAGMENTS: &[&str] = &[
        "/.local/share/mise/",
        "/.rustup/",
        "/.cargo/bin/",
        "/.nvm/",
        "/.volta/",
        "/.pyenv/",
        "/.asdf/",
        "/homebrew/Cellar/",
    ];
    FRAGMENTS.iter().any(|f| p.contains(f))
}

/// Caches shared by every tool that uses the same package manager.
fn is_shared_cache(path: &Path) -> bool {
    let p = path.to_string_lossy().replace('\\', "/");
    const SHARED: &[&str] = &[
        "/.cache/uv",
        "/.npm/_cacache",
        "/.bun/install/cache",
        "/.pnpm-store",
        "/Library/Caches/",
        "/.cache/pip",
        "/.cache/ms-playwright",
    ];
    SHARED.iter().any(|s| p.contains(s))
}

/// Total size of a path, following no symlinks. Bounded so a pathological tree
/// cannot stall a scan.
pub fn dir_size(path: &Path) -> u64 {
    const MAX_ENTRIES: usize = 60_000;
    if path.is_file() {
        return std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    let mut total = 0u64;
    for (i, entry) in WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .enumerate()
    {
        if i > MAX_ENTRIES {
            break;
        }
        if entry.file_type().is_file()
            && let Ok(meta) = entry.metadata()
        {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

fn artifact(path: PathBuf, kind: ArtifactKind, note: impl Into<String>) -> Artifact {
    // A data path is the user's own content, so it is protected regardless of
    // where it sits on disk.
    let shared = kind == ArtifactKind::DataPath || is_system_path(&path) || is_shared_cache(&path);
    let mut note = note.into();
    if kind == ArtifactKind::DataPath {
        note = format!("{note} (your data, never deleted)");
    } else if is_system_path(&path) {
        note = format!("{note} (system or package-manager path, never deleted)");
    } else if is_shared_cache(&path) {
        note = format!("{note} (shared cache, never deleted)");
    }
    Artifact {
        size_bytes: dir_size(&path),
        kind,
        shared,
        note,
        path,
    }
}

/// First argument that looks like a package specification rather than a flag.
fn package_spec(args: &[String]) -> Option<&str> {
    args.iter().map(String::as_str).find(|a| {
        !a.starts_with('-')
            && *a != "exec"
            && *a != "run"
            && !a.chars().all(|c| c.is_ascii_digit())
            && a.len() > 2
    })
}

/// Strip a version from a spec: `pkg@1.2.3` -> `pkg`, `@scope/pkg@1` -> `@scope/pkg`.
fn spec_name(spec: &str) -> String {
    if let Some(rest) = spec.strip_prefix('@') {
        match rest.find('@') {
            Some(i) => format!("@{}", &rest[..i]),
            None => spec.to_string(),
        }
    } else {
        spec.split('@').next().unwrap_or(spec).to_string()
    }
}

/// npx caches each distinct package set in `~/.npm/_npx/<hash>/`. Those hash
/// directories are per-package-set, so one can be removed on its own; the
/// surrounding `~/.npm/_cacache` cannot.
fn npx_caches(spec: &str) -> Vec<Artifact> {
    let mut out = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    let npx = home.join(".npm").join("_npx");
    if !npx.is_dir() {
        return out;
    }
    let name = spec_name(spec);
    let Ok(entries) = std::fs::read_dir(&npx) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // The package appears either as node_modules/<name> or in the
        // directory's package.json dependency list.
        let modules = dir.join("node_modules").join(&name);
        let hit = modules.exists() || {
            let pkg = dir.join("package.json");
            std::fs::read_to_string(&pkg)
                .map(|t| t.contains(&name))
                .unwrap_or(false)
        };
        if hit {
            out.push(artifact(
                dir,
                ArtifactKind::NpmCache,
                format!("npx cache for {name}; re-downloaded on next launch"),
            ));
        }
    }
    out
}

/// uv installs named tools under its data dir, one directory per tool, which
/// is removable. Its archive cache is shared and is reported read-only.
fn uv_artifacts(spec: &str) -> Vec<Artifact> {
    let mut out = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    let name = spec_name(spec);
    let mut roots = vec![home.join(".local/share/uv/tools")];
    if let Some(data) = dirs::data_dir() {
        roots.push(data.join("uv/tools"));
    }
    for root in roots {
        let dir = root.join(&name);
        if dir.is_dir() {
            out.push(artifact(
                dir,
                ArtifactKind::UvCache,
                format!("uv tool environment for {name}"),
            ));
        }
    }
    let cache = home.join(".cache/uv");
    if cache.is_dir() {
        out.push(artifact(
            cache,
            ArtifactKind::UvCache,
            "uv download cache, shared by every uv tool",
        ));
    }
    out
}

/// Global npm install location for a package, if present.
fn global_npm(spec: &str) -> Vec<Artifact> {
    let mut out = Vec::new();
    let name = spec_name(spec);
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    let candidates = [
        home.join(".npm-global/lib/node_modules"),
        home.join(".local/lib/node_modules"),
        PathBuf::from("/usr/local/lib/node_modules"),
        PathBuf::from("/opt/homebrew/lib/node_modules"),
    ];
    for base in candidates {
        let dir = base.join(&name);
        if dir.is_dir() {
            out.push(artifact(
                dir,
                ArtifactKind::NpmCache,
                format!("global npm install of {name}"),
            ));
        }
    }
    out
}

/// Paths named directly in a command or its arguments.
fn referenced_paths(command: &str, args: &[String]) -> Vec<Artifact> {
    let mut out = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();

    let mut consider = |raw: &str, is_command: bool| {
        if !(raw.contains('/') || raw.contains('\\')) {
            return;
        }
        let expanded = if let Some(rest) = raw.strip_prefix("~/") {
            dirs::home_dir()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| PathBuf::from(raw))
        } else {
            PathBuf::from(raw)
        };
        if !expanded.exists() || seen.contains(&expanded) {
            return;
        }
        seen.push(expanded.clone());

        // Only the command itself is part of the server. Anything named in the
        // arguments is a path the server was pointed AT - a workspace it may
        // read, a project it was granted with --allow-path - and deleting it
        // would destroy the user's work rather than purge a server.
        let (kind, note) = if is_command {
            (
                ArtifactKind::LocalExecutable,
                "the server's executable, as named in the config",
            )
        } else {
            (
                ArtifactKind::DataPath,
                "granted to the server as an argument",
            )
        };
        out.push(artifact(expanded, kind, note));
    };

    consider(command, true);
    for a in args {
        consider(a, false);
    }
    out
}

/// Everything on disk we can attribute to this server.
pub fn artifacts_for(transport: &Transport) -> Vec<Artifact> {
    let Transport::Stdio { command, args } = transport else {
        // Remote servers have no local files to purge.
        return Vec::new();
    };

    let mut out = referenced_paths(command, args);

    let base = Path::new(command)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if let Some(spec) = package_spec(args) {
        match base.trim_end_matches(".exe") {
            "npx" | "npm" | "pnpm" | "yarn" | "bunx" | "bun" => {
                out.extend(npx_caches(spec));
                out.extend(global_npm(spec));
            }
            "uvx" | "uv" | "pipx" => out.extend(uv_artifacts(spec)),
            "docker" | "podman" => out.push(Artifact {
                path: PathBuf::from(format!("container image: {spec}")),
                size_bytes: 0,
                kind: ArtifactKind::Other,
                shared: true,
                note: "container image; remove it with your container tool".into(),
            }),
            _ => {}
        }
    }

    // De-duplicate by path, keeping the first (most specific) entry.
    let mut seen: Vec<PathBuf> = Vec::new();
    out.retain(|a| {
        if seen.contains(&a.path) {
            false
        } else {
            seen.push(a.path.clone());
            true
        }
    });
    out.sort_by_key(|a| std::cmp::Reverse(a.size_bytes));
    out
}

/// Reveal a path in the platform file manager.
pub fn reveal(path: &Path) -> std::io::Result<()> {
    let target = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    open::that_detached(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_bundles_are_never_deletable() {
        let a = artifact(
            PathBuf::from("/Applications/SolidCraft.app/Contents/MacOS/solidcraft-cli"),
            ArtifactKind::LocalExecutable,
            "executable",
        );
        assert!(a.shared, "an app bundle must not be offered for deletion");
        assert!(a.note.contains("never deleted"));
    }

    #[test]
    fn mise_and_homebrew_installs_are_shared() {
        for p in [
            "/Users/me/.local/share/mise/installs/uv/latest/uvx",
            "/opt/homebrew/bin/uvx",
            "/usr/local/bin/node",
        ] {
            assert!(is_system_path(Path::new(p)), "{p} should be shared");
        }
    }

    #[test]
    fn uv_cache_is_shared_but_tool_dirs_are_not() {
        assert!(is_shared_cache(Path::new(
            "/Users/me/.cache/uv/archive-v0/x"
        )));
        assert!(!is_shared_cache(Path::new(
            "/Users/me/.local/share/uv/tools/godot-ai"
        )));
    }

    #[test]
    fn a_project_directory_is_deletable() {
        let a = artifact(std::env::temp_dir(), ArtifactKind::SourceDir, "source");
        assert!(!a.shared, "a plain user directory should be deletable");
    }

    #[test]
    fn version_specs_reduce_to_package_names() {
        assert_eq!(spec_name("rea-agents@6.7.0"), "rea-agents");
        assert_eq!(spec_name("godot-ai==4.4.1"), "godot-ai==4.4.1");
        assert_eq!(
            spec_name("@modelcontextprotocol/server-filesystem@1.0.0"),
            "@modelcontextprotocol/server-filesystem"
        );
        assert_eq!(spec_name("mcp-server-time"), "mcp-server-time");
    }

    #[test]
    fn package_spec_skips_flags_and_subcommands() {
        assert_eq!(
            package_spec(&["-y".into(), "rea-agents@6.7.0".into(), "mcp".into()]),
            Some("rea-agents@6.7.0")
        );
        assert_eq!(
            package_spec(&["exec".into(), "mcp-server-time".into()]),
            Some("mcp-server-time")
        );
        assert_eq!(package_spec(&["-y".into()]), None);
    }

    #[test]
    fn remote_servers_have_no_local_artifacts() {
        let t = Transport::Http {
            url: "https://example.test/mcp".into(),
        };
        assert!(artifacts_for(&t).is_empty());
    }

    #[test]
    fn docker_servers_are_reported_but_not_deletable() {
        let t = Transport::Stdio {
            command: "docker".into(),
            args: vec!["run".into(), "-i".into(), "mcp/everything".into()],
        };
        let arts = artifacts_for(&t);
        assert_eq!(arts.len(), 1);
        assert!(arts[0].shared);
        assert_eq!(arts[0].kind, ArtifactKind::Other);
    }

    #[test]
    fn allow_path_arguments_are_never_deletable() {
        // Regression: a real config read
        //   solidcraft-cli mcp --allow-path ~/projects/a-firmware-project
        // and the scanner offered that 1.4 GB project directory for deletion.
        // A path handed to a server is the user's work, not server files.
        let project = std::env::temp_dir().join(format!("mcpurge-proj-{}", std::process::id()));
        std::fs::create_dir_all(&project).unwrap();
        let t = Transport::Stdio {
            command: "/usr/local/bin/some-mcp-cli".into(),
            args: vec![
                "mcp".into(),
                "--allow-path".into(),
                project.to_string_lossy().into_owned(),
            ],
        };
        let arts = artifacts_for(&t);
        let found = arts
            .iter()
            .find(|a| a.path == project)
            .expect("the granted path should be reported");
        assert_eq!(found.kind, ArtifactKind::DataPath);
        assert!(found.shared, "a granted data path must never be deletable");
        assert!(found.note.contains("never deleted"), "got {}", found.note);
    }

    #[test]
    fn referenced_existing_paths_are_found() {
        let tmp = std::env::temp_dir();
        let t = Transport::Stdio {
            command: "node".into(),
            args: vec![tmp.to_string_lossy().into_owned()],
        };
        let arts = artifacts_for(&t);
        assert!(
            arts.iter().any(|a| a.path == tmp),
            "an existing referenced path should be reported"
        );
    }

    #[test]
    fn dir_size_counts_a_known_file() {
        let dir = std::env::temp_dir().join(format!("mcpurge-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, vec![b'x'; 1234]).unwrap();
        assert_eq!(dir_size(&f), 1234);
        assert!(dir_size(&dir) >= 1234);
        std::fs::remove_dir_all(&dir).ok();
    }
}
