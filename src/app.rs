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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::model::{
    ArtifactKind, Finding, MatchConfidence, ScanResult, human_bytes, human_duration,
};
use crate::proc::{self, KillReport};
use crate::purge::{self, Manifest, PurgeOptions, PurgeReport};
use crate::registry::{Registry, RegistrySource};
use crate::scan::{self, ScanOptions};

/// Launch the GUI.
pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([920.0, 520.0])
            .with_title("mcpurge"),
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
    Purged {
        label: String,
        result: Result<PurgeReport, String>,
    },
    Undone {
        result: Result<PurgeReport, String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Servers,
    Orphans,
    Coverage,
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
    confirm: Option<Confirm>,
    busy: Option<String>,
    /// Opt-ins for the purge dialog, remembered between uses.
    purge_delete_files: bool,
    purge_permanent: bool,
    allow_sigkill: bool,
    reveal_env: bool,
    log: Vec<LogLine>,
    last_manifest: Option<Manifest>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl McpurgeApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let registry = Registry::load().unwrap_or_else(|e| {
            eprintln!("falling back to the built-in registry: {e}");
            Registry::builtin().expect("the bundled registry must parse")
        });
        let mut app = Self::with_registry(registry);
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
            confirm: None,
            busy: None,
            purge_delete_files: false,
            purge_permanent: false,
            allow_sigkill: true,
            reveal_env: false,
            log: Vec::new(),
            last_manifest: None,
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
                    // Keep the selection if that server still exists.
                    if let Some(sel) = &self.selected
                        && !result.findings.iter().any(|f| &f.entry.key() == sel)
                    {
                        self.selected = None;
                    }
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
                }
                Msg::Purged { label, result } => {
                    self.busy = None;
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
                        }
                        Err(e) => self.warn(format!("{label}: {e}")),
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
            }
        }
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

    fn spawn_purge(&mut self, ctx: &egui::Context, key: String) {
        let Some(finding) = self.finding(&key) else {
            return;
        };
        let entry = finding.entry.clone();
        let artifacts = finding.artifacts.clone();
        let label = format!("{} ({})", entry.name, entry.client_name);
        let opts = PurgeOptions {
            remove_config: true,
            delete_files: self.purge_delete_files,
            permanent: self.purge_permanent,
        };
        self.busy = Some(format!("removing {label}"));
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = purge::purge(&entry, &artifacts, &opts).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Purged { label, result });
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

fn confidence_badge(ui: &mut egui::Ui, c: MatchConfidence) {
    let (text, color) = match c {
        MatchConfidence::Strong => ("strong", egui::Color32::from_rgb(64, 190, 120)),
        MatchConfidence::Probable => ("probable", egui::Color32::from_rgb(220, 180, 70)),
        MatchConfidence::Loose => ("loose", egui::Color32::from_rgb(200, 120, 90)),
    };
    ui.colored_label(color, format!("[{text}]"))
        .on_hover_text("how confident the process match is");
}

impl eframe::App for McpurgeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();

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
        self.confirm_dialog(&ctx);
    }
}

