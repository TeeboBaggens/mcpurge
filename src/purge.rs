//! Backing up, unregistering and deleting.
//!
//! Three rules govern this module:
//!
//! 1. **Nothing is touched before a backup exists.** Every operation writes the
//!    original files and a manifest under `~/.mcpurge/backups/<timestamp>/`
//!    first, so [`undo`] can put everything back.
//! 2. **Config edits are surgical.** JSON/JSONC is spliced by byte range and
//!    TOML is edited through `toml_edit`, so comments, key order and
//!    formatting survive. A config full of hand-written notes and API keys is
//!    not worth reformatting to delete one entry.
//! 3. **Files go to the trash, not to `unlink`.** Permanent deletion exists
//!    but has to be asked for, and shared artifacts are never deleted at all.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::model::{Artifact, ConfigFormat, ServerEntry, unescape_pointer_token};

/// Root for backups and the operation log.
pub fn state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mcpurge")
}

pub fn backups_root() -> PathBuf {
    state_dir().join("backups")
}

fn timestamp() -> String {
    jiff::ZonedDateTime::now()
        .strftime("%Y-%m-%dT%H-%M-%S")
        .to_string()
}

/// What a purge should do.
#[derive(Debug, Clone)]
pub struct PurgeOptions {
    /// Remove the entry from the agent's config file.
    pub remove_config: bool,
    /// Delete the server's non-shared files.
    pub delete_files: bool,
    /// Bypass the trash. Off by default, and never applied to shared paths.
    pub permanent: bool,
}

impl Default for PurgeOptions {
    fn default() -> Self {
        Self {
            remove_config: true,
            delete_files: false,
            permanent: false,
        }
    }
}

/// One recorded backup of one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub original: PathBuf,
    pub copy: PathBuf,
}

/// Enough information to reverse an operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub timestamp: String,
    pub server_name: String,
    pub client: String,
    pub config_backups: Vec<BackupRecord>,
    /// Files moved to the trash. The OS trash is the only way back; we record
    /// them so the report can say exactly what went where.
    pub trashed: Vec<PathBuf>,
    pub permanently_deleted: Vec<PathBuf>,
    pub dir: PathBuf,
}

/// Outcome of a purge, for the UI log.
#[derive(Debug, Clone, Default)]
pub struct PurgeReport {
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
    pub manifest: Option<Manifest>,
}

impl PurgeReport {
    fn step(&mut self, s: impl Into<String>) {
        self.steps.push(s.into());
    }
    fn warn(&mut self, s: impl Into<String>) {
        self.warnings.push(s.into());
    }
}

/// Copy a file into a backup session directory, keeping its path recognisable.
fn back_up(path: &Path, session: &Path) -> Result<BackupRecord> {
    std::fs::create_dir_all(session)
        .with_context(|| format!("creating backup directory {}", session.display()))?;
    let flat = path
        .to_string_lossy()
        .replace(['/', '\\'], "_")
        .trim_start_matches('_')
        .to_string();
    let copy = session.join(flat);
    std::fs::copy(path, &copy)
        .with_context(|| format!("backing up {} to {}", path.display(), copy.display()))?;
    Ok(BackupRecord {
        original: path.to_path_buf(),
        copy,
    })
}

