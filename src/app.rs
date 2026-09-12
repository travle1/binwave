use crate::binary_ui::{BinaryWorkspace, OpenedFile};
use crate::cache::LoadProgress;
use crate::config::AppConfig;
use crate::csv_table::CsvTable;
use crate::data::DataStore;
use crate::file_dialog::{FileKind as DialogFileKind, FileLocations, NativeFileDialog};
use crate::loaders::{file_kind, load_data, FileKind};
use crate::plotting::{PlotBuilder, PlotLines};
use crate::workspace_layout::{Pane, WorkspaceLayout};
use anyhow::Result;
use egui::{Context, Ui};
use egui_dock::{DockArea, TabViewer};
use egui_plot::{Legend, Line, Plot, PlotBounds};
use std::sync::mpsc::{self, Receiver};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Instant;

const DEFINITION_EXAMPLES: &[(&str, &str)] = &[
    (
        "Primitive records",
        include_str!("../samples/decoders/yaml/generic_primitives.yaml"),
    ),
    (
        "Types described by earlier sections",
        include_str!("../samples/decoders/yaml/generic_sections.yaml"),
    ),
    (
        "PNG chunks",
        include_str!("../samples/decoders/yaml/png_chunks.yaml"),
    ),
    (
        "RIFF chunks",
        include_str!("../samples/decoders/yaml/riff_chunks.yaml"),
    ),
];

#[derive(Debug, Clone)]
struct ViewState {
    center: f64,
    span: f64,
}

impl ViewState {
    const MIN_SPAN: f64 = 1e-9;

    fn from_store(store: &DataStore) -> Self {
        let mut this = Self {
            center: (store.time_min + store.time_max) * 0.5,
            span: (store.time_max - store.time_min).abs().max(Self::MIN_SPAN),
        };
        this.clamp(store);
        this
    }

    fn clamp(&mut self, store: &DataStore) {
        let total = (store.time_max - store.time_min).abs().max(Self::MIN_SPAN);
        self.span = self.span.clamp(Self::MIN_SPAN, total);
        let half = self.span * 0.5;
        if store.time_max - store.time_min <= Self::MIN_SPAN {
            self.center = store.time_min;
        } else {
            self.center = self
                .center
                .clamp(store.time_min + half, store.time_max - half);
        }
    }

    fn bounds(&self, store: &DataStore) -> (f64, f64) {
        let mut copy = self.clone();
        copy.clamp(store);
        let half = copy.span * 0.5;
        (copy.center - half, copy.center + half)
    }

    fn set_center_span(&mut self, center: f64, span: f64, store: &DataStore) {
        self.center = center;
        self.span = span;
        self.clamp(store);
    }

    fn set_full(&mut self, store: &DataStore) {
        self.center = (store.time_min + store.time_max) * 0.5;
        self.span = (store.time_max - store.time_min).abs().max(Self::MIN_SPAN);
        self.clamp(store);
    }

    fn nudge_fraction(&mut self, fraction: f64, store: &DataStore) {
        self.center += self.span * fraction;
        self.clamp(store);
    }

    fn jump_to_start(&mut self, store: &DataStore) {
        self.span = self
            .span
            .min((store.time_max - store.time_min).abs().max(Self::MIN_SPAN));
        let half = self.span * 0.5;
        self.center = store.time_min + half;
        self.clamp(store);
    }

    fn jump_to_end(&mut self, store: &DataStore) {
        self.span = self
            .span
            .min((store.time_max - store.time_min).abs().max(Self::MIN_SPAN));
        let half = self.span * 0.5;
        self.center = store.time_max - half;
        self.clamp(store);
    }

    fn sync_from_bounds(&mut self, xmin: f64, xmax: f64, store: &DataStore) {
        self.center = (xmin + xmax) * 0.5;
        self.span = (xmax - xmin).abs().max(Self::MIN_SPAN);
        self.clamp(store);
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CsvPlotKey {
    dataset: u64,
    generation: u64,
    bounds: [u64; 2],
    visible: Vec<bool>,
    target: usize,
    max_lines: usize,
}

impl CsvPlotKey {
    fn same_view(&self, other: &Self) -> bool {
        self.dataset == other.dataset
            && self.bounds == other.bounds
            && self.visible == other.visible
            && self.target == other.target
            && self.max_lines == other.max_lines
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Decoder {
    #[default]
    Layout,
    Csv,
}

#[derive(Debug, Clone, Copy)]
enum AppDialogAction {
    OpenData,
    LoadCsvConfig,
}

pub struct PlotterApp {
    cfg: AppConfig,
    generation: u64,
    dataset: u64,
    plot_rx: Option<Receiver<(CsvPlotKey, Result<PlotLines>)>>,
    plot_requested: Option<CsvPlotKey>,
    plot_completed: Option<CsvPlotKey>,
    plot_lines: PlotLines,
    plot_cancel: Arc<AtomicBool>,
    plot_error: Option<String>,
    data: Option<DataStore>,
    visible: Vec<bool>,
    search: String,
    status: String,
    loader_rx: Option<Receiver<Result<DataStore>>>,
    load_preview: Arc<Mutex<Option<DataStore>>>,
    load_has_data: bool,
    data_partial: bool,
    follow_loading: bool,
    view: Option<ViewState>,
    binary: BinaryWorkspace,
    decoder: Decoder,
    workspace: WorkspaceLayout,
    show_empty_workspace: bool,
    show_decoding_guide: bool,
    csv_table: CsvTable,
    pending_config: Option<AppConfig>,
    file_dialog: NativeFileDialog<AppDialogAction>,
    file_locations: FileLocations,
    dialog_template: rfd::AsyncFileDialog,
    load_progress: Arc<LoadProgress>,
    load_started: Instant,
    cache_message: Option<String>,
}

impl Default for PlotterApp {
    fn default() -> Self {
        let cfg = AppConfig::default();
        let file_locations = FileLocations::default();
        let mut binary = BinaryWorkspace::python_workspace();
        binary.set_file_locations(file_locations.clone());
        Self {
            cfg,
            generation: 0,
            dataset: 0,
            plot_rx: None,
            plot_requested: None,
            plot_completed: None,
            plot_lines: Vec::new(),
            plot_cancel: Arc::new(AtomicBool::new(false)),
            plot_error: None,
            data: None,
            visible: Vec::new(),
            search: String::new(),
            status: "Open a file to begin".into(),
            loader_rx: None,
            load_preview: Arc::new(Mutex::new(None)),
            load_has_data: false,
            data_partial: false,
            follow_loading: true,
            view: None,
            binary,
            decoder: Decoder::Layout,
            workspace: WorkspaceLayout::default(),
            show_empty_workspace: false,
            show_decoding_guide: false,
            csv_table: CsvTable::default(),
            pending_config: None,
            file_dialog: NativeFileDialog::default(),
            file_locations,
            dialog_template: rfd::AsyncFileDialog::new(),
            load_progress: Arc::new(LoadProgress::default()),
            load_started: Instant::now(),
            cache_message: None,
        }
    }
}

impl Drop for PlotterApp {
    fn drop(&mut self) {
        self.plot_cancel.store(true, Ordering::Relaxed);
        self.load_progress.cancelled.store(true, Ordering::Relaxed);
    }
}

impl PlotterApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        app.workspace = WorkspaceLayout::load(cc.storage);
        app.restore_file_locations(cc.storage);
        app
    }

    fn restore_file_locations(&mut self, storage: Option<&dyn eframe::Storage>) {
        self.file_locations = FileLocations::load(storage);
        self.binary.set_file_locations(self.file_locations.clone());
    }

    fn begin_load(&mut self, cfg: AppConfig, ctx: &Context) {
        self.load_progress.cancelled.store(true, Ordering::Relaxed);
        self.load_progress = Arc::new(LoadProgress::default());
        self.load_started = Instant::now();
        self.cfg = cfg.clone();
        self.status = format!("Checking CSV cache / loading: {:?}", cfg.data_path);

        // One replaceable preview slot bounds memory even when the UI is idle.
        self.load_preview = Arc::new(Mutex::new(None));
        self.load_has_data = false;
        let (tx, rx) = mpsc::channel();
        self.loader_rx = Some(rx);
        let progress = self.load_progress.clone();
        let preview = self.load_preview.clone();
        ctx.request_repaint();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let res = load_data(&cfg, &progress, |data| {
                *preview.lock().unwrap_or_else(|e| e.into_inner()) = Some(data);
                ctx.request_repaint();
            });
            let _ = tx.send(res);
            ctx.request_repaint();
        });
    }