impl McpurgeApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();
        egui::Panel::top("top").show(ui, |ui| {
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
                ui.selectable_value(&mut self.tab, Tab::Servers, "Servers");
                ui.selectable_value(
                    &mut self.tab,
                    Tab::Orphans,
                    format!("Orphans ({})", self.result.orphans.len()),
                );
                ui.selectable_value(&mut self.tab, Tab::Coverage, "Coverage");
                ui.separator();

                if self.tab == Tab::Servers {
                    egui::ComboBox::from_id_salt("filter")
                        .selected_text(self.filter.label())
                        .show_ui(ui, |ui| {
                            for f in Filter::ALL {
                                ui.selectable_value(&mut self.filter, f, f.label());
                            }
                        });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("search name, client, command or path")
                            .desired_width(240.0),
                    );
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

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(130.0)
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

            let claims = self.pid_claims();
            let row_height = 24.0;
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(16.0))
                .column(Column::initial(170.0).at_least(90.0))
                .column(Column::initial(150.0).at_least(80.0))
                .column(Column::initial(120.0).at_least(70.0))
                .column(Column::initial(70.0).at_least(55.0))
                .column(Column::initial(150.0).at_least(80.0))
                .column(Column::remainder().at_least(90.0))
                .header(22.0, |mut header| {
                    header.col(|_| {});
                    header.col(|ui| {
                        ui.strong("Server");
                    });
                    header.col(|ui| {
                        ui.strong("Client");
                    });
                    header.col(|ui| {
                        ui.strong("Scope");
                    });
                    header.col(|ui| {
                        ui.strong("Transport");
                    });
                    header.col(|ui| {
                        ui.strong("State");
                    });
                    header.col(|ui| {
                        ui.strong("On disk");
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
                                .on_hover_text(finding.entry.source.path.display().to_string());
                        });
                        row.col(|ui| {
                            ui.label(finding.entry.transport.label());
                        });
                        row.col(|ui| {
                            if finding.procs.is_empty() {
                                ui.weak(if finding.entry.disabled {
                                    "disabled"
                                } else {
                                    "stopped"
                                });
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
                                    if shared { " \u{26a0}" } else { "" }
                                );
                                let resp =
                                    ui.colored_label(egui::Color32::from_rgb(64, 190, 120), label);
                                if shared {
                                    resp.on_hover_text(
                                        "this process is also claimed by another config entry: \
                                         the same server is registered in more than one client, \
                                         so stopping it affects both",
                                    );
                                }
                            }
                        });
                        row.col(|ui| {
                            let bytes: u64 =
                                finding.deletable_artifacts().map(|a| a.size_bytes).sum();
                            if bytes > 0 {
                                ui.label(human_bytes(bytes));
                            } else if !finding.artifacts.is_empty() {
                                ui.weak("protected");
                            } else {
                                ui.weak("\u{2014}");
                            }
                        });

                        if row.response().clicked() {
                            self.selected = Some(key.clone());
                        }
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
                                if a.shared {
                                    ui.colored_label(egui::Color32::from_gray(140), "\u{1f512}")
                                        .on_hover_text("protected: never deleted");
                                } else {
                                    ui.label("\u{2022}");
                                }
                                ui.label(human_bytes(a.size_bytes));
                                ui.weak(format!("[{}]", a.kind.label()));
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

                    // ---- actions -----------------------------------------------
                    ui.add_space(10.0);
                    ui.separator();
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
                                .flat_map(|p| {
                                    std::iter::once(p.pid).chain(p.descendants.iter().copied())
                                })
                                .collect();
                            self.confirm = Some(Confirm::Kill {
                                label: format!("{} ({})", finding.entry.name, finding.entry.client_name),
                                pids: finding.procs.iter().map(|p| p.pid).collect(),
                                tree,
                            });
                        }
                        if ui
                            .add_enabled(can_act, egui::Button::new("Remove\u{2026}"))
                            .on_hover_text("unregister from this client, with a backup first")
                            .clicked()
                        {
                            self.confirm = Some(Confirm::Purge { key: key.clone() });
                        }
                    });
                });
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
                    ui.add_space(4.0);
                    ui.heading("What mcpurge looked at");
                    ui.label(format!(
                        "{} config file(s) parsed in {} ms. Registry: {}.",
                        self.result.scanned_paths.len(),
                        self.result.duration_ms,
                        match &self.registry.source {
                            RegistrySource::Builtin => "built in".to_string(),
                            RegistrySource::User(p) => p.display().to_string(),
                        }
                    ));
                    if let Some(p) = crate::registry::user_registry_path() {
                        ui.weak(format!(
                            "Add a client without rebuilding by copying the registry to {}",
                            p.display()
                        ));
                    }

                    // Configs that are MCP-shaped but match no known client. This
                    // is the honest coverage report: the sweep found them, the
                    // registry could not name them.
                    ui.add_space(8.0);
                    ui.strong(format!(
                        "Unrecognised MCP configs ({})",
                        self.result.unknown_configs.len()
                    ));
                    if self.result.unknown_configs.is_empty() {
                        ui.weak(if self.sweep {
                            "None: every MCP config found belongs to a known client."
                        } else {
                            "Deep sweep is off, so only known locations were checked."
                        });
                    } else {
                        ui.label(
                            "These declare MCP servers but match no registry entry. Their servers \
                         are listed under \"unidentified client\".",
                        );
                        for u in &self.result.unknown_configs {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!("{} server(s)", u.server_count));
                                ui.weak(format!("[{}]", u.container_keys.join(", ")));
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

                    // Runtimes with no MCP support, so their absence is explained.
                    if !self.result.runtimes.is_empty() {
                        ui.add_space(8.0);
                        ui.strong("Local model runtimes (not MCP clients)");
                        for r in &self.result.runtimes {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new(&r.name).strong());
                                ui.weak(format!("{} process(es)", r.pids.len()));
                            });
                            ui.weak(&r.detail);
                        }
                    }

                    ui.add_space(8.0);
                    ui.collapsing(
                        format!("Known clients ({})", self.registry.clients.len()),
                        |ui| {
                            ui.label(
                                "Paths are hints. Vendors move their config files, so anything \
                             missed here is still caught by the content sweep.",
                            );
                            for c in &self.registry.clients {
                                let found = self
                                    .result
                                    .findings
                                    .iter()
                                    .filter(|f| f.entry.client_id == c.id)
                                    .count();
                                ui.horizontal_wrapped(|ui| {
                                    if found > 0 {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(64, 190, 120),
                                            format!("{found}"),
                                        );
                                    } else {
                                        ui.weak("\u{2014}");
                                    }
                                    ui.label(egui::RichText::new(&c.name).strong());
                                    ui.weak(format!("[{}]", c.confidence.label()));
                                    ui.weak(c.container_keys.join(", "));
                                });
                            }
                        },
                    );

                    if !self.result.errors.is_empty() {
                        ui.add_space(8.0);
                        ui.collapsing(
                            format!(
                                "Files that could not be read ({})",
                                self.result.errors.len()
                            ),
                            |ui| {
                                for e in &self.result.errors {
                                    ui.weak(e);
                                }
                            },
                        );
                    }

                    ui.add_space(8.0);
                    ui.collapsing(
                        format!("Parsed config files ({})", self.result.scanned_paths.len()),
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

        // A configured server and a loose orphan pose the same question, so
        // both reduce to one target before the dialog is built.
        let kill_target: Option<(String, Vec<u32>, Vec<u32>)> = match &confirm {
            Confirm::Kill { label, pids, tree } => {
                Some((label.clone(), pids.clone(), tree.clone()))
            }
            Confirm::KillOrphan { label, pid, tree } => {
                Some((label.clone(), vec![*pid], tree.clone()))
            }
            Confirm::Purge { .. } => None,
        };

        if let Some((label, pids, tree)) = kill_target {
            {
                egui::Modal::new(egui::Id::unique("mcpurge-confirm-kill")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading("Stop this server?");
                    ui.add_space(4.0);
                    ui.label(label.as_str());
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
                        if ui.button(egui::RichText::new("Stop it").strong()).clicked() {
                            self.spawn_kill(ctx, label.clone(), pids.clone());
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
                        if ui
                            .add_enabled(
                                editable,
                                egui::Button::new(egui::RichText::new("Remove").strong()),
                            )
                            .clicked()
                        {
                            let key = key.clone();
                            self.spawn_purge(ctx, key);
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

        ScanResult {
            findings: vec![running, remote, disabled],
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
        assert_eq!(app.visible().len(), 3);

        app.filter = Filter::Running;
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.visible()[0].entry.name, "rea");

        app.filter = Filter::Stopped;
        assert_eq!(app.visible().len(), 2);

        app.filter = Filter::Remote;
        assert_eq!(
            app.visible().len(),
            2,
            "http and unknown are both non-local"
        );

        app.filter = Filter::Local;
        assert_eq!(app.visible().len(), 1);

        // Disabled servers can be hidden.
        app.filter = Filter::All;
        app.show_disabled = false;
        assert_eq!(app.visible().len(), 2);

        // Search matches name, client, command and path.
        app.show_disabled = true;
        app.search = "rea-agents".into();
        assert_eq!(app.visible().len(), 1, "should match on the command line");
        app.search = "kiro".into();
        assert_eq!(app.visible().len(), 3, "should match on the client name");
        app.search = "nothing-matches-this".into();
        assert!(app.visible().is_empty());
    }

    #[test]
    fn a_process_claimed_by_two_entries_is_counted_twice() {
        // The warning marker in the table depends on this count.
        let mut result = populated();
        let mut duplicate = result.findings[0].clone();
        duplicate.entry.client_id = "claude-code".into();
        duplicate.entry.client_name = "Claude Code".into();
        duplicate.entry.source.path = "/Users/test/.claude.json".into();
        result.findings.push(duplicate);

        let mut app = fixture();
        app.result = result;
        let claims = app.pid_claims();
        assert_eq!(claims.get(&4242), Some(&2));
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
            app.log.iter().any(|l| l.text.contains("3 server(s)")),
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