// ---------------------------------------------------------------- JSON edit ---

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Remove `pointer` from a JSON/JSONC document by splicing out its byte range.
///
/// Comments, key order, indentation and trailing commas in the rest of the
/// file are preserved, because the surrounding text is never re-serialised.
pub fn remove_pointer_from_json(text: &str, pointer: &str) -> Result<String> {
    use jsonc_parser::ast::{ObjectPropName, Value as AstValue};
    use jsonc_parser::{CollectOptions, ParseOptions, parse_to_ast};

    // Comments need no special handling: the file is edited by splicing a byte
    // range out of the original text, so everything we do not touch survives
    // exactly as written.
    let parsed = parse_to_ast(text, &CollectOptions::default(), &ParseOptions::default())
        .context("parsing config for editing")?;

    let root = parsed.value.ok_or_else(|| anyhow!("config is empty"))?;

    let tokens: Vec<String> = pointer
        .split('/')
        .filter(|t| !t.is_empty())
        .map(unescape_pointer_token)
        .collect();
    if tokens.is_empty() {
        bail!("refusing to remove the document root");
    }
    let (last, parents) = tokens.split_last().unwrap();

    // Walk down to the object that owns the property.
    let mut current = &root;
    for token in parents {
        current = match current {
            AstValue::Object(obj) => obj
                .properties
                .iter()
                .find(|p| match &p.name {
                    ObjectPropName::String(s) => s.value == *token,
                    ObjectPropName::Word(w) => w.value == *token,
                })
                .map(|p| &p.value)
                .ok_or_else(|| anyhow!("config no longer contains '{token}'"))?,
            AstValue::Array(arr) => {
                let idx: usize = token
                    .parse()
                    .map_err(|_| anyhow!("'{token}' is not an array index"))?;
                arr.elements
                    .get(idx)
                    .ok_or_else(|| anyhow!("index {idx} is out of range"))?
            }
            _ => bail!("'{token}' is not a container"),
        };
    }

    let AstValue::Object(owner) = current else {
        bail!("the server's parent is not an object");
    };
    let index = owner
        .properties
        .iter()
        .position(|p| match &p.name {
            ObjectPropName::String(s) => s.value == *last,
            ObjectPropName::Word(w) => w.value == *last,
        })
        .ok_or_else(|| anyhow!("'{last}' is not in the config any more"))?;

    let prop = &owner.properties[index];
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut start = prop.range.start;
    let mut end = prop.range.end;

    // Take the comma that follows, if any, plus the rest of that line.
    let mut j = end;
    while j < len && is_ws(bytes[j]) {
        j += 1;
    }
    let mut took_trailing_comma = false;
    if j < len && bytes[j] == b',' {
        end = j + 1;
        took_trailing_comma = true;
        let mut k = end;
        while k < len && matches!(bytes[k], b' ' | b'\t' | b'\r') {
            k += 1;
        }
        if k < len && bytes[k] == b'\n' {
            k += 1;
        }
        end = k;
    }

    if took_trailing_comma {
        // Drop this line's indentation so no blank gap is left behind.
        let mut s = start;
        while s > 0 && matches!(bytes[s - 1], b' ' | b'\t') {
            s -= 1;
        }
        if s == 0 || bytes[s - 1] == b'\n' {
            start = s;
        }
    } else {
        // Last property in the object: the comma before it must go instead,
        // otherwise the result ends in a dangling separator.
        let mut i = start;
        while i > 0 && is_ws(bytes[i - 1]) {
            i -= 1;
        }
        if i > 0 && bytes[i - 1] == b',' {
            start = i - 1;
        } else {
            let mut s = start;
            while s > 0 && matches!(bytes[s - 1], b' ' | b'\t') {
                s -= 1;
            }
            if s == 0 || bytes[s - 1] == b'\n' {
                start = s;
            }
        }
    }

    let mut out = String::with_capacity(len);
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);

    // Never hand back something that will not parse.
    jsonc_parser::parse_to_serde_value::<serde_json::Value>(
        &out,
        &jsonc_parser::ParseOptions::default(),
    )
    .context("the edit produced invalid JSON; the file was left untouched")?;
    Ok(out)
}

/// Remove a server from a TOML document, preserving comments and layout.
pub fn remove_pointer_from_toml(text: &str, pointer: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut =
        text.parse().context("parsing TOML config for editing")?;

    let tokens: Vec<String> = pointer
        .split('/')
        .filter(|t| !t.is_empty())
        .map(unescape_pointer_token)
        .collect();
    let (last, parents) = tokens
        .split_last()
        .ok_or_else(|| anyhow!("refusing to remove the document root"))?;

    let mut item: &mut toml_edit::Item = doc.as_item_mut();
    for token in parents {
        item = item
            .as_table_like_mut()
            .and_then(|t| t.get_mut(token))
            .ok_or_else(|| anyhow!("config no longer contains '{token}'"))?;
    }
    let table = item
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("the server's parent is not a TOML table"))?;
    if table.remove(last).is_none() {
        bail!("'{last}' is not in the config any more");
    }
    Ok(doc.to_string())
}

