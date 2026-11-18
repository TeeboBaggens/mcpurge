//! The egui front end.
//!
//! Scans, kills and purges all run on background threads and report back over
//! a channel, so the window never blocks: a sweep touches thousands of files
//! and a kill waits on SIGTERM, and neither should freeze the UI.
//!
//! Destructive actions are deliberately awkward. Nothing happens from a single
//! click on a row: killing and removing both go through a confirmation that
//! spells out the exact files and config keys involved, and environment values
//! stay masked until asked for, because MCP configs are full of API tokens.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::menu::{MenuState, NativeMenu};
use crate::model::{
    ArtifactKind, Finding, MatchConfidence, ScanResult, human_bytes, human_duration,
};
use crate::proc::{self, KillReport};
use crate::purge::{self, Manifest, PurgeOptions, PurgeReport};
use crate::registry::{Registry, RegistrySource};
use crate::scan::{self, ScanOptions};
use crate::theme::{self, ThemeMode};
use crate::update;

/// Launch the GUI.
pub fn run() -> anyhow::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([920.0, 520.0])
        .with_title("mcpurge");

    // The title bar and task switcher on Linux and Windows. macOS ignores this
    // and reads mcpurge.app/Contents/Resources/mcpurge.icns instead, which
    // scripts/make-icon.sh renders from the same assets/icon.svg. A window
    // that cannot decode its own icon is still a working window, so a failure
    // here is dropped rather than turned into a startup error.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-window.png"))
    {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "mcpurge",
        options,
        Box::new(|cc| Ok(Box::new(McpurgeApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("could not start the window: {e}"))
}

// ------------------------------------------------------------- background ---

/// Messages from worker threads back to the UI.
enum Msg {
    Progress(String),
    ScanDone(Box<ScanResult>),
    Killed {
        label: String,
        report: KillReport,
    },
    /// One removal run, which may have covered several entries. Carrying
    /// them together keeps the spinner on for the whole run and lets the table
    /// drop every removed row in one go.
    Purged {
        items: Vec<PurgedItem>,
    },
    Undone {
        result: Result<PurgeReport, String>,
    },
    /// The answer from a release check: `None` means this build is current.
    UpdateChecked {
        result: Result<Option<Box<update::Available>>, String>,
        /// Automatic checks stay quiet when there is nothing to say; a check
        /// the user asked for always answers.
        asked_for: bool,
    },
    /// A verified download, replaced over this executable.
    UpdateInstalled {
        result: Result<PathBuf, String>,
        version: String,
    },
}

/// One entry's outcome inside a removal run.
struct PurgedItem {
    label: String,
    /// The entry's key, so a successful removal can be reflected immediately.
    key: String,
    result: Result<PurgeReport, String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Servers,
    Orphans,
    Coverage,
}

/// One menu item's meaning, independent of which bar it was clicked in.
///
/// macOS draws the menus itself (see [`crate::menu`]) and everywhere else egui
/// draws them, so without this the two bars would be two implementations of
/// the same list, free to drift apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuAction {
    Undo,
    SelectAll,
    SelectNone,
    Find,
    ClearSearch,
    Theme(ThemeMode),
    ShowTab(Tab),
    ToggleDisabled,
    ToggleRevealEnv,
    ToggleSweep,
    ToggleMeasure,
    Rescan,
    CheckForUpdates,
    ToggleAutoUpdateCheck,
    OpenBackups,
    ShowRegistryPath,
    ProjectPage,
    About,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Running,
    Stopped,
    Local,
    Remote,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Running => "running",
            Filter::Stopped => "stopped",
            Filter::Local => "local (stdio)",
            Filter::Remote => "remote (http/sse)",
        }
    }
    const ALL: [Filter; 5] = [
        Filter::All,
        Filter::Running,
        Filter::Stopped,
        Filter::Local,
        Filter::Remote,
    ];
}

/// A confirmation the user has to answer before anything changes.
enum Confirm {
    Kill {
        label: String,
        pids: Vec<u32>,
        tree: Vec<u32>,
    },
    KillOrphan {
        pid: u32,
        label: String,
        tree: Vec<u32>,
    },
    Purge {
        key: String,
    },
    /// Several entries at once. Kept separate from [`Confirm::Purge`] because
    /// the question is a different one: the dialog has to account for several
    /// config files, several clients, and entries that cannot be edited.
    PurgeMany {
        keys: Vec<String>,
    },
    KillMany {
        /// One label per entry, so the dialog can name them all.
        labels: Vec<String>,
        pids: Vec<u32>,
        tree: Vec<u32>,
    },
}

struct LogLine {
    text: String,
    kind: LogKind,
}

#[derive(PartialEq, Eq)]
enum LogKind {
    Info,
    Good,
    Warn,
}

/// What the server table's context menu needs, copied out of a [`Finding`]
/// before the row's cells borrow it, so the menu closure is free to take
/// `&mut self`.
struct RowActions {
    key: String,
    label: String,
    pids: Vec<u32>,
    tree: Vec<u32>,
}

pub struct McpurgeApp {
    registry: Registry,
    result: ScanResult,
    scanning: bool,
    progress: String,
    tab: Tab,
    filter: Filter,
    search: String,
    show_disabled: bool,
    sweep: bool,
    measure_artifacts: bool,
    extra_roots: Vec<PathBuf>,
    selected: Option<String>,
    /// Entries ticked for a bulk action. Keys, not indices, so the set
    /// survives a rescan that reorders or drops rows.
    checked: BTreeSet<String>,
    confirm: Option<Confirm>,
    busy: Option<String>,
    /// Opt-ins for the purge dialog, remembered between uses.
    purge_delete_files: bool,
    purge_permanent: bool,
    allow_sigkill: bool,
    reveal_env: bool,
    log: Vec<LogLine>,
    last_manifest: Option<Manifest>,
    theme: ThemeMode,
    /// Set by Edit > Find, consumed by the search box on the next frame.
    focus_search: bool,
    about_open: bool,
    /// Coverage tab: hide clients that declared nothing.
    coverage_only_hits: bool,
    /// Look for a new release on launch. The whole network side of mcpurge is
    /// behind this one flag: with it off the app talks to nothing at all
    /// unless the user picks "Check for updates now".
    auto_update_check: bool,
    /// True while a check or a download is in flight.
    update_busy: bool,
    /// A release newer than this build, waiting for the user to accept it.
    update_available: Option<Box<update::Available>>,
    /// Set once the automatic check has run, so it happens once per launch
    /// rather than once per frame.
    update_checked: bool,
    /// Set after a successful install: the new binary is in place but this
    /// process is still the old one.
    update_installed: Option<String>,
    /// The platform's own menu bar, where there is one. `None` means the menus
    /// are drawn inside the window instead.
    native_menu: Option<NativeMenu>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

/// The sentences behind the one-word labels.
///
/// Every column heading and every verdict the table prints is a term of art
/// that mcpurge invented, so each one says, on hover, what it is and why it is
/// there. They live together because they are prose, not logic, and because a
/// term explained two different ways in two places is worse than one left
/// unexplained.
mod help {
    pub const SERVER: &str = "the name the client gave this MCP server in its config. The same \
                              server can appear more than once under different names.";
    pub const CLIENT: &str = "the app that declares this server: an editor, a CLI agent or a \
                              desktop chat app. Removing an entry removes it from this client \
                              only.";
    pub const SCOPE: &str = "who the entry applies to - your whole user account, one project, or \
                             the whole machine. Hover a row to see the file it came from.";
    pub const TRANSPORT: &str = "how the client reaches the server. Only stdio servers run as a \
                                 local process, so only they can be stopped.";
    pub const STATE: &str = "whether a process matching this entry is running right now, with the \
                             pids mcpurge matched to it.";
    pub const ON_DISK: &str = "how much of this server's files on disk mcpurge would be able to \
                               delete. It counts only what is safe to delete, which is why it can \
                               be smaller than the server's real footprint.";

