//! Turning a config file into [`ServerEntry`] values.
//!
//! The approach is structural rather than schema-driven. Every supported
//! format is normalised into a `serde_json::Value`, then the whole document
//! tree is walked looking for *containers*: an object under a known key whose
//! members look like server definitions. That one rule covers every schema
//! difference we found in the wild without a special case per client:
//!
//! * `mcpServers` (most clients), `servers` (VS Code, Visual Studio),
//!   `context_servers` (Zed), `mcp` (OpenCode), `mcp_servers` (Codex TOML),
//!   `extensions` (Goose YAML).
//! * Arbitrary nesting, which is what makes `~/.claude.json`'s
//!   `projects.<abs path>.mcpServers` fall out for free.
//! * `command` as a string (most) or as an array (OpenCode).
//! * `cmd` as an alias for `command` (Goose).
//! * `url` / `httpUrl` (Gemini CLI) / `serverUrl` (Windsurf) for remotes.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::model::{
    ConfigFormat, Scope, ServerEntry, SourceLocation, Transport, escape_pointer_token,
};
use crate::registry::SERVER_MARKER_KEYS;

/// Read a config file and normalise it to a JSON value.
pub fn read_as_json(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_text(&text, ConfigFormat::from_path(path), path)
}

pub fn parse_text(text: &str, format: ConfigFormat, path: &Path) -> Result<Value> {
    match format {
        // Plain .json files frequently contain comments in practice, so both
        // go through the tolerant reader.
        ConfigFormat::Json | ConfigFormat::Jsonc => {
            let parsed: Value =
                jsonc_parser::parse_to_serde_value(text, &jsonc_parser::ParseOptions::default())
                    .with_context(|| format!("parsing {} as JSON", path.display()))?;
            Ok(parsed)
        }
        ConfigFormat::Yaml => serde_norway::from_str::<Value>(text)
            .with_context(|| format!("parsing {} as YAML", path.display())),
        ConfigFormat::Toml => {
            let t: toml::Value = toml::from_str(text)
                .with_context(|| format!("parsing {} as TOML", path.display()))?;
            Ok(toml_to_json(t))
        }
    }
}

fn toml_to_json(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}

/// A container of server definitions located inside a document.
#[derive(Debug, Clone)]
pub struct Container {
    /// JSON pointer to the container object itself.
    pub pointer: String,
    /// The key it was found under.
    pub key: String,
    pub servers: Map<String, Value>,
}

/// Walk the document and collect every server container.
pub fn find_containers(root: &Value, container_keys: &[String]) -> Vec<Container> {
    let mut out = Vec::new();
    walk(root, String::new(), container_keys, &mut out, 0);
    out
}

/// Deep configs exist (`~/.claude.json` nests per project), but unbounded
/// recursion on a hostile or huge file is not worth the risk.
const MAX_DEPTH: usize = 12;

fn walk(
    value: &Value,
    pointer: String,
    container_keys: &[String],
    out: &mut Vec<Container>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_pointer = format!("{pointer}/{}", escape_pointer_token(key));
                let is_container_key = container_keys.iter().any(|k| k == key);
                if is_container_key
                    && let Value::Object(servers) = child
                    && looks_like_server_map(servers)
                {
                    out.push(Container {
                        pointer: child_pointer.clone(),
                        key: key.clone(),
                        servers: servers.clone(),
                    });
                    // Do not descend into a confirmed container: its members
                    // are server definitions, and a server's own `env` or
                    // `args` must never be mistaken for a nested container.
                    continue;
                }
                walk(child, child_pointer, container_keys, out, depth + 1);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(
                    item,
                    format!("{pointer}/{i}"),
                    container_keys,
                    out,
                    depth + 1,
                );
            }
        }
        _ => {}
    }
}

/// True when at least one member looks like a server definition. This is the
/// test that keeps `"mcp": { "enabled": true }` from being read as a server
/// list while still accepting every real schema we know of.
pub fn looks_like_server_map(map: &Map<String, Value>) -> bool {
    if map.is_empty() {
        return false;
    }
    map.values().any(|v| match v {
        Value::Object(def) => SERVER_MARKER_KEYS.iter().any(|k| def.contains_key(*k)),
        _ => false,
    })
}

