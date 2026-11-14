//! The client registry: a data-driven list of where to look for MCP configs.
//!
//! The registry is deliberately only a set of hints. Published config paths
//! conflict between sources and move between releases, so `scan::discover`
//! also sweeps for configs by content shape. If a path here is wrong, the
//! sweep still finds the file; the registry just makes attribution nicer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Bundled registry. A user copy overrides it wholesale.
const BUILTIN: &str = include_str!("../assets/clients.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Confirmed against vendor docs or observed on a real machine.
    Verified,
    /// Agreed on by secondary sources only.
    Reported,
    /// Unconfirmed; probed anyway because probing is cheap. An entry with no
    /// stated confidence is treated as a guess.
    #[default]
    Guess,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::Verified => "verified",
            Confidence::Reported => "reported",
            Confidence::Guess => "guess",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub confidence: Confidence,
    /// Object keys that hold a map of server-name -> server-definition.
    #[serde(default)]
    pub container_keys: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub paths_macos: Vec<String>,
    #[serde(default)]
    pub paths_windows: Vec<String>,
    #[serde(default)]
    pub paths_linux: Vec<String>,
    /// Paths relative to a project root.
    #[serde(default)]
    pub project_paths: Vec<String>,
    /// Substrings that identify this client's own process, used to attribute a
    /// server process to the client that spawned it.
    #[serde(default)]
    pub process_hints: Vec<String>,
    #[serde(default)]
    pub notes: String,
    /// False when the client has no editable config file (Cherry Studio).
    #[serde(default = "default_true")]
    pub editable: bool,
    /// Keys whose object members are absolute project paths worth scanning,
    /// e.g. `projects` in ~/.claude.json.
    #[serde(default)]
    pub harvest_project_keys: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Client {
    /// Config path patterns for the host OS, still containing globs.
    pub fn patterns_for_host(&self) -> Vec<String> {
        let mut out = self.paths.clone();
        if cfg!(target_os = "macos") {
            out.extend(self.paths_macos.iter().cloned());
        } else if cfg!(target_os = "windows") {
            out.extend(self.paths_windows.iter().cloned());
        } else {
            out.extend(self.paths_linux.iter().cloned());
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Runtime {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub process_hints: Vec<String>,
    #[serde(default)]
    pub default_port: Option<u16>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Registry {
    #[serde(default, rename = "client")]
    pub clients: Vec<Client>,
    #[serde(default, rename = "runtime")]
    pub runtimes: Vec<Runtime>,
    /// Set when the list came from the user's own file rather than the build.
    #[serde(skip)]
    pub source: RegistrySource,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RegistrySource {
    #[default]
    Builtin,
    User(PathBuf),
}

impl Registry {
    /// Load the user registry if present, else the bundled one.
    pub fn load() -> Result<Self> {
        if let Some(path) = user_registry_path()
            && path.is_file()
        {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut reg: Registry =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            reg.source = RegistrySource::User(path);
            return Ok(reg);
        }
        Self::builtin()
    }

    pub fn builtin() -> Result<Self> {
        let mut reg: Registry =
            toml::from_str(BUILTIN).context("parsing the bundled client registry")?;
        reg.source = RegistrySource::Builtin;
        Ok(reg)
    }

    pub fn client(&self, id: &str) -> Option<&Client> {
        self.clients.iter().find(|c| c.id == id)
    }

    /// Every container key any client uses, which is what the shape detector
    /// matches on. Includes the well-known set so an unregistered tool using a
    /// standard key is still recognised.
    pub fn all_container_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = DEFAULT_CONTAINER_KEYS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        for c in &self.clients {
            for k in &c.container_keys {
                if !keys.iter().any(|existing| existing == k) {
                    keys.push(k.clone());
                }
            }
        }
        keys
    }

    /// Map a config path back to the client that owns it, by matching the
    /// registry's expanded patterns. Used to attribute sweep hits.
    pub fn attribute_path(&self, path: &Path) -> Option<&Client> {
        let p = path.to_string_lossy().replace('\\', "/");
        let mut best: Option<(&Client, usize)> = None;
        for c in &self.clients {
            for pat in c.patterns_for_host().iter().chain(c.project_paths.iter()) {
                let expanded = expand_placeholders(pat).replace('\\', "/");
                // Compare on the glob pattern's literal prefix so that
                // `.../globalStorage/*/settings/x.json` still attributes.
                let literal: String = expanded.split(['*', '?']).next().unwrap_or("").to_string();
                if literal.len() > 1 && p.starts_with(&literal) {
                    let score = literal.len();
                    if best.is_none_or(|(_, s)| score > s) {
                        best = Some((c, score));
                    }
                } else if let Some(file) = Path::new(&expanded).file_name()
                    && let Some(actual) = path.file_name()
                    && file == actual
                    && expanded.starts_with('.')
                {
                    // Project-relative pattern such as `.cursor/mcp.json`.
                    let rel = expanded.trim_start_matches("./");
                    if p.ends_with(rel) {
                        let score = rel.len();
                        if best.is_none_or(|(_, s)| score > s) {
                            best = Some((c, score));
                        }
                    }
                }
            }
        }
        best.map(|(c, _)| c)
    }
}

/// Container keys recognised even when no registry entry mentions them.
pub const DEFAULT_CONTAINER_KEYS: &[&str] = &[
    "mcpServers",
    "mcp_servers",
    "servers",
    "context_servers",
    "mcp",
    "extensions",
];

/// Keys inside a server definition that mark it as a server definition.
pub const SERVER_MARKER_KEYS: &[&str] = &[
    "command",
    "cmd",
    "args",
    "url",
    "httpUrl",
    "serverUrl",
    "endpoint",
    "type",
    "transport",
];

/// Where a user-supplied registry override lives.
pub fn user_registry_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mcpurge").join("clients.toml"))
}

/// Resolve `~`, `$CONFIG`, `$CONFIG_LOCAL`, `$DATA` and `$XDG_CONFIG`.
pub fn expand_placeholders(pattern: &str) -> String {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
    let config_local = dirs::config_local_dir().unwrap_or_else(|| config.clone());
    let data = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));
    let xdg = home.join(".config");

    let mut s = pattern.to_string();
    for (needle, value) in [
        ("$CONFIG_LOCAL", config_local),
        ("$CONFIG", config),
        ("$XDG_CONFIG", xdg),
        ("$DATA", data),
        ("$HOME", home.clone()),
    ] {
        if s.contains(needle) {
            s = s.replace(needle, &value.to_string_lossy());
        }
    }
    if let Some(rest) = s.strip_prefix("~/") {
        s = home.join(rest).to_string_lossy().into_owned();
    } else if s == "~" {
        s = home.to_string_lossy().into_owned();
    }
    s
}

/// Expand a pattern into the files that actually exist.
pub fn expand_existing(pattern: &str) -> Vec<PathBuf> {
    let expanded = expand_placeholders(pattern);
    if !expanded.contains('*') && !expanded.contains('?') {
        let p = PathBuf::from(&expanded);
        return if p.is_file() { vec![p] } else { Vec::new() };
    }
    match glob::glob(&expanded) {
        Ok(paths) => paths
            .filter_map(Result::ok)
            .filter(|p| p.is_file())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Every existing config file the registry knows about, with its client.
/// A single file can serve several clients (`.mcp.json` is read by Claude Code
/// and VS Code alike), so the value is a list.
pub fn registry_config_files(reg: &Registry) -> BTreeMap<PathBuf, Vec<String>> {
    let mut out: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for client in &reg.clients {
        for pattern in client.patterns_for_host() {
            for path in expand_existing(&pattern) {
                out.entry(path).or_default().push(client.id.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_parses() {
        let reg = Registry::builtin().expect("bundled registry must parse");
        assert!(
            reg.clients.len() > 20,
            "expected a broad client list, got {}",
            reg.clients.len()
        );
        assert!(!reg.runtimes.is_empty(), "runtimes should be listed");
    }

    #[test]
    fn registry_ids_are_unique() {
        let reg = Registry::builtin().unwrap();
        let mut ids: Vec<&str> = reg.clients.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate client id in registry");
    }

    #[test]
    fn every_client_has_a_container_key_or_is_uneditable() {
        let reg = Registry::builtin().unwrap();
        for c in &reg.clients {
            assert!(
                !c.container_keys.is_empty(),
                "client {} declares no container keys",
                c.id
            );
        }
    }

    #[test]
    fn container_keys_include_the_odd_ones_out() {
        let reg = Registry::builtin().unwrap();
        let keys = reg.all_container_keys();
        // The three schemas that trip people up.
        for k in [
            "mcpServers",
            "servers",
            "context_servers",
            "mcp",
            "mcp_servers",
        ] {
            assert!(keys.iter().any(|x| x == k), "missing container key {k}");
        }
    }

    #[test]
    fn placeholders_expand_to_absolute_paths() {
        let out = expand_placeholders("~/.cursor/mcp.json");
        assert!(Path::new(&out).is_absolute(), "got {out}");
        assert!(!out.contains('~'));

        let cfg = expand_placeholders("$CONFIG/Claude/claude_desktop_config.json");
        assert!(Path::new(&cfg).is_absolute(), "got {cfg}");
        assert!(!cfg.contains('$'));
    }

    #[test]
    fn cherry_studio_is_marked_uneditable() {
        let reg = Registry::builtin().unwrap();
        let cherry = reg.client("cherry-studio").expect("cherry-studio entry");
        assert!(
            !cherry.editable,
            "Cherry Studio stores MCP in redux-persist state, so it must be flagged uneditable"
        );
    }

    #[test]
    fn claude_code_harvests_project_keys() {
        let reg = Registry::builtin().unwrap();
        let cc = reg.client("claude-code").unwrap();
        assert!(cc.harvest_project_keys.iter().any(|k| k == "projects"));
    }
}