    /// The answer to "what does protected mean?", which is the question the
    /// word begs the first time it is seen.
    pub const PROTECTED: &str = "protected: mcpurge found files for this server but will not \
                                 delete any of them.\n\nA path is protected when deleting it \
                                 would take something other than this server with it: your own \
                                 work handed to the server as an argument, a system or \
                                 package-manager directory, an installed application, or a cache \
                                 shared by every tool that uses the same package manager.\n\nOpen \
                                 the server to see each path and the rule that kept it.";
    pub const NO_FILES: &str = "mcpurge found no files on disk for this server. A remote server \
                                has none, and a stopped stdio server may have been launched from \
                                somewhere this scan did not reach.";
    pub const KEPT: &str = "kept: this path is never deleted, whatever is ticked in the removal \
                            dialog, because something other than this server depends on it.";
    pub const RUNNING: &str = "a live process matched this entry. Stopping it signals the whole \
                               process tree, not just this pid.";
    pub const STOPPED: &str = "no running process matched this entry. The config still declares \
                               it, so the client will start it again when it next needs it.";
    pub const DISABLED: &str = "the client's own config switches this server off. The entry is \
                                still there, so it is still listed and can still be removed.";
    pub const UPDATE_CHECK: &str = "ask GitHub whether a newer mcpurge has been released. \
                                    Nothing is downloaded or installed without you saying so, \
                                    and nothing is installed at all unless it carries a \
                                    signature from the mcpurge release key.";
    pub const UPDATE_AUTO: &str = "look for a new release once per launch. This is the only \
                                   network request mcpurge ever makes: with it off, the app \
                                   talks to nothing unless you ask it to check.";
    pub const UPDATE_INSTALL: &str = "The download is checked against the mcpurge release \
                                      signing key, which is built into this binary, before \
                                      anything is written. A download that is not signed by \
                                      that key is discarded.\n\nThe signatures are minisign \
                                      ones, so you can check a release by hand with the \
                                      minisign tool; the full public key is in the project's \
                                      README.";
    pub const TAB_SERVERS: &str = "every MCP server declared by a config file on this machine.";
    pub const TAB_ORPHANS: &str = "processes that look like MCP servers but match no config \
                                   entry: left running after their config was edited away, or \
                                   started by a client mcpurge does not recognise.";
    pub const TAB_COVERAGE: &str = "what mcpurge looked at, and what it could not account for. \
                                    Start here if a server you expected is missing.";
}

/// Storage keys for the preferences worth remembering between runs.
const THEME_KEY: &str = "mcpurge.theme";
const AUTO_UPDATE_KEY: &str = "mcpurge.auto_update_check";

impl McpurgeApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let registry = Registry::load().unwrap_or_else(|e| {
            eprintln!("falling back to the built-in registry: {e}");
            Registry::builtin().expect("the bundled registry must parse")
        });
        let mut app = Self::with_registry(registry);
        app.theme = cc
            .storage
            .and_then(|s| eframe::get_value::<ThemeMode>(s, THEME_KEY))
            .unwrap_or_default();
        app.auto_update_check = cc
            .storage
            .and_then(|s| eframe::get_value::<bool>(s, AUTO_UPDATE_KEY))
            // On by default: a tool that edits other programs' configs should
            // not quietly go stale. It is one menu item to turn off, and off
            // means no network traffic at all.
            .unwrap_or(true);
        theme::apply(&cc.egui_ctx, app.theme);
        // Built here rather than in `with_registry` because it needs a live
        // window: on macOS this hands a real NSMenu to the running app.
        app.native_menu = NativeMenu::install(&cc.egui_ctx);
        app.info(format!(
            "registry: {} ({} clients, {} runtimes)",
            match &app.registry.source {
                RegistrySource::Builtin => "built in".to_string(),
                RegistrySource::User(p) => p.display().to_string(),
            },
            app.registry.clients.len(),
            app.registry.runtimes.len()
        ));
        app.start_scan(&cc.egui_ctx);
        app
    }

    /// Build the app without an eframe context and without starting a scan.
    /// Keeping this separate is what lets the panels be rendered headlessly in
    /// tests, where there is no window and no `eframe::Frame` to hand out.
    fn with_registry(registry: Registry) -> Self {
        let (tx, rx) = channel();
        Self {
            registry,
            result: ScanResult::default(),
            scanning: false,
            progress: String::new(),
            tab: Tab::Servers,
            filter: Filter::All,
            search: String::new(),
            show_disabled: true,
            sweep: true,
            measure_artifacts: true,
            extra_roots: Vec::new(),
            selected: None,
            checked: BTreeSet::new(),
            confirm: None,
            busy: None,
            purge_delete_files: false,
            purge_permanent: false,
            allow_sigkill: true,
            reveal_env: false,
            log: Vec::new(),
            last_manifest: None,
            theme: ThemeMode::default(),
            focus_search: false,
            about_open: false,
            coverage_only_hits: false,
            auto_update_check: true,
            update_busy: false,
            update_available: None,
            update_checked: false,
            update_installed: None,
            native_menu: None,
            tx,
            rx,
        }
    }

    fn info(&mut self, text: impl Into<String>) {
        self.log.push(LogLine {
            text: text.into(),
            kind: LogKind::Info,
        });
    }
    fn good(&mut self, text: impl Into<String>) {
        self.log.push(LogLine {
            text: text.into(),
            kind: LogKind::Good,
        });
    }
    fn warn(&mut self, text: impl Into<String>) {
        self.log.push(LogLine {
            text: text.into(),
            kind: LogKind::Warn,
        });
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        self.progress = "starting".into();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let registry = self.registry.clone();
        let opts = ScanOptions {
            sweep: self.sweep,
            extra_project_roots: self.extra_roots.clone(),
            measure_artifacts: self.measure_artifacts,
        };
        std::thread::spawn(move || {
            let tx_progress = tx.clone();
            let ctx_progress = ctx.clone();
            let result = scan::scan(&registry, &opts, move |m| {
                let _ = tx_progress.send(Msg::Progress(m.to_string()));
                ctx_progress.request_repaint();
            });
            let _ = tx.send(Msg::ScanDone(Box::new(result)));
            ctx.request_repaint();
        });
    }

    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Progress(p) => self.progress = p,
                Msg::ScanDone(result) => {
                    self.scanning = false;
                    self.progress.clear();
                    let t = scan::totals(&result);
                    self.info(format!(
                        "scan: {} server(s), {} running, {} client(s), {} orphan(s), {} in {} ms",
                        t.servers,
                        t.running,
                        t.clients,
                        t.orphans,
                        human_bytes(t.bytes),
                        result.duration_ms
                    ));
                    for e in result.errors.iter().take(5) {
                        self.warn(e.clone());
                    }
                    // Keep the selection and the tick marks only for servers
                    // that still exist.
                    if let Some(sel) = &self.selected
                        && !result.findings.iter().any(|f| &f.entry.key() == sel)
                    {
                        self.selected = None;
                    }
                    let keys: BTreeSet<String> =
                        result.findings.iter().map(|f| f.entry.key()).collect();
                    self.checked.retain(|k| keys.contains(k));
                    self.result = *result;
                }
                Msg::Killed { label, report } => {
                    self.busy = None;
                    let summary = report.summary();
                    if report.all_exited() {
                        self.good(format!("{label}: {summary}"));
                    } else {
                        self.warn(format!("{label}: {summary}"));
                    }
                    for o in &report.outcomes {
                        if !o.exited {
                            self.warn(format!("  pid {} ({}): {}", o.pid, o.name, o.detail));
                        }
                    }
                    // Anything confirmed dead stops being shown as running
                    // straight away. Waiting for a rescan would leave the table
                    // asserting something we know to be false.
                    let gone: Vec<u32> = report
                        .outcomes
                        .iter()
                        .filter(|o| o.exited)
                        .map(|o| o.pid)
                        .collect();
                    self.forget_pids(&gone);
                }
                Msg::Purged { items } => {
                    self.busy = None;
                    let mut removed: Vec<String> = Vec::new();
                    for item in items {
                        let PurgedItem { label, key, result } = item;
                        match result {
                            Ok(report) => {
                                for s in &report.steps {
                                    self.good(format!("{label}: {s}"));
                                }
                                for w in &report.warnings {
                                    self.warn(format!("{label}: {w}"));
                                }
                                if let Some(m) = report.manifest {
                                    self.last_manifest = Some(m);
                                }
                                removed.push(key);
                            }
                            Err(e) => self.warn(format!("{label}: {e}")),
                        }
                    }
                    if !removed.is_empty() {
                        let n = self.forget_entries(&removed);
                        self.info(format!(
                            "{n} server(s) removed from the list. Rescan to re-read the \
                             configs from disk."
                        ));
                    }
                }
                Msg::Undone { result } => {
                    self.busy = None;
                    match result {
                        Ok(report) => {
                            for s in &report.steps {
                                self.good(format!("undo: {s}"));
                            }
                            for w in &report.warnings {
                                self.warn(format!("undo: {w}"));
                            }
                            self.last_manifest = None;
                        }
                        Err(e) => self.warn(format!("undo: {e}")),
                    }
                }
                Msg::UpdateChecked { result, asked_for } => {
                    self.update_busy = false;
                    match result {
                        Ok(Some(available)) => {
                            self.info(format!("update available: {available}"));
                            self.update_available = Some(available);
                        }
                        // An automatic check that finds nothing says nothing.
                        // Only a check the user asked for owes them an answer.
                        Ok(None) if asked_for => {
                            self.good(format!(
                                "mcpurge {} is the latest release",
                                update::current_version()
                            ));
                        }
                        Ok(None) => {}
                        Err(e) if asked_for => self.warn(format!("update check: {e}")),
                        // A failed background check is a log line, not an
                        // interruption: the machine may simply be offline.
                        Err(e) => self.info(format!("update check: {e}")),
                    }
                }
                Msg::UpdateInstalled { result, version } => {
                    self.update_busy = false;
                    match result {
                        Ok(path) => {
                            self.good(format!(
                                "mcpurge {version} installed to {}. Restart to use it.",
                                path.display()
                            ));
                            self.update_available = None;
                            self.update_installed = Some(version);
                        }
                        Err(e) => self.warn(format!("update: {e}")),
                    }
                }
            }
        }
    }

    /// Drop entries whose config was just removed, so the table reflects the
    /// removal at once. The alternative is a full rescan, which takes long
    /// enough on a real machine to look like nothing happened.
    fn forget_entries(&mut self, keys: &[String]) -> usize {
        let drop: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
        let before = self.result.findings.len();
        self.result
            .findings
            .retain(|f| !drop.contains(f.entry.key().as_str()));
        self.checked.retain(|k| !drop.contains(k.as_str()));
        if let Some(sel) = &self.selected
            && drop.contains(sel.as_str())
        {
            self.selected = None;
        }
        before - self.result.findings.len()
    }

    /// Forget processes we have just confirmed dead, everywhere they appear.
    fn forget_pids(&mut self, pids: &[u32]) {
        if pids.is_empty() {
            return;
        }
        let dead: BTreeSet<u32> = pids.iter().copied().collect();
        for f in &mut self.result.findings {
            f.procs.retain(|p| !dead.contains(&p.pid));
            for p in &mut f.procs {
                p.descendants.retain(|d| !dead.contains(d));
            }
        }
        self.result.orphans.retain(|o| !dead.contains(&o.proc.pid));
    }

    fn visible(&self) -> Vec<&Finding> {
        let needle = self.search.trim().to_ascii_lowercase();
        self.result
            .findings
            .iter()
            .filter(|f| {
                if !self.show_disabled && f.entry.disabled {
                    return false;
                }
                match self.filter {
                    Filter::All => true,
                    Filter::Running => f.is_running(),
                    Filter::Stopped => !f.is_running(),
                    Filter::Local => f.entry.transport.is_local(),
                    Filter::Remote => !f.entry.transport.is_local(),
                }
            })
            .filter(|f| {
                if needle.is_empty() {
                    return true;
                }
                f.entry.name.to_ascii_lowercase().contains(&needle)
                    || f.entry.client_name.to_ascii_lowercase().contains(&needle)
                    || f.entry
                        .transport
                        .command_line()
                        .map(|c| c.to_ascii_lowercase().contains(&needle))
                        .unwrap_or(false)
                    || f.entry
                        .source
                        .path
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(&needle)
            })
            .collect()
    }

    fn finding(&self, key: &str) -> Option<&Finding> {
        self.result.findings.iter().find(|f| f.entry.key() == key)
    }

    /// How many other entries claim the same pid. The same server registered
    /// in two clients is indistinguishable at the process level, and the user
    /// needs to know that stopping it affects both.
    fn pid_claims(&self) -> BTreeMap<u32, usize> {
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for f in &self.result.findings {
            for p in &f.procs {
                *counts.entry(p.pid).or_insert(0) += 1;
            }
        }
        counts
    }

    fn spawn_kill(&mut self, ctx: &egui::Context, label: String, pids: Vec<u32>) {
        self.busy = Some(format!("stopping {label}"));
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let allow_sigkill = self.allow_sigkill;
        std::thread::spawn(move || {
            let mut combined = KillReport::default();
            for pid in pids {
                let r = proc::kill_tree(pid, allow_sigkill);
                combined.outcomes.extend(r.outcomes);
            }
            let _ = tx.send(Msg::Killed {
                label,
                report: combined,
            });
            ctx.request_repaint();
        });
    }

    fn spawn_purge(&mut self, ctx: &egui::Context, keys: Vec<String>) {
        // Everything the worker needs is copied out first: it must not hold a
        // borrow on the app while the UI keeps drawing.
        let jobs: Vec<_> = keys
            .iter()
            .filter_map(|key| {
                let f = self.finding(key)?;
                Some((
                    key.clone(),
                    format!("{} ({})", f.entry.name, f.entry.client_name),
                    f.entry.clone(),
                    f.artifacts.clone(),
                ))
            })
            .collect();
        if jobs.is_empty() {
            return;
        }

        let opts = PurgeOptions {
            remove_config: true,
            delete_files: self.purge_delete_files,
            permanent: self.purge_permanent,
        };
        self.busy = Some(if jobs.len() == 1 {
            format!("removing {}", jobs[0].1)
        } else {
            format!("removing {} servers", jobs.len())
        });
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // Sequentially, because two entries can live in one file and each
            // removal re-reads it. Running them in parallel would have one
            // overwrite the other's edit.
            let items = jobs
                .into_iter()
                .map(|(key, label, entry, artifacts)| PurgedItem {
                    label,
                    key,
                    result: purge::purge(&entry, &artifacts, &opts).map_err(|e| e.to_string()),
                })
                .collect();
            let _ = tx.send(Msg::Purged { items });
            ctx.request_repaint();
        });
    }

    /// Every entry currently ticked, in the order the table shows them.
    fn checked_keys(&self) -> Vec<String> {
        self.visible()
            .iter()
            .map(|f| f.entry.key())
            .filter(|k| self.checked.contains(k))
            .collect()
    }

    /// Ask GitHub whether there is a newer release, on a thread so the window
    /// does not wait on the network.
    fn spawn_update_check(&mut self, ctx: &egui::Context, asked_for: bool) {
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        self.update_checked = true;
        if asked_for {
            self.info("checking for a new release");
        }
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = update::check()
                .map(|found| found.map(Box::new))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::UpdateChecked { result, asked_for });
            ctx.request_repaint();
        });
    }

    /// Download the release, check its signature, and only then put it in
    /// place. The verification is inside `update::download_verified`, so this
    /// never holds unverified bytes and a path to write them to.
    fn spawn_update_install(&mut self, ctx: &egui::Context, available: update::Available) {
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        self.info(format!("downloading mcpurge {}", available.version));
        let version = available.version.to_string();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = update::download_verified(&available)
                .and_then(|bytes| update::install(&bytes))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::UpdateInstalled { result, version });
            ctx.request_repaint();
        });
    }

    fn spawn_undo(&mut self, ctx: &egui::Context) {
        let Some(manifest) = self.last_manifest.clone() else {
            return;
        };
        self.busy = Some("restoring".into());
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = purge::undo(&manifest).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Undone { result });
            ctx.request_repaint();
        });
    }
}

// --------------------------------------------------------------- rendering ---

fn status_dot(ui: &mut egui::Ui, finding: &Finding) {
    let (color, tip) = if finding.entry.disabled {
        (
            egui::Color32::from_gray(110),
            "disabled in the client's config",
        )
    } else if finding.is_running() {
        (egui::Color32::from_rgb(64, 190, 120), "running")
    } else {
        (egui::Color32::from_gray(150), "not running")
    };
    let size = egui::vec2(12.0, 12.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let center = rect.center();
    if finding.is_running() {
        ui.painter().circle_filled(center, 5.0, color);
    } else {
        ui.painter().circle_stroke(center, 4.5, (1.5, color));
    }
    response.on_hover_text(tip);
}

/// A number with a caption under it. Used for the Coverage tab's summary,
/// where the counts are the point and the prose is support.
fn stat_tile(
    ui: &mut egui::Ui,
    value: String,
    caption: &str,
    sub: &str,
    tint: Option<egui::Color32>,
) {
    ui.group(|ui| {
        ui.vertical(|ui| {
            ui.set_min_width(118.0);
            let mut big = egui::RichText::new(value).size(26.0).strong();
            if let Some(c) = tint {
                big = big.color(c);
            }
            ui.label(big);
            ui.label(caption);
            if !sub.is_empty() {
                ui.weak(sub);
            }
        });
    });
}

/// A labelled divider between sections of a long page.
fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(16.0);
    ui.label(egui::RichText::new(title).size(15.0).strong());
    ui.separator();
}

fn confidence_badge(ui: &mut egui::Ui, c: MatchConfidence) {
    let (text, color) = match c {
        MatchConfidence::Strong => ("strong", egui::Color32::from_rgb(64, 190, 120)),
        MatchConfidence::Probable => ("probable", egui::Color32::from_rgb(220, 180, 70)),
        MatchConfidence::Loose => ("loose", egui::Color32::from_rgb(200, 120, 90)),
    };
    ui.colored_label(color, format!("[{text}]"))
        .on_hover_text(c.explain());
}

impl eframe::App for McpurgeApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, THEME_KEY, &self.theme);
        eframe::set_value(storage, AUTO_UPDATE_KEY, &self.auto_update_check);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        self.pump_menu(&ui.ctx().clone());

        // Panels are nested inside the root Ui and claim space in the order
        // they are shown, so the edges come first and the table last.
        self.top_bar(ui);
        self.bottom_bar(ui);

        match self.tab {
            Tab::Servers => {
                self.detail_panel(ui);
                self.servers_table(ui);
            }
            Tab::Orphans => self.orphans_panel(ui),
            Tab::Coverage => self.coverage_panel(ui),
        }

        // Cloning the context is cheap (it is an Arc) and releases the borrow
        // on `ui` so the dialog can take `&mut self`.
        let ctx = ui.ctx().clone();
        self.about_window(&ctx);
        self.update_window(&ctx);
        self.confirm_dialog(&ctx);

        // Once per launch, and only if asked for in advance. Deferred to the
        // first frame rather than done in the constructor so the window is up
        // before anything touches the network.
        if self.auto_update_check && !self.update_checked {
            self.spawn_update_check(&ctx, false);
        }
    }
}

