//! Core domain types shared by the scanners, the process prober and the UI.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// How a client talks to a server. `Unknown` is deliberate: we would rather
/// show a server we do not fully understand than silently drop it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio { command: String, args: Vec<String> },
    Http { url: String },
    Sse { url: String },
    Unknown,
}

impl Transport {
    /// The command line a stdio server would be launched with, for display and
    /// for matching against running processes.
    pub fn command_line(&self) -> Option<String> {
        match self {
            Transport::Stdio { command, args } => {
                let mut s = command.clone();
                for a in args {
                    s.push(' ');
                    s.push_str(a);
                }
                Some(s)
            }
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
            Transport::Sse { .. } => "sse",
            Transport::Unknown => "unknown",
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Transport::Stdio { .. })
    }

    /// How the client reaches the server, and whether that means a process on
    /// this machine at all.
    pub fn explain(&self) -> &'static str {
        match self {
            Transport::Stdio { .. } => {
                "stdio: the client launches the server as a local process and talks to it \
                 over its standard input and output. This is the kind mcpurge can stop."
            }
            Transport::Http { .. } => {
                "http: the client calls a URL. Nothing runs on this machine unless you \
                 started it yourself, so there is no process to stop."
            }
            Transport::Sse { .. } => {
                "sse: the client calls a URL and reads server-sent events. Nothing runs on \
                 this machine unless you started it yourself."
            }
            Transport::Unknown => {
                "the config declares this server in a shape mcpurge does not recognise, so \
                 how the client reaches it is unknown. It is listed anyway rather than \
                 silently dropped."
            }
        }
    }
}

/// Which config file a server came from, and where inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub path: PathBuf,
    /// JSON-pointer-ish path to the *container* object, e.g.
    /// `/mcpServers` or `/projects/~1Users~1me~1proj/mcpServers`.
    pub container_pointer: String,
    /// The key the container was found under (`mcpServers`, `servers`, ...).
    pub container_key: String,
    pub format: ConfigFormat,
}

impl SourceLocation {
    /// Pointer to the individual server entry, used by the rewriter.
    pub fn entry_pointer(&self, name: &str) -> String {
        format!("{}/{}", self.container_pointer, escape_pointer_token(name))
    }
}

pub fn escape_pointer_token(tok: &str) -> String {
    tok.replace('~', "~0").replace('/', "~1")
}

pub fn unescape_pointer_token(tok: &str) -> String {
    tok.replace("~1", "/").replace("~0", "~")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Json,
    Jsonc,
    Yaml,
    Toml,
}

impl ConfigFormat {
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "jsonc" => ConfigFormat::Jsonc,
            "yaml" | "yml" => ConfigFormat::Yaml,
            "toml" => ConfigFormat::Toml,
            // Plain .json is parsed with the JSONC reader anyway, because several
            // clients ship comments in files named .json (VS Code especially).
            _ => ConfigFormat::Json,
        }
    }

    /// Whether we can rewrite this format in place without reformatting it.
    pub fn supports_surgical_edit(self) -> bool {
        matches!(self, ConfigFormat::Json | ConfigFormat::Jsonc)
    }
}

/// Where a config sits in a client's precedence chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Applies everywhere for this user.
    User,
    /// Scoped to one project/workspace directory.
    Project(PathBuf),
    /// Machine-wide / system config.
    System,
    Unknown,
}

impl Scope {
    /// Who the entry applies to. The distinction matters before removing one:
    /// a user-scope entry is gone from every project at once.
    pub fn explain(&self) -> &'static str {
        match self {
            Scope::User => {
                "user scope: declared in your own config, so this client offers the server \
                 everywhere. Removing it removes it everywhere."
            }
            Scope::Project(_) => {
                "project scope: declared for one project or workspace, and offered only \
                 while working there."
            }
            Scope::System => {
                "system scope: declared machine-wide, so it applies to every user account \
                 on this computer."
            }
            Scope::Unknown => {
                "mcpurge could not tell who this entry applies to from where it was found."
            }
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::User => write!(f, "user"),
            Scope::Project(p) => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("project");
                write!(f, "project:{name}")
            }
            Scope::System => write!(f, "system"),
            Scope::Unknown => write!(f, "?"),
        }
    }
}