/// Remove a server from a YAML document.
///
/// YAML has no format-preserving editor in this dependency set, so the file is
/// re-serialised and comments are lost. The caller warns about this, and the
/// backup is what makes it recoverable.
pub fn remove_pointer_from_yaml(text: &str, pointer: &str) -> Result<String> {
    let mut root: serde_json::Value =
        serde_norway::from_str(text).context("parsing YAML config for editing")?;

    let tokens: Vec<String> = pointer
        .split('/')
        .filter(|t| !t.is_empty())
        .map(unescape_pointer_token)
        .collect();
    let (last, parents) = tokens
        .split_last()
        .ok_or_else(|| anyhow!("refusing to remove the document root"))?;

    let mut current = &mut root;
    for token in parents {
        current = current
            .get_mut(token)
            .ok_or_else(|| anyhow!("config no longer contains '{token}'"))?;
    }
    let obj = current
        .as_object_mut()
        .ok_or_else(|| anyhow!("the server's parent is not a YAML mapping"))?;
    if obj.shift_remove(last).is_none() {
        bail!("'{last}' is not in the config any more");
    }
    serde_norway::to_string(&root).context("re-serialising YAML config")
}

/// Apply the right editor for the file's format.
pub fn remove_pointer(text: &str, pointer: &str, format: ConfigFormat) -> Result<String> {
    match format {
        ConfigFormat::Json | ConfigFormat::Jsonc => remove_pointer_from_json(text, pointer),
        ConfigFormat::Toml => remove_pointer_from_toml(text, pointer),
        ConfigFormat::Yaml => remove_pointer_from_yaml(text, pointer),
    }
}

/// Write a file by writing a sibling temp file and renaming it, so a crash
/// cannot leave a half-written config behind.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.mcpurge-tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config".into())
    ));
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

// ------------------------------------------------------------------ purging ---

/// Unregister a server and optionally delete its files.
pub fn purge(
    entry: &ServerEntry,
    artifacts: &[Artifact],
    opts: &PurgeOptions,
) -> Result<PurgeReport> {
    let mut report = PurgeReport::default();
    let session = backups_root().join(timestamp());

    let mut manifest = Manifest {
        timestamp: timestamp(),
        server_name: entry.name.clone(),
        client: entry.client_name.clone(),
        config_backups: Vec::new(),
        trashed: Vec::new(),
        permanently_deleted: Vec::new(),
        dir: session.clone(),
    };

    if opts.remove_config {
        let path = &entry.source.path;
        if !path.is_file() {
            bail!("config file {} is gone", path.display());
        }
        let record = back_up(path, &session)?;
        report.step(format!(
            "backed up {} to {}",
            path.display(),
            record.copy.display()
        ));
        manifest.config_backups.push(record);

        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let pointer = entry.source.entry_pointer(&entry.name);
        let edited = remove_pointer(&text, &pointer, entry.source.format)?;

        if !entry.source.format.supports_surgical_edit() {
            report.warn(format!(
                "{} was re-serialised, so comments and layout may have changed; the backup has the original",
                path.display()
            ));
        }
        write_atomic(path, &edited)?;
        report.step(format!(
            "removed '{}' from {}{}",
            entry.name,
            entry.source.container_key,
            if entry.source.container_pointer == format!("/{}", entry.source.container_key) {
                String::new()
            } else {
                format!(" at {}", entry.source.container_pointer)
            }
        ));
    }

    if opts.delete_files {
        for artifact in artifacts {
            if artifact.shared {
                report.warn(format!(
                    "left {} alone: {}",
                    artifact.path.display(),
                    artifact.note
                ));
                continue;
            }
            if !artifact.path.exists() {
                continue;
            }
            if opts.permanent {
                let result = if artifact.path.is_dir() {
                    std::fs::remove_dir_all(&artifact.path)
                } else {
                    std::fs::remove_file(&artifact.path)
                };
                match result {
                    Ok(()) => {
                        report.step(format!(
                            "permanently deleted {} ({})",
                            artifact.path.display(),
                            crate::model::human_bytes(artifact.size_bytes)
                        ));
                        manifest.permanently_deleted.push(artifact.path.clone());
                    }
                    Err(e) => {
                        report.warn(format!("could not delete {}: {e}", artifact.path.display()))
                    }
                }
            } else {
                match trash::delete(&artifact.path) {
                    Ok(()) => {
                        report.step(format!(
                            "moved {} ({}) to the trash",
                            artifact.path.display(),
                            crate::model::human_bytes(artifact.size_bytes)
                        ));
                        manifest.trashed.push(artifact.path.clone());
                    }
                    Err(e) => {
                        report.warn(format!("could not trash {}: {e}", artifact.path.display()))
                    }
                }
            }
        }
    }

    // Record the manifest last, so it describes only what actually happened.
    if !manifest.config_backups.is_empty()
        || !manifest.trashed.is_empty()
        || !manifest.permanently_deleted.is_empty()
    {
        std::fs::create_dir_all(&session).ok();
        let manifest_path = session.join("manifest.json");
        if let Ok(json) = serde_json::to_string_pretty(&manifest) {
            if let Err(e) = std::fs::write(&manifest_path, json) {
                report.warn(format!("could not write the undo manifest: {e}"));
            } else {
                report.step(format!("undo manifest at {}", manifest_path.display()));
            }
        }
        report.manifest = Some(manifest);
    }

    Ok(report)
}