impl McpurgeApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();
        egui::Panel::top("top").show(ui, |ui| {
            // Nothing to draw when the platform owns the menu bar.
            if self.native_menu.is_none() {
                self.menu_bar(ui);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("mcpurge");
                ui.separator();

                let rescan = ui.add_enabled(
                    !self.scanning && self.busy.is_none(),
                    egui::Button::new("Rescan"),
                );
                if rescan.clicked() {
                    self.start_scan(ctx);
                }
                if self.scanning {
                    ui.spinner();
                    ui.label(&self.progress);
                } else if let Some(busy) = self.busy.clone() {
                    ui.spinner();
                    ui.label(busy);
                } else {
                    let t = scan::totals(&self.result);
                    ui.label(format!(
                        "{} servers · {} running · {} clients · {} reclaimable",
                        t.servers,
                        t.running,
                        t.clients,
                        human_bytes(t.bytes)
                    ));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.last_manifest.is_some()
                        && ui
                            .add_enabled(
                                self.busy.is_none(),
                                egui::Button::new("Undo last removal"),
                            )
                            .on_hover_text("restore the config files from the last backup")
                            .clicked()
                    {
                        self.spawn_undo(ctx);
                    }
                });
            });

            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Servers, "Servers")
                    .on_hover_text(help::TAB_SERVERS);
                ui.selectable_value(
                    &mut self.tab,
                    Tab::Orphans,
                    format!("Orphans ({})", self.result.orphans.len()),
                )
                .on_hover_text(help::TAB_ORPHANS);
                ui.selectable_value(&mut self.tab, Tab::Coverage, "Coverage")
                    .on_hover_text(help::TAB_COVERAGE);
                ui.separator();

                if self.tab == Tab::Servers {
                    // The combo's own text is its value ("all", "running"), which
                    // says nothing about what it filters, so it gets a visible
                    // label and is wired to it for screen readers.
                    let filter_label = ui.label("filter");
                    let filter = egui::ComboBox::from_id_salt("filter")
                        .selected_text(self.filter.label())
                        .show_ui(ui, |ui| {
                            for f in Filter::ALL {
                                ui.selectable_value(&mut self.filter, f, f.label());
                            }
                        });
                    filter.response.labelled_by(filter_label.id);
                    let search = ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("search name, client, command or path")
                            .desired_width(240.0),
                    );
                    if std::mem::take(&mut self.focus_search) {
                        search.request_focus();
                    }
                    ui.checkbox(&mut self.show_disabled, "show disabled");
                }

                ui.separator();
                ui.checkbox(&mut self.sweep, "deep sweep").on_hover_text(
                    "also search the filesystem for MCP-shaped configs, which finds clients \
                         the registry does not know about. Slower.",
                );
                ui.checkbox(&mut self.measure_artifacts, "measure sizes")
                    .on_hover_text("total up each server's files on disk");
            });
            ui.add_space(4.0);
        });
    }

    /// Edit / View / Window / Help, drawn inside the window.
    ///
    /// Only reached on platforms whose menus live in the window. On macOS the
    /// same list is in the system menu bar and this is never called.
    ///
    /// Everything here is reachable some other way as well; the menus exist so
    /// that it can be *found*, which a row of unlabelled checkboxes does not
    /// manage on its own.
    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let state = self.menu_state();
        let mut action = None;
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(
                        state.can_undo && state.can_act,
                        egui::Button::new("Undo last removal"),
                    )
                    .clicked()
                {
                    action = Some(MenuAction::Undo);
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(state.on_servers, egui::Button::new("Select all servers"))
                    .clicked()
                {
                    action = Some(MenuAction::SelectAll);
                    ui.close();
                }
                if ui
                    .add_enabled(state.has_checked, egui::Button::new("Deselect all"))
                    .clicked()
                {
                    action = Some(MenuAction::SelectNone);
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(state.on_servers, egui::Button::new("Find\u{2026}"))
                    .clicked()
                {
                    action = Some(MenuAction::Find);
                    ui.close();
                }
                if ui
                    .add_enabled(state.has_search, egui::Button::new("Clear search"))
                    .clicked()
                {
                    action = Some(MenuAction::ClearSearch);
                    ui.close();
                }
            });

            ui.menu_button("View", |ui| {
                ui.label("Theme");
                for mode in ThemeMode::ALL {
                    if ui
                        .radio(state.theme == mode, mode.label())
                        .on_hover_text(match mode {
                            ThemeMode::System => "match the desktop's light or dark setting",
                            ThemeMode::Light => "greige paper, dark bark ink",
                            ThemeMode::Dark => "warm brown, soft parchment ink",
                        })
                        .clicked()
                    {
                        action = Some(MenuAction::Theme(mode));
                        ui.close();
                    }
                }
                ui.separator();
                for (tab, label) in [
                    (Tab::Servers, "Servers"),
                    (Tab::Orphans, "Orphans"),
                    (Tab::Coverage, "Coverage"),
                ] {
                    if ui.selectable_label(state.tab == tab, label).clicked() {
                        action = Some(MenuAction::ShowTab(tab));
                        ui.close();
                    }
                }
                ui.separator();
                let mut show_disabled = state.show_disabled;
                if ui
                    .checkbox(&mut show_disabled, "Show disabled servers")
                    .changed()
                {
                    action = Some(MenuAction::ToggleDisabled);
                }
                let mut reveal_env = state.reveal_env;
                if ui
                    .checkbox(&mut reveal_env, "Reveal environment values")
                    .changed()
                {
                    action = Some(MenuAction::ToggleRevealEnv);
                }
                ui.separator();
                let mut sweep = state.sweep;
                if ui.checkbox(&mut sweep, "Deep sweep on next scan").changed() {
                    action = Some(MenuAction::ToggleSweep);
                }
                let mut measure = state.measure;
                if ui.checkbox(&mut measure, "Measure file sizes").changed() {
                    action = Some(MenuAction::ToggleMeasure);
                }
                if ui
                    .add_enabled(state.can_act, egui::Button::new("Rescan now"))
                    .clicked()
                {
                    action = Some(MenuAction::Rescan);
                    ui.close();
                }
            });

            ui.menu_button("Window", |ui| {
                if ui.button("Minimise").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    ui.close();
                }
                if ui.button("Zoom").clicked() {
                    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                    ui.close();
                }
                ui.separator();
                if ui.button("Close window").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });

            ui.menu_button("Help", |ui| {
                if ui
                    .add_enabled(
                        !state.update_busy,
                        egui::Button::new("Check for updates\u{2026}"),
                    )
                    .on_hover_text(help::UPDATE_CHECK)
                    .clicked()
                {
                    action = Some(MenuAction::CheckForUpdates);
                    ui.close();
                }
                let mut auto = state.auto_update_check;
                if ui
                    .checkbox(&mut auto, "Check for updates on launch")
                    .on_hover_text(help::UPDATE_AUTO)
                    .changed()
                {
                    action = Some(MenuAction::ToggleAutoUpdateCheck);
                }
                ui.separator();
                if ui
                    .button("Open backups folder")
                    .on_hover_text(purge::backups_root().display().to_string())
                    .clicked()
                {
                    action = Some(MenuAction::OpenBackups);
                    ui.close();
                }
                if crate::registry::user_registry_path().is_some()
                    && ui
                        .button("Show the client registry location")
                        .on_hover_text("copy the bundled registry here to add a client")
                        .clicked()
                {
                    action = Some(MenuAction::ShowRegistryPath);
                    ui.close();
                }
                ui.separator();
                if ui.button("Project page").clicked() {
                    action = Some(MenuAction::ProjectPage);
                    ui.close();
                }
                if ui.button("About mcpurge").clicked() {
                    action = Some(MenuAction::About);
                    ui.close();
                }
            });
        });
        if let Some(action) = action {
            self.run_menu_action(action, &ctx);
        }
    }

    /// Run whatever was clicked in the platform's menu bar, then tell it what
    /// the app now looks like. A no-op where there is no native menu.
    fn pump_menu(&mut self, ctx: &egui::Context) {
        let Some(actions) = self.native_menu.as_ref().map(NativeMenu::drain) else {
            return;
        };
        for action in actions {
            self.run_menu_action(action, ctx);
        }
        // After, not before: an action that changed a tick should show it on
        // this frame rather than the next one.
        let state = self.menu_state();
        if let Some(menu) = self.native_menu.as_ref() {
            menu.sync(&state);
        }
    }

    /// What the menus need to know about the app: what is ticked, what is
    /// greyed out.
    fn menu_state(&self) -> MenuState {
        MenuState {
            can_undo: self.last_manifest.is_some(),
            can_act: self.busy.is_none() && !self.scanning,
            on_servers: self.tab == Tab::Servers,
            has_checked: !self.checked.is_empty(),
            has_search: !self.search.is_empty(),
            theme: self.theme,
            tab: self.tab,
            show_disabled: self.show_disabled,
            reveal_env: self.reveal_env,
            sweep: self.sweep,
            measure: self.measure_artifacts,
            auto_update_check: self.auto_update_check,
            update_busy: self.update_busy,
        }
    }

    /// Carry out a menu item. One implementation for both menu bars.
    fn run_menu_action(&mut self, action: MenuAction, ctx: &egui::Context) {
        match action {
            MenuAction::Undo => self.spawn_undo(ctx),
            MenuAction::SelectAll => {
                self.checked = self.visible().iter().map(|f| f.entry.key()).collect();
            }
            MenuAction::SelectNone => self.checked.clear(),
            MenuAction::Find => {
                self.tab = Tab::Servers;
                self.focus_search = true;
            }
            MenuAction::ClearSearch => self.search.clear(),
            MenuAction::Theme(mode) => {
                self.theme = mode;
                theme::apply(ctx, mode);
            }
            MenuAction::ShowTab(tab) => self.tab = tab,
            MenuAction::ToggleDisabled => self.show_disabled = !self.show_disabled,
            MenuAction::ToggleRevealEnv => self.reveal_env = !self.reveal_env,
            MenuAction::ToggleSweep => self.sweep = !self.sweep,
            MenuAction::ToggleMeasure => self.measure_artifacts = !self.measure_artifacts,
            MenuAction::Rescan => self.start_scan(ctx),
            MenuAction::CheckForUpdates => self.spawn_update_check(ctx, true),
            MenuAction::ToggleAutoUpdateCheck => {
                self.auto_update_check = !self.auto_update_check;
                if self.auto_update_check {
                    self.info("mcpurge will check for a new release on launch");
                } else {
                    self.info(
                        "automatic update checks are off: mcpurge will make no network requests",
                    );
                }
            }
            MenuAction::OpenBackups => {
                let dir = purge::backups_root();
                std::fs::create_dir_all(&dir).ok();
                if let Err(e) = crate::files::reveal(&dir) {
                    self.warn(format!("could not open {}: {e}", dir.display()));
                }
            }
            MenuAction::ShowRegistryPath => {
                if let Some(path) = crate::registry::user_registry_path() {
                    self.info(format!("client registry override: {}", path.display()));
                }
            }
            MenuAction::ProjectPage => {
                if let Err(e) = open::that("https://github.com/TeeboBaggens/mcpurge") {
                    self.warn(format!("could not open the browser: {e}"));
                }
            }
            MenuAction::About => self.about_open = true,
        }
    }

    /// The update offer.
    ///
    /// Deliberately a decision rather than a notification: the window names the
    /// version, shows what the release says about itself, and says in as many
    /// words what will be checked before anything is written. Nothing happens
    /// until "Download and install" is clicked.
    fn update_window(&mut self, ctx: &egui::Context) {
        if let Some(version) = self.update_installed.clone() {
            let mut open = true;
            egui::Window::new("Update installed")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.label(format!(
                        "mcpurge {version} is in place. This window is still running the old \
                         build - quit and start it again to use the new one."
                    ));
                    ui.add_space(8.0);
                    if ui.button("Quit mcpurge").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            if !open {
                self.update_installed = None;
            }
            return;
        }

        let Some(available) = self.update_available.clone() else {
            return;
        };
        let mut open = true;
        let mut install = false;
        egui::Window::new("A new mcpurge is available")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading(format!("mcpurge {}", available.version));
                ui.weak(format!(
                    "you are running {} \u{b7} {} download",
                    update::current_version(),
                    human_bytes(available.size_bytes)
                ));
                if !available.notes.trim().is_empty() {
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(available.notes.trim()).wrap());
                        });
                }
                ui.add_space(6.0);
                ui.separator();
                ui.label(help::UPDATE_INSTALL);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.update_busy, egui::Button::new("Download and install"))
                        .clicked()
                    {
                        install = true;
                    }
                    if ui.button("Not now").clicked() {
                        self.update_available = None;
                    }
                    if self.update_busy {
                        ui.spinner();
                        ui.weak("downloading and verifying");
                    }
                });
            });
        if !open {
            self.update_available = None;
        }
        if install {
            self.spawn_update_install(ctx, *available);
        }
    }

    /// A small About window, opened from the Help menu.
    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let mut open = true;
        egui::Window::new("About mcpurge")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.heading(format!("mcpurge {}", env!("CARGO_PKG_VERSION")));
                ui.label(env!("CARGO_PKG_DESCRIPTION"));
                ui.add_space(8.0);
                ui.label(format!(
                    "Registry: {} ({} clients, {} runtimes)",
                    match &self.registry.source {
                        RegistrySource::Builtin => "built in".to_string(),
                        RegistrySource::User(p) => p.display().to_string(),
                    },
                    self.registry.clients.len(),
                    self.registry.runtimes.len()
                ));
                ui.label(format!("Backups: {}", purge::backups_root().display()));
                ui.add_space(8.0);
                ui.label(format!(
                    "Updates: {}",
                    if self.auto_update_check {
                        "checked on launch"
                    } else {
                        "off - checked only when you ask"
                    }
                ));
                // The key ID is here so it can be compared against the one
                // published in the repository. A verifier whose key nobody can
                // check is a verifier you have to take on faith.
                ui.weak(format!("Releases signed by {}", update::SIGNING_KEY_ID))
                    .on_hover_text(help::UPDATE_INSTALL);
                ui.add_space(8.0);
                ui.weak("MIT OR Apache-2.0");
            });
        self.about_open = open;
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("log")
            .resizable(true)
            // Enough for the header and a couple of lines. It is resizable, and
            // an empty log should not eat a sixth of the window on launch.
            .default_size(84.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Activity");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("clear").clicked() {
                            self.log.clear();
                        }
                        if ui
                            .small_button("open backups")
                            .on_hover_text(purge::backups_root().display().to_string())
                            .clicked()
                        {
                            let dir = purge::backups_root();
                            std::fs::create_dir_all(&dir).ok();
                            if let Err(e) = crate::files::reveal(&dir) {
                                self.warn(format!("could not open {}: {e}", dir.display()));
                            }
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in &self.log {
                            let color = match line.kind {
                                LogKind::Info => ui.visuals().text_color(),
                                LogKind::Good => egui::Color32::from_rgb(64, 190, 120),
                                LogKind::Warn => egui::Color32::from_rgb(225, 150, 70),
                            };
                            ui.colored_label(color, &line.text);
                        }
                    });
            });
    }

    /// The line above the table: a hint when nothing is ticked, and the batch
    /// actions when something is. It takes the same vertical space either way,
    /// so ticking a row does not shove the table down.
    fn bulk_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let keys = self.checked_keys();
        if keys.is_empty() {
            ui.horizontal(|ui| {
                ui.weak(
                    "Click a server to inspect it, or tick several to stop or remove them \
                     together. Right-click a row for the same actions.",
                );
            });
            return;
        }

        // Collect what the batch covers before drawing, so the closure below is
        // free to mutate the app.
        let mut labels = Vec::new();
        let mut pids = Vec::new();
        let mut tree = Vec::new();
        let mut running = 0usize;
        for k in &keys {
            let Some(f) = self.finding(k) else { continue };
            labels.push(format!("{} ({})", f.entry.name, f.entry.client_name));
            if f.is_running() {
                running += 1;
            }
            for p in &f.procs {
                pids.push(p.pid);
                tree.push(p.pid);
                tree.extend(p.descendants.iter().copied());
            }
        }
        pids.sort_unstable();
        pids.dedup();
        tree.sort_unstable();
        tree.dedup();
        let can_act = self.busy.is_none() && !self.scanning;

        ui.horizontal(|ui| {
            // Plain strong text, not the accent colour: the accent is a
            // background tint and is deliberately low-contrast as a foreground.
            ui.label(egui::RichText::new(format!("{} selected", keys.len())).strong());
            ui.separator();
            if ui
                .add_enabled(
                    can_act && !pids.is_empty(),
                    egui::Button::new(format!("Stop {running} running\u{2026}")),
                )
                .on_hover_text("signal every process tree behind the ticked servers")
                .clicked()
            {
                self.confirm = Some(Confirm::KillMany {
                    labels: labels.clone(),
                    pids: pids.clone(),
                    tree: tree.clone(),
                });
            }
            if ui
                .add_enabled(
                    can_act,
                    egui::Button::new(
                        egui::RichText::new(format!("Remove {}\u{2026}", keys.len())).strong(),
                    ),
                )
                .on_hover_text("unregister all of them, with a backup of every file first")
                .clicked()
            {
                self.confirm = Some(Confirm::PurgeMany { keys: keys.clone() });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("clear selection").clicked() {
                    self.checked.clear();
                }
                let _ = &ctx;
            });
        });
    }

    fn servers_table(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            let visible: Vec<String> = self.visible().iter().map(|f| f.entry.key()).collect();
            if visible.is_empty() {
                ui.centered_and_justified(|ui| {
                    if self.scanning {
                        ui.label("scanning\u{2026}");
                    } else if self.result.findings.is_empty() {
                        ui.label("No MCP servers found in any agent config on this machine.");
                    } else {
                        ui.label("No servers match this filter.");
                    }
                });
                return;
            }

            ui.add_space(2.0);
            self.bulk_bar(ui);
            ui.add_space(2.0);

            // Labels sense click-and-drag for text selection by default, and a
            // label sitting in a click-sensing cell swallows the click before
            // the row ever sees it — so the row's own `clicked()` stays false
            // wherever there is text to click, which is everywhere that matters.
            // Selectable text belongs in the detail pane, not the table.
            ui.style_mut().interaction.selectable_labels = false;

            let claims = self.pid_claims();
            let can_act = self.busy.is_none() && !self.scanning;
            let row_height = 24.0;
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                // egui_extras senses `hover()` on cells by default, which leaves
                // `row.response().clicked()` permanently false: rows highlight
                // under the pointer and then do nothing. Without this the detail
                // pane can never open, so every per-server action sits behind a
                // click that is silently discarded.
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(22.0))
                .column(Column::exact(16.0))
                .column(Column::initial(170.0).at_least(90.0))
                .column(Column::initial(150.0).at_least(80.0))
                .column(Column::initial(120.0).at_least(70.0))
                .column(Column::initial(70.0).at_least(55.0))
                .column(Column::initial(150.0).at_least(80.0))
                .column(Column::remainder().at_least(90.0))
                .header(22.0, |mut header| {
                    header.col(|ui| {
                        // Ticking the header ticks everything the filter is
                        // currently showing, never the rows hidden by it.
                        let mut all =
                            !visible.is_empty() && visible.iter().all(|k| self.checked.contains(k));
                        if ui
                            .checkbox(&mut all, "")
                            .accessible_name("select all servers")
                            .on_hover_text("select every server matching the current filter")
                            .changed()
                        {
                            if all {
                                self.checked.extend(visible.iter().cloned());
                            } else {
                                for k in &visible {
                                    self.checked.remove(k);
                                }
                            }
                        }
                    });
                    header.col(|_| {});
                    header.col(|ui| {
                        ui.strong("Server").on_hover_text(help::SERVER);
                    });
                    header.col(|ui| {
                        ui.strong("Client").on_hover_text(help::CLIENT);
                    });
                    header.col(|ui| {
                        ui.strong("Scope").on_hover_text(help::SCOPE);
                    });
                    header.col(|ui| {
                        ui.strong("Transport").on_hover_text(help::TRANSPORT);
                    });
                    header.col(|ui| {
                        ui.strong("State").on_hover_text(help::STATE);
                    });
                    header.col(|ui| {
                        ui.strong("On disk").on_hover_text(help::ON_DISK);
                    });
                })
                .body(|body| {
                    body.rows(row_height, visible.len(), |mut row| {
                        let key = &visible[row.index()];
                        let Some(finding) = self.finding(key) else {
                            return;
                        };
                        let selected = self.selected.as_deref() == Some(key.as_str());
                        row.set_selected(selected);
                        let ticked = self.checked.contains(key);
                        // The checkbox is drawn inside the row, which borrows
                        // the finding, so the change is recorded and applied
                        // once that borrow is gone.
                        let mut tick_changed: Option<bool> = None;

                        let actions = RowActions {
                            key: key.clone(),
                            label: format!(
                                "{} ({})",
                                finding.entry.name, finding.entry.client_name
                            ),
                            pids: finding.procs.iter().map(|p| p.pid).collect(),
                            tree: finding
                                .procs
                                .iter()
                                .flat_map(|p| {
                                    std::iter::once(p.pid).chain(p.descendants.iter().copied())
                                })
                                .collect(),
                        };

                        row.col(|ui| {
                            let mut on = ticked;
                            // A bare tick box says nothing to a screen reader,
                            // and there is one per row, so each is named.
                            if ui
                                .checkbox(&mut on, "")
                                .accessible_name(format!(
                                    "select {} in {}",
                                    finding.entry.name, finding.entry.client_name
                                ))
                                .changed()
                            {
                                tick_changed = Some(on);
                            }
                        });
                        row.col(|ui| status_dot(ui, finding));
                        row.col(|ui| {
                            let mut text = egui::RichText::new(&finding.entry.name);
                            if finding.entry.disabled {
                                text = text.weak().strikethrough();
                            }
                            ui.label(text);
                        });
                        row.col(|ui| {
                            ui.label(&finding.entry.client_name);
                        });
                        row.col(|ui| {
                            ui.label(finding.entry.scope.to_string())
                                .on_hover_text(format!(
                                    "{}\n\nfrom {}",
                                    finding.entry.scope.explain(),
                                    finding.entry.source.path.display()
                                ));
                        });
                        row.col(|ui| {
                            ui.label(finding.entry.transport.label())
                                .on_hover_text(finding.entry.transport.explain());
                        });
                        row.col(|ui| {
                            if finding.procs.is_empty() {
                                let (word, why) = if finding.entry.disabled {
                                    ("disabled", help::DISABLED)
                                } else {
                                    ("stopped", help::STOPPED)
                                };
                                ui.weak(word).on_hover_text(why);
                            } else {
                                let pids: Vec<String> =
                                    finding.procs.iter().map(|p| p.pid.to_string()).collect();
                                let shared = finding
                                    .procs
                                    .iter()
                                    .any(|p| claims.get(&p.pid).copied().unwrap_or(0) > 1);
                                let label = format!(
                                    "pid {}{}",
                                    pids.join(", "),
                                    if shared { " (shared)" } else { "" }
                                );
                                let resp =
                                    ui.colored_label(egui::Color32::from_rgb(64, 190, 120), label);
                                resp.on_hover_text(if shared {
                                    "this process is also claimed by another config entry: the \
                                     same server is registered in more than one client, so \
                                     stopping it affects both"
                                } else {
                                    help::RUNNING
                                });
                            }
                        });
                        row.col(|ui| {
                            let bytes: u64 =
                                finding.deletable_artifacts().map(|a| a.size_bytes).sum();
                            if bytes > 0 {
                                ui.label(human_bytes(bytes)).on_hover_text(help::ON_DISK);
                            } else if !finding.artifacts.is_empty() {
                                // The question this word begs is answered on
                                // hover rather than left to be guessed.
                                ui.weak("protected").on_hover_text(help::PROTECTED);
                            } else {
                                ui.weak("\u{2014}").on_hover_text(help::NO_FILES);
                            }
                        });

                        let resp = row.response();
                        match tick_changed {
                            Some(true) => {
                                self.checked.insert(key.clone());
                            }
                            Some(false) => {
                                self.checked.remove(key);
                            }
                            None => {
                                if resp.clicked() {
                                    self.selected = Some(key.clone());
                                }
                            }
                        }
                        resp.context_menu(|ui| {
                            ui.label(egui::RichText::new(&actions.label).strong());
                            ui.separator();
                            let mut t = self.checked.contains(&actions.key);
                            if ui.checkbox(&mut t, "Select for batch").changed() {
                                if t {
                                    self.checked.insert(actions.key.clone());
                                } else {
                                    self.checked.remove(&actions.key);
                                }
                            }
                            ui.separator();
                            if ui
                                .add_enabled(
                                    can_act && !actions.tree.is_empty(),
                                    egui::Button::new("Stop process tree\u{2026}"),
                                )
                                .clicked()
                            {
                                self.selected = Some(actions.key.clone());
                                self.confirm = Some(Confirm::Kill {
                                    label: actions.label.clone(),
                                    pids: actions.pids.clone(),
                                    tree: actions.tree.clone(),
                                });
                                ui.close();
                            }
                            if ui
                                .add_enabled(can_act, egui::Button::new("Remove\u{2026}"))
                                .clicked()
                            {
                                self.selected = Some(actions.key.clone());
                                self.confirm = Some(Confirm::Purge {
                                    key: actions.key.clone(),
                                });
                                ui.close();
                            }
                        });
                    });
                });
        });
    }

    fn detail_panel(&mut self, ui: &mut egui::Ui) {
        let Some(key) = self.selected.clone() else {
            return;
        };
        let Some(finding) = self.finding(&key).cloned() else {
            return;
        };

        egui::Panel::right("detail")
            .resizable(true)
            .default_size(400.0)
            .min_size(300.0)
            .show(ui, |ui| {
                // The actions get their own bottom panel, shown before the scroll
                // area so they claim their space first and stay on screen. At the
                // end of the scrolled content instead, a server with a long
                // artifact list pushes "Remove" below the fold.
                egui::Panel::bottom("detail-actions").show(ui, |ui| {
                    ui.add_space(6.0);
                    self.detail_actions(ui, &key, &finding);
                    ui.add_space(2.0);
                });
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading(&finding.entry.name);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("close").clicked() {
                                self.selected = None;
                            }
                        });
                    });
                    ui.label(format!(
                        "{} · {} · {}",
                        finding.entry.client_name,
                        finding.entry.scope,
                        finding.entry.transport.label()
                    ));
                    if finding.entry.disabled {
                        ui.colored_label(
                            egui::Color32::from_gray(150),
                            "disabled in this client's config",
                        );
                    }
                    ui.separator();

                    // ---- launch ------------------------------------------------
                    match &finding.entry.transport {
                        crate::model::Transport::Stdio { command, args } => {
                            ui.strong("Command");
                            ui.add(
                                egui::Label::new(egui::RichText::new(command).monospace())
                                    .wrap(),
                            );
                            if !args.is_empty() {
                                ui.add_space(2.0);
                                ui.strong("Arguments");
                                for a in args {
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(a).monospace().weak())
                                            .wrap(),
                                    );
                                }
                            }
                            if let Some(resolved) = proc::which(command) {
                                ui.add_space(2.0);
                                ui.weak(format!("resolves to {}", resolved.display()));
                            } else if !command.contains('/') {
                                ui.colored_label(
                                    egui::Color32::from_rgb(225, 150, 70),
                                    format!("'{command}' is not on PATH; this server cannot start"),
                                );
                            }
                        }
                        crate::model::Transport::Http { url }
                        | crate::model::Transport::Sse { url } => {
                            ui.strong("Endpoint");
                            ui.add(egui::Label::new(egui::RichText::new(url).monospace()).wrap());
                            ui.weak("remote server: nothing runs locally and there are no files to purge");
                        }
                        crate::model::Transport::Unknown => {
                            ui.colored_label(
                                egui::Color32::from_rgb(225, 150, 70),
                                "mcpurge could not work out how this server is launched",
                            );
                        }
                    }

                    // ---- env ---------------------------------------------------
                    if !finding.entry.env.is_empty() {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.strong(format!("Environment ({})", finding.entry.env.len()));
                            ui.checkbox(&mut self.reveal_env, "reveal values");
                        });
                        for (k, v) in &finding.entry.env {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new(k).monospace());
                                ui.label("=");
                                if self.reveal_env {
                                    ui.label(egui::RichText::new(v).monospace().weak());
                                } else {
                                    ui.weak(format!("\u{2022}\u{2022}\u{2022} ({} chars)", v.len()));
                                }
                            });
                        }
                        if !self.reveal_env {
                            ui.weak("values hidden: MCP configs commonly hold API tokens");
                        }
                    }

                    // ---- source ------------------------------------------------
                    ui.add_space(6.0);
                    ui.strong("Declared in");
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(finding.entry.source.path.display().to_string())
                                .monospace(),
                        )
                        .wrap(),
                    );
                    ui.weak(format!(
                        "key {} at {}",
                        finding.entry.source.container_key, finding.entry.source.container_pointer
                    ));
                    ui.horizontal(|ui| {
                        if ui.small_button("reveal file").clicked() {
                            let p = finding.entry.source.path.clone();
                            if let Err(e) = crate::files::reveal(&p) {
                                self.warn(format!("could not reveal {}: {e}", p.display()));
                            }
                        }
                        if !finding.entry.source.format.supports_surgical_edit() {
                            ui.colored_label(
                                egui::Color32::from_rgb(225, 150, 70),
                                "editing this format reformats the file",
                            );
                        }
                    });
                    if let Some(client) = self.registry.client(&finding.entry.client_id) {
                        if !client.editable {
                            ui.colored_label(
                                egui::Color32::from_rgb(225, 150, 70),
                                format!("{} has no editable config file. {}", client.name, client.notes),
                            );
                        }
                        if !client.notes.is_empty() && client.editable {
                            ui.weak(&client.notes);
                        }
                    }

                    // ---- processes ---------------------------------------------
                    ui.add_space(6.0);
                    ui.strong("Processes");
                    if finding.procs.is_empty() {
                        ui.weak("not running");
                    } else {
                        for p in &finding.procs {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!("pid {}", p.pid)).strong(),
                                    );
                                    ui.label(&p.name);
                                    confidence_badge(ui, p.confidence);
                                });
                                ui.weak(format!(
                                    "up {} · {} · matched on {}",
                                    human_duration(p.run_time),
                                    human_bytes(p.memory_bytes),
                                    p.match_reason
                                ));
                                if !p.descendants.is_empty() {
                                    ui.weak(format!(
                                        "{} child process(es): {}",
                                        p.descendants.len(),
                                        p.descendants
                                            .iter()
                                            .map(u32::to_string)
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    ));
                                }
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&p.cmdline).monospace().weak().size(10.0),
                                    )
                                    .wrap(),
                                );
                            });
                        }
                    }

                    // ---- artifacts ---------------------------------------------
                    ui.add_space(6.0);
                    ui.strong("Files on disk");
                    if finding.artifacts.is_empty() {
                        ui.weak(if self.measure_artifacts {
                            "none found"
                        } else {
                            "not measured (turn on \"measure sizes\")"
                        });
                    } else {
                        for a in &finding.artifacts {
                            ui.horizontal_wrapped(|ui| {
                                // Words, not symbols: egui's bundled fonts
                                // (Ubuntu-Light plus a five-glyph icon font)
                                // have no lock or warning sign, and a missing
                                // glyph is drawn as an empty box.
                                if a.shared {
                                    ui.colored_label(egui::Color32::from_gray(150), "kept")
                                        .on_hover_text(help::KEPT);
                                } else {
                                    ui.label("\u{2022}")
                                        .on_hover_text("this path can be deleted with the server");
                                }
                                ui.label(human_bytes(a.size_bytes));
                                ui.weak(format!("[{}]", a.kind.label()))
                                    .on_hover_text(a.kind.explain());
                                let text = if a.kind == ArtifactKind::DataPath {
                                    egui::RichText::new(a.path.display().to_string())
                                        .monospace()
                                        .color(egui::Color32::from_rgb(140, 170, 220))
                                } else {
                                    egui::RichText::new(a.path.display().to_string()).monospace()
                                };
                                ui.add(egui::Label::new(text).wrap())
                                    .on_hover_text(&a.note);
                            });
                        }
                        let reclaim: u64 = finding.deletable_artifacts().map(|a| a.size_bytes).sum();
                        ui.weak(format!("{} could be reclaimed", human_bytes(reclaim)));
                    }

                });
            });
    }

    /// The per-server actions, pinned to the bottom of the detail pane.
    fn detail_actions(&mut self, ui: &mut egui::Ui, key: &str, finding: &Finding) {
        ui.horizontal(|ui| {
            let can_act = self.busy.is_none() && !self.scanning;
            if ui
                .add_enabled(
                    can_act && finding.is_running(),
                    egui::Button::new("Stop process tree\u{2026}"),
                )
                .on_hover_text("SIGTERM the server and its children, then SIGKILL any survivors")
                .clicked()
            {
                let tree: Vec<u32> = finding
                    .procs
                    .iter()
                    .flat_map(|p| std::iter::once(p.pid).chain(p.descendants.iter().copied()))
                    .collect();
                self.confirm = Some(Confirm::Kill {
                    label: format!("{} ({})", finding.entry.name, finding.entry.client_name),
                    pids: finding.procs.iter().map(|p| p.pid).collect(),
                    tree,
                });
            }
            if ui
                .add_enabled(
                    can_act,
                    egui::Button::new(egui::RichText::new("Remove\u{2026}").strong()),
                )
                .on_hover_text("unregister from this client, with a backup first")
                .clicked()
            {
                self.confirm = Some(Confirm::Purge {
                    key: key.to_string(),
                });
            }
        });
    }

    fn orphans_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            ui.label(
                "Processes that look like MCP servers but match no config entry: servers left \
                 running after their config was changed, or started by a client mcpurge does \
                 not recognise.",
            );
            ui.separator();
            if self.result.orphans.is_empty() {
                ui.weak("None. Every running MCP server matches a config entry.");
                return;
            }

            let by_host = scan::orphans_by_host(&self.result.orphans);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (host, orphans) in by_host {
                        ui.add_space(6.0);
                        ui.strong(format!("spawned by {host}"));
                        for o in orphans {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(format!("pid {}", o.proc.pid)).strong(),
                                    );
                                    ui.label(&o.proc.name);
                                    ui.weak(format!(
                                        "up {} · {}",
                                        human_duration(o.proc.run_time),
                                        human_bytes(o.proc.memory_bytes)
                                    ));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_enabled(
                                                    self.busy.is_none(),
                                                    egui::Button::new("Stop\u{2026}"),
                                                )
                                                .clicked()
                                            {
                                                let mut tree = vec![o.proc.pid];
                                                tree.extend(o.proc.descendants.iter().copied());
                                                self.confirm = Some(Confirm::KillOrphan {
                                                    pid: o.proc.pid,
                                                    label: format!(
                                                        "orphan {} (pid {})",
                                                        o.proc.name, o.proc.pid
                                                    ),
                                                    tree,
                                                });
                                            }
                                        },
                                    );
                                });
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&o.proc.cmdline).monospace().size(10.0),
                                    )
                                    .wrap(),
                                );
                                if let Some(exe) = &o.proc.exe {
                                    ui.weak(exe.display().to_string());
                                }
                            });
                        }
                    }
                });
        });
    }

    fn coverage_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.heading("Did mcpurge miss anything?");
                    ui.label(
                        "Two passes run over this machine. First the registry below is used to \
                         look where each known client is supposed to keep its config. Then, if \
                         deep sweep is on, the filesystem is searched for config files by their \
                         shape rather than their name, which finds clients the registry has \
                         never heard of. This page reports what both passes turned up, so a \
                         gap shows as a number rather than as silence.",
                    );

                    // ---- the five numbers that matter -------------------
                    let servers = self.result.findings.len();
                    let clients_with_servers: BTreeSet<&str> = self
                        .result
                        .findings
                        .iter()
                        .map(|f| f.entry.client_id.as_str())
                        .collect();
                    let unknown = self.result.unknown_configs.len();
                    let unreadable = self.result.errors.len();
                    let warn = ui.visuals().warn_fg_color;

                    ui.add_space(10.0);
                    ui.horizontal_wrapped(|ui| {
                        stat_tile(
                            ui,
                            self.result.scanned_paths.len().to_string(),
                            "config files read",
                            &format!("in {} ms", self.result.duration_ms),
                            None,
                        );
                        stat_tile(ui, servers.to_string(), "servers declared", "", None);
                        stat_tile(
                            ui,
                            clients_with_servers.len().to_string(),
                            "clients in use",
                            &format!("out of {} known", self.registry.clients.len()),
                            None,
                        );
                        stat_tile(
                            ui,
                            unknown.to_string(),
                            "not recognised",
                            "MCP config belonging to no known client",
                            (unknown > 0).then_some(warn),
                        );
                        stat_tile(
                            ui,
                            unreadable.to_string(),
                            "could not be read",
                            "files that exist but could not be parsed",
                            (unreadable > 0).then_some(warn),
                        );
                    });

                    if !self.sweep {
                        ui.add_space(8.0);
                        ui.colored_label(
                            warn,
                            "Deep sweep is off, so only the registry's own locations were \
                             checked. Anything belonging to an unknown client was not looked for.",
                        );
                    }

                    // ---- unrecognised configs ---------------------------
                    section(ui, "Config that matches no known client");
                    if self.result.unknown_configs.is_empty() {
                        ui.weak(if self.sweep {
                            "None. Every MCP config found on this machine belongs to a client \
                             in the registry below."
                        } else {
                            "Not checked: turn on deep sweep and rescan."
                        });
                    } else {
                        ui.label(
                            "These files declare MCP servers in a recognisable shape, but sit \
                             somewhere no registry entry claims. Their servers do appear in the \
                             Servers tab, under \"unidentified client\". Adding an entry to the \
                             registry is what gives them a name.",
                        );
                        ui.add_space(4.0);
                        for u in &self.result.unknown_configs {
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(warn, format!("{} server(s)", u.server_count));
                                ui.weak(format!("under {}", u.container_keys.join(", ")));
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(u.path.display().to_string())
                                            .monospace(),
                                    )
                                    .wrap(),
                                );
                            });
                        }
                    }

                    // ---- the registry -----------------------------------
                    section(ui, "Where mcpurge looked");
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!(
                            "{} clients in the registry ({}).",
                            self.registry.clients.len(),
                            match &self.registry.source {
                                RegistrySource::Builtin => "built in".to_string(),
                                RegistrySource::User(p) => p.display().to_string(),
                            }
                        ));
                        ui.checkbox(&mut self.coverage_only_hits, "only clients with servers");
                    });
                    ui.weak(
                        "Paths are hints, not promises: vendors move their config files, which \
                         is why the sweep exists. \"verified\" means confirmed against vendor \
                         docs or seen on a real machine; \"guess\" means probed on spec, \
                         because probing a path that does not exist costs nothing.",
                    );
                    ui.add_space(6.0);
                    self.clients_table(ui);

                    // ---- runtimes ---------------------------------------
                    if !self.result.runtimes.is_empty() {
                        section(ui, "Local model runtimes");
                        ui.label(
                            "Found running, and listed here rather than as servers: these are \
                             inference servers with no MCP support of their own. MCP reaches \
                             their models through a separate host such as LM Studio, Goose or \
                             Continue.",
                        );
                        ui.add_space(4.0);
                        for r in &self.result.runtimes {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new(&r.name).strong());
                                ui.weak(format!(
                                    "{} process(es): {}",
                                    r.pids.len(),
                                    r.pids
                                        .iter()
                                        .map(u32::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ));
                            });
                            ui.weak(&r.detail);
                        }
                    }

                    // ---- problems ---------------------------------------
                    if !self.result.errors.is_empty() {
                        section(ui, "Could not be read");
                        ui.label(
                            "These exist but could not be parsed, so anything they declare is \
                             missing from the Servers tab.",
                        );
                        ui.add_space(4.0);
                        for e in &self.result.errors {
                            ui.colored_label(warn, e);
                        }
                    }

                    // ---- the receipt ------------------------------------
                    ui.add_space(12.0);
                    ui.collapsing(
                        format!("Every file read ({})", self.result.scanned_paths.len()),
                        |ui| {
                            for p in &self.result.scanned_paths {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(p.display().to_string())
                                            .monospace()
                                            .size(11.0),
                                    )
                                    .wrap(),
                                );
                            }
                        },
                    );
                    ui.add_space(12.0);
                });
        });
    }

    /// Every known client, how many servers it yielded, and the paths that were
    /// probed for it on this OS. A table rather than a list, because the
    /// question being asked is a lookup: "was *my* client checked?"
    fn clients_table(&mut self, ui: &mut egui::Ui) {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for f in &self.result.findings {
            *counts.entry(f.entry.client_id.as_str()).or_insert(0) += 1;
        }
        let rows: Vec<(&crate::registry::Client, usize)> = self
            .registry
            .clients
            .iter()
            .map(|c| (c, counts.get(c.id.as_str()).copied().unwrap_or(0)))
            .filter(|(_, n)| !self.coverage_only_hits || *n > 0)
            .collect();

        if rows.is_empty() {
            ui.weak("No client in the registry declared a server on this machine.");
            return;
        }

        let good = theme::status(ui.visuals()).good;
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .id_salt("clients")
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(170.0).at_least(110.0))
            .column(Column::exact(60.0))
            .column(Column::exact(70.0))
            .column(Column::remainder().at_least(200.0))
            .header(22.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Client");
                });
                header.col(|ui| {
                    ui.strong("Servers");
                });
                header.col(|ui| {
                    ui.strong("Paths");
                });
                header.col(|ui| {
                    ui.strong("Looked in");
                });
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| {
                    let (c, n) = rows[row.index()];
                    row.col(|ui| {
                        if n > 0 {
                            ui.label(egui::RichText::new(&c.name).strong());
                        } else {
                            ui.weak(&c.name);
                        }
                    });
                    row.col(|ui| {
                        if n > 0 {
                            ui.colored_label(good, n.to_string());
                        } else {
                            ui.weak("\u{2014}");
                        }
                    });
                    row.col(|ui| {
                        ui.weak(c.confidence.label());
                    });
                    row.col(|ui| {
                        let patterns = c.patterns_for_host();
                        let text = if patterns.is_empty() {
                            "found by process and sweep only".to_string()
                        } else {
                            patterns.join("  ")
                        };
                        ui.add(
                            egui::Label::new(egui::RichText::new(text).monospace().size(11.0))
                                .truncate(),
                        )
                        .on_hover_text(if c.notes.is_empty() {
                            c.container_keys.join(", ")
                        } else {
                            format!("{}\n\nkeys: {}", c.notes, c.container_keys.join(", "))
                        });
                    });
                });
            });
    }

    /// The confirmation step. Every destructive action states exactly what it
    /// will touch before it is allowed to proceed.
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        let mut keep_open = true;

        // One server, a loose orphan and a ticked batch all pose the same
        // question, so they reduce to one target before the dialog is built.
        let kill_target: Option<(Vec<String>, Vec<u32>, Vec<u32>)> = match &confirm {
            Confirm::Kill { label, pids, tree } => {
                Some((vec![label.clone()], pids.clone(), tree.clone()))
            }
            Confirm::KillOrphan { label, pid, tree } => {
                Some((vec![label.clone()], vec![*pid], tree.clone()))
            }
            Confirm::KillMany { labels, pids, tree } => {
                Some((labels.clone(), pids.clone(), tree.clone()))
            }
            Confirm::Purge { .. } | Confirm::PurgeMany { .. } => None,
        };

        if let Some((labels, pids, tree)) = kill_target {
            {
                let label = labels.join(", ");
                egui::Modal::new(egui::Id::unique("mcpurge-confirm-kill")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading(if labels.len() == 1 {
                        "Stop this server?".to_string()
                    } else {
                        format!("Stop these {} servers?", labels.len())
                    });
                    ui.add_space(4.0);
                    for l in &labels {
                        ui.label(l.as_str());
                    }
                    ui.add_space(6.0);
                    ui.label(format!(
                        "{} process(es) will be signalled, including {} in the subtree:",
                        tree.len(),
                        tree.len().saturating_sub(pids.len())
                    ));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                tree.iter()
                                    .map(u32::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            )
                            .monospace(),
                        )
                        .wrap(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        "Children are signalled before their parents, so a wrapper cannot \
                         respawn them. SIGTERM first.",
                    );
                    ui.checkbox(
                        &mut self.allow_sigkill,
                        "escalate to SIGKILL if a process ignores SIGTERM",
                    );
                    ui.add_space(6.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(225, 150, 70),
                        "The client may restart the server automatically when it next needs it.",
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                        let danger = ui.visuals().error_fg_color;
                        if ui
                            .button(egui::RichText::new("Stop it").strong().color(danger))
                            .clicked()
                        {
                            self.spawn_kill(ctx, label.clone(), pids.clone());
                            keep_open = false;
                        }
                    });
                });
            }
        } else if let Confirm::PurgeMany { keys } = &confirm {
            {
                // Everything the dialog has to account for, gathered once:
                // which entries can actually be edited, how many distinct
                // files are involved, and what is safe to delete.
                struct Row {
                    key: String,
                    name: String,
                    client: String,
                    scope: String,
                    path: PathBuf,
                    editable: bool,
                    running: usize,
                    rewrites: bool,
                }
                let rows: Vec<Row> = keys
                    .iter()
                    .filter_map(|k| {
                        let f = self.finding(k)?;
                        Some(Row {
                            key: k.clone(),
                            name: f.entry.name.clone(),
                            client: f.entry.client_name.clone(),
                            scope: f.entry.scope.to_string(),
                            path: f.entry.source.path.clone(),
                            editable: self
                                .registry
                                .client(&f.entry.client_id)
                                .map(|c| c.editable)
                                .unwrap_or(true),
                            running: f.procs.len(),
                            rewrites: !f.entry.source.format.supports_surgical_edit(),
                        })
                    })
                    .collect();
                if rows.is_empty() {
                    return;
                }
                let editable_keys: Vec<String> = rows
                    .iter()
                    .filter(|r| r.editable)
                    .map(|r| r.key.clone())
                    .collect();
                let files: BTreeSet<&std::path::Path> =
                    rows.iter().map(|r| r.path.as_path()).collect();
                let running: usize = rows.iter().map(|r| r.running).sum();
                let blocked = rows.len() - editable_keys.len();
                let rewrites = rows.iter().filter(|r| r.rewrites).count();

                // One path can back several entries, so size it once. Owned,
                // so no borrow of the app outlives this into the modal.
                let mut unique: BTreeMap<PathBuf, u64> = BTreeMap::new();
                for r in &rows {
                    if let Some(f) = self.finding(&r.key) {
                        for a in f.deletable_artifacts() {
                            unique.insert(a.path.clone(), a.size_bytes);
                        }
                    }
                }
                let delete_bytes: u64 = unique.values().sum();
                let delete_count = unique.len();

                egui::Modal::new(egui::Id::unique("mcpurge-confirm-purge-many")).show(ctx, |ui| {
                    ui.set_width(640.0);
                    let warn = ui.visuals().warn_fg_color;
                    ui.heading(format!("Remove {} servers?", rows.len()));
                    ui.label(format!("from {} config file(s)", files.len()));
                    ui.add_space(8.0);

                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for r in &rows {
                                ui.horizontal_wrapped(|ui| {
                                    if r.editable {
                                        ui.label(egui::RichText::new(&r.name).strong());
                                    } else {
                                        ui.colored_label(warn, format!("{} (skipped)", r.name));
                                    }
                                    ui.weak(format!("{} · {}", r.client, r.scope));
                                    if r.running > 0 {
                                        ui.weak(format!("{} running", r.running));
                                    }
                                });
                            }
                        });

                    ui.add_space(8.0);
                    ui.strong("This will:");
                    ui.label(format!(
                        "1. Back up {} config file(s) to {}",
                        files.len(),
                        purge::backups_root().display()
                    ));
                    ui.label(format!(
                        "2. Remove {} key(s), one file at a time",
                        editable_keys.len()
                    ));
                    if rewrites > 0 {
                        ui.colored_label(
                            warn,
                            format!(
                                "   {rewrites} of these are in a format with no format-preserving \
                                 editor, so comments and layout may change."
                            ),
                        );
                    }
                    if blocked > 0 {
                        ui.colored_label(
                            warn,
                            format!(
                                "{blocked} entr(ies) keep their MCP list in application \
                                 state rather than a config file and will be skipped."
                            ),
                        );
                    }
                    if running > 0 {
                        ui.add_space(4.0);
                        ui.colored_label(
                            warn,
                            format!(
                                "{running} process(es) are still running. Removing the config \
                                 does not stop them: use \"Stop\" as well."
                            ),
                        );
                    }

                    ui.add_space(8.0);
                    ui.checkbox(
                        &mut self.purge_delete_files,
                        format!(
                            "Also delete {delete_count} file set(s) on disk ({})",
                            human_bytes(delete_bytes)
                        ),
                    );
                    if self.purge_delete_files {
                        ui.indent("bulk-files", |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(120.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    for (path, bytes) in &unique {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(human_bytes(*bytes));
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(path.display().to_string())
                                                        .monospace(),
                                                )
                                                .wrap(),
                                            );
                                        });
                                    }
                                    if unique.is_empty() {
                                        ui.weak("Nothing on disk is safe to delete for these.");
                                    }
                                });
                            ui.add_space(4.0);
                            ui.checkbox(
                                &mut self.purge_permanent,
                                "Delete permanently instead of moving to the trash",
                            );
                            if self.purge_permanent {
                                ui.colored_label(
                                    ui.visuals().error_fg_color,
                                    "Permanent deletion cannot be undone.",
                                );
                            }
                        });
                    }
                    ui.weak(
                        "Protected paths - system paths, shared caches and directories a \
                         server was given access to - are never touched.",
                    );

                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                        let danger = ui.visuals().error_fg_color;
                        if ui
                            .add_enabled(
                                !editable_keys.is_empty(),
                                egui::Button::new(
                                    egui::RichText::new(format!("Remove {}", editable_keys.len()))
                                        .strong()
                                        .color(danger),
                                ),
                            )
                            .clicked()
                        {
                            self.spawn_purge(ctx, editable_keys.clone());
                            keep_open = false;
                        }
                    });
                });
            }
        } else if let Confirm::Purge { key } = &confirm {
            {
                let Some(finding) = self.finding(key).cloned() else {
                    return;
                };
                let editable = self
                    .registry
                    .client(&finding.entry.client_id)
                    .map(|c| c.editable)
                    .unwrap_or(true);

                egui::Modal::new(egui::Id::unique("mcpurge-confirm-purge")).show(ctx, |ui| {
                    ui.set_width(620.0);
                    ui.heading(format!("Remove '{}'?", finding.entry.name));
                    ui.label(format!(
                        "from {} ({})",
                        finding.entry.client_name, finding.entry.scope
                    ));
                    ui.add_space(8.0);

                    if !editable {
                        ui.colored_label(
                            egui::Color32::from_rgb(225, 150, 70),
                            "This client keeps its MCP list in application state, not a config \
                             file. mcpurge cannot unregister the server; remove it inside the \
                             app instead.",
                        );
                        ui.add_space(8.0);
                    }

                    ui.strong("This will:");
                    ui.label(format!(
                        "1. Back up {} to {}",
                        finding.entry.source.path.display(),
                        purge::backups_root().display()
                    ));
                    ui.label(format!(
                        "2. Remove the '{}' key from {}",
                        finding.entry.name, finding.entry.source.container_key
                    ));
                    if finding.entry.source.format.supports_surgical_edit() {
                        ui.weak("   Comments, key order and formatting are preserved.");
                    } else {
                        ui.colored_label(
                            egui::Color32::from_rgb(225, 150, 70),
                            "   This format is rewritten, so comments and layout may change.",
                        );
                    }

                    if finding.is_running() {
                        ui.add_space(4.0);
                        ui.colored_label(
                            egui::Color32::from_rgb(225, 150, 70),
                            format!(
                                "This server is running ({} process(es)). Removing the config \
                                 does not stop it: use \"Stop process tree\" as well.",
                                finding.procs.len()
                            ),
                        );
                    }

                    ui.add_space(8.0);
                    let deletable: Vec<_> = finding.deletable_artifacts().collect();
                    let protected = finding.artifacts.len() - deletable.len();
                    ui.checkbox(
                        &mut self.purge_delete_files,
                        format!(
                            "Also delete {} file set(s) on disk ({})",
                            deletable.len(),
                            human_bytes(deletable.iter().map(|a| a.size_bytes).sum())
                        ),
                    );
                    if self.purge_delete_files {
                        ui.indent("files", |ui| {
                            for a in &deletable {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(human_bytes(a.size_bytes));
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(a.path.display().to_string())
                                                .monospace(),
                                        )
                                        .wrap(),
                                    );
                                });
                            }
                            if deletable.is_empty() {
                                ui.weak("Nothing on disk is safe to delete for this server.");
                            }
                            ui.add_space(4.0);
                            ui.checkbox(
                                &mut self.purge_permanent,
                                "Delete permanently instead of moving to the trash",
                            );
                            if self.purge_permanent {
                                ui.colored_label(
                                    egui::Color32::from_rgb(235, 110, 90),
                                    "Permanent deletion cannot be undone.",
                                );
                            }
                        });
                    }
                    if protected > 0 {
                        ui.weak(format!(
                            "{protected} path(s) are protected and will not be touched: system \
                             paths, shared caches, and directories the server was given access to."
                        ));
                    }

                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                        let danger = ui.visuals().error_fg_color;
                        if ui
                            .add_enabled(
                                editable,
                                egui::Button::new(
                                    egui::RichText::new("Remove").strong().color(danger),
                                ),
                            )
                            .clicked()
                        {
                            let key = key.clone();
                            self.spawn_purge(ctx, vec![key]);
                            keep_open = false;
                        }
                    });
                });
            }
        }

        if keep_open {
            self.confirm = Some(confirm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Artifact, ConfigFormat, DetectedRuntime, OrphanProc, RunningProc, Scope, ServerEntry,
        SourceLocation, Transport, UnknownConfig,
    };
    use std::collections::BTreeMap;

    fn entry(name: &str, transport: Transport, disabled: bool) -> ServerEntry {
        let mut env = BTreeMap::new();
        env.insert("API_TOKEN".to_string(), "sk-secret-value".to_string());
        ServerEntry {
            name: name.to_string(),
            client_id: "kiro".into(),
            client_name: "Kiro".into(),
            scope: Scope::User,
            transport,
            env,
            disabled,
            source: SourceLocation {
                path: "/Users/test/.kiro/settings/mcp.json".into(),
                container_pointer: "/mcpServers".into(),
                container_key: "mcpServers".into(),
                format: ConfigFormat::Json,
            },
            raw: serde_json::json!({ "command": "npx" }),
        }
    }

    /// A result covering every branch the UI has to render: running and
    /// stopped, local and remote, disabled, protected and deletable artifacts,
    /// an orphan, a runtime and an unrecognised config.
    fn populated() -> ScanResult {
        let running = Finding {
            entry: entry(
                "rea",
                Transport::Stdio {
                    command: "npx".into(),
                    args: vec!["-y".into(), "rea-agents@6.7.0".into(), "mcp".into()],
                },
                false,
            ),
            procs: vec![RunningProc {
                pid: 4242,
                parent: Some(1),
                name: "npm exec".into(),
                cmdline: "npm exec rea-agents@6.7.0 mcp".into(),
                exe: Some("/usr/bin/npm".into()),
                run_time: 3600,
                memory_bytes: 51_200_000,
                descendants: vec![4243],
                match_reason: "command npx and argument rea-agents@6.7.0".into(),
                confidence: MatchConfidence::Strong,
            }],
            artifacts: vec![
                Artifact {
                    path: "/Users/test/.npm/_npx/abc123".into(),
                    size_bytes: 223_000_000,
                    kind: ArtifactKind::NpmCache,
                    shared: false,
                    note: "npx cache".into(),
                },
                Artifact {
                    path: "/Users/test/projects/work".into(),
                    size_bytes: 1_400_000_000,
                    kind: ArtifactKind::DataPath,
                    shared: true,
                    note: "granted to the server (your data, never deleted)".into(),
                },
            ],
        };
        let remote = Finding {
            entry: entry(
                "crawl4ai",
                Transport::Http {
                    url: "https://example.test/mcp".into(),
                },
                false,
            ),
            procs: vec![],
            artifacts: vec![],
        };
        let disabled = Finding {
            entry: entry("godot-ai", Transport::Unknown, true),
            procs: vec![],
            artifacts: vec![],
        };

        // The same server registered in a second client, claiming the same
        // process. This is what marks a pid "(shared)", and it is the case where
        // stopping one entry's process also stops the other's.
        let mut shared = running.clone();
        shared.entry.client_id = "claude-code".into();
        shared.entry.client_name = "Claude Code".into();
        shared.entry.source.path = "/Users/test/.claude.json".into();
        // Keeps only the path that is never deletable, so one row in the table
        // reads "protected" in the On disk column and the word gets rendered,
        // hovered and explained by the tests below.
        shared.artifacts.retain(|a| a.shared);

        ScanResult {
            findings: vec![running, remote, disabled, shared],
            orphans: vec![OrphanProc {
                proc: RunningProc {
                    pid: 9001,
                    parent: Some(1),
                    name: "node".into(),
                    cmdline: "node /tmp/stale-mcp-server/index.js".into(),
                    exe: Some("/usr/bin/node".into()),
                    run_time: 90_000,
                    memory_bytes: 12_000_000,
                    descendants: vec![],
                    match_reason: "matches no known config entry".into(),
                    confidence: MatchConfidence::Loose,
                },
                likely_host: Some("Cursor".into()),
            }],
            unknown_configs: vec![UnknownConfig {
                path: "/Users/test/.newtool/mcp.json".into(),
                container_keys: vec!["mcpServers".into()],
                server_count: 2,
            }],
            runtimes: vec![DetectedRuntime {
                name: "Ollama".into(),
                detail: "Inference server with no native MCP support.".into(),
                pids: vec![1797, 1825],
            }],
            errors: vec!["/Users/test/broken.json: unexpected token".into()],
            scanned_paths: vec!["/Users/test/.kiro/settings/mcp.json".into()],
            duration_ms: 1234,
        }
    }

    fn fixture() -> McpurgeApp {
        let mut app = McpurgeApp::with_registry(Registry::builtin().unwrap());
        app.result = populated();
        app
    }

    /// Renders every panel of every tab. This is the substitute for a visual
    /// check: it will not tell us the layout looks right, but it does prove no
    /// tab panics, which is what an id collision or a bad layout call causes.
    #[test]
    fn every_tab_renders_without_panicking() {
        for tab in [Tab::Servers, Tab::Orphans, Tab::Coverage] {
            let mut app = fixture();
            app.tab = tab;
            egui::__run_test_ui(|ui| {
                app.top_bar(ui);
                app.bottom_bar(ui);
                match app.tab {
                    Tab::Servers => {
                        app.detail_panel(ui);
                        app.servers_table(ui);
                    }
                    Tab::Orphans => app.orphans_panel(ui),
                    Tab::Coverage => app.coverage_panel(ui),
                }
            });
        }
    }

    /// Renders every tab, and the detail pane for every finding, through a real
    /// font stack.
    ///
    /// `egui::__run_test_ui` lays widgets out but leaves the missing-glyph
    /// policy at its default, which silently substitutes an empty box, so it
    /// cannot see a character the bundled fonts lack. kittest sets the policy to
    /// panic. That is what caught the lock and warning symbols this UI used to
    /// draw: egui 0.37's default fonts are Ubuntu-Light plus a five-glyph icon
    /// font, and neither has them.
    #[test]
    fn every_view_renders_with_real_fonts() {
        let keys: Vec<String> = populated().findings.iter().map(|f| f.entry.key()).collect();
        for tab in [Tab::Servers, Tab::Orphans, Tab::Coverage] {
            for selected in std::iter::once(None).chain(keys.iter().cloned().map(Some)) {
                let mut app = fixture();
                app.tab = tab;
                app.selected = selected;
                app.reveal_env = true;
                // With rows ticked, so the batch bar and its buttons draw too.
                app.checked = keys.iter().cloned().collect();
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(egui::vec2(1280.0, 800.0))
                    .build_ui_state(
                        |ui, app: &mut McpurgeApp| {
                            app.top_bar(ui);
                            app.bottom_bar(ui);
                            match app.tab {
                                Tab::Servers => {
                                    app.detail_panel(ui);
                                    app.servers_table(ui);
                                }
                                Tab::Orphans => app.orphans_panel(ui),
                                Tab::Coverage => app.coverage_panel(ui),
                            }
                        },
                        app,
                    );
                harness.run();
            }
        }
    }

    /// Drives the real pointer through the real table.
    ///
    /// This exists because of the bug it catches: `egui_extras` senses
    /// `hover()` on table cells by default, so `row.response().clicked()` was
    /// permanently false. Rows highlighted under the pointer and did nothing,
    /// the detail pane never opened, and every per-server action was therefore
    /// unreachable. The other UI tests here all set `selected` by hand, so they
    /// passed while the app could not be used at all.
    #[test]
    fn clicking_a_row_opens_the_detail_pane_with_its_actions() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .build_ui_state(
                |ui, app: &mut McpurgeApp| {
                    app.top_bar(ui);
                    app.bottom_bar(ui);
                    app.detail_panel(ui);
                    app.servers_table(ui);
                },
                fixture(),
            );
        harness.run();
        assert!(
            harness.state().selected.is_none(),
            "nothing is selected before the click"
        );

        // "rea" is registered in two clients, so take the first of the two rows:
        // either one proves the click landed.
        harness
            .get_all_by_label("rea")
            .next()
            .expect("a 'rea' row")
            .click();
        harness.run();

        let selected = harness.state().selected.clone();
        assert!(
            selected.as_deref().is_some_and(|k| k.ends_with("::rea")),
            "clicking the 'rea' row should select it, got {selected:?}"
        );

        // And the actions have to be on screen rather than below the fold:
        // `get_by_label` panics if the widget is not in the tree.
        harness.get_by_label("Remove\u{2026}");
        harness.get_by_label("Stop process tree\u{2026}");
    }

    /// "protected" is a word mcpurge invented, and the first thing anyone asks
    /// of it is what it means. The answer is a tooltip, so this hovers the way
    /// a person would and reads what comes up; a constant that is never wired
    /// to a widget would pass a string test and fail here.
    #[test]
    fn hovering_protected_explains_what_it_means() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .build_ui_state(
                |ui, app: &mut McpurgeApp| {
                    app.top_bar(ui);
                    app.servers_table(ui);
                },
                fixture(),
            );
        harness.run();

        harness.get_by_label("protected").hover();
        // Two passes: one to register the hover, one to lay the tooltip out.
        harness.run();
        harness.run();

        assert!(
            harness
                .query_by_label_contains("A path is protected when deleting it")
                .is_some(),
            "hovering \"protected\" should explain what it means and why"
        );
    }

    /// The menus are two bars - egui's on Windows and Linux, the system's on
    /// macOS - and one list of actions. This tests the list, which is the half
    /// that cannot be seen on the platform the other half runs on.
    #[test]
    fn menu_actions_do_what_they_say() {
        let ctx = egui::Context::default();
        let mut app = fixture();

        app.run_menu_action(MenuAction::SelectAll, &ctx);
        assert_eq!(app.checked.len(), app.visible().len());
        app.run_menu_action(MenuAction::SelectNone, &ctx);
        assert!(app.checked.is_empty());

        app.search = "rea".into();
        app.run_menu_action(MenuAction::ClearSearch, &ctx);
        assert!(app.search.is_empty());

        app.run_menu_action(MenuAction::ShowTab(Tab::Coverage), &ctx);
        assert_eq!(app.tab, Tab::Coverage);
        // Find has to bring the table back: the search box lives there, and
        // focusing a widget that is not being drawn does nothing at all.
        app.run_menu_action(MenuAction::Find, &ctx);
        assert_eq!(app.tab, Tab::Servers);
        assert!(app.focus_search);

        app.run_menu_action(MenuAction::Theme(ThemeMode::Light), &ctx);
        assert_eq!(app.theme, ThemeMode::Light);

        /// A menu item that flips a setting, paired with a way to read it.
        type Toggle = (MenuAction, fn(&McpurgeApp) -> bool);
        let toggles: [Toggle; 5] = [
            (MenuAction::ToggleDisabled, |a| a.show_disabled),
            (MenuAction::ToggleRevealEnv, |a| a.reveal_env),
            (MenuAction::ToggleSweep, |a| a.sweep),
            (MenuAction::ToggleMeasure, |a| a.measure_artifacts),
            (MenuAction::ToggleAutoUpdateCheck, |a| a.auto_update_check),
        ];
        for (action, read) in toggles {
            let before = read(&app);
            app.run_menu_action(action, &ctx);
            assert_ne!(read(&app), before, "{action:?} did not toggle anything");
        }

        app.run_menu_action(MenuAction::About, &ctx);
        assert!(app.about_open);
    }

    /// What the menu greys out, which on macOS is the only signal that an
    /// action is unavailable: there is no button to look at.
    #[test]
    fn menu_state_follows_the_app() {
        let mut app = fixture();
        let state = app.menu_state();
        assert!(state.can_act, "an idle app can act");
        assert!(!state.can_undo, "nothing has been removed yet");
        assert!(state.on_servers);
        assert!(!state.has_checked);
        assert!(!state.has_search);

        app.scanning = true;
        assert!(!app.menu_state().can_act, "a scan blocks the actions");
        app.scanning = false;
        app.busy = Some("removing".into());
        assert!(!app.menu_state().can_act, "a purge blocks the actions");
        app.busy = None;

        app.tab = Tab::Orphans;
        app.search = "rea".into();
        app.checked.insert("k".into());
        let state = app.menu_state();
        assert!(!state.on_servers);
        assert!(state.has_search);
        assert!(state.has_checked);
        assert_eq!(state.tab, Tab::Orphans);
    }

    /// Ticking a row for a batch must not also select it: the tick box sits
    /// inside a click-sensing cell, so without the checkbox winning the hit
    /// test every tick would yank the detail pane to a different server.
    #[test]
    fn ticking_a_row_does_not_open_the_detail_pane() {
        use egui_kittest::kittest::Queryable as _;

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .build_ui_state(
                |ui, app: &mut McpurgeApp| {
                    app.top_bar(ui);
                    app.bottom_bar(ui);
                    app.detail_panel(ui);
                    app.servers_table(ui);
                },
                fixture(),
            );
        harness.run();

        harness.get_by_label("select crawl4ai in Kiro").click();
        harness.run();

        assert_eq!(harness.state().checked.len(), 1, "the row is ticked");
        assert!(
            harness
                .state()
                .checked
                .iter()
                .all(|k| k.ends_with("::crawl4ai")),
            "and it is the right row"
        );
        assert!(
            harness.state().selected.is_none(),
            "but ticking is not selecting"
        );
        // The batch bar replaced the hint, so its actions are now reachable.
        harness.get_by_label("Remove 1\u{2026}");
    }

    #[test]
    fn the_detail_pane_renders_for_every_kind_of_server() {
        let keys: Vec<String> = populated().findings.iter().map(|f| f.entry.key()).collect();
        // Local+running, remote, and disabled/unknown-transport in turn.
        for key in keys {
            for reveal in [false, true] {
                let mut app = fixture();
                app.selected = Some(key.clone());
                app.reveal_env = reveal;
                egui::__run_test_ui(|ui| {
                    app.detail_panel(ui);
                    app.servers_table(ui);
                });
            }
        }
    }

    /// Both taupe variants, over every view. A palette that renders in dark
    /// and panics or vanishes in light is a theme that only half exists.
    #[test]
    fn both_themes_render_every_view() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            for tab in [Tab::Servers, Tab::Orphans, Tab::Coverage] {
                let mut app = fixture();
                app.tab = tab;
                app.theme = mode;
                app.selected = Some(populated().findings[0].entry.key());
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(egui::vec2(1280.0, 800.0))
                    .build_ui_state(
                        move |ui, app: &mut McpurgeApp| {
                            theme::apply(ui.ctx(), mode);
                            app.top_bar(ui);
                            app.bottom_bar(ui);
                            match app.tab {
                                Tab::Servers => {
                                    app.detail_panel(ui);
                                    app.servers_table(ui);
                                }
                                Tab::Orphans => app.orphans_panel(ui),
                                Tab::Coverage => app.coverage_panel(ui),
                            }
                        },
                        app,
                    );
                harness.run();
            }
        }
    }

    #[test]
    fn every_confirmation_dialog_renders() {
        let keys: Vec<String> = populated().findings.iter().map(|f| f.entry.key()).collect();

        let mut app = fixture();
        app.confirm = Some(Confirm::PurgeMany { keys: keys.clone() });
        app.purge_delete_files = true;
        app.purge_permanent = true;
        egui::__run_test_ctx(|ctx| app.confirm_dialog(ctx));
        assert!(app.confirm.is_some(), "an unanswered dialog stays open");

        let mut app = fixture();
        app.confirm = Some(Confirm::KillMany {
            labels: vec!["rea (Kiro)".into(), "rea (Claude Code)".into()],
            pids: vec![4242],
            tree: vec![4242, 4243],
        });
        egui::__run_test_ctx(|ctx| app.confirm_dialog(ctx));
        assert!(app.confirm.is_some());
    }

    /// A removal has to be visible. Waiting for the next scan would leave the
    /// table asserting the server is still configured when it is not.
    #[test]
    fn a_successful_removal_drops_the_row_at_once() {
        let mut app = fixture();
        let key = populated().findings[0].entry.key();
        app.selected = Some(key.clone());
        app.checked.insert(key.clone());
        let before = app.result.findings.len();

        app.tx
            .send(Msg::Purged {
                items: vec![PurgedItem {
                    label: "rea (Kiro)".into(),
                    key: key.clone(),
                    result: Ok(PurgeReport::default()),
                }],
            })
            .unwrap();
        app.pump();

        assert_eq!(app.result.findings.len(), before - 1);
        assert!(app.finding(&key).is_none(), "the removed row is gone");
        assert!(app.selected.is_none(), "the selection followed it");
        assert!(!app.checked.contains(&key), "so did the tick");
        assert!(app.busy.is_none());
    }

    /// The other half: a removal that failed must leave the row alone, or the
    /// UI would claim a server was unregistered when its config still has it.
    #[test]
    fn a_failed_removal_keeps_the_row() {
        let mut app = fixture();
        let key = populated().findings[0].entry.key();
        let before = app.result.findings.len();

        app.tx
            .send(Msg::Purged {
                items: vec![PurgedItem {
                    label: "rea (Kiro)".into(),
                    key: key.clone(),
                    result: Err("permission denied".into()),
                }],
            })
            .unwrap();
        app.pump();

        assert_eq!(app.result.findings.len(), before);
        assert!(app.finding(&key).is_some());
        assert!(app.log.iter().any(|l| l.text.contains("permission denied")));
    }

    #[test]
    fn a_batch_removal_drops_every_entry_that_succeeded() {
        let mut app = fixture();
        let keys: Vec<String> = populated().findings.iter().map(|f| f.entry.key()).collect();
        app.tx
            .send(Msg::Purged {
                items: vec![
                    PurgedItem {
                        label: "a".into(),
                        key: keys[0].clone(),
                        result: Ok(PurgeReport::default()),
                    },
                    PurgedItem {
                        label: "b".into(),
                        key: keys[1].clone(),
                        result: Err("nope".into()),
                    },
                    PurgedItem {
                        label: "c".into(),
                        key: keys[2].clone(),
                        result: Ok(PurgeReport::default()),
                    },
                ],
            })
            .unwrap();
        app.pump();

        assert!(app.finding(&keys[0]).is_none());
        assert!(app.finding(&keys[1]).is_some(), "the failure stays");
        assert!(app.finding(&keys[2]).is_none());
    }

    /// A process confirmed dead stops being shown as running, everywhere.
    #[test]
    fn a_stopped_process_disappears_from_the_view() {
        let mut app = fixture();
        assert!(app.result.findings[0].is_running());
        assert_eq!(app.result.orphans.len(), 1);

        app.tx
            .send(Msg::Killed {
                label: "rea (Kiro)".into(),
                report: KillReport {
                    outcomes: vec![
                        crate::proc::KillOutcome {
                            pid: 4242,
                            name: "npm exec".into(),
                            signalled: true,
                            exited: true,
                            escalated: false,
                            detail: "exited".into(),
                        },
                        crate::proc::KillOutcome {
                            pid: 4243,
                            name: "node".into(),
                            signalled: true,
                            exited: true,
                            escalated: false,
                            detail: "exited".into(),
                        },
                        crate::proc::KillOutcome {
                            pid: 9001,
                            name: "node".into(),
                            signalled: true,
                            exited: false,
                            escalated: true,
                            detail: "still alive".into(),
                        },
                    ],
                },
            })
            .unwrap();
        app.pump();

        assert!(
            app.result.findings.iter().all(|f| !f.is_running()),
            "both entries claiming pid 4242 stop showing it"
        );
        assert!(
            app.result
                .findings
                .iter()
                .all(|f| f.procs.iter().all(|p| !p.descendants.contains(&4243))),
            "the dead child is forgotten too"
        );
        assert_eq!(
            app.result.orphans.len(),
            1,
            "a process that refused to die is still listed"
        );
    }

    #[test]
    fn ticks_follow_the_filter_and_survive_only_while_the_server_does() {
        let mut app = fixture();
        let visible: Vec<String> = app.visible().iter().map(|f| f.entry.key()).collect();
        app.checked = visible.iter().cloned().collect();
        assert_eq!(app.checked_keys().len(), 4);

        // Narrowing the filter narrows the batch without losing the ticks.
        app.filter = Filter::Running;
        assert_eq!(app.checked_keys().len(), 2);
        assert_eq!(app.checked.len(), 4);
        app.filter = Filter::All;
        assert_eq!(app.checked_keys().len(), 4);

        // A rescan that no longer has a server drops its tick.
        let mut smaller = populated();
        smaller.findings.truncate(1);
        app.tx.send(Msg::ScanDone(Box::new(smaller))).unwrap();
        app.pump();
        assert_eq!(app.checked.len(), 1);
    }

    #[test]
    fn both_confirmation_dialogs_render() {
        let key = populated().findings[0].entry.key();
        let mut app = fixture();
        app.confirm = Some(Confirm::Purge { key });
        // With file deletion expanded, which is the branch with the most to draw.
        app.purge_delete_files = true;
        app.purge_permanent = true;
        egui::__run_test_ctx(|ctx| app.confirm_dialog(ctx));
        assert!(app.confirm.is_some(), "an unanswered dialog stays open");

        let mut app = fixture();
        app.confirm = Some(Confirm::Kill {
            label: "rea (Kiro)".into(),
            pids: vec![4242],
            tree: vec![4242, 4243],
        });
        egui::__run_test_ctx(|ctx| app.confirm_dialog(ctx));
        assert!(app.confirm.is_some());

        let mut app = fixture();
        app.confirm = Some(Confirm::KillOrphan {
            pid: 9001,
            label: "orphan node (pid 9001)".into(),
            tree: vec![9001],
        });
        egui::__run_test_ctx(|ctx| app.confirm_dialog(ctx));
        assert!(app.confirm.is_some());
    }

    fn an_update() -> update::Available {
        update::Available {
            version: semver::Version::new(9, 9, 9),
            tag: "v9.9.9".into(),
            notes: "- Something worth having\n- Something else\n".into(),
            binary_url: "https://example.test/mcpurge".into(),
            signature_url: "https://example.test/mcpurge.minisig".into(),
            size_bytes: 8_471_232,
        }
    }

    /// The About box is where someone compares the key mcpurge verifies with
    /// against the one published in the README, so the key ID has to actually
    /// be on screen. Nothing else in the app would notice if it stopped being.
    #[test]
    fn the_about_box_shows_the_signing_key() {
        use egui_kittest::kittest::Queryable as _;

        let mut app = fixture();
        app.about_open = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                |ui, app: &mut McpurgeApp| {
                    let ctx = ui.ctx().clone();
                    app.about_window(&ctx);
                },
                app,
            );
        harness.run();
        assert!(
            harness
                .query_by_label_contains(update::SIGNING_KEY_ID)
                .is_some(),
            "the About box does not name the release signing key"
        );
    }

    /// The updater's two windows, drawn through the real font stack: the offer
    /// and the "now restart" note. Neither is reachable from the table, so
    /// nothing else would notice if one of them panicked or drew a box.
    #[test]
    fn the_update_windows_render() {
        let mut app = fixture();
        app.update_available = Some(Box::new(an_update()));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                |ui, app: &mut McpurgeApp| {
                    let ctx = ui.ctx().clone();
                    app.update_window(&ctx);
                },
                app,
            );
        harness.run();
        {
            use egui_kittest::kittest::Queryable as _;
            harness.get_by_label("Download and install");
            harness.get_by_label("Not now");
        }

        let mut app = fixture();
        app.update_installed = Some("9.9.9".into());
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                |ui, app: &mut McpurgeApp| {
                    let ctx = ui.ctx().clone();
                    app.update_window(&ctx);
                },
                app,
            );
        harness.run();
    }

    /// An offer that is declined has to go away, or the window is a nag.
    #[test]
    fn declining_an_update_dismisses_it() {
        use egui_kittest::kittest::Queryable as _;

        let mut app = fixture();
        app.update_available = Some(Box::new(an_update()));
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 700.0))
            .build_ui_state(
                |ui, app: &mut McpurgeApp| {
                    let ctx = ui.ctx().clone();
                    app.update_window(&ctx);
                },
                app,
            );
        harness.run();
        harness.get_by_label("Not now").click();
        harness.run();
        assert!(
            harness.state().update_available.is_none(),
            "'Not now' should put the offer away"
        );
    }

    /// The messages the worker threads send back, without the network: a found
    /// release opens the offer, and nothing else is touched.
    #[test]
    fn an_update_message_opens_the_offer() {
        let mut app = fixture();
        app.update_busy = true;
        app.tx
            .send(Msg::UpdateChecked {
                result: Ok(Some(Box::new(an_update()))),
                asked_for: true,
            })
            .unwrap();
        app.pump();
        assert!(!app.update_busy, "the spinner stops");
        assert_eq!(
            app.update_available.as_ref().map(|a| a.version.to_string()),
            Some("9.9.9".into())
        );

        // A quiet "you are current" from a background check says nothing; the
        // same answer to a check the user asked for does.
        let mut app = fixture();
        app.tx
            .send(Msg::UpdateChecked {
                result: Ok(None),
                asked_for: false,
            })
            .unwrap();
        app.pump();
        assert!(app.log.is_empty(), "a quiet check stays quiet");

        app.tx
            .send(Msg::UpdateChecked {
                result: Ok(None),
                asked_for: true,
            })
            .unwrap();
        app.pump();
        assert_eq!(app.log.len(), 1, "an asked-for check answers");
    }

    /// A failed install must not claim success, and must leave the offer up so
    /// it can be tried again.
    #[test]
    fn a_failed_install_is_reported_and_keeps_the_offer() {
        let mut app = fixture();
        app.update_available = Some(Box::new(an_update()));
        app.update_busy = true;
        app.tx
            .send(Msg::UpdateInstalled {
                result: Err("the download is not signed by the mcpurge release key".into()),
                version: "9.9.9".into(),
            })
            .unwrap();
        app.pump();
        assert!(!app.update_busy);
        assert!(app.update_installed.is_none(), "nothing was installed");
        assert!(app.update_available.is_some(), "the offer stays");
        assert!(
            app.log.iter().any(|l| l.kind == LogKind::Warn
                && l.text.contains("not signed by the mcpurge release key")),
            "the refusal has to be visible, got {:?}",
            app.log.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_empty_result_renders_the_empty_state() {
        let mut app = McpurgeApp::with_registry(Registry::builtin().unwrap());
        for tab in [Tab::Servers, Tab::Orphans, Tab::Coverage] {
            app.tab = tab;
            egui::__run_test_ui(|ui| {
                app.top_bar(ui);
                app.bottom_bar(ui);
                match app.tab {
                    Tab::Servers => app.servers_table(ui),
                    Tab::Orphans => app.orphans_panel(ui),
                    Tab::Coverage => app.coverage_panel(ui),
                }
            });
        }
    }

    #[test]
    fn filters_and_search_narrow_the_table() {
        let mut app = fixture();
        // rea in two clients, crawl4ai, godot-ai.
        assert_eq!(app.visible().len(), 4);

        app.filter = Filter::Running;
        assert_eq!(app.visible().len(), 2);
        assert!(app.visible().iter().all(|f| f.entry.name == "rea"));

        app.filter = Filter::Stopped;
        assert_eq!(app.visible().len(), 2);

        app.filter = Filter::Remote;
        assert_eq!(
            app.visible().len(),
            2,
            "http and unknown are both non-local"
        );

        app.filter = Filter::Local;
        assert_eq!(app.visible().len(), 2);

        // Disabled servers can be hidden.
        app.filter = Filter::All;
        app.show_disabled = false;
        assert_eq!(app.visible().len(), 3);

        // Search matches name, client, command and path.
        app.show_disabled = true;
        app.search = "rea-agents".into();
        assert_eq!(app.visible().len(), 2, "should match on the command line");
        app.search = "kiro".into();
        assert_eq!(app.visible().len(), 3, "should match on the client name");
        app.search = "nothing-matches-this".into();
        assert!(app.visible().is_empty());
    }

    #[test]
    fn a_process_claimed_by_two_entries_is_counted_twice() {
        // The "(shared)" marker in the table depends on this count. The
        // fixture registers the same server in Kiro and in Claude Code, both
        // resolving to pid 4242.
        let app = fixture();
        let claims = app.pid_claims();
        assert_eq!(claims.get(&4242), Some(&2));
        // Only the pids the table shows are counted; descendants are not.
        assert_eq!(claims.get(&4243), None);
    }

    #[test]
    fn scan_messages_update_the_log_and_clear_the_spinner() {
        let mut app = fixture();
        app.scanning = true;
        app.tx
            .send(Msg::Progress("sweeping /Users/test".into()))
            .unwrap();
        app.pump();
        assert_eq!(app.progress, "sweeping /Users/test");
        assert!(app.scanning);

        app.tx.send(Msg::ScanDone(Box::new(populated()))).unwrap();
        app.pump();
        assert!(!app.scanning, "the spinner must stop when the scan lands");
        assert!(app.progress.is_empty());
        assert!(
            app.log.iter().any(|l| l.text.contains("4 server(s)")),
            "the scan summary should be logged"
        );
        // The parse error from the result is surfaced as a warning.
        assert!(app.log.iter().any(|l| l.kind == LogKind::Warn));
    }

    #[test]
    fn a_selection_that_disappears_is_dropped() {
        let mut app = fixture();
        app.selected = Some("gone::/nowhere::x".into());
        app.tx.send(Msg::ScanDone(Box::new(populated()))).unwrap();
        app.pump();
        assert!(
            app.selected.is_none(),
            "a stale selection must not survive a rescan"
        );
    }
}