    fn accept_data(&mut self, data: DataStore, partial: bool) {
        self.generation = self.generation.wrapping_add(1);
        if !self.load_has_data {
            if self.binary.has_file() && self.decoder == Decoder::Csv {
                self.workspace.show_samples();
            }
            self.dataset = self.dataset.wrapping_add(1);
            self.plot_lines.clear();
        }
        if !self.load_has_data {
            self.visible = vec![true; data.signals.len()];
            self.view = Some(ViewState::from_store(&data));
            self.follow_loading = true;
        } else if self.follow_loading {
            if let Some(view) = &mut self.view {
                view.set_full(&data);
            }
        }
        self.status = if partial {
            format!(
                "{} rows / {} signals ready · loading more…",
                data.rows,
                data.signals.len()
            )
        } else {
            format!(
                "Loaded {} rows / {} signals in {:.2}s · {}",
                data.rows,
                data.signals.len(),
                self.load_started.elapsed().as_secs_f64(),
                if self.load_progress.cache_hit.load(Ordering::Relaxed) {
                    format!("{} · index/cache hit", data.storage)
                } else {
                    data.storage.clone()
                }
            )
        };
        self.data = Some(data);
        self.load_has_data = true;
        self.data_partial = partial;
    }

    fn poll_loader(&mut self, ctx: &Context) {
        if self.loader_rx.is_none() {
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
        let preview = self
            .load_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(data) = preview {
            self.accept_data(data, true);
        }
        let result = match self.loader_rx.as_ref().unwrap().try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("CSV worker stopped unexpectedly")))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        };
        if let Some(result) = result {
            match result {
                Ok(data) => self.accept_data(data, false),
                Err(err) => {
                    let stopped = self.load_progress.cancelled.load(Ordering::Relaxed);
                    self.status = if stopped {
                        "Loading cancelled.".into()
                    } else {
                        format!("Load error: {err:#}")
                    };
                    if self.load_has_data {
                        self.status.push_str(&format!(" {} loaded rows remain usable; this is an incomplete file. Reload CSV to finish.", self.data.as_ref().unwrap().rows));
                    }
                }
            }
            self.loader_rx = None;
        }
    }

    fn ui_left_panel(&mut self, ui: &mut Ui) {
        ui.collapsing("CSV settings", |ui| {
            ui.label("Storage: original CSV with a compact index (default)");
            ui.horizontal(|ui| {
                ui.label("Decoded RAM cache (MiB)");
                ui.add(egui::DragValue::new(&mut self.cfg.csv.ram_cache_mib).clamp_range(1..=4096));
            });
            ui.checkbox(
                &mut self.cfg.csv.decoded_disk_cache,
                "Keep a full decoded copy on disk",
            );
            if self.cfg.csv.decoded_disk_cache {
                ui.small(
                    "Optional: faster rereads, but the copy can exceed the original file size.",
                );
                ui.horizontal(|ui| {
                    ui.label("Total decoded data limit (MiB)");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.csv.decoded_cache_limit_mib)
                            .clamp_range(1..=1_048_576),
                    );
                });
                ui.small(
                    "Uses the original CSV when the copy cannot fit. Index metadata is additional.",
                );
            }
            if ui
                .checkbox(&mut self.cfg.csv.has_header, "Has header row")
                .changed()
            {
                if self.cfg.csv.time_column == "time" || self.cfg.csv.time_column == "c0" {
                    self.cfg.csv.time_column = if self.cfg.csv.has_header {
                        "time"
                    } else {
                        "c0"
                    }
                    .into();
                }
            }
            ui.horizontal(|ui| {
                ui.label("Time column");
                ui.text_edit_singleline(&mut self.cfg.csv.time_column);
            });
            ui.horizontal(|ui| {
                ui.label("Delimiter");
                let mut delimiter = self.cfg.csv.delimiter.to_string();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut delimiter)
                            .char_limit(1)
                            .desired_width(30.),
                    )
                    .changed()
                {
                    if let Some(delimiter) = delimiter.chars().next() {
                        self.cfg.csv.delimiter = delimiter;
                    }
                }
            });
            if ui
                .add_enabled(
                    self.cfg.data_path.is_some(),
                    egui::Button::new("Reload CSV"),
                )
                .clicked()
            {
                self.begin_load(self.cfg.clone(), ui.ctx());
            }
        });
        ui.label(&self.status);
        if self.loader_rx.is_some() {
            let done = self.load_progress.done.load(Ordering::Relaxed);
            let total = self.load_progress.total.load(Ordering::Relaxed);
            ui.add(
                egui::ProgressBar::new(if total > 0 {
                    done as f32 / total as f32
                } else {
                    0.
                })
                .text(format!(
                    "{:.1} / {:.1} MiB · {:.1}s",
                    done as f64 / 1048576.,
                    total as f64 / 1048576.,
                    self.load_started.elapsed().as_secs_f64()
                )),
            );
            if ui.button("Cancel loading").clicked() {
                self.load_progress.cancelled.store(true, Ordering::Relaxed);
            }
        }

        if self.data.is_some() {
            let (time_min, time_max, signals_len) = {
                let ds = self.data.as_ref().unwrap();
                (ds.time_min, ds.time_max, ds.signals.len())
            };
            ui.separator();
            ui.label(format!(
                "Time: [{:.3}, {:.3}]  signals: {}",
                time_min, time_max, signals_len
            ));
            if self.loader_rx.is_some() && self.load_has_data {
                if ui
                    .checkbox(&mut self.follow_loading, "Follow loaded range")
                    .changed()
                    && self.follow_loading
                {
                    self.view
                        .as_mut()
                        .unwrap()
                        .set_full(self.data.as_ref().unwrap());
                }
            }
            self.ui_view_controls(ui);

            ui.horizontal(|ui| {
                if ui.button("Select All").clicked() {
                    self.visible.iter_mut().for_each(|v| *v = true);
                }
                if ui.button("Select None").clicked() {
                    self.visible.iter_mut().for_each(|v| *v = false);
                }
            });

            ui.text_edit_singleline(&mut self.search);
            let search_lower = self.search.to_lowercase();

            if let Some(ds) = &self.data {
                for (i, s) in ds.signals.iter().enumerate() {
                    if !search_lower.is_empty() && !s.name.to_lowercase().contains(&search_lower) {
                        continue;
                    }
                    let mut vis = self.visible.get(i).copied().unwrap_or(true);
                    if ui.checkbox(&mut vis, &s.name).changed() {
                        if i < self.visible.len() {
                            self.visible[i] = vis;
                        }
                    }
                }
            }
        }
    }

    fn open_path(&mut self, path: std::path::PathBuf, ctx: &Context) {
        if !self.commands_available() {
            return;
        }
        self.pending_config = None;
        self.binary.open(path, ctx);
    }

    fn clear_csv(&mut self) {
        self.plot_cancel.store(true, Ordering::Relaxed);
        self.load_progress.cancelled.store(true, Ordering::Relaxed);
        self.loader_rx = None;
        self.load_preview = Arc::new(Mutex::new(None));
        self.plot_rx = None;
        self.plot_requested = None;
        self.plot_completed = None;
        self.plot_lines.clear();
        self.plot_error = None;
        self.data = None;
        self.visible.clear();
        self.view = None;
        self.load_has_data = false;
        self.data_partial = false;
        self.csv_table = CsvTable::default();
        self.dataset = self.dataset.wrapping_add(1);
        self.cfg.data_path = None;
        self.status = "Choose a decoder to populate Plot and Table.".into();
    }

    fn poll_workspace(&mut self, ctx: &Context) {
        self.poll_file_dialog(ctx);
        self.binary.tick(ctx);
        if let Some(opened) = self.binary.take_opened_file() {
            self.clear_csv();
            self.decoder = Decoder::Layout;
            self.workspace.reveal(Pane::Hex);
            if let OpenedFile::Path(path) = opened {
                let config = self
                    .pending_config
                    .take()
                    .filter(|cfg| cfg.data_path.as_ref() == Some(&path));
                let is_csv = config.is_some() || file_kind(&path) == FileKind::Csv;
                let mut cfg = config.unwrap_or_else(|| self.cfg.clone());
                cfg.data_path = Some(path);
                self.cfg = cfg.clone();
                if is_csv {
                    self.decoder = Decoder::Csv;
                    self.begin_load(cfg, ctx);
                }
            } else {
                self.pending_config = None;
            }
            if self.decoder == Decoder::Layout {
                self.binary.show_yaml();
                self.workspace.show_decoder();
            } else {
                self.workspace.reveal(Pane::Inspector);
            }
        }
        if self.binary.take_bytes_changed() {
            self.invalidate_csv_after_edit();
        }
        if self.binary.take_layout_applied() {
            self.workspace.show_samples();
            self.decoder = Decoder::Layout;
            self.load_progress.cancelled.store(true, Ordering::Relaxed);
        }
        self.reveal_requested_panels();
        self.poll_loader(ctx);
    }

    fn invalidate_csv_after_edit(&mut self) {
        // The CSV reader addresses the source file. Its results cannot represent an
        // overlay edit, including results still in flight when the edit occurred.
        let source = self.cfg.data_path.clone();
        self.clear_csv();
        self.cfg.data_path = source;
        self.decoder = Decoder::Layout;
        self.status = "Bytes changed. CSV views were cleared; save and reopen the edited copy to decode it as CSV.".into();
    }

    fn reveal_requested_panels(&mut self) {
        if self.binary.take_reveal_definition() {
            self.show_empty_workspace = true;
            self.workspace.show_decoder();
        }
        if self.binary.take_reveal_hex() {
            self.show_empty_workspace = true;
            self.workspace.reveal(Pane::Hex);
        }
    }

    fn ui_inspector(&mut self, ui: &mut Ui) {
        self.binary.selection_inspector(ui);
        if !self.binary.has_file() {
            return;
        }
        egui::CollapsingHeader::new("Data interpretation")
            .default_open(self.decoder == Decoder::Csv)
            .show(ui, |ui| {
                ui.separator();
                let previous = self.decoder;
                ui.label("Interpretation for Plot and Table");
                egui::ComboBox::from_id_source("workspace_decoder")
                    .selected_text(match self.decoder {
                        Decoder::Layout => "Decoder",
                        Decoder::Csv => "CSV columns",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.decoder, Decoder::Layout, "Decoder");
                        ui.add_enabled_ui(
                            self.binary.source_path().is_some() && !self.binary.has_byte_edits(),
                            |ui| {
                                ui.selectable_value(&mut self.decoder, Decoder::Csv, "CSV columns");
                            },
                        );
                    });
                if self.binary.has_byte_edits() {
                    ui.small("To interpret edited bytes as CSV, save a copy and open that file.");
                }
                if previous != self.decoder {
                    if self.decoder == Decoder::Csv
                        && self.data.is_none()
                        && self.loader_rx.is_none()
                    {
                        self.begin_load(self.cfg.clone(), ui.ctx());
                    } else if self.decoder == Decoder::Layout {
                        self.load_progress.cancelled.store(true, Ordering::Relaxed);
                    }
                }
                if self.decoder == Decoder::Csv {
                    self.ui_left_panel(ui);
                }
            });
    }

    fn show_view(&mut self, pane: Pane) {
        self.show_empty_workspace = true;
        if pane == Pane::Table {
            self.workspace.show_samples();
        } else if pane == Pane::Layout {
            self.workspace.show_decoder();
        } else {
            self.workspace.reveal(pane);
        }
    }

    fn commands_available(&self) -> bool {
        !self.file_dialog.pending() && self.binary.commands_available()
    }

    fn choose_data_file(&mut self, ctx: &Context) {
        if !self.commands_available() {
            return;
        }
        let dialog = self.file_locations.configure(
            DialogFileKind::Data,
            self.dialog_template
                .clone()
                .set_title("Open file — Plotter Max"),
        );
        self.file_dialog
            .begin_open(dialog, AppDialogAction::OpenData, ctx);
        self.binary
            .set_external_dialog_pending(self.file_dialog.pending());
    }

    fn choose_csv_config(&mut self, ctx: &Context) {
        if !self.commands_available() {
            return;
        }
        let dialog = self.file_locations.configure(
            DialogFileKind::CsvConfig,
            self.dialog_template
                .clone()
                .set_title("Load CSV configuration — Plotter Max")
                .add_filter("Markdown config", &["md", "markdown"]),
        );
        self.file_dialog
            .begin_open(dialog, AppDialogAction::LoadCsvConfig, ctx);
        self.binary
            .set_external_dialog_pending(self.file_dialog.pending());
    }

    fn poll_file_dialog(&mut self, ctx: &Context) {
        let result = self.file_dialog.poll(ctx);
        self.binary
            .set_external_dialog_pending(self.file_dialog.pending());
        let Some((action, Some(path))) = result else {
            return;
        };
        match action {
            AppDialogAction::OpenData => self.open_path(path, ctx),
            AppDialogAction::LoadCsvConfig => match AppConfig::load_from_file(&path) {
                Ok(cfg) => {
                    self.file_locations
                        .remember_file(DialogFileKind::CsvConfig, &path);
                    if let Some(path) = cfg.data_path.clone() {
                        self.pending_config = Some(cfg);
                        self.binary.open(path, ctx);
                    } else {
                        self.cache_message =
                            Some("Config needs a data_path before it can be opened.".into());
                    }
                }
                Err(error) => self.cache_message = Some(format!("Config: {error:#}")),
            },
        }
    }

    fn ui_toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let available = self.commands_available();
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui
                    .add_enabled(
                        available,
                        egui::Button::new("Open file…").shortcut_text("Ctrl+O"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.choose_data_file(&ctx);
                }
                if ui
                    .add_enabled(
                        available && self.binary.has_file(),
                        egui::Button::new("Save edited copy…").shortcut_text("Ctrl+Shift+S"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.binary.save_copy(&ctx);
                }
                ui.separator();
                if ui
                    .add_enabled(available, egui::Button::new("Open sensor demo"))
                    .clicked()
                {
                    ui.close_menu();
                    self.pending_config = None;
                    self.binary.open_demo(&ctx);
                }
                if ui
                    .add_enabled(available, egui::Button::new("Load CSV configuration…"))
                    .clicked()
                {
                    ui.close_menu();
                    self.choose_csv_config(&ctx);
                }
            });
            ui.menu_button("Edit", |ui| {
                self.binary.edit_menu(ui);
                ui.separator();
                if ui
                    .add_enabled(
                        available && self.binary.has_file(),
                        egui::Button::new("Go to byte…").shortcut_text("Ctrl+G"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.show_view(Pane::Hex);
                    self.binary.jump_to_byte();
                }
                if ui
                    .add_enabled(
                        self.binary.has_file(),
                        egui::Button::new("Find bytes or text…"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.show_view(Pane::Hex);
                    self.binary.find_bytes();
                }
                if ui
                    .add_enabled(
                        available && self.binary.has_selection(),
                        egui::Button::new("Replace selected bytes…"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.show_view(Pane::Hex);
                    self.binary.replace_selected_bytes();
                }
            });
            ui.menu_button("View", |ui| {
                for pane in Pane::ALL {
                    let open = self.workspace.dock.find_tab(&pane).is_some();
                    if ui.selectable_label(open, pane.title()).clicked() {
                        self.show_view(pane);
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui.button("Decoder workspace").on_hover_text("Edit Python or YAML beside the source bytes, with decoded results below.").clicked() {
                    self.binary.show_yaml();
                    self.workspace = WorkspaceLayout::default();
                    self.show_empty_workspace = true;
                    ui.close_menu();
                }
                if ui.add_enabled(self.binary.has_file(), egui::Button::new("Plot and bytes workspace"))
                    .on_hover_text("See the plot and selected bytes together, with decoded samples one tab away.")
                    .clicked() {
                    self.workspace = WorkspaceLayout::plot_and_bytes();
                    self.show_empty_workspace = true;
                    ui.close_menu();
                }
            });
            ui.menu_button("Decode", |ui| {
                if ui.button("Edit decoder").clicked() {
                    ui.close_menu();
                    self.binary.show_yaml();
                    self.show_view(Pane::Layout);
                }
                if ui
                    .add_enabled(
                        !self.binary.is_python(),
                        egui::Button::new("Edit decoding steps…"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.binary.show_steps();
                    self.show_view(Pane::Layout);
                }
                if ui
                    .add_enabled(
                        self.binary.has_selection(),
                        egui::Button::new(if self.binary.is_python() {
                            "Copy Python read"
                        } else {
                            "Describe selected bytes…"
                        }),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.binary.define_selection(&ctx);
                    self.show_view(Pane::Layout);
                }
                if ui
                    .add_enabled(
                        available && self.binary.can_apply_definition(),
                        egui::Button::new(if self.binary.is_python() {
                            "Run script"
                        } else {
                            "Decode file"
                        })
                        .shortcut_text("F5"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.binary.apply_definition(&ctx);
                }
                if ui.button("Decoded samples").clicked() {
                    ui.close_menu();
                    self.show_view(Pane::Table);
                }
                ui.separator();
                if ui
                    .add_enabled(available, egui::Button::new("Open decoder…"))
                    .clicked()
                {
                    ui.close_menu();
                    self.binary.choose_definition(&ctx);
                    self.show_view(Pane::Layout);
                }
                if ui
                    .add_enabled(
                        available && self.binary.can_save_definition(),
                        egui::Button::new("Save decoder…"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.binary.save_definition(&ctx);
                }
                if ui
                    .add_enabled(available, egui::Button::new("New Python script"))
                    .clicked()
                {
                    self.binary.new_python(&ctx);
                    self.show_view(Pane::Layout);
                    ui.close_menu();
                }
                ui.menu_button("Examples", |ui| {
                    if ui
                        .add_enabled(available, egui::Button::new("Python: Waveform container"))
                        .clicked()
                    {
                        self.binary.load_python_text(
                            include_str!("../samples/decoders/python/waveform.py"),
                            &ctx,
                        );
                        self.show_view(Pane::Layout);
                        ui.close_menu();
                    }
                    for (title, yaml) in DEFINITION_EXAMPLES {
                        if ui
                            .add_enabled(available, egui::Button::new(*title))
                            .clicked()
                        {
                            ui.close_menu();
                            self.binary.load_definition_text(yaml, &ctx);
                            self.show_view(Pane::Layout);
                        }
                    }
                });
            });
            ui.menu_button("Tools", |ui| {
                if ui.add_enabled(self.loader_rx.is_none() && available, egui::Button::new("Clear disk cache")).on_hover_text("Remove disposable indexes and cached snapshots; open documents remain available.").clicked() {
                    self.cache_message = Some(match crate::cache::clear() {
                        Ok(()) => "Disk cache cleared. Open files remain available.".into(), Err(error) => format!("Cache: {error:#}"),
                    });
                    ui.close_menu();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Decoding guide").clicked() {
                    self.show_decoding_guide = true;
                    ui.close_menu();
                }
            });
        });
        if !self.binary.has_file() {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            let source = self.binary.source_path();
            let title = source
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Sensor demo".into());
            ui.strong(title).on_hover_text(
                source
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            );
            let size = self.binary.file_len();
            let label = if size >= 1_000_000_000 {
                format!("{:.1} GB", size as f64 / 1_000_000_000.)
            } else if size >= 1_000_000 {
                format!("{:.1} MB", size as f64 / 1_000_000.)
            } else if size >= 1_000 {
                format!("{:.1} KB", size as f64 / 1_000.)
            } else {
                format!("{size} bytes")
            };
            ui.weak(label).on_hover_text(format!("{size} bytes"));
            if self.binary.is_dirty() {
                ui.colored_label(egui::Color32::YELLOW, "Unsaved changes");
            }
            ui.separator();
            if ui
                .button("Edit decoder")
                .on_hover_text("Open the YAML editor and decoding controls")
                .clicked()
            {
                self.binary.show_yaml();
                self.show_view(Pane::Layout);
            }
            let results = match self.decoder {
                Decoder::Layout => self.binary.decoded_ready(),
                Decoder::Csv => self.data.is_some(),
            };
            ui.add_enabled_ui(results, |ui| {
                if ui.button("Show samples").clicked() {
                    self.show_view(Pane::Table);
                }
                if ui.button("Show plot").clicked() {
                    self.show_view(Pane::Plot);
                }
            });
        });
    }

    fn ui_workspace(&mut self, ctx: &Context) {
        self.poll_workspace(ctx);
        let open = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
        let save = egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
            egui::Key::S,
        );
        let jump = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::G);
        if ctx.input_mut(|i| i.consume_shortcut(&jump))
            && self.commands_available()
            && self.binary.has_file()
        {
            self.show_view(Pane::Hex);
            self.binary.jump_to_byte();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&open)) {
            self.choose_data_file(ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&save))
            && self.commands_available()
            && self.binary.has_file()
        {
            self.binary.save_copy(ctx);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F5))
            && self.commands_available()
            && self.binary.can_apply_definition()
        {
            self.binary.apply_definition(ctx);
        }
        for file in ctx.input(|i| i.raw.dropped_files.clone()) {
            if let Some(path) = file.path {
                self.open_path(path, ctx);
            }
        }
        egui::TopBottomPanel::top("workspace_toolbar").show(ctx, |ui| self.ui_toolbar(ui));
        if self.show_decoding_guide {
            egui::Window::new("Decoding guide").open(&mut self.show_decoding_guide).default_width(540.).resizable(true).show(ctx, |ui| {
                ui.heading("Write a decoder, inspect its results");
                ui.label("Open a data file. Decoder shows your Python or YAML source beside Hex.");
                ui.label("Open decoder… loads Python or YAML. Run script or Decode file (F5) reads the open data file.");
                ui.label("Decoded samples opens after a successful decode. Show plot opens the same data as a plot; select a value to find its bytes.");
                ui.separator();
                ui.strong("Build a decoder from bytes");
                ui.label("Python supports imports, functions, loops and conditions. Copy Python read in Hex copies a read for the selected range. Register structures and records to populate the decoded views.");
                ui.label("For YAML, select bytes in Hex and use Fields to name fields and choose their formats. Decode selection previews those fields.");
                ui.label("Steps edits reading order, conditions and calculations. YAML and the guided tools edit the same decoder.");
                ui.label("Decoded structure is the result tree. It shows what the decoder read; select an element to locate it in Hex.");
                ui.label("Save Python… or Save YAML… keeps the decoder for reuse. Decode → Examples provides starting points.");
                ui.separator();
                ui.small("Hex edits preserve file length. Compressed data and automatic checksum/offset rebuilding need additional support. CSV uses its existing progressive reader.");
            });
        }
        egui::TopBottomPanel::bottom("workspace_status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if self.binary.busy() || self.loader_rx.is_some() {
                    ui.spinner();
                }
                let (status, error) = self.binary.status();
                let status = if error || self.decoder == Decoder::Layout {
                    status
                } else {
                    &self.status
                };
                ui.colored_label(
                    if error {
                        egui::Color32::LIGHT_RED
                    } else {
                        ui.visuals().text_color()
                    },
                    status,
                );
                if let Some(message) = &self.cache_message {
                    ui.label(message);
                    if ui.small_button("Dismiss").clicked() {
                        self.cache_message = None;
                    }
                }
            });
        });
        let mut workspace = std::mem::take(&mut self.workspace);
        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.binary.has_file() && !self.show_empty_workspace {
                self.ui_welcome(ui);
                return;
            }
            DockArea::new(&mut workspace.dock)
                .id(egui::Id::new("workspace_dock"))
                .style(egui_dock::Style::from_egui(ui.style().as_ref()))
                .show_inside(ui, &mut WorkspaceViewer { app: self });
        });
        self.workspace = workspace;
        if ctx
            .data_mut(|data| data.remove_temp::<bool>(egui::Id::new("welcome_definitions")))
            .unwrap_or(false)
        {
            self.workspace.reveal(Pane::Layout);
        }
        self.reveal_requested_panels();
    }

    fn ui_welcome(&mut self, ui: &mut Ui) {
        ui.add_space((ui.available_height() * 0.22).max(20.));
        ui.vertical_centered(|ui| {
            ui.heading("Inspect and edit a file");
            ui.add_space(8.);
            ui.label("Open any file to explore its bytes.");
            ui.label("Build reusable definitions from the structures you discover.");
            ui.add_space(18.);
            if ui
                .add_enabled(
                    self.commands_available(),
                    egui::Button::new("Open file…").min_size(egui::vec2(150., 34.)),
                )
                .clicked()
            {
                self.choose_data_file(ui.ctx());
            }
            ui.add_space(8.);
            ui.weak("You can also drop a file anywhere in the workspace.");
            ui.add_space(18.);
            if ui.link("Write a decoder").clicked() {
                self.binary.show_yaml();
                self.show_empty_workspace = true;
                // The docking state is temporarily moved out while drawing.
                // Defer this command until it has been restored below.
                ui.ctx()
                    .data_mut(|data| data.insert_temp(egui::Id::new("welcome_definitions"), true));
            }
        });
    }

    fn ui_view_controls(&mut self, ui: &mut Ui) {
        let Some(ds) = self.data.as_ref() else {
            return;
        };
        if self.view.is_none() {
            self.view = Some(ViewState::from_store(ds));
        }
        let Some(view) = &mut self.view else {
            return;
        };

        let previous = (view.center, view.span);
        let mut full_range = false;
        let (xmin, xmax) = view.bounds(ds);
        ui.separator();
        ui.label(format!(
            "Viewing [{:.3}, {:.3}] (span {:.3})",
            xmin,
            xmax,
            (xmax - xmin).abs()
        ));

        ui.horizontal(|ui| {
            if ui.button("Full Range").clicked() {
                view.set_full(ds);
                full_range = true;
            }
            if ui.button("Jump to Start").clicked() {
                view.jump_to_start(ds);
            }
            if ui.button("Jump to End").clicked() {
                view.jump_to_end(ds);
            }
        });

        let total_span = (ds.time_max - ds.time_min).abs().max(ViewState::MIN_SPAN);
        let min_window = (total_span / 10_000.0).max(ViewState::MIN_SPAN);

        let mut center = view.center;
        let mut span = view.span;

        ui.add(egui::Slider::new(&mut center, ds.time_min..=ds.time_max).text("Center"));
        ui.add(
            egui::Slider::new(&mut span, min_window..=total_span)
                .logarithmic(true)
                .text("Window"),
        );

        if (center - view.center).abs() > f64::EPSILON || (span - view.span).abs() > f64::EPSILON {
            view.set_center_span(center, span, ds);
        }

        ui.horizontal(|ui| {
            if ui
                .small_button("<<")
                .on_hover_text("Pan left by half-window")
                .clicked()
            {
                view.nudge_fraction(-0.5, ds);
            }
            if ui
                .small_button("<")
                .on_hover_text("Pan left by 20%")
                .clicked()
            {
                view.nudge_fraction(-0.2, ds);
            }
            if ui
                .small_button(">")
                .on_hover_text("Pan right by 20%")
                .clicked()
            {
                view.nudge_fraction(0.2, ds);
            }
            if ui
                .small_button(">>")
                .on_hover_text("Pan right by half-window")
                .clicked()
            {
                view.nudge_fraction(0.5, ds);
            }
        });
        if full_range {
            self.follow_loading = true;
        } else if previous != (view.center, view.span) {
            self.follow_loading = false;
        }
    }

    fn prepare_plot(&mut self, key: CsvPlotKey, ctx: &Context) {
        if self.plot_requested.as_ref() != Some(&key) {
            if self
                .plot_requested
                .as_ref()
                .is_none_or(|old| !old.same_view(&key))
            {
                self.plot_cancel.store(true, Ordering::Relaxed);
            }
            self.plot_requested = Some(key.clone());
            self.plot_error = None;
        }
        let received = self.plot_rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some((
                key.clone(),
                Err(anyhow::anyhow!("plot worker stopped unexpectedly")),
            )),
        });
        if let Some((finished, result)) = received {
            self.plot_rx = None;
            if self
                .plot_requested
                .as_ref()
                .is_some_and(|next| next.same_view(&finished))
            {
                self.plot_completed = Some(finished);
                match result {
                    Ok(lines) => self.plot_lines = lines,
                    Err(err) => self.plot_error = Some(format!("Plot: {err:#}")),
                }
            }
        }
        if self.plot_rx.is_none() && self.plot_completed.as_ref() != Some(&key) {
            let data = self.data.as_ref().unwrap().clone();
            let cfg = self.cfg.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            self.plot_cancel = cancel.clone();
            let (tx, rx) = mpsc::channel();
            self.plot_rx = Some(rx);
            let ctx = ctx.clone();
            thread::spawn(move || {
                let builder = PlotBuilder {
                    store: &data,
                    cfg: &cfg,
                };
                let result = builder.lines_for_range(
                    &key.visible,
                    key.bounds.map(f64::from_bits),
                    key.target,
                    &cancel,
                );
                let _ = tx.send((key, result));
                ctx.request_repaint();
            });
        }
        if self.plot_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn ui_plot(&mut self, ui: &mut Ui) {
        let Some(ds) = &self.data else {
            ui.centered_and_justified(|ui| {
                ui.label(if self.loader_rx.is_some() {
                    "Reading the first rows… plotting will start while loading continues."
                } else {
                    "Load data to begin"
                })
            });
            return;
        };
        if self.data_partial {
            ui.label(format!(
                "Partial file: {} rows available so far. Navigation covers the loaded portion.",
                ds.rows
            ));
        }
        if self.loader_rx.is_some() && !self.load_has_data {
            ui.label(
                "Opening the new file; showing the previous dataset until its first rows arrive.",
            );
        }
        if self.view.is_none() {
            self.view = Some(ViewState::from_store(ds));
        }
        let bounds = self.view.as_ref().unwrap().bounds(ds);
        let target = self
            .cfg
            .view
            .target_points_per_line
            .min(ui.available_width().max(100.) as usize);
        let key = CsvPlotKey {
            dataset: self.dataset,
            generation: self.generation,
            bounds: [bounds.0.to_bits(), bounds.1.to_bits()],
            visible: self.visible.clone(),
            target,
            max_lines: self.cfg.view.max_lines_in_plot,
        };
        self.prepare_plot(key, &ui.ctx().clone());
        if self.plot_rx.is_some() {
            ui.small("Updating plot… navigation remains available.");
        }
        if let Some(err) = &self.plot_error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }
        let response = Plot::new("plot")
            .legend(Legend::default())
            .allow_zoom(true)
            .allow_scroll(true)
            .allow_boxed_zoom(true)
            .allow_drag(true)
            .show(ui, |plot_ui| {
                let pb = plot_ui.plot_bounds();
                plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                    [bounds.0, pb.min()[1]],
                    [bounds.1, pb.max()[1]],
                ));
                plot_ui.set_auto_bounds(egui::Vec2b { x: false, y: true });
                for (i, name, points) in &self.plot_lines {
                    if self.visible.get(*i) == Some(&true) {
                        plot_ui.line(Line::new(points.clone()).name(name));
                    }
                }
            });
        let b = response.transform.bounds();
        let tolerance = ((bounds.1 - bounds.0).abs() * 1e-6).max(1e-9);
        if (b.min()[0] - bounds.0).abs() > tolerance || (b.max()[0] - bounds.1).abs() > tolerance {
            self.follow_loading = false;
        }
        self.view.as_mut().unwrap().sync_from_bounds(
            b.min()[0],
            b.max()[0],
            self.data.as_ref().unwrap(),
        );
        let hidden = self
            .visible
            .iter()
            .filter(|v| **v)
            .count()
            .saturating_sub(self.cfg.view.max_lines_in_plot);
        if hidden > 0 {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("{hidden} signals hidden by the plot line limit"),
            );
        }
    }
}