/// Extract the transport from a single server definition.
pub fn transport_of(def: &Map<String, Value>) -> Transport {
    // Remote first: a definition carrying a URL is remote regardless of what
    // else it holds.
    for key in ["url", "httpUrl", "serverUrl", "endpoint"] {
        if let Some(Value::String(url)) = def.get(key)
            && !url.trim().is_empty()
        {
            let declared = def
                .get("type")
                .or_else(|| def.get("transport"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            return if declared.contains("sse") {
                Transport::Sse { url: url.clone() }
            } else {
                Transport::Http { url: url.clone() }
            };
        }
    }

    // Local: `command` as a string, `command` as an array (OpenCode), or `cmd`
    // as an alias (Goose).
    let mut command = String::new();
    let mut args: Vec<String> = Vec::new();

    match def.get("command").or_else(|| def.get("cmd")) {
        Some(Value::String(s)) => command = s.clone(),
        Some(Value::Array(parts)) => {
            let mut it = parts.iter().filter_map(Value::as_str);
            if let Some(first) = it.next() {
                command = first.to_string();
            }
            args.extend(it.map(str::to_string));
        }
        _ => {}
    }

    if let Some(Value::Array(list)) = def.get("args") {
        // Non-string arguments are rendered rather than dropped: a port
        // number in a config is still part of the command line.
        args.extend(list.iter().map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }));
    }

    if command.is_empty() {
        Transport::Unknown
    } else {
        Transport::Stdio { command, args }
    }
}

/// Environment variables declared for a server.
pub fn env_of(def: &Map<String, Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for key in ["env", "environment", "envVars"] {
        if let Some(Value::Object(map)) = def.get(key) {
            for (k, v) in map {
                let rendered = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.insert(k.clone(), rendered);
            }
        }
    }
    out
}

/// Whether the client has this server switched off. Clients spell it several
/// ways, and `enabled`/`isActive` invert the meaning.
pub fn disabled_of(def: &Map<String, Value>) -> bool {
    for key in ["disabled", "isDisabled"] {
        if let Some(Value::Bool(true)) = def.get(key) {
            return true;
        }
    }
    for key in ["enabled", "isActive", "active"] {
        if let Some(Value::Bool(false)) = def.get(key) {
            return true;
        }
    }
    false
}

/// Goose lists its own built-in extensions alongside MCP servers. They are not
/// external servers and have nothing to kill or purge.
fn is_builtin_extension(def: &Map<String, Value>) -> bool {
    matches!(
        def.get("type").and_then(Value::as_str),
        Some("builtin") | Some("frontend")
    )
}

/// Parse one config file into entries. `client_id`/`client_name` describe the
/// attribution we are making; `scope` says which precedence slot it occupies.
pub fn entries_from_file(
    path: &Path,
    container_keys: &[String],
    client_id: &str,
    client_name: &str,
    scope: &Scope,
) -> Result<Vec<ServerEntry>> {
    let format = ConfigFormat::from_path(path);
    let root = read_as_json(path)?;
    if root.is_null() {
        return Ok(Vec::new());
    }
    Ok(entries_from_value(
        &root,
        path,
        format,
        container_keys,
        client_id,
        client_name,
        scope,
    ))
}

#[allow(clippy::too_many_arguments)]
pub fn entries_from_value(
    root: &Value,
    path: &Path,
    format: ConfigFormat,
    container_keys: &[String],
    client_id: &str,
    client_name: &str,
    scope: &Scope,
) -> Vec<ServerEntry> {
    let mut out = Vec::new();
    for container in find_containers(root, container_keys) {
        for (name, def) in &container.servers {
            let Value::Object(def_map) = def else {
                continue;
            };
            if !SERVER_MARKER_KEYS.iter().any(|k| def_map.contains_key(*k)) {
                continue;
            }
            if is_builtin_extension(def_map) {
                continue;
            }
            out.push(ServerEntry {
                name: name.clone(),
                client_id: client_id.to_string(),
                client_name: client_name.to_string(),
                scope: scope.clone(),
                transport: transport_of(def_map),
                env: env_of(def_map),
                disabled: disabled_of(def_map),
                source: SourceLocation {
                    path: path.to_path_buf(),
                    container_pointer: container.pointer.clone(),
                    container_key: container.key.clone(),
                    format,
                },
                raw: def.clone(),
            });
        }
    }
    out
}