/// A single MCP server as declared by one client in one config file.
#[derive(Debug, Clone)]
pub struct ServerEntry {
    pub name: String,
    /// Registry id of the owning client, or `"unknown"` for a config found by
    /// the generic sweep that matched no known client.
    pub client_id: String,
    pub client_name: String,
    pub scope: Scope,
    pub transport: Transport,
    pub env: BTreeMap<String, String>,
    /// Client-specific disable flags (`disabled`, `enabled: false`, ...).
    pub disabled: bool,
    pub source: SourceLocation,
    /// The verbatim entry, so the detail pane can show fields we do not model.
    pub raw: serde_json::Value,
}

impl ServerEntry {
    /// Stable identity across rescans: same server, same config slot.
    pub fn key(&self) -> String {
        format!(
            "{}::{}::{}",
            self.source.path.display(),
            self.source.container_pointer,
            self.name
        )
    }
}

/// A live process believed to be an MCP server.
#[derive(Debug, Clone)]
pub struct RunningProc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub cmdline: String,
    pub exe: Option<PathBuf>,
    /// Seconds since the process started.
    pub run_time: u64,
    pub memory_bytes: u64,
    /// PIDs of the whole subtree below this process, deepest last.
    pub descendants: Vec<u32>,
    /// Why we think this process belongs to the entry, for the UI to explain.
    pub match_reason: String,
    pub confidence: MatchConfidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchConfidence {
    /// Looks MCP-ish but we could not tie it to a config entry.
    Loose,
    /// Command basename agrees.
    Probable,
    /// Command and a distinctive argument agree.
    Strong,
}

impl MatchConfidence {
    /// Why this process was tied to this config entry, and how much to trust
    /// it. Attribution is inference, and the UI says so rather than implying a
    /// certainty it does not have.
    pub fn explain(self) -> &'static str {
        match self {
            MatchConfidence::Strong => {
                "strong match: the running command is the one the config declares, down to an \
                 absolute path or a distinctive argument. Stopping it stops this server."
            }
            MatchConfidence::Probable => {
                "probable match: the command name agrees, or some of the arguments do. Two \
                 servers launched the same way can look alike, so check the command line \
                 below before stopping it."
            }
            MatchConfidence::Loose => {
                "loose match: this looks like an MCP server but ties to no config entry. That \
                 is what makes it an orphan."
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MatchConfidence::Loose => "loose",
            MatchConfidence::Probable => "probable",
            MatchConfidence::Strong => "strong",
        }
    }
}

/// Something on disk that backs a server: a package cache, a venv, a script.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub kind: ArtifactKind,
    /// True when other servers or tools legitimately use this path, which makes
    /// deleting it unsafe. Shared artifacts are never deleted by default.
    pub shared: bool,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// npx / npm cache directory for the package.
    NpmCache,
    /// uv / uvx cache or tool venv.
    UvCache,
    /// A Python virtualenv.
    Venv,
    /// A local script or binary referenced directly by the config.
    LocalExecutable,
    /// A path handed to the server as an argument, e.g. `--allow-path <dir>`.
    /// This is the user's own data that the server was granted access to, not
    /// part of the server. Never deletable.
    DataPath,
    /// A checked-out source directory.
    SourceDir,
    Other,
}