/// Restore the config files recorded in a manifest.
///
/// Trashed files are not restored automatically: the OS trash is the user's to
/// manage, and silently moving files back out of it would be its own surprise.
pub fn undo(manifest: &Manifest) -> Result<PurgeReport> {
    let mut report = PurgeReport::default();
    for record in &manifest.config_backups {
        if !record.copy.is_file() {
            report.warn(format!("backup {} is missing", record.copy.display()));
            continue;
        }
        let text = std::fs::read_to_string(&record.copy)
            .with_context(|| format!("reading backup {}", record.copy.display()))?;
        write_atomic(&record.original, &text)?;
        report.step(format!("restored {}", record.original.display()));
    }
    if !manifest.trashed.is_empty() {
        report.warn(format!(
            "{} file(s) are in the trash and were not restored: {}",
            manifest.trashed.len(),
            manifest
                .trashed
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !manifest.permanently_deleted.is_empty() {
        report.warn(format!(
            "{} file(s) were permanently deleted and cannot be restored",
            manifest.permanently_deleted.len()
        ));
    }
    Ok(report)
}

/// Every undo manifest on disk, newest first.
pub fn list_manifests() -> Vec<Manifest> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(backups_root()) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path().join("manifest.json");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(m) = serde_json::from_str::<Manifest>(&text)
        {
            out.push(m);
        }
    }
    out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn removed(text: &str, pointer: &str) -> String {
        remove_pointer_from_json(text, pointer).expect("edit should succeed")
    }

    #[test]
    fn removes_a_middle_entry_and_leaves_valid_json() {
        let text = r#"{
  "mcpServers": {
    "a": { "command": "x" },
    "b": { "command": "y" },
    "c": { "command": "z" }
  }
}"#;
        let out = removed(text, "/mcpServers/b");
        assert!(!out.contains("\"b\""));
        assert!(out.contains("\"a\"") && out.contains("\"c\""));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn removes_the_last_entry_without_leaving_a_dangling_comma() {
        let text = r#"{
  "mcpServers": {
    "a": { "command": "x" },
    "b": { "command": "y" }
  }
}"#;
        let out = removed(text, "/mcpServers/b");
        assert!(!out.contains("\"b\""));
        // Would be a syntax error if the separator survived.
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"].as_object().unwrap().len(), 1);
        assert!(!out.contains(",\n  }"), "dangling comma left: {out}");
    }

    #[test]
    fn removes_the_only_entry_leaving_an_empty_container() {
        let text = r#"{"mcpServers":{"a":{"command":"x"}}}"#;
        let out = removed(text, "/mcpServers/a");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["mcpServers"].as_object().unwrap().is_empty());
    }

    #[test]
    fn preserves_comments_and_other_settings() {
        let text = r#"{
  // Keep this note.
  "telemetry": false,
  "servers": {
    /* leading block comment */
    "keep": { "command": "k" },
    "drop": { "command": "d" }
  },
  "other": 1
}"#;
        let out = removed(text, "/servers/drop");
        assert!(out.contains("// Keep this note."), "comment lost: {out}");
        assert!(out.contains("/* leading block comment */"));
        assert!(out.contains("\"telemetry\": false"));
        assert!(out.contains("\"other\": 1"));
        assert!(!out.contains("\"drop\""));
    }

    #[test]
    fn preserves_unrelated_env_values_verbatim() {
        // An API key in a sibling entry must come through byte-identical.
        let text = r#"{
  "mcpServers": {
    "keep": { "command": "k", "env": { "TOKEN": "sk-abc123==/+" } },
    "drop": { "command": "d" }
  }
}"#;
        let out = removed(text, "/mcpServers/drop");
        assert!(out.contains(r#""TOKEN": "sk-abc123==/+""#), "got {out}");
    }

    #[test]
    fn removes_a_deeply_nested_entry() {
        // The ~/.claude.json shape, with a path key needing pointer escaping.
        let text = r#"{
  "numStartups": 4,
  "projects": {
    "/Users/me/proj": {
      "mcpServers": {
        "keep": { "command": "k" },
        "drop": { "command": "d" }
      }
    }
  }
}"#;
        let out = removed(text, "/projects/~1Users~1me~1proj/mcpServers/drop");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let servers = v["projects"]["/Users/me/proj"]["mcpServers"]
            .as_object()
            .unwrap();
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("keep"));
        assert_eq!(v["numStartups"], 4);
    }

    #[test]
    fn tolerates_trailing_commas_in_the_source() {
        let text = "{\n  \"servers\": {\n    \"a\": { \"command\": \"x\" },\n    \"b\": { \"command\": \"y\" },\n  },\n}";
        let out = removed(text, "/servers/b");
        assert!(!out.contains("\"b\""));
        // Re-parsed with the tolerant reader, since the source was JSONC.
        let v: serde_json::Value =
            jsonc_parser::parse_to_serde_value(&out, &jsonc_parser::ParseOptions::default())
                .unwrap();
        assert_eq!(v["servers"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn missing_entries_are_an_error_not_a_silent_success() {
        let text = r#"{"mcpServers":{"a":{"command":"x"}}}"#;
        let err = remove_pointer_from_json(text, "/mcpServers/nope").unwrap_err();
        assert!(err.to_string().contains("not in the config"), "{err}");
        let err = remove_pointer_from_json(text, "/nope/a").unwrap_err();
        assert!(err.to_string().contains("no longer contains"), "{err}");
    }

    #[test]
    fn refuses_to_remove_the_root() {
        assert!(remove_pointer_from_json(r#"{"a":1}"#, "/").is_err());
    }

    #[test]
    fn toml_edit_preserves_comments() {
        let text = "# Codex config\nmodel = \"gpt-5\"\n\n[mcp_servers.keep]\ncommand = \"k\"\n\n[mcp_servers.drop]\ncommand = \"d\"\n";
        let out = remove_pointer_from_toml(text, "/mcp_servers/drop").unwrap();
        assert!(out.contains("# Codex config"), "comment lost: {out}");
        assert!(out.contains("model = \"gpt-5\""));
        assert!(out.contains("[mcp_servers.keep]"));
        assert!(!out.contains("drop"), "got {out}");
        // Still valid TOML.
        toml::from_str::<toml::Value>(&out).unwrap();
    }

    #[test]
    fn yaml_removal_works_even_though_it_reformats() {
        let text = "extensions:\n  keep:\n    cmd: k\n    type: stdio\n  drop:\n    cmd: d\n    type: stdio\n";
        let out = remove_pointer_from_yaml(text, "/extensions/drop").unwrap();
        let v: serde_json::Value = serde_norway::from_str(&out).unwrap();
        let ext = v["extensions"].as_object().unwrap();
        assert_eq!(ext.len(), 1);
        assert!(ext.contains_key("keep"));
    }

    #[test]
    fn format_dispatch_picks_the_right_editor() {
        let json = remove_pointer(
            r#"{"mcpServers":{"a":{"command":"x"},"b":{"command":"y"}}}"#,
            "/mcpServers/b",
            ConfigFormat::Json,
        )
        .unwrap();
        assert!(!json.contains("\"b\""));

        let toml_out = remove_pointer(
            "[mcp_servers.a]\ncommand = \"x\"\n[mcp_servers.b]\ncommand = \"y\"\n",
            "/mcp_servers/b",
            ConfigFormat::Toml,
        )
        .unwrap();
        assert!(!toml_out.contains('b'), "got {toml_out}");
    }

    #[test]
    fn backup_and_restore_round_trip() {
        let dir = std::env::temp_dir().join(format!("mcpurge-backup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("mcp.json");
        let original = r#"{"mcpServers":{"a":{"command":"x"},"b":{"command":"y"}}}"#;
        std::fs::write(&cfg, original).unwrap();

        let session = dir.join("session");
        let record = back_up(&cfg, &session).unwrap();
        assert!(record.copy.is_file());

        // Mutate, then restore from the manifest.
        write_atomic(&cfg, r#"{"mcpServers":{}}"#).unwrap();
        assert_ne!(std::fs::read_to_string(&cfg).unwrap(), original);

        let manifest = Manifest {
            timestamp: "t".into(),
            server_name: "b".into(),
            client: "Test".into(),
            config_backups: vec![record],
            trashed: vec![],
            permanently_deleted: vec![],
            dir: session,
        };
        undo(&manifest).unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("mcpurge-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("c.json");
        write_atomic(&f, "{}").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("mcpurge-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_options_are_conservative() {
        let o = PurgeOptions::default();
        assert!(o.remove_config);
        assert!(!o.delete_files, "file deletion must be opt-in");
        assert!(!o.permanent, "permanent deletion must be opt-in");
    }

    #[test]
    fn shared_artifacts_are_never_deleted() {
        let dir = std::env::temp_dir().join(format!("mcpurge-shared-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("mcp.json");
        std::fs::write(&cfg, r#"{"mcpServers":{"a":{"command":"x"}}}"#).unwrap();
        let guarded = dir.join("precious");
        std::fs::write(&guarded, "keep me").unwrap();

        let entry = ServerEntry {
            name: "a".into(),
            client_id: "test".into(),
            client_name: "Test".into(),
            scope: crate::model::Scope::User,
            transport: crate::model::Transport::Stdio {
                command: "x".into(),
                args: vec![],
            },
            env: Default::default(),
            disabled: false,
            source: crate::model::SourceLocation {
                path: cfg.clone(),
                container_pointer: "/mcpServers".into(),
                container_key: "mcpServers".into(),
                format: ConfigFormat::Json,
            },
            raw: serde_json::json!({"command":"x"}),
        };
        let artifacts = vec![Artifact {
            path: guarded.clone(),
            size_bytes: 7,
            kind: crate::model::ArtifactKind::Other,
            shared: true,
            note: "shared cache".into(),
        }];
        let opts = PurgeOptions {
            remove_config: true,
            delete_files: true,
            permanent: true,
        };
        let report = purge(&entry, &artifacts, &opts).unwrap();
        assert!(guarded.is_file(), "a shared artifact must survive");
        assert!(report.warnings.iter().any(|w| w.contains("left")));

        std::fs::remove_dir_all(&dir).ok();
        if let Some(m) = report.manifest {
            std::fs::remove_dir_all(m.dir).ok();
        }
    }
}