/// Project roots named inside a config, e.g. the `projects` map in
/// `~/.claude.json`. Scanning these finds project-scoped configs without
/// crawling the whole filesystem.
pub fn harvest_project_roots(root: &Value, keys: &[String]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Value::Object(map) = root {
        for key in keys {
            if let Some(Value::Object(projects)) = map.get(key) {
                for name in projects.keys() {
                    let p = std::path::PathBuf::from(name);
                    if p.is_absolute() && p.is_dir() {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

/// Quick structural check used by the sweep before a full parse.
pub fn sniff_mcp_shaped(text: &str, format: ConfigFormat, path: &Path) -> Result<Vec<String>> {
    let root = parse_text(text, format, path)?;
    if root.is_null() {
        bail!("empty document");
    }
    let keys = crate::registry::DEFAULT_CONTAINER_KEYS
        .iter()
        .map(|s| (*s).to_string())
        .collect::<Vec<_>>();
    Ok(find_containers(&root, &keys)
        .into_iter()
        .map(|c| c.key)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn keys() -> Vec<String> {
        crate::registry::DEFAULT_CONTAINER_KEYS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn parse(text: &str) -> Vec<ServerEntry> {
        let v = parse_text(text, ConfigFormat::Json, Path::new("/t/mcp.json")).unwrap();
        entries_from_value(
            &v,
            &PathBuf::from("/t/mcp.json"),
            ConfigFormat::Json,
            &keys(),
            "test",
            "Test",
            &Scope::User,
        )
    }

    #[test]
    fn finds_standard_mcp_servers() {
        let entries = parse(
            r#"{"mcpServers":{"rea":{"command":"npx","args":["-y","rea-agents@6.7.0","mcp"]}}}"#,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "rea");
        assert_eq!(
            entries[0].transport.command_line().unwrap(),
            "npx -y rea-agents@6.7.0 mcp"
        );
        assert_eq!(entries[0].source.container_pointer, "/mcpServers");
    }

    #[test]
    fn finds_vscode_servers_key() {
        let entries = parse(r#"{"servers":{"time":{"command":"uvx","args":["mcp-server-time"]}}}"#);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source.container_key, "servers");
    }

    #[test]
    fn finds_zed_context_servers() {
        let entries = parse(r#"{"context_servers":{"gh":{"command":"gh-mcp","args":[]}}}"#);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source.container_key, "context_servers");
    }

    #[test]
    fn tolerates_comments_and_trailing_commas() {
        let entries = parse(
            r#"{
            // VS Code ships commented config
            "servers": {
                "fs": { "command": "npx", "args": ["-y", "@mcp/fs"], },
            },
        }"#,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "fs");
    }

    #[test]
    fn finds_nested_per_project_servers() {
        // The ~/.claude.json shape.
        let entries = parse(
            r#"{
              "numStartups": 3,
              "mcpServers": { "global": { "command": "a" } },
              "projects": {
                "/Users/me/proj": { "mcpServers": { "local": { "command": "b" } } }
              }
            }"#,
        );
        let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["global", "local"]);
        let local = entries.iter().find(|e| e.name == "local").unwrap();
        assert_eq!(
            local.source.container_pointer,
            "/projects/~1Users~1me~1proj/mcpServers"
        );
    }

    #[test]
    fn opencode_command_array_is_split() {
        let entries = parse(r#"{"mcp":{"x":{"type":"local","command":["bun","x","srv"]}}}"#);
        assert_eq!(
            entries[0].transport,
            Transport::Stdio {
                command: "bun".into(),
                args: vec!["x".into(), "srv".into()]
            }
        );
    }

    #[test]
    fn remote_keys_are_recognised_per_client_dialect() {
        for (key, text) in [
            ("url", r#"{"mcpServers":{"r":{"url":"https://a/mcp"}}}"#),
            (
                "httpUrl",
                r#"{"mcpServers":{"r":{"httpUrl":"https://a/mcp"}}}"#,
            ),
            (
                "serverUrl",
                r#"{"mcpServers":{"r":{"serverUrl":"https://a/mcp"}}}"#,
            ),
        ] {
            let entries = parse(text);
            assert_eq!(entries.len(), 1, "{key} should parse");
            assert!(
                matches!(entries[0].transport, Transport::Http { .. }),
                "{key} should be http"
            );
        }
    }

    #[test]
    fn sse_type_is_distinguished_from_http() {
        let entries = parse(r#"{"mcpServers":{"r":{"url":"https://a/sse","type":"sse"}}}"#);
        assert!(matches!(entries[0].transport, Transport::Sse { .. }));
    }

    #[test]
    fn disabled_spellings_all_register() {
        for text in [
            r#"{"mcpServers":{"a":{"command":"x","disabled":true}}}"#,
            r#"{"mcpServers":{"a":{"command":"x","enabled":false}}}"#,
            r#"{"mcpServers":{"a":{"command":"x","isActive":false}}}"#,
        ] {
            assert!(parse(text)[0].disabled, "should be disabled: {text}");
        }
        assert!(!parse(r#"{"mcpServers":{"a":{"command":"x"}}}"#)[0].disabled);
    }

    #[test]
    fn server_env_is_captured() {
        let entries =
            parse(r#"{"mcpServers":{"a":{"command":"x","env":{"TOKEN":"abc","B":"2"}}}}"#);
        assert_eq!(entries[0].env.len(), 2);
        assert_eq!(entries[0].env.get("TOKEN").unwrap(), "abc");
    }

    #[test]
    fn env_block_is_not_mistaken_for_a_nested_container() {
        // A server named "servers" with an env var would break a naive walker.
        let entries =
            parse(r#"{"mcpServers":{"a":{"command":"x","env":{"PATH":"/bin"},"args":["--mcp"]}}}"#);
        assert_eq!(entries.len(), 1, "only the real server should be found");
    }

    #[test]
    fn non_server_objects_are_rejected() {
        assert!(parse(r#"{"mcp":{"enabled":true,"logLevel":"debug"}}"#).is_empty());
        assert!(parse(r#"{"servers":{}}"#).is_empty());
        assert!(parse(r#"{"unrelated":{"a":{"command":"x"}}}"#).is_empty());
    }

    #[test]
    fn goose_yaml_extensions_parse_with_cmd_alias() {
        let yaml = "extensions:\n  fetch:\n    type: stdio\n    cmd: uvx\n    args: [mcp-server-fetch]\n    enabled: true\n  developer:\n    type: builtin\n    enabled: true\n";
        let v = parse_text(yaml, ConfigFormat::Yaml, Path::new("/t/config.yaml")).unwrap();
        let entries = entries_from_value(
            &v,
            &PathBuf::from("/t/config.yaml"),
            ConfigFormat::Yaml,
            &keys(),
            "goose",
            "Goose",
            &Scope::User,
        );
        // `developer` is a built-in, not an MCP server, so it is skipped.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "fetch");
        assert_eq!(
            entries[0].transport,
            Transport::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-fetch".into()]
            }
        );
    }

    #[test]
    fn codex_toml_mcp_servers_parse() {
        let t = "[mcp_servers.fs]\ncommand = \"npx\"\nargs = [\"-y\", \"@mcp/fs\"]\n";
        let v = parse_text(t, ConfigFormat::Toml, Path::new("/t/config.toml")).unwrap();
        let entries = entries_from_value(
            &v,
            &PathBuf::from("/t/config.toml"),
            ConfigFormat::Toml,
            &keys(),
            "codex",
            "Codex CLI",
            &Scope::User,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "fs");
    }

    #[test]
    fn sniff_reports_container_keys() {
        let found = sniff_mcp_shaped(
            r#"{"mcpServers":{"a":{"command":"x"}}}"#,
            ConfigFormat::Json,
            Path::new("/t/x.json"),
        )
        .unwrap();
        assert_eq!(found, vec!["mcpServers".to_string()]);
    }

    #[test]
    fn harvests_project_roots_that_exist() {
        // Use a directory guaranteed to exist, plus one that does not.
        let tmp = std::env::temp_dir();
        let v = serde_json::json!({
            "projects": {
                tmp.to_string_lossy(): {},
                "/definitely/not/here/xyzzy": {}
            }
        });
        let roots = harvest_project_roots(&v, &["projects".to_string()]);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], tmp);
    }
}