impl ArtifactKind {
    /// What this kind of path is, and what that means for deleting it.
    ///
    /// The UI shows a one-word label; this is the sentence behind it, so the
    /// explanation lives next to the thing it explains rather than in the
    /// widget that happens to draw it.
    pub fn explain(self) -> &'static str {
        match self {
            ArtifactKind::NpmCache => {
                "the npm/npx cache directory for this package. Deleting it only costs a \
                 re-download the next time the server starts."
            }
            ArtifactKind::UvCache => {
                "the uv/uvx cache or tool environment for this package. Deleting it only \
                 costs a re-download, unless it is the cache shared by every uv tool on \
                 the machine, in which case it is kept."
            }
            ArtifactKind::Venv => {
                "a Python virtualenv created for this server. Deleting it costs a \
                 reinstall of its packages."
            }
            ArtifactKind::LocalExecutable => {
                "a script or binary the config launches directly. It may be something you \
                 wrote, so look before deleting it."
            }
            ArtifactKind::DataPath => {
                "a directory the server was granted access to, named in its own arguments. \
                 This is your work, not the server's, so it is never deleted."
            }
            ArtifactKind::SourceDir => {
                "a checked-out source directory the server runs from. It may hold \
                 uncommitted work."
            }
            ArtifactKind::Other => "a path that belongs to this server but fits no other category.",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ArtifactKind::NpmCache => "npm cache",
            ArtifactKind::UvCache => "uv cache",
            ArtifactKind::Venv => "venv",
            ArtifactKind::LocalExecutable => "executable",
            ArtifactKind::DataPath => "data path",
            ArtifactKind::SourceDir => "source",
            ArtifactKind::Other => "other",
        }
    }
}

/// One row in the UI: a declared server plus everything we learned about it.
#[derive(Debug, Clone)]
pub struct Finding {
    pub entry: ServerEntry,
    pub procs: Vec<RunningProc>,
    pub artifacts: Vec<Artifact>,
}

impl Finding {
    pub fn is_running(&self) -> bool {
        !self.procs.is_empty()
    }

    pub fn artifact_bytes(&self) -> u64 {
        self.artifacts.iter().map(|a| a.size_bytes).sum()
    }

    /// Artifacts we are willing to delete: the ones not shared with anything else.
    pub fn deletable_artifacts(&self) -> impl Iterator<Item = &Artifact> {
        self.artifacts.iter().filter(|a| !a.shared)
    }
}

/// A process that looks like an MCP server but matches no config we found.
/// These are the interesting ones: servers left running after their config was
/// edited away, or spawned by a client we do not know about.
#[derive(Debug, Clone)]
pub struct OrphanProc {
    pub proc: RunningProc,
    /// Best guess at the client that spawned it, from walking up the tree.
    pub likely_host: Option<String>,
}

/// A config file the sweep identified as MCP-shaped but could not attribute to
/// a known client. Surfaced so the user can see coverage gaps.
#[derive(Debug, Clone)]
pub struct UnknownConfig {
    pub path: PathBuf,
    pub container_keys: Vec<String>,
    pub server_count: usize,
}

/// A detected tool that is relevant but declares no MCP servers, e.g. Ollama,
/// which is an inference runtime with no MCP support of its own. Shown so the
/// user knows we looked rather than wondering why it is missing.
#[derive(Debug, Clone)]
pub struct DetectedRuntime {
    pub name: String,
    pub detail: String,
    pub pids: Vec<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub findings: Vec<Finding>,
    pub orphans: Vec<OrphanProc>,
    pub unknown_configs: Vec<UnknownConfig>,
    pub runtimes: Vec<DetectedRuntime>,
    /// Files we tried to read and could not, with the reason.
    pub errors: Vec<String>,
    /// Config paths that existed and were parsed, for the coverage view.
    pub scanned_paths: Vec<PathBuf>,
    pub duration_ms: u128,
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if b == 0 {
        return "0 B".into();
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn human_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_tokens_round_trip() {
        let raw = "/Users/me/my~proj";
        let esc = escape_pointer_token(raw);
        assert_eq!(esc, "~1Users~1me~1my~0proj");
        assert_eq!(unescape_pointer_token(&esc), raw);
    }

    #[test]
    fn format_detection_treats_plain_json_as_json() {
        assert_eq!(
            ConfigFormat::from_path(Path::new("/a/mcp.json")),
            ConfigFormat::Json
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("/a/opencode.jsonc")),
            ConfigFormat::Jsonc
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("/a/config.yaml")),
            ConfigFormat::Yaml
        );
    }

    #[test]
    fn bytes_are_human_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(150 * 1024 * 1024), "150 MB");
    }
}