impl eframe::App for PlotterApp {
    fn update(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
        self.dialog_template = rfd::AsyncFileDialog::new().set_parent(frame);
        self.binary
            .set_dialog_template(self.dialog_template.clone());
        self.ui_workspace(ctx);
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.workspace.save(storage);
        self.file_locations.save(storage);
    }
}

struct WorkspaceViewer<'a> {
    app: &'a mut PlotterApp,
}
impl TabViewer for WorkspaceViewer<'_> {
    type Tab = Pane;
    fn title(&mut self, pane: &mut Pane) -> egui::WidgetText {
        pane.title().into()
    }
    fn id(&mut self, pane: &mut Pane) -> egui::Id {
        egui::Id::new(*pane)
    }
    fn scroll_bars(&self, _pane: &Pane) -> [bool; 2] {
        [false, false]
    }
    fn ui(&mut self, ui: &mut Ui, pane: &mut Pane) {
        match pane {
            Pane::Structures => self.app.binary.structures(ui),
            Pane::Inspector => {
                egui::ScrollArea::vertical().show(ui, |ui| self.app.ui_inspector(ui));
            }
            Pane::Layout => self.app.binary.controls(ui),
            Pane::Hex => self.app.binary.hex(ui),
            Pane::Plot => {
                if self.app.decoder == Decoder::Csv {
                    self.app.ui_plot(ui);
                } else {
                    self.app.binary.plot(ui);
                }
            }
            Pane::Table => {
                if self.app.decoder == Decoder::Csv {
                    if let Some(time) = self.app.csv_table.ui(
                        ui,
                        self.app.data.as_ref(),
                        self.app.dataset,
                        &self.app.visible,
                    ) {
                        if let (Some(view), Some(data)) = (&mut self.app.view, &self.app.data) {
                            view.set_center_span(time, view.span, data);
                            self.app.follow_loading = false;
                        }
                    }
                } else {
                    self.app.binary.table(ui);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{MappedChunk, MappedTable, PointSeries, Signal};
    use crate::plot_index::{IndexBuilder, Sample};
    use std::io::Write;

    fn store(rows: usize) -> DataStore {
        let mut file = tempfile::tempfile().unwrap();
        let mut a = IndexBuilder::new(rows);
        let mut b = IndexBuilder::new(rows);
        for record in 0..rows {
            let x = record as f64;
            file.write_all(bytemuck::cast_slice(&[x, x * 2., x * 3.]))
                .unwrap();
            a.push(Some(Sample {
                record,
                byte_offset: 0,
                point: [x, x * 2.],
            }));
            b.push(Some(Sample {
                record,
                byte_offset: 0,
                point: [x, x * 3.],
            }));
        }
        crate::cache::freeze(&file).unwrap();
        let table = MappedTable::from_chunks(vec![MappedChunk::open(file, rows, 3).unwrap()], None)
            .unwrap();
        DataStore::from_signals(
            vec![
                Signal {
                    name: "a".into(),
                    points: PointSeries::new(table.clone(), 1, a.finish().unwrap()),
                },
                Signal {
                    name: "b".into(),
                    points: PointSeries::new(table, 2, b.finish().unwrap()),
                },
            ],
            0.,
            (rows - 1) as f64,
            rows,
        )
    }

    fn frame(app: &mut PlotterApp, ctx: &Context, events: Vec<egui::Event>) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400., 950.),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                app.poll_loader(ctx);
                egui::SidePanel::left("controls")
                    .exact_width(350.)
                    .show(ctx, |ui| app.ui_left_panel(ui));
                egui::CentralPanel::default().show(ctx, |ui| app.ui_plot(ui));
            },
        )
    }

    fn click(app: &mut PlotterApp, ctx: &Context, label: &str) {
        frame(app, ctx, vec![]);
        let output = frame(app, ctx, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.rect.center().to_vec2())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing UI text {label:?}"));
        frame(app, ctx, vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            frame(
                app,
                ctx,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
    }

    fn workspace_frame(app: &mut PlotterApp, ctx: &Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280., 850.),
                )),
                ..Default::default()
            },
            |ctx| app.ui_workspace(ctx),
        )
    }

    fn workspace_events(
        app: &mut PlotterApp,
        ctx: &Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280., 850.),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.ui_workspace(ctx),
        )
    }
    fn workspace_click(app: &mut PlotterApp, ctx: &Context, label: &str) {
        workspace_frame(app, ctx);
        let output = workspace_frame(app, ctx);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.rect.center().to_vec2())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing workspace label {label}"));
        workspace_events(app, ctx, vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            workspace_events(
                app,
                ctx,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
    }
    #[test]
    fn file_menu_and_apply_shortcut_show_samples_beside_the_bytes() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let output = workspace_frame(&mut app, &ctx);
        let top: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.pos.y < 25. => {
                    Some(text.galley.text().to_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(top, ["File", "Edit", "View", "Decode", "Tools", "Help"]);
        workspace_click(&mut app, &ctx, "File");
        workspace_click(&mut app, &ctx, "Open sensor demo");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.workspace.dock.find_tab(&Pane::Table).is_none());
        app.show_view(Pane::Layout);
        workspace_events(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::F5,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        while app.workspace.dock.find_tab(&Pane::Table).is_none() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let output = workspace_frame(&mut app, &ctx);
        let (surface, table, _) = app.workspace.dock.find_tab(&Pane::Table).unwrap();
        let rect = app.workspace.dock[surface][table].rect().unwrap();
        assert!(
            rect.width() > 600. && rect.height() > 250.,
            "Sample table should remain readable beside the decoder: {rect:?}"
        );
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,egui::epaint::Shape::Text(text) if text.galley.text()=="temperature")));
        let location = app.workspace.dock.find_tab(&Pane::Table).unwrap();
        app.workspace.dock.remove_tab(location);
        for _ in 0..5 {
            workspace_frame(&mut app, &ctx);
        }
        assert!(
            app.workspace.dock.find_tab(&Pane::Table).is_none(),
            "completed workers must not reopen a closed view"
        );
        app.binary.apply_definition(&ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.workspace.dock.find_tab(&Pane::Table).is_none() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn welcome_does_not_reserve_empty_editors_and_views() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let output = workspace_frame(&mut app, &ctx);
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect();
        assert!(labels.iter().any(|text| text == "Inspect and edit a file"));
        for pane in [
            Pane::Hex,
            Pane::Plot,
            Pane::Table,
            Pane::Structures,
            Pane::Inspector,
        ] {
            assert!(!labels.iter().any(|text| text == pane.title()));
        }
    }

    #[test]
    fn byte_changes_discard_csv_results_and_pending_workers_without_losing_source() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        app.cfg.data_path = Some("source.csv".into());
        app.decoder = Decoder::Csv;
        app.accept_data(store(100), false);
        let old_progress = app.load_progress.clone();
        let old_preview = app.load_preview.clone();
        let (tx, rx) = mpsc::channel();
        app.loader_rx = Some(rx);
        app.invalidate_csv_after_edit();
        *old_preview.lock().unwrap() = Some(store(10));
        app.poll_loader(&ctx);
        assert!(old_progress.cancelled.load(Ordering::Relaxed));
        assert!(tx.send(Ok(store(10))).is_err());
        assert!(app.data.is_none());
        assert!(app.plot_lines.is_empty());
        assert!(app.loader_rx.is_none());
        assert!(app.decoder == Decoder::Layout);
        assert_eq!(app.cfg.data_path, Some("source.csv".into()));
    }

    #[test]
    fn shared_workspace_opens_any_file_and_discards_stale_csv_workers() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        app.accept_data(store(100), false);
        let old_progress = app.load_progress.clone();
        let (tx, rx) = mpsc::channel();
        app.loader_rx = Some(rx);
        let dir = tempfile::tempdir().unwrap();
        for name in ["layout.yaml", "config.md", "capture.dat", "no-extension"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"TEST\0\x01").unwrap();
            app.open_path(path.clone(), &ctx);
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            loop {
                workspace_frame(&mut app, &ctx);
                if app.binary.source_path() == Some(path.as_path()) {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(app.data.is_none());
            assert!(app.loader_rx.is_none());
            assert!(app.binary.has_file());
            assert!(!app.binary.has_layout());
            let output = workspace_frame(&mut app, &ctx);
            let labels: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect();
            let (surface, node, _) = app.workspace.dock.find_tab(&Pane::Hex).unwrap();
            assert!(app.workspace.dock[surface][node].rect().unwrap().width() > 700.);
            let (surface, node, _) = app.workspace.dock.find_tab(&Pane::Inspector).unwrap();
            assert!(app.workspace.dock[surface][node].rect().unwrap().width() > 400.);
            let (surface, node, _) = app.workspace.dock.find_tab(&Pane::Hex).unwrap();
            assert!(app.workspace.dock[surface][node].rect().unwrap().height() > 650.);
            assert!(app.workspace.dock.find_tab(&Pane::Layout).is_some());
            assert!(app.workspace.dock.find_tab(&Pane::Plot).is_none());
            assert!(app.workspace.dock.find_tab(&Pane::Table).is_none());
            for pane in [Pane::Hex, Pane::Layout, Pane::Structures, Pane::Inspector] {
                assert!(
                    labels.iter().any(|text| text == pane.title()),
                    "missing {}",
                    pane.title()
                );
            }
            assert!(!labels
                .iter()
                .any(|text| text == "CSV plots" || text == "Binary records"));
        }
        assert!(old_progress.cancelled.load(Ordering::Relaxed));
        assert!(tx.send(Ok(store(10))).is_err());
    }

    #[test]
    fn csv_table_reads_bounded_rows_and_source_stays_available_after_csv_error() {
        let data = store(150);
        let points = data.signals[1]
            .points
            .read_rows(100..150, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(points.len(), 50);
        assert_eq!(points[0], [100., 300.]);
        assert!(data.signals[0]
            .points
            .read_rows(0..101, &AtomicBool::new(false))
            .is_err());
        assert!(data.signals[0]
            .points
            .read_rows(0..1, &AtomicBool::new(true))
            .is_err());
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.csv");
        std::fs::write(&path, b"wrong,header\n1,2\n").unwrap();
        app.open_path(path.clone(), &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            workspace_frame(&mut app, &ctx);
            if app.binary.has_file() && app.loader_rx.is_none() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.status.contains("Load error"), "{}", app.status);
        assert_eq!(app.binary.source_path(), Some(path.as_path()));
        assert_eq!(app.binary.file_len(), 17);
        assert!(!app.binary.has_layout());
    }

    #[test]
    fn progressive_plot_controls_work_and_preserve_view_until_completion() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let (tx, rx) = mpsc::channel();
        app.loader_rx = Some(rx);
        *app.load_preview.lock().unwrap() = Some(store(100));
        frame(&mut app, &ctx, vec![]);
        assert!(app.loader_rx.is_some());
        assert!(app.data_partial && app.follow_loading);
        assert_eq!(app.data.as_ref().unwrap().rows, 100);
        assert!(!app.data.as_ref().unwrap().signals[0]
            .points
            .for_range(20., 25., 100)
            .unwrap()
            .is_empty());
        // The actual egui signal controls and navigator are active before EOF.
        click(&mut app, &ctx, "Select None");
        assert_eq!(app.visible, [false, false]);
        app.view
            .as_mut()
            .unwrap()
            .set_center_span(50., 20., app.data.as_ref().unwrap());
        click(&mut app, &ctx, "<");
        assert!(!app.follow_loading);
        let view = app.view.as_ref().unwrap().clone();
        *app.load_preview.lock().unwrap() = Some(store(200));
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.data.as_ref().unwrap().rows, 200);
        assert_eq!(app.visible, [false, false]);
        assert_eq!(app.view.as_ref().unwrap().center, view.center);
        assert_eq!(app.view.as_ref().unwrap().span, view.span);
        tx.send(Ok(store(300))).unwrap();
        frame(&mut app, &ctx, vec![]);
        assert!(app.loader_rx.is_none());
        assert!(!app.data_partial);
        assert_eq!(app.data.as_ref().unwrap().rows, 300);
        assert_eq!(app.visible, [false, false]);
        assert_eq!(app.view.as_ref().unwrap().center, view.center);
        assert_eq!(app.view.as_ref().unwrap().span, view.span);
    }

    #[test]
    fn growing_full_range_cancel_and_stale_loads_are_explicit() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let (tx, rx) = mpsc::channel();
        app.loader_rx = Some(rx);
        *app.load_preview.lock().unwrap() = Some(store(100));
        app.poll_loader(&ctx);
        *app.load_preview.lock().unwrap() = Some(store(200));
        app.poll_loader(&ctx);
        assert_eq!(
            app.view
                .as_ref()
                .unwrap()
                .bounds(app.data.as_ref().unwrap()),
            (0., 199.)
        );
        app.load_progress.cancelled.store(true, Ordering::Relaxed);
        tx.send(Err(anyhow::anyhow!("Loading cancelled"))).unwrap();
        app.poll_loader(&ctx);
        assert!(app.data_partial);
        assert!(app.status.contains("200 loaded rows remain usable"));
        assert_eq!(
            app.data.as_ref().unwrap().signals[0].points.point(199),
            [199., 398.]
        );
        let old_slot = app.load_preview.clone();
        // Missing path fails before touching the default cache directory.
        app.begin_load(AppConfig::default(), &ctx);
        *old_slot.lock().unwrap() = Some(store(999));
        app.poll_loader(&ctx);
        assert_eq!(
            app.data.as_ref().unwrap().rows,
            200,
            "old load cannot replace new job's data"
        );
        assert!(!app.load_has_data);
    }

    #[test]
    fn more_loaded_rows_do_not_starve_an_existing_plot_query() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        app.accept_data(store(100), true);
        let old = CsvPlotKey {
            dataset: app.dataset,
            generation: app.generation,
            bounds: [0f64.to_bits(), 99f64.to_bits()],
            visible: vec![true, true],
            target: 100,
            max_lines: 2,
        };
        app.plot_requested = Some(old.clone());
        let (tx, rx) = mpsc::channel();
        app.plot_rx = Some(rx);
        let cancellation = app.plot_cancel.clone();
        let mut next = old.clone();
        next.generation += 1;
        app.prepare_plot(next.clone(), &ctx);
        assert!(
            !cancellation.load(Ordering::Relaxed),
            "index growth must let the existing query finish"
        );
        tx.send((old, Ok(vec![(0, "a".into(), vec![[1., 2.]])])))
            .unwrap();
        app.prepare_plot(next.clone(), &ctx);
        assert_eq!(app.plot_lines[0].2, [[1., 2.]]);
        assert!(
            app.plot_rx.is_some(),
            "latest index is queried after showing the completed prefix"
        );
        let cancellation = app.plot_cancel.clone();
        next.bounds = [10f64.to_bits(), 20f64.to_bits()];
        app.prepare_plot(next, &ctx);
        assert!(
            cancellation.load(Ordering::Relaxed),
            "a changed viewport should cancel obsolete work"
        );
    }
    #[test]
    fn go_to_byte_shortcut_opens_hex_and_focuses_navigation() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        app.binary.open_demo(&ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        workspace_frame(&mut app, &ctx);
        let hex = app.workspace.dock.find_tab(&Pane::Hex).unwrap();
        app.workspace.dock.remove_tab(hex);
        workspace_events(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::G,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
        );
        assert!(app.workspace.dock.find_tab(&Pane::Hex).is_some());
        workspace_frame(&mut app, &ctx);
        workspace_events(&mut app, &ctx, vec![egui::Event::Text("0x100".into())]);
        workspace_events(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let output = workspace_frame(&mut app, &ctx);
        assert!(output.shapes.iter().any(|shape|matches!(&shape.shape,egui::Shape::Text(text) if text.galley.text() == "Cursor 0x100 · byte 256")),"Ctrl+G must focus the navigation input and move the selected byte");
    }

    #[test]
    fn open_dialog_keeps_workspace_rendering_and_cancellation_keeps_current_state() {
        let mut app = PlotterApp::default();
        let ctx = Context::default();
        app.status = "Existing workspace".into();
        let mut config = AppConfig::default();
        config.data_path = Some("keep.csv".into());
        app.pending_config = Some(config);
        let (tx, rx) = mpsc::channel();
        assert!(app.file_dialog.begin_test(
            async move { rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap() },
            AppDialogAction::OpenData,
            &ctx
        ));
        app.binary.set_external_dialog_pending(true);
        let started = Instant::now();
        for _ in 0..60 {
            let frame = workspace_frame(&mut app, &ctx);
            assert!(!frame.shapes.is_empty());
            assert!(app.file_dialog.pending());
            assert!(!app.commands_available());
            assert!(!app.binary.commands_available());
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "native picker wait blocked rendering"
        );
        tx.send(None).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.file_dialog.pending() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            app.pending_config.as_ref().unwrap().data_path.as_deref(),
            Some(std::path::Path::new("keep.csv"))
        );
        assert_eq!(app.status, "Existing workspace");
        assert!(app.commands_available());
        assert!(app.binary.commands_available());
    }

    #[test]
    fn completed_open_dialog_dispatches_once_and_loads_the_selected_file() {
        let mut app = PlotterApp::default();
        let ctx = Context::default();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("any.extension");
        std::fs::write(&path, b"raw bytes").unwrap();
        app.file_dialog.begin_test(
            std::future::ready(Some(path.clone())),
            AppDialogAction::OpenData,
            &ctx,
        );
        app.binary.set_external_dialog_pending(true);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.file_dialog.pending() || app.binary.source_path() != Some(path.as_path()) {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(app.binary.file_len(), 9);
        assert_eq!(
            app.file_locations.directory(DialogFileKind::Data),
            Some(directory.path().to_path_buf())
        );
        assert!(app.file_dialog.poll(&ctx).is_none());
        assert!(app.commands_available());
    }

    #[test]
    fn cancelled_csv_config_picker_keeps_loaded_config() {
        let mut app = PlotterApp::default();
        let ctx = Context::default();
        app.cfg.data_path = Some("current.csv".into());
        app.file_dialog.begin_test(
            std::future::ready(None),
            AppDialogAction::LoadCsvConfig,
            &ctx,
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.file_dialog.pending() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            app.cfg.data_path.as_deref(),
            Some(std::path::Path::new("current.csv"))
        );
        assert!(app.cache_message.is_none());
    }

    #[test]
    fn file_locations_persist_and_remain_shared_with_decoder_dialogs() {
        #[derive(Default)]
        struct MemoryStorage(std::collections::HashMap<String, String>);
        impl eframe::Storage for MemoryStorage {
            fn get_string(&self, key: &str) -> Option<String> {
                self.0.get(key).cloned()
            }
            fn set_string(&mut self, key: &str, value: String) {
                self.0.insert(key.to_owned(), value);
            }
            fn flush(&mut self) {}
        }
        let root = tempfile::tempdir().unwrap();
        let cases = [
            (DialogFileKind::Data, "data", "waveform.raw"),
            (DialogFileKind::Yaml, "yaml", "format.yaml"),
            (DialogFileKind::Python, "python", "decoder.py"),
            (DialogFileKind::CsvConfig, "configs", "settings.md"),
            (DialogFileKind::Exports, "exports", "selection.bin"),
        ];
        let mut app = PlotterApp::default();
        for (kind, directory, name) in cases {
            let directory = root.path().join(directory);
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join(name);
            std::fs::write(&path, "").unwrap();
            app.file_locations.remember_file(kind, &path);
        }
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let mut restored = PlotterApp::default();
        restored.restore_file_locations(Some(&storage));
        for (kind, directory, _) in cases {
            assert_eq!(
                restored.file_locations.directory(kind),
                Some(root.path().join(directory))
            );
        }
        // A decoder opened after restore must update the same state the app saves.
        let next = root.path().join("other-yaml");
        std::fs::create_dir(&next).unwrap();
        let path = next.join("next.yml");
        std::fs::write(&path, "actions: []\n").unwrap();
        restored.binary.load_yaml(path, &Context::default());
        assert_eq!(
            restored.file_locations.directory(DialogFileKind::Yaml),
            Some(next.clone())
        );
        assert_eq!(
            restored.file_locations.directory(DialogFileKind::Python),
            Some(root.path().join("python"))
        );
        assert_eq!(
            restored.file_locations.directory(DialogFileKind::Data),
            Some(root.path().join("data"))
        );
        eframe::App::save(&mut restored, &mut storage);
        assert_eq!(
            FileLocations::load(Some(&storage)).directory(DialogFileKind::Yaml),
            Some(next)
        );
    }

    #[test]
    fn decoder_source_and_run_are_visible_then_results_open_beside_it() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let directory = tempfile::tempdir().unwrap();
        let data_path = directory.path().join("waveform.data");
        std::fs::write(&data_path, [1u8, 0, 2, 0, 3, 0, 4, 0]).unwrap();
        let yaml_path = directory.path().join("waveform.yaml");
        let yaml = "endian: little\nstructures:\n  Sample:\n    fields:\n      - {name: amplitude, type: u16}\nactions:\n  - read: {name: samples, type: Sample, repeat: until end}\n";
        std::fs::write(&yaml_path, yaml).unwrap();
        app.open_path(data_path, &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let labels = |output: egui::FullOutput| -> Vec<String> {
            output
                .shapes
                .into_iter()
                .filter_map(|shape| match shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect()
        };
        let opened = labels(workspace_frame(&mut app, &ctx));
        for label in ["Decoder", "Open decoder…", "Run script", "Python"] {
            assert!(opened.iter().any(|text| text == label), "missing {label}");
        }
        assert!(!opened
            .iter()
            .any(|text| text == "Overview" || text.contains("All definitions")));
        app.binary.load_yaml(yaml_path, &ctx);
        let loaded = labels(workspace_frame(&mut app, &ctx));
        assert!(loaded.iter().any(|text| text.contains("waveform.yaml")));
        assert!(
            loaded.iter().any(|text| text.contains("structures:")),
            "loading must display source, without another tab click"
        );
        assert!(!app.binary.decoded_ready());
        workspace_click(&mut app, &ctx, "Decode file");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.decoded_ready() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let decoded = labels(workspace_frame(&mut app, &ctx));
        assert!(decoded.iter().any(|text| text == "amplitude"));
        assert!(
            decoded.iter().any(|text| text.contains("structures:")),
            "running must keep the source visible"
        );
        let (_, editor, _) = app.workspace.dock.find_tab(&Pane::Layout).unwrap();
        let (_, samples, _) = app.workspace.dock.find_tab(&Pane::Table).unwrap();
        assert_ne!(editor, samples);
        workspace_click(&mut app, &ctx, "Show plot");
        assert!(app.workspace.dock.find_tab(&Pane::Plot).is_some());
        let plotted = labels(workspace_frame(&mut app, &ctx));
        assert!(plotted.iter().any(|text| text.contains("structures:")));
    }

    #[test]
    fn python_file_runs_only_on_request_and_shows_results_in_same_workspace() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("capture.raw");
        let script = directory.path().join("decoder.py");
        std::fs::write(&data, [1u8, 0, 2, 0, 3, 0, 4, 0]).unwrap();
        std::fs::write(&script, "from plottermax import Struct\nimport math\ndef sample_type():\n    return Struct('Sample', [('amplitude', 'u16')])\nfile.records('samples', sample_type())\nprint('Completed', math.sqrt(16))\n").unwrap();
        app.open_path(data, &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        app.binary.load_yaml(script, &ctx);
        let output = workspace_frame(&mut app, &ctx);
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        for label in ["Python", "Run script", "Console", "decoder.py"] {
            assert!(labels.contains(&label), "missing {label}");
        }
        assert!(!labels.contains(&"Fields") && !labels.contains(&"Steps"));
        assert!(!app.binary.is_decoding());
        assert!(!app.binary.decoded_ready());
        workspace_click(&mut app, &ctx, "Run script");
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !app.binary.decoded_ready() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline, "{}", app.binary.status().0);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let output = workspace_frame(&mut app, &ctx);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "amplitude")));
        assert!(app.workspace.dock.find_tab(&Pane::Table).is_some());
        let (_, editor, _) = app.workspace.dock.find_tab(&Pane::Layout).unwrap();
        let (_, table, _) = app.workspace.dock.find_tab(&Pane::Table).unwrap();
        assert_ne!(editor, table);
        workspace_click(&mut app, &ctx, "Show plot");
        assert!(app.workspace.dock.find_tab(&Pane::Plot).is_some());
    }

    #[test]
    fn stop_python_keeps_the_editor_responsive() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("capture.raw");
        let script = directory.path().join("loop.py");
        std::fs::write(&data, [0u8]).unwrap();
        std::fs::write(&script, "while True:\n    pass\n").unwrap();
        app.open_path(data, &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        app.binary.load_yaml(script, &ctx);
        workspace_click(&mut app, &ctx, "Run script");
        for _ in 0..30 {
            let output = workspace_frame(&mut app, &ctx);
            assert!(!output.shapes.is_empty());
            assert!(app.binary.is_decoding());
        }
        workspace_click(&mut app, &ctx, "Stop");
        assert!(!app.binary.is_decoding());
        assert!(!app.binary.decoded_ready());
        assert!(app.binary.can_apply_definition());
    }

    fn capture_workspace(app: &mut PlotterApp, ctx: &Context, name: &str) {
        let output = workspace_frame(app, ctx);
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let atlas = ctx.fonts(|fonts| fonts.image());
        let pixels: Vec<[u8; 4]> = atlas
            .srgba_pixels(None)
            .map(|pixel| pixel.to_array())
            .collect();
        let meshes: Vec<_> = primitives.into_iter().filter_map(|primitive| {
            let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive else { return None; };
            Some(serde_json::json!({
                "clip": [primitive.clip_rect.min.x, primitive.clip_rect.min.y, primitive.clip_rect.max.x, primitive.clip_rect.max.y],
                "vertices": mesh.vertices.iter().map(|vertex| (vertex.pos.x, vertex.pos.y, vertex.uv.x, vertex.uv.y, vertex.color.to_array())).collect::<Vec<_>>(),
                "indices": mesh.indices,
            }))
        }).collect();
        let capture = serde_json::json!({"size": [1280, 850], "atlas_size": atlas.size, "atlas": pixels, "meshes": meshes});
        std::fs::write(
            format!("/tmp/plottermax-{name}.json"),
            serde_json::to_vec(&capture).unwrap(),
        )
        .unwrap();
    }

    /// A renderer-independent capture for reviewing the actual egui workspace.
    #[test]
    #[ignore = "writes a tessellated UI capture to /tmp; requires the generated preview fixture"]
    fn capture_editing_workspace() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("samples/data/waveform_preview.bin");
        app.binary.open(path, &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !app.binary.has_file() || app.binary.busy() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        workspace_frame(&mut app, &ctx);
        capture_workspace(&mut app, &ctx, "decoder-empty");
        app.binary.load_yaml(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("samples/decoders/yaml/waveform.yaml"),
            &ctx,
        );
        workspace_frame(&mut app, &ctx);
        capture_workspace(&mut app, &ctx, "decoder-ready");
        workspace_click(&mut app, &ctx, "Fields");
        workspace_click(&mut app, &ctx, "Edit format…");
        workspace_click(&mut app, &ctx, "Header");
        workspace_frame(&mut app, &ctx);
        capture_workspace(&mut app, &ctx, "decoder-fields");
        workspace_click(&mut app, &ctx, "Source");
        app.binary.apply_definition(&ctx);
        for _ in 0..80 {
            workspace_frame(&mut app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        capture_workspace(&mut app, &ctx, "decoder-results");
        app.workspace = WorkspaceLayout::plot_and_bytes();
        workspace_frame(&mut app, &ctx);
        workspace_events(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::G,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
        );
        workspace_frame(&mut app, &ctx);
        workspace_events(&mut app, &ctx, vec![egui::Event::Text("0x1789".into())]);
        workspace_events(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        for _ in 0..80 {
            workspace_frame(&mut app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for (name, samples) in [("plot-bytes", false), ("plot-samples", true)] {
            if samples {
                app.workspace.show_samples();
            }
            capture_workspace(&mut app, &ctx, name);
        }
        app.binary.load_yaml(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("samples/decoders/python/waveform.py"),
            &ctx,
        );
        app.workspace = WorkspaceLayout::default();
        workspace_frame(&mut app, &ctx);
        capture_workspace(&mut app, &ctx, "python-ready");
        workspace_click(&mut app, &ctx, "Run script");
        let deadline = Instant::now() + std::time::Duration::from_secs(15);
        while !app.binary.decoded_ready() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline, "{}", app.binary.status().0);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        workspace_frame(&mut app, &ctx);
        capture_workspace(&mut app, &ctx, "python-results");
    }
    #[test]
    #[ignore = "captures the actual 40 GB overview to /tmp; requires the generated fixture"]
    fn capture_plot_overview() {
        let ctx = Context::default();
        let mut app = PlotterApp::default();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        app.open_path(root.join("samples/data/waveform_stress_40gb.bin"), &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !app.binary.has_file() || app.binary.busy() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        app.binary
            .load_yaml(root.join("samples/decoders/python/waveform.py"), &ctx);
        app.binary.apply_definition(&ctx);
        while !app.binary.decoded_ready() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline, "{}", app.binary.status().0);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        app.workspace.dock = egui_dock::DockState::new(vec![Pane::Plot]);
        for _ in 0..5 {
            workspace_frame(&mut app, &ctx);
        }
        workspace_click(&mut app, &ctx, "Overview");
        workspace_events(
            &mut app,
            &ctx,
            vec![egui::Event::PointerMoved(egui::pos2(1200., 30.))],
        );
        for _ in 0..120 {
            workspace_frame(&mut app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        capture_workspace(&mut app, &ctx, "overview-bands");
    }
}
