#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use crate::workspace_layout::LegacyFindTab as _;
use crate::binary_ui::{BinaryWorkspace, OpenedFile};
use crate::cache::LoadProgress;
use crate::config::AppConfig;
use crate::csv_table::{CsvCellSelection, CsvRowSelection, CsvTable};
use crate::data::{CsvRow, DataStore};
use crate::decoder_library::{self, Format as LibraryFormat};
use crate::file_dialog::{FileKind as DialogFileKind, FileLocations, NativeFileDialog};
use crate::hex_editor::HexHighlight;
use crate::loaders::{file_kind, load_csv_rows, FileKind, CSV_INITIAL_ROWS};
use crate::plotting::{PlotBuilder, PlotLines};
use crate::structure_ui::DecoderLanguage;
use crate::workspace_layout::{Pane, WorkspaceLayout};
use anyhow::Result;
use egui::{Context, Ui};
use egui_dock::{DockArea, TabViewer};
use egui_plot::{Line, Plot, PlotBounds, PlotPoint, Points, VLine};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::io::Read;
use std::sync::mpsc::{self, Receiver};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
#[cfg(test)]
use std::time::Instant;

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

#[derive(Clone, Debug, PartialEq, Eq)]
enum CsvLinkRequest {
    Cell(CsvCellSelection),
    Rows(CsvRowSelection),
    Bytes(Range<usize>),
}

struct CsvLinkedSelection {
    row: CsvRow,
    column: Option<usize>,
    bytes: Range<usize>,
    rows: Option<CsvRowSelection>,
    anchor: Option<CsvRow>,
}

/// A range lookup reads only its two endpoints; intermediate records stay on disk.
#[derive(Debug)]
struct CsvLinkRows {
    row: CsvRow,
    anchor: Option<CsvRow>,
}

#[derive(Clone)]
struct CsvLinkJob {
    serial: u64,
    dataset: u64,
    request: CsvLinkRequest,
}

#[derive(Debug, Clone, Copy)]
enum AppDialogAction {
    OpenData,
    LoadCsvConfig,
    ImportDecoder,
}

pub struct BinwaveApp {
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
    displayed_decoder: Decoder,
    workspace: WorkspaceLayout,
    show_empty_workspace: bool,
    show_decoding_guide: bool,
    sidebar_open: bool,
    sidebar_search_open: bool,
    sidebar_query: String,
    decoder_library_open: bool,
    library_query: String,
    library_filter: LibraryFormat,
    library_selected: Option<PathBuf>,
    library_renaming: bool,
    library_rename_draft: String,
    library_delete_confirm: bool,
    sidebar_settings_open: bool,
    csv_table: CsvTable,
    pending_config: Option<AppConfig>,
    pending_recent_decoder: Option<(std::path::PathBuf, std::path::PathBuf)>,
    file_dialog: NativeFileDialog<AppDialogAction>,
    file_locations: FileLocations,
    dialog_template: rfd::AsyncFileDialog,
    load_progress: Arc<LoadProgress>,
    csv_applied: Option<AppConfig>,
    csv_result_x: String,
    csv_target: Option<usize>,
    csv_full_index: bool,
    csv_advance_plot: bool,
    csv_cache_root: std::path::PathBuf,
    csv_selection: Option<CsvLinkedSelection>,
    csv_link_request: Option<CsvLinkRequest>,
    csv_link_serial: u64,
    csv_link_indexing: bool,
    csv_link_job: Option<CsvLinkJob>,
    csv_link_rx: Option<Receiver<Result<Option<CsvLinkRows>>>>,
    csv_link_cancel: Arc<AtomicBool>,
    last_hex_selection: Option<Range<usize>>,
    cache_message: Option<String>,
}

impl Default for BinwaveApp {
    fn default() -> Self {
        let cfg = AppConfig::default();
        let file_locations = FileLocations::default();
        let mut binary = BinaryWorkspace::default();
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
            displayed_decoder: Decoder::Layout,
            workspace: WorkspaceLayout::default(),
            show_empty_workspace: false,
            show_decoding_guide: false,
            sidebar_open: true,
            sidebar_search_open: false,
            sidebar_query: String::new(),
            decoder_library_open: false,
            library_query: String::new(),
            library_filter: LibraryFormat::All,
            library_selected: None,
            library_renaming: false,
            library_rename_draft: String::new(),
            library_delete_confirm: false,
            sidebar_settings_open: false,
            csv_table: CsvTable::default(),
            pending_config: None,
            pending_recent_decoder: None,
            file_dialog: NativeFileDialog::default(),
            file_locations,
            dialog_template: rfd::AsyncFileDialog::new(),
            load_progress: Arc::new(LoadProgress::default()),
            csv_applied: None,
            csv_result_x: "X".into(),
            csv_target: None,
            csv_full_index: false,
            csv_advance_plot: false,
            csv_cache_root: crate::cache::root(),
            csv_selection: None,
            csv_link_request: None,
            csv_link_serial: 0,
            csv_link_indexing: false,
            csv_link_job: None,
            csv_link_rx: None,
            csv_link_cancel: Arc::new(AtomicBool::new(false)),
            last_hex_selection: None,
            cache_message: None,
        }
    }
}

impl Drop for BinwaveApp {
    fn drop(&mut self) {
        self.plot_cancel.store(true, Ordering::Relaxed);
        self.load_progress.cancelled.store(true, Ordering::Relaxed);
        self.csv_link_cancel.store(true, Ordering::Relaxed);
    }
}

impl BinwaveApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::install(&cc.egui_ctx);
        let mut app = Self::default();
        app.workspace = WorkspaceLayout::load(cc.storage);
        app.restore_file_locations(cc.storage);
        app.sidebar_open = cc.storage
            .and_then(|storage| storage.get_string("sidebar-open-v2"))
            .as_deref() != Some("false");
        if let Some(width) = cc
            .storage
            .and_then(|storage| storage.get_string("editor-tab-width"))
            .and_then(|width| width.parse().ok())
        {
            app.binary.set_editor_tab_width(width);
        }
        app
    }

    pub fn open_launch_paths(&mut self, data: Option<PathBuf>, decoder: Option<PathBuf>, ctx: &Context) {
        match (data, decoder) {
            (Some(data), Some(decoder)) if LibraryFormat::from_path(&decoder) == Some(LibraryFormat::Csv) => {
                match AppConfig::load_from_file(&decoder) {
                    Ok(mut cfg) => {
                        cfg.data_path = Some(data.clone());
                        self.pending_config = Some(cfg);
                        self.decoder_library_open = false;
                        self.binary.open(data, ctx);
                    }
                    Err(error) => self.cache_message = Some(format!("Config: {error:#}")),
                }
            }
            (Some(data), Some(decoder)) => {
                self.open_path(data.clone(), ctx);
                self.pending_recent_decoder = Some((data, decoder));
            }
            (Some(data), None) => self.open_path(data, ctx),
            (None, Some(decoder)) if LibraryFormat::from_path(&decoder) == Some(LibraryFormat::Csv) => {
                self.load_csv_config_path(decoder, ctx);
            }
            (None, Some(decoder)) => {
                self.decoder = Decoder::Layout;
                self.decoder_library_open = false;
                self.show_empty_workspace = true;
                self.binary.load_yaml(decoder, ctx);
                self.workspace.show_decoder();
            }
            (None, None) => {}
        }
    }

    fn restore_file_locations(&mut self, storage: Option<&dyn eframe::Storage>) {
        self.file_locations = FileLocations::load(storage);
        self.binary.set_file_locations(self.file_locations.clone());
    }

    fn begin_load(&mut self, cfg: AppConfig, ctx: &Context) {
        self.clear_csv_selection();
        self.cfg = cfg.clone();
        self.csv_applied = Some(cfg);
        self.load_has_data = false;
        self.csv_target = None;
        self.csv_advance_plot = false;
        self.csv_table = CsvTable::default();
        self.start_csv_read(CSV_INITIAL_ROWS, false, ctx);
    }

    fn start_csv_read(&mut self, rows: usize, full: bool, ctx: &Context) {
        let Some(cfg) = self.csv_applied.clone() else {
            return;
        };
        self.load_progress.cancelled.store(true, Ordering::Relaxed);
        self.load_progress = Arc::new(LoadProgress::default());
        self.csv_full_index = full;
        self.status = if full {
            "Indexing entire CSV…"
        } else {
            "Reading CSV rows…"
        }
        .into();
        self.load_preview = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel();
        self.loader_rx = Some(rx);
        let progress = self.load_progress.clone();
        let preview = self.load_preview.clone();
        let root = self.csv_cache_root.clone();
        ctx.request_repaint();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let update = |data| {
                *preview.lock().unwrap_or_else(|e| e.into_inner()) = Some(data);
                ctx.request_repaint();
            };
            let res = if full {
                crate::loaders::csv_index::load(&cfg, &root, &progress, update)
            } else {
                load_csv_rows(&cfg, &root, &progress, rows, update)
            };
            let _ = tx.send(res);
            ctx.request_repaint();
        });
    }

    fn csv_complete(&self) -> bool {
        self.data.as_ref().is_some_and(|data| {
            data.csv_extent
                .as_ref()
                .map_or(!self.data_partial, |extent| extent.complete)
        })
    }

    fn request_csv_rows(&mut self, rows: usize, ctx: &Context) {
        if !self.load_has_data || self.csv_complete() || self.csv_applied.is_none() {
            return;
        }
        if self.data.as_ref().is_some_and(|data| data.rows >= rows) {
            return;
        }
        self.follow_loading = false;
        self.csv_link_indexing = false;
        self.csv_target = Some(rows);
        if self.loader_rx.is_none() {
            self.start_csv_read(rows, false, ctx);
        }
    }

    fn index_entire_csv(&mut self, ctx: &Context) {
        if !self.load_has_data || self.csv_complete() || self.csv_applied.is_none() {
            return;
        }
        self.csv_target = None;
        self.csv_link_indexing = false;
        self.follow_loading = false;
        self.start_csv_read(usize::MAX, true, ctx);
    }

    fn stop_csv_read(&mut self) {
        self.csv_link_indexing = false;
        self.csv_link_request = None;
        self.csv_link_cancel.store(true, Ordering::Relaxed);
        self.load_progress.cancelled.store(true, Ordering::Relaxed);
        self.csv_target = None;
        self.csv_advance_plot = false;
    }

    fn clear_csv_selection(&mut self) {
        self.csv_link_serial = self.csv_link_serial.wrapping_add(1);
        self.csv_link_cancel.store(true, Ordering::Relaxed);
        self.csv_link_request = None;
        self.csv_selection = None;
        self.csv_table.clear_selection();
        if self.csv_link_indexing {
            self.load_progress.cancelled.store(true, Ordering::Relaxed);
            self.csv_target = None;
            self.csv_link_indexing = false;
            // A superseded byte lookup must not publish into or cancel its successor.
            self.loader_rx = None;
            self.load_preview = Arc::new(Mutex::new(None));
            self.csv_full_index = false;
        }
        self.last_hex_selection = self.binary.byte_selection();
    }

    fn request_csv_link(&mut self, request: CsvLinkRequest) {
        if self.displayed_decoder != Decoder::Csv || !self.load_has_data {
            return;
        }
        self.clear_csv_selection();
        match &request {
            CsvLinkRequest::Cell(cell) => self.csv_table.select(*cell),
            CsvLinkRequest::Rows(rows) => self.csv_table.select_rows(*rows),
            CsvLinkRequest::Bytes(_) => {}
        }
        self.csv_link_request = Some(request);
        self.follow_loading = false;
    }

    fn poll_csv_links(&mut self, ctx: &Context) {
        if self.displayed_decoder != Decoder::Csv || !self.load_has_data {
            return;
        }
        let bytes = self.binary.byte_selection();
        if bytes != self.last_hex_selection {
            self.clear_csv_selection();
            if let Some(range) = bytes {
                self.request_csv_link(CsvLinkRequest::Bytes(range));
            }
        }
        let result = self
            .csv_link_rx
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("CSV selection worker stopped")))
                }
            });
        if let Some(result) = result {
            self.csv_link_rx = None;
            let job = self.csv_link_job.take().unwrap();
            if job.serial == self.csv_link_serial
                && job.dataset == self.dataset
                && self.csv_link_request.as_ref() == Some(&job.request)
            {
                self.csv_link_request = None;
                self.csv_link_indexing = false;
                match result {
                    Ok(Some(CsvLinkRows { row, anchor })) => {
                        let mut selected_rows = None;
                        let (column, bytes, reveal) = match job.request {
                            CsvLinkRequest::Cell(cell) => {
                                let bytes = cell
                                    .column
                                    .and_then(|column| row.fields.get(column).cloned())
                                    .unwrap_or_else(|| row.range.clone());
                                (cell.column, bytes, true)
                            }
                            CsvLinkRequest::Rows(rows) => {
                                selected_rows = Some(rows);
                                let first = anchor.as_ref().unwrap_or(&row);
                                let bytes = first.range.start.min(row.range.start)
                                    ..first.range.end.max(row.range.end);
                                (None, bytes, true)
                            }
                            CsvLinkRequest::Bytes(bytes) => {
                                let column = row.fields.iter().position(|field| {
                                    !field.is_empty()
                                        && field.start <= bytes.start
                                        && bytes.end <= field.end
                                });
                                (column, bytes, false)
                            }
                        };
                        if let Some(signal) = column.and_then(|column| column.checked_sub(1)) {
                            if let Some(visible) = self.visible.get_mut(signal) {
                                *visible = true;
                            }
                        }
                        if let Some(rows) = selected_rows {
                            self.csv_table.select_rows(rows);
                        } else {
                            self.csv_table.select(CsvCellSelection {
                                row: row.index,
                                column,
                            });
                        }
                        if let (Some(&x), Some(view), Some(data)) =
                            (row.values.first(), &mut self.view, &self.data)
                        {
                            let bounds = view.bounds(data);
                            if x.is_finite() && (x < bounds.0 || x > bounds.1) {
                                view.set_center_span(x, view.span, data);
                            }
                        }
                        if reveal {
                            self.binary.select_source_range(bytes.clone(), true);
                            self.last_hex_selection = self.binary.byte_selection();
                        }
                        self.csv_selection = Some(CsvLinkedSelection {
                            row,
                            column,
                            bytes,
                            rows: selected_rows,
                            anchor,
                        });
                    }
                    Ok(None) => {
                        // csv::Reader can stop between CR and LF. Probe the exact
                        // boundary before requesting more rows, so its LF still
                        // selects the preceding row without scanning the next one.
                        if let CsvLinkRequest::Bytes(bytes) = &job.request {
                            if let Some(data) = &self.data {
                                if data.csv_extent.is_some_and(|extent| {
                                    !extent.complete
                                        && bytes.start as u64 >= extent.indexed_bytes
                                        && (bytes.start as u64) < extent.source_bytes
                                }) {
                                    let target = data.rows.saturating_add(CSV_INITIAL_ROWS);
                                    self.csv_link_request = Some(job.request);
                                    if self.loader_rx.is_none() {
                                        self.csv_link_indexing = true;
                                        self.start_csv_read(target, false, ctx);
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        log::error!("CSV selection: {error:#}");
                        self.status = format!("CSV selection: {error:#}");
                    }
                }
            }
        }
        if self.csv_link_rx.is_none() && !(self.csv_link_indexing && self.loader_rx.is_some()) {
            if let (Some(request), Some(data)) = (&self.csv_link_request, &self.data) {
                let beyond_prefix = match request {
                    CsvLinkRequest::Bytes(bytes) => data.csv_extent.is_some_and(|extent| {
                        !extent.complete
                            && bytes.start as u64 > extent.indexed_bytes
                            && (bytes.start as u64) < extent.source_bytes
                    }),
                    CsvLinkRequest::Cell(cell) => cell.row >= data.rows && !self.csv_complete(),
                    CsvLinkRequest::Rows(rows) => {
                        rows.anchor.max(rows.active) >= data.rows && !self.csv_complete()
                    }
                };
                if beyond_prefix {
                    if self.loader_rx.is_none() {
                        let target = data.rows.saturating_add(CSV_INITIAL_ROWS);
                        self.csv_link_indexing = true;
                        self.start_csv_read(target, false, ctx);
                    }
                } else if let Some(rows) = data.csv_rows.clone() {
                    let job = CsvLinkJob {
                        serial: self.csv_link_serial,
                        dataset: self.dataset,
                        request: request.clone(),
                    };
                    self.csv_link_job = Some(job.clone());
                    let (tx, rx) = mpsc::channel();
                    self.csv_link_rx = Some(rx);
                    let cancel = Arc::new(AtomicBool::new(false));
                    self.csv_link_cancel = cancel.clone();
                    let ctx = ctx.clone();
                    thread::spawn(move || {
                        let result = match job.request {
                            CsvLinkRequest::Cell(cell) => rows
                                .row(cell.row, &cancel)
                                .map(|row| Some(CsvLinkRows { row, anchor: None })),
                            CsvLinkRequest::Rows(selection) => (|| {
                                let row = rows.row(selection.active, &cancel)?;
                                let anchor = if selection.anchor != selection.active {
                                    Some(rows.row(selection.anchor, &cancel)?)
                                } else {
                                    None
                                };
                                Ok(Some(CsvLinkRows { row, anchor }))
                            })(),
                            CsvLinkRequest::Bytes(bytes) => rows
                                .find_byte(bytes.start, &cancel)
                                .map(|row| row.map(|row| CsvLinkRows { row, anchor: None })),
                        };
                        let _ = tx.send(result);
                        ctx.request_repaint();
                    });
                } else {
                    self.csv_link_request = None;
                }
            }
        }
        if self.csv_link_request.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(25));
        }
    }

    fn csv_column_name(&self, column: usize) -> &str {
        if column == 0 {
            &self.csv_result_x
        } else {
            self.data
                .as_ref()
                .and_then(|data| data.signals.get(column - 1))
                .map_or("Value", |signal| signal.name.as_str())
        }
    }

    fn csv_selected_samples(&self) -> Vec<(usize, crate::plot_index::Sample)> {
        let Some(selection) = &self.csv_selection else {
            return Vec::new();
        };
        let mut samples = Vec::new();
        if let Some(rows) = selection.rows {
            // Highlight the bounded plot preview by record identity: duplicate or
            // unsorted X values must not select unrelated records in between them.
            let range = rows.rows();
            samples.extend(
                self.plot_lines
                    .iter()
                    .filter(|(column, _, _)| self.visible.get(*column) == Some(&true))
                    .flat_map(|(column, _, points)| {
                        points.iter().filter_map(|sample| {
                            (range.contains(&sample.record)
                                && sample.point.iter().all(|value| value.is_finite()))
                            .then_some((*column, *sample))
                        })
                    }),
            );
        }
        for row in std::iter::once(&selection.row).chain(selection.anchor.as_ref()) {
            let Some(&x) = row.values.first().filter(|x| x.is_finite()) else {
                continue;
            };
            for (column, &y) in row.values.iter().enumerate().skip(1) {
                if y.is_finite()
                    && self.visible.get(column - 1) == Some(&true)
                    && selection
                        .column
                        .is_none_or(|selected| selected == 0 || selected == column)
                    && !samples
                        .iter()
                        .any(|(signal, sample)| *signal == column - 1 && sample.record == row.index)
                {
                    samples.push((
                        column - 1,
                        crate::plot_index::Sample {
                            record: row.index,
                            byte_offset: row.fields[column].start,
                            point: [x, y],
                        },
                    ));
                }
            }
        }
        samples
    }

    fn csv_highlights(&self) -> Vec<HexHighlight> {
        let Some(selection) = &self.csv_selection else {
            return Vec::new();
        };
        selection
            .row
            .fields
            .iter()
            .enumerate()
            .filter(|(_, range)| !range.is_empty())
            .map(|(column, range)| HexHighlight {
                range: range.clone(),
                name: format!(
                    "Row {} · {}",
                    selection.row.index,
                    self.csv_column_name(column)
                ),
                color: [
                    egui::Color32::from_rgb(91, 154, 184),
                    egui::Color32::from_rgb(176, 145, 83),
                    egui::Color32::from_rgb(125, 163, 104),
                ][column % 3],
            })
            .collect()
    }

    fn run_active_decoder(&mut self, ctx: &Context) {
        if self.decoder == Decoder::Csv {
            if self.binary.source_path().is_some() && !self.binary.has_byte_edits() {
                self.begin_load(self.cfg.clone(), ctx);
            }
        } else if self.binary.can_apply_definition() {
            self.binary.apply_definition(ctx);
        }
    }

    fn accept_data(&mut self, data: DataStore, partial: bool) {
        if let Some(cfg) = &self.csv_applied {
            self.csv_result_x.clone_from(&cfg.csv.time_column);
        }
        self.displayed_decoder = Decoder::Csv;
        self.generation = self.generation.wrapping_add(1);
        if !self.load_has_data {
            if self.binary.has_file() {
                self.workspace.show_samples();
                self.workspace.show_initial_plot();
                self.workspace.show_decoder();
            }
            self.dataset = self.dataset.wrapping_add(1);
            self.last_hex_selection = None;
            self.plot_lines.clear();
        }
        if !self.load_has_data {
            self.visible = vec![true; data.signals.len()];
            self.view = Some(ViewState::from_store(&data));
            self.follow_loading = !self.csv_full_index;
        } else if self.csv_advance_plot {
            if let Some(view) = &mut self.view {
                view.jump_to_end(&data);
            }
        } else if self.follow_loading {
            if let Some(view) = &mut self.view {
                view.set_full(&data);
            }
        }
        let partial = partial
            || data
                .csv_extent
                .as_ref()
                .is_some_and(|extent| !extent.complete);
        self.status = if partial {
            format!("{} CSV rows available", data.rows)
        } else {
            format!("{} CSV rows · complete", data.rows)
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
            let success = result.is_ok();
            let stopped = self.load_progress.cancelled.load(Ordering::Relaxed);
            if !success || stopped {
                self.csv_link_request = None;
                self.csv_link_indexing = false;
                self.csv_link_cancel.store(true, Ordering::Relaxed);
            }
            match result {
                Ok(data) => self.accept_data(data, false),
                Err(err) => {
                    if !stopped {
                        log::error!("CSV: {err:#}");
                    }
                    self.status = if stopped {
                        "CSV reading stopped.".into()
                    } else {
                        format!("CSV: {err:#}")
                    };
                    if self.load_has_data {
                        self.status.push_str(&format!(
                            " {} rows remain available.",
                            self.data.as_ref().unwrap().rows
                        ));
                    }
                }
            }
            self.loader_rx = None;
            self.csv_full_index = false;
            if success && !stopped && !self.csv_complete() {
                if let Some(target) = self.csv_target {
                    if self.data.as_ref().is_some_and(|data| data.rows < target) {
                        self.start_csv_read(target, false, ctx);
                        return;
                    }
                }
            }
            self.csv_target = None;
            self.csv_advance_plot = false;
        }
    }

    fn ui_decoder(&mut self, ui: &mut Ui) {
        let mut mode = if self.decoder == Decoder::Csv {
            2
        } else if self.binary.is_python() {
            0
        } else {
            1
        };
        let previous = mode;
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(self.commands_available(), |ui| {
                ui.selectable_value(&mut mode, 1, "YAML");
                ui.selectable_value(&mut mode, 0, "Python");
                ui.selectable_value(&mut mode, 2, "CSV");
            });
        });
        if previous != mode {
            if mode == 2 {
                self.decoder = Decoder::Csv;
            } else {
                self.decoder = Decoder::Layout;
                self.binary.select_language(if mode == 0 {
                    DecoderLanguage::Python
                } else {
                    DecoderLanguage::Yaml
                });
            }
        }
        ui.separator();
        if self.decoder == Decoder::Csv {
            egui::ScrollArea::vertical()
                .id_salt("csv_decoder_settings")
                .show(ui, |ui| self.ui_csv_decoder(ui));
        } else {
            self.binary.decoder_controls(ui);
        }
    }

    fn ui_csv_decoder(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    self.commands_available(),
                    egui::Button::new("Open configuration…"),
                )
                .clicked()
            {
                self.choose_csv_config(ui.ctx());
            }
            if ui
                .add_enabled(
                    self.commands_available()
                        && self.binary.source_path().is_some()
                        && !self.binary.has_byte_edits(),
                    egui::Button::new("Apply").shortcut_text("F5"),
                )
                .clicked()
            {
                self.run_active_decoder(ui.ctx());
            }
        });
        ui.add_space(8.);
        egui::Grid::new("csv_format_settings")
            .num_columns(2)
            .spacing(egui::vec2(12., 10.))
            .show(ui, |ui| {
                ui.label("Separator");
                egui::ComboBox::from_id_salt("csv_separator")
                    .selected_text(match self.cfg.csv.delimiter {
                        ',' => "Comma",
                        '\t' => "Tab",
                        ';' => "Semicolon",
                        '|' => "Pipe",
                        _ => "Custom",
                    })
                    .show_ui(ui, |ui| {
                        for (value, name) in [
                            (',', "Comma"),
                            ('\t', "Tab"),
                            (';', "Semicolon"),
                            ('|', "Pipe"),
                        ] {
                            ui.selectable_value(&mut self.cfg.csv.delimiter, value, name);
                        }
                    });
                ui.end_row();
                ui.label("Custom separator");
                let mut separator = if self.cfg.csv.delimiter == '\t' {
                    String::new()
                } else {
                    self.cfg.csv.delimiter.to_string()
                };
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut separator)
                            .char_limit(1)
                            .desired_width(35.),
                    )
                    .changed()
                {
                    if let Some(value) = separator.chars().next().filter(char::is_ascii) {
                        self.cfg.csv.delimiter = value;
                    }
                }
                ui.end_row();
                ui.label("Column names");
                if ui
                    .checkbox(&mut self.cfg.csv.has_header, "First row is a header")
                    .changed()
                    && (self.cfg.csv.time_column == "time" || self.cfg.csv.time_column == "c0")
                {
                    self.cfg.csv.time_column = if self.cfg.csv.has_header {
                        "time"
                    } else {
                        "c0"
                    }
                    .into();
                }
                ui.end_row();
                ui.label("X column");
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.csv.time_column).desired_width(150.),
                );
                ui.end_row();
            });
        if self.csv_applied.as_ref().is_some_and(|cfg| {
            serde_json::to_value(cfg.csv.parse_options()).ok()
                != serde_json::to_value(self.cfg.csv.parse_options()).ok()
        }) {
            ui.colored_label(
                egui::Color32::YELLOW,
                "Settings changed · Apply to update samples",
            );
        }
        ui.add_space(10.);
        if self.binary.has_byte_edits() {
            ui.label("Save and reopen the edited copy to decode its CSV rows.");
        } else if !self.binary.has_file() {
            ui.label("Open a data file to decode CSV rows.");
        }
        ui.label(&self.status);
        if let Some(extent) = self.data.as_ref().and_then(|data| data.csv_extent.as_ref()) {
            if !extent.complete {
                ui.weak(format!(
                    "{:.1} of {:.1} MiB indexed",
                    extent.indexed_bytes as f64 / 1048576.,
                    extent.source_bytes as f64 / 1048576.
                ));
            }
        }
        if self.loader_rx.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.csv_full_index {
                    "Indexing entire file…"
                } else {
                    "Reading requested rows…"
                });
                if ui.button("Stop").clicked() {
                    self.stop_csv_read();
                }
            });
            if self.csv_full_index {
                let done = self.load_progress.done.load(Ordering::Relaxed);
                let total = self.load_progress.total.load(Ordering::Relaxed);
                ui.add(egui::ProgressBar::new(if total == 0 {
                    0.
                } else {
                    done as f32 / total as f32
                }));
            }
        } else if self.load_has_data && self.data.is_some() && !self.csv_complete() {
            if ui.button("Index entire file").on_hover_text("Scan every row for a complete time range and row count. Browsing does not require this.").clicked() {
                self.index_entire_csv(ui.ctx());
            }
        }
    }

    fn ui_csv_columns(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui.button("Show all").clicked() {
                self.visible.fill(true);
            }
            if ui.button("Hide all").clicked() {
                self.visible.fill(false);
            }
        });
        ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Find column"));
        let search = self.search.to_lowercase();
        egui::ScrollArea::vertical()
            .max_height(260.)
            .show(ui, |ui| {
                if let Some(data) = &self.data {
                    for (index, signal) in data.signals.iter().enumerate() {
                        if search.is_empty() || signal.name.to_lowercase().contains(&search) {
                            if let Some(visible) = self.visible.get_mut(index) {
                                ui.checkbox(visible, &signal.name);
                            }
                        }
                    }
                }
            });
    }

    fn open_path(&mut self, path: std::path::PathBuf, ctx: &Context) {
        if !self.commands_available() {
            return;
        }
        self.pending_config = None;
        self.pending_recent_decoder = None;
        self.decoder_library_open = false;
        self.binary.open(path, ctx);
    }

    fn open_recent(&mut self, index: usize, ctx: &Context) {
        if !self.commands_available() { return; }
        let Some(pair) = self.file_locations.recent_pairs().get(index).cloned() else { return; };
        if !pair.data.is_file() {
            self.cache_message = Some("That recent data file is no longer available. Choose it again with Open file.".into());
            return;
        }
        if let Some(decoder) = &pair.decoder {
            if !decoder.is_file() {
                self.cache_message = Some("That recent decoder is no longer available. Open the data file, then choose its decoder again.".into());
                return;
            }
        }
        self.pending_config = None;
        self.pending_recent_decoder = pair.decoder.map(|decoder| (pair.data.clone(), decoder));
        self.decoder_library_open = false;
        self.binary.open(pair.data, ctx);
    }

    fn recent_label(pair: &crate::file_dialog::RecentPair) -> String {
        let data = pair.data.file_name().unwrap_or(pair.data.as_os_str()).to_string_lossy();
        match &pair.decoder {
            Some(decoder) => {
                let name = decoder.file_name().unwrap_or(decoder.as_os_str()).to_string_lossy();
                format!("{data}  ·  {name}")
            }
            None => data.into_owned(),
        }
    }

    fn clear_csv(&mut self) {
        self.clear_csv_selection();
        self.displayed_decoder = Decoder::Layout;
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
        self.csv_applied = None;
        self.csv_target = None;
        self.csv_full_index = false;
        self.csv_advance_plot = false;
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
                let recent_decoder = self.pending_recent_decoder.take()
                    .filter(|(data, _)| data == &path)
                    .map(|(_, decoder)| decoder);
                let config = self
                    .pending_config
                    .take()
                    .filter(|cfg| cfg.data_path.as_ref() == Some(&path));
                let is_csv = config.is_some()
                    || (recent_decoder.is_none() && file_kind(&path) == FileKind::Csv);
                let mut cfg = config.unwrap_or_else(|| self.cfg.clone());
                cfg.data_path = Some(path);
                self.cfg = cfg.clone();
                if is_csv {
                    self.decoder = Decoder::Csv;
                    self.begin_load(cfg, ctx);
                } else {
                    if let Some(decoder) = recent_decoder {
                        self.binary.load_yaml(decoder, ctx);
                    } else {
                        self.binary.start_byte_preview(ctx);
                    }
                }
            } else {
                self.pending_config = None;
            }
            if self.decoder == Decoder::Layout {
                self.binary.show_yaml();
            }
            if self.decoder != Decoder::Layout { self.workspace.show_decoder(); }
        }
        if let Some(data) = self.binary.source_path() {
            if file_kind(data) != FileKind::Csv {
                self.file_locations.remember_pair(data, self.binary.decoder_path());
            }
        }
        if self.binary.take_bytes_changed() {
            self.invalidate_csv_after_edit();
        }
        if self.binary.take_interpretation_ready() { self.workspace.reveal(Pane::Structures); }
        let initial_plot_ready = self.binary.take_initial_plot_ready();
        if self.binary.take_layout_applied() {
            if !self.binary.showing_byte_preview() {
                if let (Some(data), Some(decoder)) = (self.binary.source_path(), self.binary.decoder_path()) {
                    self.file_locations.remember_decoder_run(decoder, data);
                }
            }
            if initial_plot_ready {
                self.workspace.show_initial_plot();
            } else {
                self.workspace.show_samples();
            }
            self.displayed_decoder = Decoder::Layout;
            self.clear_csv_selection();
            self.stop_csv_read();
            // A cancelled CSV worker must not replace this successful decode later.
            self.loader_rx = None;
            self.load_preview = Arc::new(Mutex::new(None));
        }
        self.reveal_requested_panels();
        self.poll_loader(ctx);
        self.poll_csv_links(ctx);
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
            self.decoder = Decoder::Layout;
            self.show_empty_workspace = true;
            self.workspace.show_decoder();
        }
        if self.binary.take_reveal_hex() {
            self.show_empty_workspace = true;
            self.workspace.reveal(Pane::Hex);
        }
    }

    fn ui_inspector(&mut self, ui: &mut Ui) {
        if self.displayed_decoder == Decoder::Csv {
            if let Some(selection) = &self.csv_selection {
                if let Some(rows) = selection.rows {
                    let range = rows.rows();
                    ui.strong(format!("CSV rows {}–{}", range.start, range.end - 1));
                    ui.label(format!("{} rows selected", range.len()));
                } else {
                    ui.strong(format!("CSV row {}", selection.row.index));
                }
                if let Some(column) = selection.column {
                    ui.label(format!(
                        "{}: {}",
                        self.csv_column_name(column),
                        selection.row.values[column]
                    ));
                }
                ui.weak(format!(
                    "{} selected bytes · 0x{:X}..0x{:X}",
                    selection.bytes.len(),
                    selection.bytes.start,
                    selection.bytes.end
                ));
                ui.separator();
            }
            self.binary.source_selection_inspector(ui);
        } else {
            self.binary.selection_inspector(ui);
        }
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
                .set_title("Open file — Binwave"),
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
                .set_title("Load CSV configuration — Binwave")
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
            AppDialogAction::LoadCsvConfig => self.load_csv_config_path(path, ctx),
            AppDialogAction::ImportDecoder => {
                if let Some(format) = LibraryFormat::from_path(&path) {
                    self.file_locations.remember_file(Self::library_file_kind(format), &path);
                    self.library_selected = Some(path);
                } else {
                    self.cache_message = Some("Select a YAML, Python, or CSV configuration file.".into());
                }
            }
        }
    }

    fn load_csv_config_path(&mut self, path: PathBuf, ctx: &Context) {
        match AppConfig::load_from_file(&path) {
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
                Err(error) => {
                    log::error!("Config: {error:#}");
                    self.cache_message = Some(format!("Config: {error:#}"));
                }
        }
    }

    fn ui_toolbar(&mut self, ui: &mut Ui) {
        use crate::app_menu::{self, Command::*};
        let ctx=ui.ctx().clone();
        let t = &crate::theme::DARK;
        let available=self.commands_available();
        let (undo,redo)=self.binary.menu_history();
        let recent=self.file_locations.recent_pairs().iter().map(Self::recent_label).collect();
        let menus=app_menu::menus(&app_menu::State { native:true, available, recent,
            file:self.binary.has_file(), selection:self.binary.has_selection(), undo,redo,
            save_decoder:self.decoder==Decoder::Layout && self.binary.can_save_definition(), apply:available && if self.decoder==Decoder::Csv { self.binary.source_path().is_some() && !self.binary.has_byte_edits() } else { self.binary.can_apply_definition() },
            busy:self.binary.is_decoding(), });
        let mut chosen = None;
        let mut recent_file = None;
        let mut open_file = false;
        let mut export = false;
        ui.horizontal(|ui| {
            if !self.sidebar_open {
                let icon_font = egui::FontId::new(t.size.sidebar_icon, egui::FontFamily::Name("phosphor".into()));
                if ui.button(egui::RichText::new(egui_phosphor::regular::SIDEBAR_SIMPLE).font(icon_font.clone()))
                    .on_hover_text("Show sidebar (Ctrl+B)").clicked() { self.sidebar_open = true; }
                if ui.button(egui::RichText::new(egui_phosphor::regular::FOLDER_OPEN).font(icon_font))
                    .on_hover_text("Open file (Ctrl+O)").clicked() { open_file = true; }
            }
            let title = self.binary.source_path().and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "Open a file".into());
            ui.menu_button(egui::RichText::new(title).size(t.font.title).strong(), |ui| {
                if ui.button("Open file…").clicked() { open_file = true; ui.close(); }
                for (index, pair) in self.file_locations.recent_pairs().iter().enumerate() {
                    let label = Self::recent_label(pair);
                    if ui.button(label).clicked() { recent_file = Some(index); ui.close(); }
                }
            });
            if self.binary.has_file() {
                let len = self.binary.file_len();
                let size = if len >= 1024 * 1024 { format!("{:.1} MiB", len as f64 / (1024. * 1024.)) }
                    else if len >= 1024 { format!("{:.1} KiB", len as f64 / 1024.) }
                    else { format!("{len} B") };
                ui.label(egui::RichText::new(format!("{size} · {}{}",
                    self.binary.default_byte_order().unwrap_or("byte order unspecified"),
                    if self.binary.is_dirty() { " · unsaved changes" } else { "" }))
                    .size(t.font.small).color(t.color.text.muted));
                let partial = self.data_partial || self.binary.status().1;
                let (badge, color) = if self.binary.is_decoding() { ("Decoding", t.color.accent.base) }
                    else if partial { ("Partial decode", t.color.status.warning) }
                    else { ("Ready", t.color.status.success) };
                egui::Frame::new().fill(color.gamma_multiply(t.color.status.warning_badge_alpha))
                    .corner_radius(egui::CornerRadius::same(t.radius.pill.min(u8::MAX as u16) as u8))
                    .inner_margin(egui::Margin::symmetric((t.spacing.card_padding / 2.) as i8,
                        t.spacing.sidebar_row_gap as i8))
                    .show(ui, |ui| { ui.label(egui::RichText::new(badge).size(t.font.label).color(color)); })
                    .response.on_hover_text(self.binary.status().0);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                chosen = app_menu::overflow_menu(ui, &menus);
                if ui.add_enabled(self.binary.has_file(), egui::Button::new("Export")
                    .stroke(egui::Stroke::new(t.size.frame_border, t.color.border.control)))
                    .on_hover_text("Save an edited copy of this file").clicked() { export = true; }
            });
        });
        if open_file { self.choose_data_file(&ctx); }
        if let Some(index) = recent_file { self.open_recent(index, &ctx); }
        if export { self.binary.save_copy(&ctx); }
        if let Some((command,argument))=chosen {
            if let Some(pane)=command.pane() {
                if pane==Pane::Layout { self.binary.show_yaml(); }
                self.show_view(pane);
            } else if command.editor_action().is_some() {
                self.binary.menu_action(command,&ctx); self.show_view(Pane::Layout);
            } else { match command {
                OpenFile => self.choose_data_file(&ctx),
                OpenRecent => self.open_recent(argument, &ctx),
                SimpleDemo => { self.pending_config=None; self.binary.open_simple_demo(&ctx); },
                AdvancedDemo => { self.pending_config=None; self.binary.open_advanced_demo(&ctx); },
                SensorDemo => { self.pending_config=None; self.binary.open_demo(&ctx); },
                SaveCopy => self.binary.save_copy(&ctx),
                Extract | Undo | Redo | CopyHex => self.binary.menu_action(command,&ctx),
                Cancel => self.binary.stop_decoding(),
                OpenDecoder => { self.decoder=Decoder::Layout; self.binary.choose_definition(&ctx); self.show_view(Pane::Layout); },
                SaveDecoder => self.binary.save_definition(&ctx),
                SaveDecoderAs => self.binary.save_definition_as(&ctx),
                NewDecoder => { self.decoder=Decoder::Layout; self.binary.new_yaml(&ctx); self.show_view(Pane::Layout); },
                Apply => self.run_active_decoder(&ctx),
                Example => if let Some((_,yaml))=crate::source_editor::YAML_EXAMPLES.get(argument) { self.decoder=Decoder::Layout; self.binary.load_definition_text(yaml,&ctx); self.show_view(Pane::Layout); },
                Reset => { self.workspace=WorkspaceLayout::default(); self.show_empty_workspace=true; },
                PlotWorkspace => { self.workspace=WorkspaceLayout::plot_and_bytes(); self.show_empty_workspace=true; },
                Steps => { self.decoder=Decoder::Layout; self.binary.show_steps(); self.show_view(Pane::Layout); },
                Describe => { self.binary.define_selection(&ctx); self.show_view(Pane::Layout); },
                Python => { self.decoder=Decoder::Layout; self.binary.new_python(&ctx); self.show_view(Pane::Layout); },
                PythonExample => { self.decoder=Decoder::Layout; self.binary.load_python_text(include_str!("../samples/decoders/python/waveform.py"),&ctx); self.show_view(Pane::Layout); },
                CsvConfig => self.choose_csv_config(&ctx),
                Find => { self.show_view(Pane::Hex); self.binary.find_bytes(); },
                Replace => { self.show_view(Pane::Hex); self.binary.replace_selected_bytes(); },
                Jump => { self.show_view(Pane::Hex); self.binary.jump_to_byte(); },
                ClearCache => {
                    self.cache_message = Some(match crate::cache::clear() {
                        Ok(()) => "Disk cache cleared. Open files remain available.".into(),
                        Err(error) => {
                            log::error!("Cache: {error:#}");
                            format!("Cache: {error:#}")
                        }
                    });
                },
                Help => self.show_decoding_guide=true,
                _ => {}
            }}
        }
    }

    fn ui_sidebar(&mut self, root_ui: &mut Ui) {
        let t = &crate::theme::DARK;
        let recent = self.file_locations.recent_pairs();
        let current = self.binary.source_path().map(std::path::Path::to_path_buf);
        let partial = self.data_partial || self.binary.status().1;
        let username = std::env::var("USER").or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "User".into());
        let result = egui::Panel::left("workspace_sidebar")
            .exact_size(t.size.sidebar_width)
            .resizable(false)
            .frame(egui::Frame::new().fill(t.color.surface.sidebar)
                .inner_margin(egui::Margin::same(t.spacing.card_padding as i8)))
            .show_collapsible(root_ui, &mut self.sidebar_open, |ui| {
                crate::sidebar::show(ui, &recent, current.as_deref(), partial, &username)
            });
        let ctx = root_ui.ctx().clone();
        match result.and_then(|result| result.inner) {
            Some(crate::sidebar::Action::Collapse) => self.sidebar_open = false,
            Some(crate::sidebar::Action::OpenFile) => self.choose_data_file(&ctx),
            Some(crate::sidebar::Action::Search) => self.sidebar_search_open = true,
            Some(crate::sidebar::Action::DecoderLibrary) => self.decoder_library_open = true,
            Some(crate::sidebar::Action::Recent(index)) => self.open_recent(index, &ctx),
            Some(crate::sidebar::Action::Settings) => self.sidebar_settings_open = true,
            None => {}
        }
    }

    fn ui_sidebar_search(&mut self, ctx: &Context) {
        if !self.sidebar_search_open { return; }
        let mut selected = None;
        let recent = self.file_locations.recent_pairs();
        egui::Window::new("Search files")
            .open(&mut self.sidebar_search_open)
            .default_width(crate::theme::DARK.size.sidebar_width * 2.)
            .show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.sidebar_query).hint_text("Find a recent file or decoder…"));
                let query = self.sidebar_query.trim().to_lowercase();
                for (index, pair) in recent.iter().enumerate() {
                    let label = Self::recent_label(pair);
                    if (query.is_empty() || label.to_lowercase().contains(&query))
                        && ui.button(&label).clicked() {
                        selected = Some(index);
                    }
                }
            });
        if let Some(index) = selected {
            self.sidebar_search_open = false;
            self.open_recent(index, ctx);
        }
    }

    fn ui_sidebar_settings(&mut self, ctx: &Context) {
        if !self.sidebar_settings_open { return; }
        egui::Window::new("Settings").open(&mut self.sidebar_settings_open).show(ctx, |ui| {
            ui.label("BinWave workspace");
            if ui.button("Show decoding guide").clicked() { self.show_decoding_guide = true; }
        });
    }

    fn ui_decoder_library_entry(&mut self, ui: &mut Ui) {
        let t = &crate::theme::DARK;
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Decoder library").size(t.font.title).strong());
            ui.weak(format!("{} decoders", self.file_locations.decoder_paths().len()));
            if ui.add(egui::Button::new("Back to workspace").frame(false)).clicked() {
                self.decoder_library_open = false;
            }
        });
        let mut paths: Vec<_> = self.file_locations.decoder_paths().into_iter()
            .filter(|path| path.is_file() && LibraryFormat::from_path(path).is_some())
            .collect();
        paths.sort_by_key(|path| decoder_library::name(path).to_ascii_lowercase());
        if self.library_selected.as_ref().is_none_or(|selected| !paths.contains(selected)) {
            self.library_selected = paths.first().cloned();
        }
        let mut open = None;
        let mut duplicate = None;
        let mut delete = None;
        let mut rename = None;
        let mut imported: Vec<PathBuf> = Vec::new();
        egui::Panel::right("decoder_library_details")
            .exact_size(t.size.library_details_width)
            .resizable(false)
            .frame(egui::Frame::new().fill(t.color.surface.card)
                .corner_radius(t.radius.card).inner_margin(t.spacing.card_padding as i8))
            .show(ui, |ui| {
                ui.set_width(t.size.library_details_width - 2. * t.spacing.card_padding);
                if let Some(path) = self.library_selected.clone() {
                    let name = decoder_library::name(&path);
                    let format = LibraryFormat::from_path(&path).unwrap_or_default();
                    ui.label(egui::RichText::new(&name).size(t.font.title).strong())
                        .on_hover_text(path.display().to_string());
                    ui.weak(format.label());
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(egui::RichText::new("Open in workspace")
                            .color(t.color.text.on_accent)).fill(t.color.accent.base)).clicked() {
                            open = Some(path.clone());
                        }
                        if ui.button("Duplicate").clicked() { duplicate = Some(path.clone()); }
                        if ui.button("Rename").clicked() {
                            self.library_renaming = true;
                            self.library_rename_draft = name.clone();
                        }
                    });
                    if self.library_renaming {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.library_rename_draft)
                                .hint_text("Decoder file name"));
                            if ui.button("Save name").clicked() { rename = Some(path.clone()); }
                            if ui.button("Cancel").clicked() { self.library_renaming = false; }
                        });
                    }
                    ui.separator();
                    ui.label("Last run");
                    if let Some(run) = self.file_locations.decoder_run(&path) {
                        ui.colored_label(t.color.status.success,
                            format!("Last run succeeded · {}", decoder_library::name(&run.data)));
                    } else {
                        ui.weak("Not run yet");
                    }
                    ui.add_space(t.spacing.card_padding);
                    let structures = decoder_library::yaml_structures(&path);
                    if !structures.is_empty() {
                        ui.label(format!("Structures · {}", structures.len()));
                        ui.horizontal(|ui| {
                            for structure in structures.iter().take(2) {
                                egui::Frame::new().fill(t.color.surface.raised)
                                    .corner_radius(t.radius.chip)
                                    .inner_margin(t.spacing.sidebar_row_gap as i8)
                                    .show(ui, |ui| { ui.label(structure); });
                            }
                            if structures.len() > 2 {
                                ui.weak(format!("+{} more", structures.len() - 2));
                            }
                        });
                    }
                    ui.add_space(t.spacing.card_padding);
                    ui.label("Preview");
                    let preview = std::fs::File::open(&path).ok().and_then(|file| {
                        let mut bytes = Vec::new();
                        file.take(t.size.library_preview_bytes as u64)
                            .read_to_end(&mut bytes).ok()?;
                        Some(String::from_utf8_lossy(&bytes).lines().take(14)
                            .map(|line| line.chars().take(58).collect::<String>())
                            .collect::<Vec<_>>().join("\n"))
                    });
                    egui::Frame::new().fill(t.color.surface.window)
                        .corner_radius(t.radius.well)
                        .inner_margin(t.spacing.card_padding as i8)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical().max_height(t.size.library_preview_height)
                                .show(ui, |ui| {
                                    ui.add(egui::Label::new(egui::RichText::new(
                                        preview.as_deref().unwrap_or("Preview unavailable"))
                                        .monospace()).wrap());
                                });
                        });
                    ui.add_space(t.spacing.card_padding);
                    let used_with: Vec<_> = self.file_locations.recent_pairs().into_iter()
                        .filter(|pair| pair.decoder.as_deref() == Some(path.as_path()))
                        .map(|pair| decoder_library::name(&pair.data)).collect();
                    if !used_with.is_empty() {
                        ui.label("Used with");
                        for file in used_with { ui.weak(file); }
                    }
                    ui.separator();
                    if ui.add(egui::Button::new(egui::RichText::new("Delete decoder")
                        .color(t.color.status.danger)).frame(false)).clicked() {
                        self.library_delete_confirm = true;
                    }
                    if self.library_delete_confirm {
                        ui.colored_label(t.color.status.danger,
                            format!("Delete {name} from disk? This cannot be undone."));
                        ui.horizontal(|ui| {
                            if ui.button("Delete file").clicked() { delete = Some(path.clone()); }
                            if ui.button("Cancel").clicked() { self.library_delete_confirm = false; }
                        });
                    }
                } else {
                    ui.weak("Select a decoder to inspect it.");
                }
            });
        egui::Frame::new().fill(t.color.surface.card)
            .corner_radius(t.radius.card).inner_margin(t.spacing.card_padding as i8)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_sized([260., t.size.control_height],
                        egui::TextEdit::singleline(&mut self.library_query)
                            .hint_text("Search decoders and structures"));
                    for filter in LibraryFormat::FILTERS {
                        ui.selectable_value(&mut self.library_filter, filter, filter.label());
                    }
                    if ui.button("Import…").clicked() {
                        let dialog = self.file_locations.configure(DialogFileKind::Yaml,
                            self.dialog_template.clone().set_title("Import decoder — Binwave")
                                .add_filter("Decoder", &["yaml", "yml", "py", "md", "markdown", "csv"]));
                        self.file_dialog.begin_open(dialog, AppDialogAction::ImportDecoder, &ctx);
                    }
                    if ui.add(egui::Button::new(egui::RichText::new("+ New decoder")
                        .color(t.color.text.on_accent)).fill(t.color.accent.base)).clicked() {
                        self.decoder = Decoder::Layout;
                        self.binary.new_yaml(&ctx);
                        self.decoder_library_open = false;
                        self.show_view(Pane::Layout);
                    }
                });
                ui.separator();
                let query = self.library_query.trim().to_ascii_lowercase();
                let filtered: Vec<_> = paths.iter().filter(|path| {
                    (self.library_filter == LibraryFormat::All
                        || LibraryFormat::from_path(path) == Some(self.library_filter))
                        && decoder_library::name(path).to_ascii_lowercase().contains(&query)
                }).collect();
                if filtered.is_empty() { ui.weak("No decoders match this view."); }
                if !filtered.is_empty() {
                    let header_width = ui.available_width();
                    ui.horizontal(|ui| {
                        ui.add_sized([header_width * 0.39, t.size.control_height], egui::Label::new("Name"));
                        ui.add_sized([header_width * 0.14, t.size.control_height], egui::Label::new("Format"));
                        ui.add_sized([header_width * 0.15, t.size.control_height], egui::Label::new("Structures"));
                        ui.add_sized([header_width * 0.15, t.size.control_height], egui::Label::new("Last edited"));
                        ui.label("Last run");
                    });
                }
                egui::ScrollArea::vertical()
                    .max_height((ui.available_height() - t.size.library_drop_height).max(t.size.library_row_height))
                    .show(ui, |ui| {
                    for path in filtered {
                        let name = decoder_library::name(path);
                        let format = LibraryFormat::from_path(path).unwrap_or_default();
                        let selected = self.library_selected.as_ref() == Some(path);
                        let edited = decoder_library::edited_label(path).unwrap_or_default();
                        let run = self.file_locations.decoder_run(path)
                            .map_or("Not run", |_| "Run succeeded");
                        let width = ui.available_width();
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(width, t.size.library_row_height), egui::Sense::click());
                        if selected || response.hovered() {
                            ui.painter().rect_filled(rect, t.radius.row,
                                if selected { t.color.surface.active } else { t.color.surface.raised });
                        }
                        let y = rect.center().y;
                        let x = |fraction: f32| rect.left() + t.spacing.card_padding + width * fraction;
                        for (fraction, value, color) in [
                            (0., name.as_str(), t.color.text.primary),
                            (0.40, format.label(), t.color.text.muted),
                            (0.55, "", t.color.text.secondary),
                            (0.70, edited.as_str(), t.color.text.muted),
                            (0.85, run, t.color.text.muted),
                        ] {
                            if !value.is_empty() {
                                ui.painter().text(egui::pos2(x(fraction), y), egui::Align2::LEFT_CENTER,
                                    value, egui::FontId::proportional(t.font.body), color);
                            }
                        }
                        ui.painter().text(egui::pos2(x(0.55), y), egui::Align2::LEFT_CENTER,
                            decoder_library::yaml_structures(path).len().to_string(),
                            egui::FontId::proportional(t.font.body), t.color.text.secondary);
                        if response.on_hover_text(path.display().to_string()).clicked() {
                            self.library_selected = Some(path.clone());
                            self.library_delete_confirm = false;
                            self.library_renaming = false;
                        }
                    }
                });
                ui.separator();
                let drop = egui::Frame::new()
                    .corner_radius(t.radius.inset).inner_margin(t.spacing.card_padding as i8)
                    .show(ui, |ui| { ui.centered_and_justified(|ui| {
                        ui.weak("Drop YAML, Python, or CSV configuration files here to import");
                    }); });
                let rect = drop.response.rect;
                ui.painter().extend(egui::Shape::dashed_line(
                    &[rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()],
                    egui::Stroke::new(t.size.frame_border, t.color.border.control),
                    t.size.library_drop_dash, t.size.library_drop_gap));
            });
        for file in ctx.input(|input| input.raw.dropped_files.clone()) {
            let path = file.path();
            if path.is_file() {
                if let Some(format) = LibraryFormat::from_path(&path) {
                    self.file_locations.remember_file(Self::library_file_kind(format), &path);
                    imported.push(path.to_path_buf());
                }
            }
        }
        if let Some(path) = imported.last() { self.library_selected = Some(path.clone()); }
        if let Some(path) = open { self.open_library_decoder(path, &ctx); }
        if let Some(path) = duplicate { self.duplicate_library_decoder(&path); }
        if let Some(path) = rename { self.rename_library_decoder(&path, &ctx); }
        if let Some(path) = delete { self.delete_library_decoder(&path, &ctx); }
    }

    fn library_file_kind(format: LibraryFormat) -> DialogFileKind {
        match format {
            LibraryFormat::Yaml | LibraryFormat::All => DialogFileKind::Yaml,
            LibraryFormat::Python => DialogFileKind::Python,
            LibraryFormat::Csv => DialogFileKind::CsvConfig,
        }
    }

    fn open_library_decoder(&mut self, path: PathBuf, ctx: &Context) {
        match LibraryFormat::from_path(&path) {
            Some(LibraryFormat::Yaml | LibraryFormat::Python) => {
                self.decoder = Decoder::Layout;
                self.binary.load_yaml(path, ctx);
                self.decoder_library_open = false;
                self.show_view(Pane::Layout);
            }
            Some(LibraryFormat::Csv) => self.load_csv_config_path(path, ctx),
            _ => self.cache_message = Some("Unsupported decoder format.".into()),
        }
    }

    fn duplicate_library_decoder(&mut self, path: &Path) {
        let Some(destination) = decoder_library::duplicate_path(path) else {
            self.cache_message = Some("Could not find an unused copy name.".into());
            return;
        };
        let result = (|| -> std::io::Result<()> {
            let mut source = std::fs::File::open(path)?;
            let mut target = std::fs::OpenOptions::new().write(true).create_new(true).open(&destination)?;
            std::io::copy(&mut source, &mut target)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                let format = LibraryFormat::from_path(&destination).unwrap();
                self.file_locations.remember_file(Self::library_file_kind(format), &destination);
                self.library_selected = Some(destination);
            }
            Err(error) => self.cache_message = Some(format!("Could not duplicate decoder: {error}")),
        }
    }

    fn rename_library_decoder(&mut self, path: &Path, ctx: &Context) {
        let active = self.binary.decoder_path() == Some(path);
        if active && self.binary.is_dirty() {
            self.cache_message = Some("Save the open decoder before renaming it.".into());
            return;
        }
        let name = self.library_rename_draft.trim();
        if !decoder_library::valid_new_name(name) {
            self.cache_message = Some("Enter a file name without directory separators.".into());
            return;
        }
        let destination = path.with_file_name(name);
        let Some(format) = LibraryFormat::from_path(&destination) else {
            self.cache_message = Some("Use a YAML, Python, or CSV configuration extension.".into());
            return;
        };
        if destination != path && destination.exists() {
            self.cache_message = Some("A file with that name already exists.".into());
            return;
        }
        if let Err(error) = std::fs::rename(path, &destination) {
            self.cache_message = Some(format!("Could not rename decoder: {error}"));
            return;
        }
        self.file_locations.rename_decoder(path, &destination);
        self.file_locations.remember_file(Self::library_file_kind(format), &destination);
        if active && format != LibraryFormat::Csv {
            self.binary.load_yaml(destination.clone(), ctx);
        }
        self.library_selected = Some(destination);
        self.library_renaming = false;
    }

    fn delete_library_decoder(&mut self, path: &Path, ctx: &Context) {
        let active = self.binary.decoder_path() == Some(path);
        if active && self.binary.is_dirty() {
            self.cache_message = Some("Save the open decoder before deleting it.".into());
            return;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {
                self.file_locations.forget_decoder(path);
                if active { self.binary.new_yaml(ctx); }
                self.library_selected = None;
                self.library_delete_confirm = false;
            }
            Err(error) => self.cache_message = Some(format!("Could not delete decoder: {error}")),
        }
    }

    fn ui_workspace(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        self.poll_workspace(&ctx);
        let close_tab = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);
        if ctx.input_mut(|i| i.consume_shortcut(&close_tab)) {
            if let Some(path) = self.workspace.dock.focused_leaf() {
                if let Some(active) = self.workspace.dock[path.surface][path.node]
                    .get_leaf().map(|leaf| leaf.active) {
                    self.workspace.dock.remove_tab(egui_dock::TabPath::new(path.surface, path.node, active));
                    if self.workspace.dock.iter_all_tabs().next().is_none() {
                        self.show_empty_workspace = true;
                    }
                }
            }
        }
        let open = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::O);
        let toggle_sidebar = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::B);
        let search_sidebar = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::K);
        if ctx.input_mut(|i| i.consume_shortcut(&toggle_sidebar)) { self.sidebar_open = !self.sidebar_open; }
        if ctx.input_mut(|i| i.consume_shortcut(&search_sidebar)) { self.sidebar_search_open = true; }
        let save = egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
            egui::Key::S,
        );
        let save_decoder = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::S);
        let jump = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::G);
        if ctx.input_mut(|i| i.consume_shortcut(&jump))
            && self.commands_available()
            && self.binary.has_file()
        {
            self.show_view(Pane::Hex);
            self.binary.jump_to_byte();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&open)) {
            self.choose_data_file(&ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&save))
            && self.commands_available()
            && self.binary.has_file()
        {
            self.binary.save_copy(&ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&save_decoder))
            && self.commands_available()
            && self.decoder == Decoder::Layout
        {
            self.binary.save_definition(&ctx);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F5))
            && self.commands_available()
        {
            self.run_active_decoder(&ctx);
        }
        if !self.decoder_library_open {
            for file in ctx.input(|i| i.raw.dropped_files.clone()) {
                self.open_path(file.path().to_path_buf(), &ctx);
            }
        }
        self.ui_sidebar(root_ui);
        self.ui_sidebar_search(&ctx);
        self.ui_sidebar_settings(&ctx);
        egui::Panel::top("workspace_header")
            .exact_size(crate::theme::DARK.size.header_height)
            .frame(egui::Frame::new().fill(crate::theme::DARK.color.surface.window))
            .show(root_ui, |ui| crate::window_frame::header(ui, |ui| self.ui_toolbar(ui)));
        if self.show_decoding_guide {
            egui::Window::new("Decoding guide").open(&mut self.show_decoding_guide).default_width(540.).resizable(true).show(&ctx, |ui| {
                crate::source_editor::decoding_guide(ui);
            });
        }
        let mut workspace = std::mem::take(&mut self.workspace);
        egui::CentralPanel::default().show(root_ui, |ui| {
            if self.decoder_library_open {
                self.ui_decoder_library_entry(ui);
                return;
            }
            if !self.binary.has_file() && !self.show_empty_workspace {
                self.ui_welcome(ui);
                return;
            }
            DockArea::new(&mut workspace.dock)
                .id(egui::Id::new("workspace_dock"))
                .style(crate::theme::dock_style(ui.style().as_ref()))
                .show_close_buttons(false)
                .show_leaf_close_all_buttons(false)
                .show_leaf_collapse_buttons(false)
                .tab_context_menus(true)
                .show_inside(ui, &mut WorkspaceViewer { app: self });
            if let Some((surface, node, _)) = workspace.dock.find_tab_legacy(&Pane::Layout) {
                if let Some(rect) = workspace.dock[surface][node].rect() {
                    if rect.width() >= crate::theme::DARK.size.decoder_header_actions_min_width {
                        let t = &crate::theme::DARK;
                        let position = egui::pos2(
                            rect.right() - t.size.decoder_header_actions_width,
                            rect.top() + (t.size.card_header_height - t.size.control_height) / 2.0,
                        );
                        egui::Area::new(egui::Id::new("decoder_header_actions"))
                            .order(egui::Order::Foreground)
                            .fixed_pos(position)
                            .show(ui.ctx(), |ui| {
                                ui.horizontal(|ui| {
                                    if ui.add_enabled(
                                        self.binary.commands_available() && self.binary.can_save_definition(),
                                        egui::Button::new("Save"),
                                    ).clicked() {
                                        self.binary.save_definition(ui.ctx());
                                    }
                                    if crate::decoder_ui::run_button(
                                        ui,
                                        self.binary.commands_available() && self.binary.can_apply_definition(),
                                        None,
                                    ).clicked() {
                                        self.binary.apply_definition(ui.ctx());
                                    }
                                    if self.binary.is_decoding() && ui.button("Stop").clicked() {
                                        self.binary.stop_decoding();
                                    }
                                });
                            });
                    }
                }
            }
        });
        self.workspace = workspace;
        if let Some(message) = self.cache_message.clone() {
            let mut open = true;
            let mut dismiss = false;
            egui::Window::new("Notice")
                .open(&mut open)
                .anchor(egui::Align2::RIGHT_BOTTOM,
                    egui::vec2(-crate::theme::DARK.spacing.card_gap,
                        -crate::theme::DARK.spacing.card_gap))
                .show(&ctx, |ui| {
                    ui.label(message);
                    if ui.button("Dismiss").clicked() { dismiss = true; }
                });
            if !open || dismiss { self.cache_message = None; }
        }
        self.poll_csv_links(&ctx);
        if ctx
            .data_mut(|data| data.remove_temp::<bool>(egui::Id::new("welcome_definitions")))
            .unwrap_or(false)
        {
            self.workspace.reveal(Pane::Layout);
        }
        self.reveal_requested_panels();
    }

    fn ui_welcome(&mut self, ui: &mut Ui) {
        let recent = self.file_locations.recent_pairs();
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
            if !recent.is_empty() {
                ui.add_space(18.);
                ui.label("Open recent");
                for (index, pair) in recent.iter().enumerate() {
                    let label = Self::recent_label(pair);
                    if ui.add_enabled(self.commands_available(), egui::Button::new(label))
                        .on_hover_text(format!("Data: {}\nDecoder: {}", pair.data.display(), pair.decoder.as_ref().map_or_else(|| "none".into(), |p| p.display().to_string())))
                        .clicked() {
                        self.open_recent(index, ui.ctx());
                    }
                }
            }
            ui.add_space(18.);
            if ui.link("Create a definition").clicked() {
                if self.decoder == Decoder::Layout {
                    self.binary.show_yaml();
                }
                self.show_empty_workspace = true;
                // The docking state is temporarily moved out while drawing.
                // Defer this command until it has been restored below.
                ui.ctx()
                    .data_mut(|data| data.insert_temp(egui::Id::new("welcome_definitions"), true));
            }
        });
    }

    fn ui_view_controls(&mut self, ui: &mut Ui) {
        let complete = self.csv_complete();
        let Some(data) = self.data.as_ref() else {
            return;
        };
        let view = self.view.get_or_insert_with(|| ViewState::from_store(data));
        let previous = (view.center, view.span);
        let mut next_rows = None;
        let mut overview = false;
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(if complete {
                    "Overview"
                } else {
                    "Available range"
                })
                .clicked()
            {
                view.set_full(data);
                overview = true;
            }
            if ui.button("Start").clicked() {
                view.jump_to_start(data);
            }
            if ui.button("Previous").clicked() {
                view.nudge_fraction(-0.8, data);
            }
            if ui
                .add_enabled(self.loader_rx.is_none(), egui::Button::new("Next"))
                .clicked()
            {
                if !complete && view.bounds(data).1 >= data.time_max - ViewState::MIN_SPAN {
                    next_rows = Some(data.rows.saturating_add(CSV_INITIAL_ROWS));
                } else {
                    view.nudge_fraction(0.8, data);
                }
            }
            if ui.button("Zoom in").clicked() {
                view.set_center_span(view.center, view.span / 2., data);
            }
            if ui.button("Zoom out").clicked() {
                view.set_center_span(view.center, view.span * 2., data);
            }
        });
        if overview {
            self.follow_loading = true;
        } else if previous != (view.center, view.span) {
            self.follow_loading = false;
        }
        let (min, max) = view.bounds(data);
        let rows = data.rows;
        let visible_series = self.visible.iter().filter(|shown| **shown).count();
        let total_series = data.signals.len();
        ui.horizontal_wrapped(|ui| {
            ui.menu_button(
                format!("Series {visible_series}/{total_series}"),
                |ui| self.ui_csv_columns(ui),
            );
            ui.weak(format!(
                "X {min:.3}–{max:.3} · {rows} rows{}",
                if complete { "" } else { " available" }
            ));
            if !complete && self.loader_rx.is_none() && ui.button("Index entire file").clicked() {
                self.index_entire_csv(ui.ctx());
            }
            if self.loader_rx.is_some() && ui.button("Stop").clicked() {
                self.stop_csv_read();
            }
        });
        if let Some(rows) = next_rows {
            self.csv_advance_plot = true;
            self.follow_loading = false;
            self.request_csv_rows(rows, ui.ctx());
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
                    Err(err) => {
                        log::error!("Plot: {err:#}");
                        self.plot_error = Some(format!("Plot: {err:#}"));
                    }
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
        self.ui_view_controls(ui);
        if !self.load_has_data && self.data.is_some() {
            ui.weak(if self.loader_rx.is_some() {
                "Applying CSV settings… previous results shown"
            } else {
                "Previous results · current settings not applied"
            });
        }
        let Some(ds) = &self.data else {
            ui.centered_and_justified(|ui| {
                ui.label(if self.loader_rx.is_some() {
                    "Reading requested rows…"
                } else {
                    "Load data to begin"
                })
            });
            return;
        };
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
        let selected_samples = self.csv_selected_samples();
        let response = Plot::new("plot")
            .id(egui::Id::new(("csv_plot", self.dataset)))
            .allow_zoom(true)
            .allow_scroll(true)
            .allow_boxed_zoom(false)
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
                        plot_ui.line(
                            Line::new(name, points.iter().map(|sample| sample.point).collect::<Vec<_>>())
                                .name(name),
                        );
                    }
                }
                if let Some(selection) = &self.csv_selection {
                    for row in std::iter::once(&selection.row).chain(selection.anchor.as_ref()) {
                        if let Some(&x) = row.values.first().filter(|x| x.is_finite()) {
                            plot_ui.vline(VLine::new("selection", x).color(egui::Color32::YELLOW));
                        }
                    }
                    let selected_points: Vec<_> = selected_samples
                        .iter()
                        .map(|(_, sample)| sample.point)
                        .collect();
                    plot_ui.points(
                        Points::new("selection", selected_points)
                            .color(egui::Color32::YELLOW)
                            .radius(5_f32),
                    );
                }
            });
        if response.response.clicked() {
            if let Some(pointer) = response
                .response
                .interact_pointer_pos()
                .filter(|point| response.transform.frame().contains(*point))
            {
                let selected = selected_samples
                    .iter()
                    .map(|(column, sample)| (*column, sample))
                    .chain(
                        self.plot_lines
                            .iter()
                            .filter(|(column, _, _)| self.visible.get(*column) == Some(&true))
                            .flat_map(|(column, _, samples)| {
                                samples.iter().map(move |sample| (*column, sample))
                            }),
                    )
                    .filter_map(|(column, sample)| {
                        let position = response
                            .transform
                            .position_from_point(&PlotPoint::new(sample.point[0], sample.point[1]));
                        let distance = position.distance_sq(pointer);
                        (distance <= 144.).then_some((
                            distance,
                            CsvCellSelection {
                                row: sample.record,
                                column: Some(column + 1),
                            },
                        ))
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .map(|(_, selected)| selected);
                if let Some(selected) = selected {
                    self.request_csv_link(CsvLinkRequest::Cell(selected));
                }
            }
        }
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

impl eframe::App for BinwaveApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.dialog_template = rfd::AsyncFileDialog::new().set_parent(frame);
        self.binary
            .set_dialog_template(self.dialog_template.clone());
        #[cfg(not(target_arch = "wasm32"))]
        crate::window_frame::show(ui, |ui| self.ui_workspace(ui));
        #[cfg(target_arch = "wasm32")]
        self.ui_workspace(ui);
    }
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.workspace.save(storage);
        self.file_locations.save(storage);
        storage.set_string("sidebar-open-v2", self.sidebar_open.to_string());
        storage.set_string(
            "editor-tab-width",
            self.binary.editor_tab_width().to_string(),
        );
    }
}

struct WorkspaceViewer<'a> {
    app: &'a mut BinwaveApp,
}
impl TabViewer for WorkspaceViewer<'_> {
    type Tab = Pane;
    fn title(&mut self, pane: &mut Pane) -> egui::WidgetText {
        if *pane == Pane::Layout {
            self.app.binary.decoder_tab_title().into()
        } else {
            pane.title().into()
        }
    }
    fn id(&mut self, pane: &mut Pane) -> egui::Id {
        egui::Id::new(*pane)
    }
    fn scroll_bars(&self, _pane: &Pane) -> [bool; 2] {
        [false, false]
    }
    fn ui(&mut self, ui: &mut Ui, pane: &mut Pane) {
        match pane {
            Pane::Structures => {
                if self.app.displayed_decoder == Decoder::Csv {
                    ui.heading("CSV columns");
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(format!("X · {}", self.app.csv_column_name(0)));
                        if let Some(data) = &self.app.data {
                            for signal in &data.signals {
                                ui.label(&signal.name);
                            }
                        }
                    });
                } else {
                    self.app.binary.structures(ui);
                }
            }
            Pane::Inspector => {
                egui::ScrollArea::vertical().show(ui, |ui| self.app.ui_inspector(ui));
            }
            Pane::Layout => self.app.ui_decoder(ui),
            Pane::Hex => {
                let t = &crate::theme::DARK;
                if ui.available_width() >= t.size.inspector_min_host_width {
                    egui::Panel::right("hex_embedded_inspector")
                        .resizable(true)
                        .default_size(t.size.inspector_width)
                        .frame(egui::Frame::new()
                            .fill(t.color.surface.raised)
                            .corner_radius(t.radius.inset))
                        .show(ui, |ui| {
                            ui.strong("Inspector");
                            ui.separator();
                            egui::ScrollArea::vertical().show(ui, |ui| self.app.ui_inspector(ui));
                        });
                }
                if let Some(range) = self.app.binary.byte_selection() {
                    ui.weak(format!("Cursor 0x{:X} · {} B selected", range.start, range.len()));
                }
                if self.app.displayed_decoder == Decoder::Csv {
                    let highlights = self.app.csv_highlights();
                    self.app.binary.hex_with_source_highlights(ui, &highlights);
                } else {
                    self.app.binary.hex(ui);
                }
            }
            Pane::Plot => {
                let t = &crate::theme::DARK;
                egui::Frame::new()
                    .fill(t.color.surface.window)
                    .corner_radius(t.radius.well)
                    .inner_margin(t.spacing.card_padding as i8)
                    .show(ui, |ui| {
                        if self.app.displayed_decoder == Decoder::Csv {
                            self.app.ui_plot(ui);
                        } else {
                            self.app.binary.plot(ui);
                        }
                    });
            }
            Pane::Table => {
                if self.app.displayed_decoder == Decoder::Csv {
                    if !self.app.load_has_data && self.app.data.is_some() {
                        ui.weak(if self.app.loader_rx.is_some() {
                            "Applying CSV settings… previous results shown"
                        } else {
                            "Previous results · current settings not applied"
                        });
                    }
                    let complete = self.app.csv_complete();
                    let response = ui
                        .add_enabled_ui(self.app.load_has_data, |ui| {
                            self.app.csv_table.ui(
                                ui,
                                self.app.data.as_ref(),
                                self.app.dataset,
                                &self.app.visible,
                                complete,
                                self.app.loader_rx.is_some(),
                            )
                        })
                        .inner;
                    if response.navigation_changed
                        && response.requested_rows.is_none()
                        && self.app.csv_target.is_some()
                    {
                        self.app.stop_csv_read();
                    }
                    if let Some(rows) = response.requested_rows {
                        self.app.request_csv_rows(rows, ui.ctx());
                    }
                    if let Some(selection) = response.selected_rows {
                        self.app.request_csv_link(CsvLinkRequest::Rows(selection));
                    } else if let Some(selection) = response.selected_cell {
                        self.app.request_csv_link(CsvLinkRequest::Cell(selection));
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

    fn frame(app: &mut BinwaveApp, ctx: &Context, events: Vec<egui::Event>) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400., 950.),
                )),
                events,
                ..Default::default()
            },
            |ctx, ui| {
                app.poll_loader(ctx);
                egui::Panel::left("controls")
                    .exact_size(350.)
                    .show(ui, |ui| app.ui_decoder(ui));
                egui::CentralPanel::default().show(ui, |ui| app.ui_plot(ui));
            },
        )
    }

    fn click(app: &mut BinwaveApp, ctx: &Context, label: &str) {
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

    fn workspace_frame(app: &mut BinwaveApp, ctx: &Context) -> egui::FullOutput {
        crate::theme::install(ctx);
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280., 850.),
                )),
                ..Default::default()
            },
            |_ctx, ui| app.ui_workspace(ui),
        )
    }

    fn workspace_events(
        app: &mut BinwaveApp,
        ctx: &Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        crate::theme::install(ctx);
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280., 850.),
                )),
                events,
                ..Default::default()
            },
            |_ctx, ui| app.ui_workspace(ui),
        )
    }
    #[test]
    fn sidebar_shortcut_toggles_without_changing_the_dock() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        let tabs = app.workspace.dock.iter_all_tabs().map(|(_, tab)| *tab).collect::<Vec<_>>();
        for expected in [false, true] {
            workspace_events(&mut app, &ctx, vec![egui::Event::Key {
                key: egui::Key::B,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }]);
            assert_eq!(app.sidebar_open, expected);
            assert_eq!(app.workspace.dock.iter_all_tabs().map(|(_, tab)| *tab).collect::<Vec<_>>(), tabs);
        }
    }
    fn workspace_click(app: &mut BinwaveApp, ctx: &Context, label: &str) {
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
    fn decoder_library_lists_imported_files_without_a_data_file() {
        let dir = tempfile::tempdir().unwrap();
        let decoder = dir.path().join("example.yaml");
        std::fs::write(&decoder, "structures:\n  - name: Header\n").unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.file_locations.remember_file(DialogFileKind::Yaml, &decoder);
        app.decoder_library_open = true;
        let shown = text(workspace_frame(&mut app, &ctx));
        assert!(shown.contains("example.yaml"));
        assert!(shown.contains("Header"));
        assert!(shown.contains("Open in workspace"));
        assert!(app.binary.source_path().is_none());
    }

    #[test]
    fn sidebar_header_button_reopens_and_sidebar_button_collapses() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.sidebar_open = false;
        app.show_empty_workspace = true;
        workspace_click(&mut app, &ctx, egui_phosphor::regular::SIDEBAR_SIMPLE);
        assert!(app.sidebar_open, "header sidebar control must reopen navigation");
        let timed_frame = |app: &mut BinwaveApp, time: f64, events: Vec<egui::Event>| {
            crate::theme::install(&ctx);
            ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280., 850.))),
                time: Some(time),
                predicted_dt: 1. / 60.,
                events,
                ..Default::default()
            }, |_ctx, ui| app.ui_workspace(ui))
        };
        let mut opened = timed_frame(&mut app, 1., vec![]);
        for frame in 1..=30 {
            opened = timed_frame(&mut app, 1. + frame as f64 / 60., vec![]);
        }
        let pos = opened.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == egui_phosphor::regular::SIDEBAR_SIMPLE =>
                Some(text.pos + text.galley.rect.center().to_vec2()),
            _ => None,
        }).expect("sidebar collapse control must render after opening");
        timed_frame(&mut app, 1. + 31. / 60., vec![egui::Event::PointerMoved(pos)]);
        timed_frame(&mut app, 1. + 32. / 60., vec![egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE,
        }]);
        timed_frame(&mut app, 1. + 33. / 60., vec![egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE,
        }]);
        assert!(!app.sidebar_open, "sidebar control must collapse navigation");
    }

    #[test]
    fn sidebar_decoder_library_opens_from_workspace() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.show_empty_workspace = true;
        workspace_click(&mut app, &ctx, "Decoder library");
        assert!(app.decoder_library_open);
        assert!(text(workspace_frame(&mut app, &ctx)).contains("New decoder"));
    }
    fn csv_fixture(directory: &std::path::Path, rows: usize) -> std::path::PathBuf {
        let path = directory.join("values.csv");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        writeln!(file, "time,value").unwrap();
        for row in 0..rows {
            writeln!(file, "{row},{}", row * 3).unwrap();
        }
        file.flush().unwrap();
        path
    }

    fn wait_csv(app: &mut BinwaveApp, ctx: &Context) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            workspace_frame(app, ctx);
            if app.binary.has_file() && !app.binary.busy() && app.loader_rx.is_none() {
                break;
            }
            assert!(Instant::now() < deadline, "{}", app.status);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn wait_byte_preview(app: &mut BinwaveApp, ctx: &Context) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            workspace_frame(app, ctx);
            if app.binary.has_file()
                && !app.binary.busy()
                && !app.binary.is_decoding()
                && app.binary.showing_byte_preview()
            {
                break;
            }
            assert!(Instant::now() < deadline, "{}", app.binary.status().0);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn launch_paths_open_data_and_its_decoder_without_running_it() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("capture.csv");
        let decoder = directory.path().join("definition.yaml");
        std::fs::write(&data, [1u8, 0, 2, 0]).unwrap();
        std::fs::write(&decoder, "endian: little\nstructures:\n  - name: Entry\n    fields:\n      - {name: reading, type: u16}\n").unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.open_launch_paths(Some(data.clone()), Some(decoder.clone()), &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.binary.decoder_path() != Some(decoder.as_path()) {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline, "{}", app.binary.status().0);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(app.binary.source_path(), Some(data.as_path()));
        assert!(app.decoder == Decoder::Layout);
        assert!(!app.binary.is_decoding());
        assert!(app.binary.can_apply_definition());
    }

    #[test]
    fn decoder_only_launch_opens_editor_without_data() {
        let directory = tempfile::tempdir().unwrap();
        let decoder = directory.path().join("definition.yaml");
        std::fs::write(&decoder, "structures:\n  - name: Entry\n").unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.open_launch_paths(None, Some(decoder.clone()), &ctx);
        assert_eq!(app.binary.decoder_path(), Some(decoder.as_path()));
        assert!(app.show_empty_workspace);
        assert!(app.binary.has_definition());
    }

    #[test]
    fn launch_file_overrides_csv_config_data_path() {
        let directory = tempfile::tempdir().unwrap();
        let data = csv_fixture(directory.path(), 8);
        let config = directory.path().join("settings.md");
        std::fs::write(&config,
            "```json\n{\"data_path\":\"missing.csv\",\"csv\":{\"time_column\":\"time\"}}\n```\n")
            .unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.open_launch_paths(Some(data.clone()), Some(config), &ctx);
        assert_eq!(app.pending_config.as_ref().and_then(|cfg| cfg.data_path.as_ref()), Some(&data));
        wait_csv(&mut app, &ctx);
        assert_eq!(app.cfg.data_path.as_ref(), Some(&data));
        assert!(app.decoder == Decoder::Csv);
    }

    fn text(output: egui::FullOutput) -> String {
        output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn csv_opens_a_bounded_prefix_and_only_explicit_requests_extend_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 30_000);
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path.clone(), &ctx);
        wait_csv(&mut app, &ctx);
        assert!(app.decoder == Decoder::Csv);
        assert_eq!(app.data.as_ref().unwrap().rows, 8192);
        assert!(!app.csv_complete());
        assert!(app.status.contains("available"));
        assert!(app.commands_available());
        assert!(!app.binary.showing_byte_preview());
        assert!(
            !app.binary.has_layout(),
            "CSV must keep its native bounded decoder"
        );
        for pane in [Pane::Plot, Pane::Hex, Pane::Table, Pane::Layout] {
            let (surface, node, tab) = app.workspace.dock.find_tab_legacy(&pane).unwrap();
            assert!(matches!(&app.workspace.dock[surface][node],
                egui_dock::Node::Leaf(leaf) if leaf.active == tab));
        }
        assert_ne!(
            app.workspace.dock.find_tab_legacy(&Pane::Plot).unwrap().1,
            app.workspace.dock.find_tab_legacy(&Pane::Hex).unwrap().1,
            "the initial plot must not hide the source bytes"
        );
        assert_eq!(
            app.binary.file_len(),
            std::fs::metadata(&path).unwrap().len() as usize
        );
        let dataset = app.dataset;
        for _ in 0..12 {
            workspace_frame(&mut app, &ctx);
        }
        assert!(
            app.loader_rx.is_none(),
            "idle rendering must not resume indexing"
        );
        assert_eq!(app.data.as_ref().unwrap().rows, 8192);
        // Unapplied text settings must never change a continuation's schema.
        app.cfg.csv.delimiter = ';';
        app.follow_loading = false;
        let old_view = app
            .view
            .as_ref()
            .unwrap()
            .bounds(app.data.as_ref().unwrap());
        let plot = app.workspace.dock.find_tab_legacy(&Pane::Plot).unwrap();
        app.workspace.dock.remove_tab(egui_dock::TabPath::new(plot.0, plot.1, plot.2));
        app.request_csv_rows(16_384, &ctx);
        wait_csv(&mut app, &ctx);
        assert!(
            app.workspace.dock.find_tab_legacy(&Pane::Plot).is_none(),
            "continuing CSV reads must not reopen a plot the user closed"
        );
        assert_eq!(app.dataset, dataset);
        assert_eq!(app.data.as_ref().unwrap().rows, 16_384);
        assert_eq!(
            app.data.as_ref().unwrap().signals[0].points.point(16_383),
            [16_383., 49_149.]
        );
        assert_eq!(
            app.view
                .as_ref()
                .unwrap()
                .bounds(app.data.as_ref().unwrap()),
            old_view
        );
        // Opening the same source reuses its saved partial index and stays idle.
        let mut reopened = BinwaveApp::default();
        reopened.csv_cache_root = dir.path().join("cache");
        reopened.open_path(path, &ctx);
        wait_csv(&mut reopened, &ctx);
        assert_eq!(reopened.data.as_ref().unwrap().rows, 16_384);
        assert!(!reopened.csv_complete());
        assert!(reopened.loader_rx.is_none());
        reopened.index_entire_csv(&ctx);
        wait_csv(&mut reopened, &ctx);
        assert_eq!(reopened.data.as_ref().unwrap().rows, 30_000);
        assert!(reopened.csv_complete());
    }

    fn wait_csv_selection(app: &mut BinwaveApp, ctx: &Context) {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            workspace_frame(app, ctx);
            if app.csv_link_request.is_none()
                && app.csv_link_rx.is_none()
                && app.loader_rx.is_none()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "selection did not finish: {}",
                app.status
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn csv_cells_hex_and_editor_switches_keep_one_selection_and_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 300);
        let raw = std::fs::read(&path).unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        app.show_view(Pane::Plot);
        let data = app.data.as_ref().unwrap();
        app.view.as_mut().unwrap().set_center_span(20., 20., data);
        app.request_csv_link(CsvLinkRequest::Cell(CsvCellSelection {
            row: 200,
            column: Some(1),
        }));
        wait_csv_selection(&mut app, &ctx);
        let selection = app.csv_selection.as_ref().unwrap();
        assert_eq!(selection.row.index, 200);
        assert_eq!(selection.column, Some(1));
        let selected = app.binary.byte_selection().unwrap();
        assert_eq!(&raw[selected.clone()], b"600");
        assert_eq!(app.csv_highlights().len(), 2);
        let dataset = app.dataset;
        let bounds = app
            .view
            .as_ref()
            .unwrap()
            .bounds(app.data.as_ref().unwrap());
        assert!(bounds.0 <= 200. && bounds.1 >= 200.);
        assert_eq!(bounds.1 - bounds.0, 20.);
        for mode in ["Python", "YAML", "CSV", "Python"] {
            workspace_click(&mut app, &ctx, mode);
            assert!(app.displayed_decoder == Decoder::Csv);
            assert_eq!(app.dataset, dataset);
            assert_eq!(app.binary.byte_selection(), Some(selected.clone()));
            assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 200);
            assert_eq!(
                app.view
                    .as_ref()
                    .unwrap()
                    .bounds(app.data.as_ref().unwrap()),
                bounds
            );
            assert!(app.loader_rx.is_none());
            assert!(!text(workspace_frame(&mut app, &ctx)).contains("Run a decoder to plot"));
        }
        app.binary
            .load_python_text("raise RuntimeError('intentional test error')", &ctx);
        app.run_active_decoder(&ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while app.binary.is_decoding() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        workspace_frame(&mut app, &ctx);
        assert!(app.displayed_decoder == Decoder::Csv);
        assert_eq!(app.dataset, dataset);
        assert_eq!(app.binary.byte_selection(), Some(selected));
        let row = app
            .data
            .as_ref()
            .unwrap()
            .csv_rows
            .as_ref()
            .unwrap()
            .row(27, &AtomicBool::new(false))
            .unwrap();
        let byte = row.fields[1].start;
        app.binary.select_source_range(byte..byte + 1, false);
        wait_csv_selection(&mut app, &ctx);
        assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 27);
        assert_eq!(app.csv_selection.as_ref().unwrap().column, Some(1));
        assert_eq!(
            app.binary.byte_selection(),
            Some(byte..byte + 1),
            "reverse linking preserves the user's exact bytes"
        );
        app.request_csv_link(CsvLinkRequest::Cell(CsvCellSelection {
            row: 27,
            column: None,
        }));
        wait_csv_selection(&mut app, &ctx);
        assert_eq!(&raw[app.binary.byte_selection().unwrap()], b"27,81\n");
        app.binary.select_source_range(0..1, false);
        wait_csv_selection(&mut app, &ctx);
        assert!(
            app.csv_selection.is_none(),
            "header is not a numeric sample"
        );
    }

    #[test]
    fn csv_row_ranges_link_exact_bytes_and_plot_records_with_two_endpoint_reads() {
        #[derive(Debug)]
        struct TrackedRows {
            source: Arc<dyn crate::data::CsvRows>,
            reads: Mutex<Vec<usize>>,
        }
        impl crate::data::CsvRows for TrackedRows {
            fn row(&self, index: usize, cancel: &AtomicBool) -> Result<CsvRow> {
                self.reads.lock().unwrap().push(index);
                self.source.row(index, cancel)
            }
            fn find_byte(&self, offset: usize, cancel: &AtomicBool) -> Result<Option<CsvRow>> {
                self.source.find_byte(offset, cancel)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ranges.csv");
        let mut raw = b"time,value,note\r\n".to_vec();
        let mut expected_bytes = Vec::new();
        for row in 0..300 {
            let start = raw.len();
            raw.extend_from_slice(
                format!("{},\"{}\",\"line\r\n\"\"quoted\"\"\"\r\n", row % 5, row * 3).as_bytes(),
            );
            expected_bytes.push(start..raw.len());
        }
        std::fs::write(&path, &raw).unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        let tracked = Arc::new(TrackedRows {
            source: app.data.as_ref().unwrap().csv_rows.clone().unwrap(),
            reads: Mutex::new(Vec::new()),
        });
        app.data.as_mut().unwrap().csv_rows = Some(tracked.clone());
        app.show_view(Pane::Plot);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.plot_lines.is_empty() || app.plot_rx.is_some() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for active in [205, 3, 210] {
            tracked.reads.lock().unwrap().clear();
            let rows = CsvRowSelection {
                anchor: 200,
                active,
            };
            app.request_csv_link(CsvLinkRequest::Rows(rows));
            wait_csv_selection(&mut app, &ctx);
            assert_eq!(*tracked.reads.lock().unwrap(), [active, 200]);
            let range = rows.rows();
            let expected = expected_bytes[range.start].start..expected_bytes[range.end - 1].end;
            assert_eq!(app.binary.byte_selection(), Some(expected.clone()));
            assert_eq!(
                &raw[expected.clone()],
                &raw[app.csv_selection.as_ref().unwrap().bytes.clone()]
            );
            let selection = app.csv_selection.as_ref().unwrap();
            assert_eq!(selection.rows, Some(rows));
            assert_eq!(selection.row.index, active);
            assert_eq!(selection.anchor.as_ref().unwrap().index, 200);
            assert_eq!(selection.column, None);
            let samples = app.csv_selected_samples();
            assert!(!samples.is_empty());
            assert!(samples
                .iter()
                .all(|(_, sample)| range.contains(&sample.record)));
            for record in range.clone() {
                assert!(samples.iter().any(|(_, sample)| sample.record == record));
            }
            assert!(
                !samples.iter().any(|(_, sample)| sample.record == 0),
                "duplicate X values outside the selected rows must not be highlighted"
            );
        }
        app.request_csv_link(CsvLinkRequest::Cell(CsvCellSelection {
            row: 17,
            column: Some(1),
        }));
        wait_csv_selection(&mut app, &ctx);
        assert_eq!(&raw[app.binary.byte_selection().unwrap()], b"\"51\"");
        assert!(app.csv_selection.as_ref().unwrap().rows.is_none());
        assert_eq!(app.csv_selected_samples().len(), 1);
        // A new Hex selection leaves the whole-row range and keeps its exact bytes.
        let row = app
            .data
            .as_ref()
            .unwrap()
            .csv_rows
            .as_ref()
            .unwrap()
            .row(9, &AtomicBool::new(false))
            .unwrap();
        let byte = row.fields[1].start + 1;
        app.binary.select_source_range(byte..byte + 1, false);
        wait_csv_selection(&mut app, &ctx);
        assert!(app.csv_selection.as_ref().unwrap().rows.is_none());
        assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 9);
        assert_eq!(app.binary.byte_selection(), Some(byte..byte + 1));
    }

    #[test]
    fn csv_plot_click_reveals_exact_record_when_x_values_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repeated.csv");
        let raw = b"time,value\n1,10\n1,20\n0,30\n2,40\n";
        std::fs::write(&path, raw).unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        app.show_view(Pane::Plot);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            workspace_frame(&mut app, &ctx);
            if !app.plot_lines.is_empty() && app.plot_rx.is_none() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        workspace_frame(&mut app, &ctx);
        let transform = egui_plot::PlotMemory::load(&ctx, egui::Id::new(("csv_plot", app.dataset)))
            .unwrap()
            .transform();
        let pos = transform.position_from_point(&PlotPoint::new(1., 20.));
        workspace_events(&mut app, &ctx, vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            workspace_events(
                &mut app,
                &ctx,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
        wait_csv_selection(&mut app, &ctx);
        assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 1);
        assert_eq!(app.csv_selection.as_ref().unwrap().column, Some(1));
        assert_eq!(&raw[app.binary.byte_selection().unwrap()], b"20");
        app.plot_lines[0].2.retain(|sample| sample.record != 1);
        workspace_frame(&mut app, &ctx);
        for pressed in [true, false] {
            workspace_events(
                &mut app,
                &ctx,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
        wait_csv_selection(&mut app, &ctx);
        assert_eq!(
            app.csv_selection.as_ref().unwrap().row.index,
            1,
            "explicit marker retains its identity after decimation"
        );
        let (surface, node, plot_tab) = app.workspace.dock.find_tab_legacy(&Pane::Plot).unwrap();
        match &app.workspace.dock[surface][node] {
            egui_dock::Node::Leaf(leaf) => {
                assert_eq!(leaf.active, plot_tab, "linking bytes must not hide the plot")
            }
            _ => panic!("plot is not docked in a leaf"),
        }
    }

    #[test]
    fn csv_hex_selection_extends_only_until_its_record_and_can_be_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 30_000);
        let raw = std::fs::read(&path).unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        let target = raw
            .windows(b"19000,57000".len())
            .position(|bytes| bytes == b"19000,57000")
            .unwrap();
        app.binary
            .select_source_range(target + 6..target + 7, false);
        wait_csv_selection(&mut app, &ctx);
        assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 19_000);
        assert_eq!(app.csv_selection.as_ref().unwrap().column, Some(1));
        assert_eq!(app.data.as_ref().unwrap().rows, 24_576);
        assert!(!app.csv_complete());
        // Deterministically leave an old selection-owned loader in flight.
        let (old_tx, old_rx) = mpsc::channel();
        app.loader_rx = Some(old_rx);
        app.csv_link_indexing = true;
        app.csv_link_request = Some(CsvLinkRequest::Bytes(raw.len() - 2..raw.len() - 1));
        app.request_csv_link(CsvLinkRequest::Cell(CsvCellSelection {
            row: 4,
            column: Some(0),
        }));
        let _ = old_tx.send(Err(anyhow::anyhow!("cancelled obsolete request")));
        wait_csv_selection(&mut app, &ctx);
        assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 4);
        assert_eq!(&raw[app.binary.byte_selection().unwrap()], b"4");
        // Stop cancels an explicit distant selection without restart on future frames.
        app.request_csv_link(CsvLinkRequest::Bytes(raw.len() - 2..raw.len() - 1));
        app.stop_csv_read();
        wait_csv_selection(&mut app, &ctx);
        for _ in 0..5 {
            workspace_frame(&mut app, &ctx);
        }
        assert!(app.csv_link_request.is_none());
        assert!(app.loader_rx.is_none());
        assert_eq!(app.data.as_ref().unwrap().rows, 24_576);
        // Explicit indexing takes ownership from any earlier selection request.
        app.csv_link_indexing = true;
        app.index_entire_csv(&ctx);
        app.request_csv_link(CsvLinkRequest::Cell(CsvCellSelection {
            row: 2,
            column: None,
        }));
        assert!(!app.load_progress.cancelled.load(Ordering::Relaxed));
        wait_csv_selection(&mut app, &ctx);
        assert!(app.csv_complete());
        assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 2);
    }

    #[test]
    fn csv_byte_lookup_failure_keeps_prefix_and_does_not_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 8192);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"invalid,row\n10000,30000\n").unwrap();
        drop(file);
        let size = std::fs::metadata(&path).unwrap().len() as usize;
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        app.binary.select_source_range(size - 3..size - 2, false);
        wait_csv_selection(&mut app, &ctx);
        assert!(app.status.contains("CSV:"), "{}", app.status);
        assert_eq!(app.data.as_ref().unwrap().rows, 8192);
        assert!(app.csv_selection.is_none());
        for _ in 0..5 {
            workspace_frame(&mut app, &ctx);
        }
        assert!(app.loader_rx.is_none());
        assert!(app.csv_link_request.is_none());
    }

    #[test]
    fn csv_crlf_boundary_and_empty_cell_do_not_select_unrelated_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.csv");
        let mut raw = b"time,value\r\n0,\r\n".to_vec();
        for row in 1..8192 {
            raw.extend_from_slice(format!("{row},{}\r\n", row * 3).as_bytes());
        }
        let last_lf = raw.len() - 1;
        raw.extend_from_slice(b"invalid,tail\r\n");
        std::fs::write(&path, &raw).unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        app.binary.select_source_range(last_lf..last_lf + 1, false);
        wait_csv_selection(&mut app, &ctx);
        assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 8191);
        assert_eq!(app.csv_selection.as_ref().unwrap().column, None);
        assert!(
            !app.status.contains("CSV:"),
            "must not scan invalid tail: {}",
            app.status
        );
        assert_eq!(app.data.as_ref().unwrap().rows, 8192);
        app.request_csv_link(CsvLinkRequest::Cell(CsvCellSelection {
            row: 0,
            column: Some(1),
        }));
        wait_csv_selection(&mut app, &ctx);
        assert!(app.csv_selection.as_ref().unwrap().bytes.is_empty());
        assert!(
            app.binary.byte_selection().is_none(),
            "empty cell has no bytes to edit/export"
        );
        assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 0);
    }

    #[test]
    fn successful_binary_decode_replaces_csv_but_editor_changes_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 12);
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        workspace_click(&mut app, &ctx, "Python");
        app.binary.load_python_text("from binwave import Struct\nByte = Struct('Byte', [('value', 'u8')])\nfile.records('bytes', Byte, count=file.size)", &ctx);
        assert!(app.displayed_decoder == Decoder::Csv);
        app.run_active_decoder(&ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while app.displayed_decoder != Decoder::Layout {
            workspace_frame(&mut app, &ctx);
            assert!(
                Instant::now() < deadline,
                "binary decode did not replace CSV"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(app.binary.decoded_ready());
        for mode in ["CSV", "YAML", "Python"] {
            workspace_click(&mut app, &ctx, mode);
            assert!(app.displayed_decoder == Decoder::Layout);
            assert!(app.binary.decoded_ready());
            assert!(app.loader_rx.is_none());
        }
    }

    #[test]
    fn plain_python_keeps_csv_result_selection_and_plot_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 300);
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        app.show_view(Pane::Plot);
        app.request_csv_link(CsvLinkRequest::Cell(CsvCellSelection {
            row: 42,
            column: Some(1),
        }));
        wait_csv_selection(&mut app, &ctx);
        let selection = app.binary.byte_selection();
        let dataset = app.dataset;
        let bounds = app
            .view
            .as_ref()
            .unwrap()
            .bounds(app.data.as_ref().unwrap());
        workspace_click(&mut app, &ctx, "Python");
        for code in ["test = 5", "test = 5\nprint(test)", "print(file.size)"] {
            app.binary.load_python_text(code, &ctx);
            if text(workspace_frame(&mut app, &ctx)).contains("Replace decoder") {
                workspace_click(&mut app, &ctx, "Replace decoder");
            }
            workspace_click(&mut app, &ctx, crate::decoder_ui::RUN_LABEL);
            wait_csv(&mut app, &ctx);
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while app.binary.is_decoding() {
                workspace_frame(&mut app, &ctx);
                assert!(Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let shown = text(workspace_frame(&mut app, &ctx));
            assert!(shown.contains("Run finished"), "{shown}");
            assert!(!shown.contains("ValueError"));
            assert!(app.displayed_decoder == Decoder::Csv);
            assert_eq!(app.dataset, dataset);
            assert_eq!(app.binary.byte_selection(), selection);
            assert_eq!(app.csv_selection.as_ref().unwrap().row.index, 42);
            assert_eq!(
                app.view
                    .as_ref()
                    .unwrap()
                    .bounds(app.data.as_ref().unwrap()),
                bounds
            );
            let (surface, node, plot_tab) = app.workspace.dock.find_tab_legacy(&Pane::Plot).unwrap();
            match &app.workspace.dock[surface][node] {
                egui_dock::Node::Leaf(leaf) => assert_eq!(leaf.active, plot_tab),
                _ => panic!("plot is not docked in a leaf"),
            }
        }
    }

    #[test]
    fn csv_decoder_controls_and_f5_follow_the_visible_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 12_000);
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        let visible = text(workspace_frame(&mut app, &ctx));
        for label in [
            "Python",
            "YAML",
            "CSV",
            "Separator",
            "X column",
            "Open configuration…",
            "Apply",
        ] {
            assert!(
                visible.contains(label),
                "missing CSV decoder control {label}"
            );
        }
        assert!(!visible.contains("Decoded RAM cache"));
        assert!(!visible.contains("Data interpretation"));
        let inspected = ctx.run(Default::default(), |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| app.ui_inspector(ui));
        });
        assert!(
            !text(inspected).contains("X column"),
            "Inspector must not contain decoder controls"
        );
        workspace_click(&mut app, &ctx, "Python");
        app.binary
            .load_python_text("raise RuntimeError('hidden decoder must not run')", &ctx);
        workspace_frame(&mut app, &ctx);
        workspace_click(&mut app, &ctx, "CSV");
        app.cfg.csv.time_column = "value".into();
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
        wait_csv(&mut app, &ctx);
        assert!(app.decoder == Decoder::Csv);
        assert_eq!(app.csv_applied.as_ref().unwrap().csv.time_column, "value");
        assert_eq!(
            app.data.as_ref().unwrap().signals[0].points.point(1),
            [3., 1.]
        );
        assert!(!app.binary.is_decoding());
        assert!(!app.binary.decoded_ready());
        workspace_click(&mut app, &ctx, "Python");
        assert!(text(workspace_frame(&mut app, &ctx)).contains("hidden decoder must not run"));
        workspace_click(&mut app, &ctx, "YAML");
        assert!(text(workspace_frame(&mut app, &ctx)).contains(crate::decoder_ui::RUN_LABEL));
        workspace_click(&mut app, &ctx, "CSV");
        assert_eq!(app.cfg.csv.time_column, "value");
        assert!(app.loader_rx.is_none());
    }

    #[test]
    fn failed_csv_settings_keep_previous_results_marked_until_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 12_000);
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        app.cfg.csv.time_column = "missing".into();
        app.run_active_decoder(&ctx);
        wait_csv(&mut app, &ctx);
        assert!(!app.load_has_data);
        assert!(app.status.contains("not found"));
        assert_eq!(
            app.csv_column_name(0),
            "time",
            "previous results retain their actual X name"
        );
        app.show_view(Pane::Table);
        assert!(text(workspace_frame(&mut app, &ctx)).contains("Previous results"));
        assert_eq!(
            app.data.as_ref().unwrap().signals[0].points.point(2),
            [2., 6.]
        );
        app.request_csv_rows(20_000, &ctx);
        app.index_entire_csv(&ctx);
        assert!(
            app.loader_rx.is_none(),
            "old results must not drive reads with failed settings"
        );
        app.cfg.csv.time_column = "time".into();
        app.run_active_decoder(&ctx);
        wait_csv(&mut app, &ctx);
        assert!(app.load_has_data);
        assert!(!text(workspace_frame(&mut app, &ctx)).contains("Previous results"));
    }

    #[test]
    fn stopped_csv_demand_never_restarts_and_late_errors_keep_rows_available() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 10_000);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"invalid,time\n")
            .unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        assert_eq!(app.data.as_ref().unwrap().rows, 8192);
        app.request_csv_rows(9000, &ctx);
        app.stop_csv_read();
        wait_csv(&mut app, &ctx);
        assert!(app.csv_target.is_none());
        for _ in 0..5 {
            workspace_frame(&mut app, &ctx);
        }
        assert!(app.loader_rx.is_none());
        app.index_entire_csv(&ctx);
        wait_csv(&mut app, &ctx);
        assert!(app.status.contains("CSV:"));
        assert!(app.data.as_ref().unwrap().rows >= 8192);
        assert!(!app.csv_complete());
        assert_eq!(
            app.data.as_ref().unwrap().signals[0].points.point(10),
            [10., 30.]
        );
        assert!(app.binary.has_file());
    }

    #[test]
    fn file_menu_and_apply_shortcut_show_samples_beside_the_bytes() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
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
        assert!(top.iter().any(|label| label == "Open a file")
            && top.iter().any(|label| label == "⋯")
            && !top.iter().any(|label| label == "File"), "{top:?}");
        workspace_click(&mut app, &ctx, "⋯");
        workspace_click(&mut app, &ctx, "File");
        workspace_click(&mut app, &ctx, "Open sensor demo");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.workspace.dock.find_tab_legacy(&Pane::Table).is_none());
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
        while app.workspace.dock.find_tab_legacy(&Pane::Table).is_none() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let output = workspace_frame(&mut app, &ctx);
        let (surface, table, _) = app.workspace.dock.find_tab_legacy(&Pane::Table).unwrap();
        let rect = app.workspace.dock[surface][table].rect().unwrap();
        assert!(
            rect.width() > 450. && rect.height() > 250.,
            "Sample table should remain readable beside the decoder: {rect:?}"
        );
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,egui::epaint::Shape::Text(text) if text.galley.text()=="temperature")));
        let location = app.workspace.dock.find_tab_legacy(&Pane::Table).unwrap();
        app.workspace.dock.remove_tab(egui_dock::TabPath::new(location.0, location.1, location.2));
        for _ in 0..5 {
            workspace_frame(&mut app, &ctx);
        }
        assert!(
            app.workspace.dock.find_tab_legacy(&Pane::Table).is_none(),
            "completed workers must not reopen a closed view"
        );
        app.binary.apply_definition(&ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.workspace.dock.find_tab_legacy(&Pane::Table).is_none() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn welcome_does_not_reserve_empty_editors_and_views() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
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
    fn open_recent_restores_data_and_its_decoder() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("capture.bin");
        let decoder = directory.path().join("layout.yaml");
        std::fs::write(&data, b"TEST").unwrap();
        std::fs::write(&decoder, "endian: little\nactions: []\n").unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.file_locations.remember_pair(&data, Some(&decoder));
        app.open_recent(0, &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.binary.source_path() != Some(data.as_path()) {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(app.binary.decoder_path(), Some(decoder.as_path()));
        assert_eq!(app.file_locations.recent_pairs()[0].decoder.as_deref(), Some(decoder.as_path()));
    }

    #[test]
    fn byte_changes_discard_csv_results_and_pending_workers_without_losing_source() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
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
        let mut app = BinwaveApp::default();
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
            wait_byte_preview(&mut app, &ctx);
            assert!(app.data.is_none());
            assert!(app.loader_rx.is_none());
            assert!(app.binary.has_file());
            assert!(app.binary.has_layout());
            assert!(app.binary.decoded_ready());
            let output = workspace_frame(&mut app, &ctx);
            let labels: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect();
            let (surface, node, _) = app.workspace.dock.find_tab_legacy(&Pane::Hex).unwrap();
            assert!(app.workspace.dock[surface][node].rect().unwrap().width() > 400.);
            assert!(app.workspace.dock.find_tab_legacy(&Pane::Inspector).is_none());
            let (surface, node, _) = app.workspace.dock.find_tab_legacy(&Pane::Hex).unwrap();
            let hex_rect = app.workspace.dock[surface][node].rect().unwrap();
            assert!(hex_rect.height() > 250.);
            assert!(app.workspace.dock.find_tab_legacy(&Pane::Layout).is_some());
            let (plot_surface, plot_node, _) = app.workspace.dock.find_tab_legacy(&Pane::Plot).unwrap();
            let plot_rect = app.workspace.dock[plot_surface][plot_node].rect().unwrap();
            assert!(plot_rect.height() > 200.);
            assert!(plot_rect.bottom() <= hex_rect.top());
            assert!(app.workspace.dock.find_tab_legacy(&Pane::Table).is_none());
            for pane in [Pane::Hex, Pane::Layout, Pane::Structures, Pane::Plot] {
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
    fn opening_raw_data_preserves_loaded_decoders_without_running_them() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("capture.raw");
        std::fs::write(&data, [1u8, 0, 2, 0, 3, 0, 4, 0]).unwrap();
        for (extension, mode, code) in [
            ("py", "Python", "raise RuntimeError('saved_python_must_not_run')\n"),
            ("yaml", "YAML", "endian: little\nstructures:\n  - name: Custom\n    fields:\n      - {name: saved_yaml_field, type: u16}\nactions:\n  - read: {name: custom, type: {structure: Custom}, repeat: until end}\n"),
        ] {
            let ctx = Context::default();
            let mut app = BinwaveApp::default();
            let definition = dir.path().join(format!("custom.{extension}"));
            std::fs::write(&definition, code).unwrap();
            app.binary.load_yaml(definition, &ctx);
            workspace_frame(&mut app, &ctx);
            app.open_path(data.clone(), &ctx);
            wait_byte_preview(&mut app, &ctx);
            assert!(!app.binary.status().1, "{}", app.binary.status().0);
            assert!(!app.binary.is_dirty());
            assert!(app.workspace.dock.find_tab_legacy(&Pane::Plot).is_some());
            workspace_click(&mut app, &ctx, mode);
            let shown = text(workspace_frame(&mut app, &ctx));
            let saved_field = if extension == "py" {
                "saved_python_must_not_run"
            } else {
                "saved_yaml_field"
            };
            assert!(shown.contains(saved_field), "saved decoder was replaced: {shown}");
            assert!(app.binary.showing_byte_preview(), "choosing an editor must retain the preview");
        }
    }

    #[test]
    fn empty_raw_file_keeps_hex_and_decoder_available_without_an_empty_plot() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("empty.raw");
        std::fs::write(&data, []).unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.open_path(data, &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() || app.binary.busy() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        workspace_frame(&mut app, &ctx);
        assert!(!app.binary.showing_byte_preview());
        assert!(!app.binary.is_decoding());
        assert!(app.workspace.dock.find_tab_legacy(&Pane::Plot).is_some());
        assert!(app.workspace.dock.find_tab_legacy(&Pane::Hex).is_some());
        assert!(app.workspace.dock.find_tab_legacy(&Pane::Layout).is_some());
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
        let mut app = BinwaveApp::default();
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
        assert!(app.status.contains("CSV:"), "{}", app.status);
        assert_eq!(app.binary.source_path(), Some(path.as_path()));
        assert_eq!(app.binary.file_len(), 17);
        assert!(!app.binary.has_layout());
    }

    #[test]
    fn progressive_plot_controls_work_and_preserve_view_until_completion() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
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
        click(&mut app, &ctx, "Series 2/2");
        click(&mut app, &ctx, "Hide all");
        assert_eq!(app.visible, [false, false]);
        app.view
            .as_mut()
            .unwrap()
            .set_center_span(50., 20., app.data.as_ref().unwrap());
        click(&mut app, &ctx, "Previous");
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
        let mut app = BinwaveApp::default();
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
        assert!(app.status.contains("200 rows remain available"));
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
        let mut app = BinwaveApp::default();
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
        tx.send((
            old,
            Ok(vec![(
                0,
                "a".into(),
                vec![crate::plot_index::Sample {
                    record: 1,
                    byte_offset: 0,
                    point: [1., 2.],
                }],
            )]),
        ))
        .unwrap();
        app.prepare_plot(next.clone(), &ctx);
        assert_eq!(app.plot_lines[0].2[0].point, [1., 2.]);
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
        let mut app = BinwaveApp::default();
        app.binary.open_demo(&ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        workspace_frame(&mut app, &ctx);
        let hex = app.workspace.dock.find_tab_legacy(&Pane::Hex).unwrap();
        app.workspace.dock.remove_tab(egui_dock::TabPath::new(hex.0, hex.1, hex.2));
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
        assert!(app.workspace.dock.find_tab_legacy(&Pane::Hex).is_some());
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
        let mut app = BinwaveApp::default();
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
        let mut app = BinwaveApp::default();
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
        let mut app = BinwaveApp::default();
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
            fn remove_string(&mut self, key: &str) { self.0.remove(key); }
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
        let mut app = BinwaveApp::default();
        for (kind, directory, name) in cases {
            let directory = root.path().join(directory);
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join(name);
            std::fs::write(&path, "").unwrap();
            app.file_locations.remember_file(kind, &path);
        }
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let mut restored = BinwaveApp::default();
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
        let mut app = BinwaveApp::default();
        let directory = tempfile::tempdir().unwrap();
        let data_path = directory.path().join("waveform.data");
        std::fs::write(&data_path, [1u8, 0, 2, 0, 3, 0, 4, 0]).unwrap();
        let yaml_path = directory.path().join("waveform.yaml");
        let yaml = "endian: little\nstructures:\n  - name: Sample\n    fields:\n      - {name: amplitude, type: u16}\nactions:\n  - read: {name: samples, type: {structure: Sample}, repeat: until end}\n";
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
        // Raw data starts with bytes and meaning. Source controls are one tab away.
        workspace_click(&mut app, &ctx, "Decoder");
        let opened_output = workspace_frame(&mut app, &ctx);
        let (surface, node, _) = app.workspace.dock.find_tab_legacy(&Pane::Layout).unwrap();
        let decoder_rect = app.workspace.dock[surface][node].rect().unwrap();
        assert!(
            !opened_output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(text)
                if decoder_rect.contains(text.pos) && text.galley.text() == "Overview")
            }),
            "the decoder should open directly in its source editor"
        );
        let opened = labels(opened_output);
        for label in ["Decoder", crate::decoder_ui::RUN_LABEL, "YAML"] {
            assert!(opened.iter().any(|text| text == label), "missing {label}");
        }
        assert!(!opened.iter().any(|text| text.contains("All definitions")));
        wait_byte_preview(&mut app, &ctx);
        app.binary.load_yaml(yaml_path, &ctx);
        let loaded = labels(workspace_frame(&mut app, &ctx));
        assert!(loaded.iter().any(|text| text.contains("waveform.yaml")));
        assert!(
            loaded.iter().any(|text| text.contains("structures:")),
            "loading must display source, without another tab click"
        );
        assert!(app.binary.showing_byte_preview());
        workspace_click(&mut app, &ctx, crate::decoder_ui::RUN_LABEL);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.binary.is_decoding() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!app.binary.showing_byte_preview());
        let decoded = labels(workspace_frame(&mut app, &ctx));
        assert!(decoded.iter().any(|text| text == "amplitude"));
        assert!(
            decoded.iter().any(|text| text.contains("structures:")),
            "running must keep the source visible"
        );
        let (_, editor, _) = app.workspace.dock.find_tab_legacy(&Pane::Layout).unwrap();
        let (_, samples, _) = app.workspace.dock.find_tab_legacy(&Pane::Table).unwrap();
        assert_ne!(editor, samples);
        workspace_click(&mut app, &ctx, "View");
        workspace_click(&mut app, &ctx, "Signal");
        assert!(app.workspace.dock.find_tab_legacy(&Pane::Plot).is_some());
        let plotted = labels(workspace_frame(&mut app, &ctx));
        assert!(plotted.iter().any(|text| text.contains("structures:")));
    }

    #[test]
    fn python_file_runs_only_on_request_and_shows_results_in_same_workspace() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("capture.raw");
        let script = directory.path().join("decoder.py");
        std::fs::write(&data, [1u8, 0, 2, 0, 3, 0, 4, 0]).unwrap();
        std::fs::write(&script, "from binwave import Struct\nimport math\ndef sample_type():\n    return Struct('Sample', [('amplitude', 'u16')])\nfile.records('samples', sample_type())\nprint('Completed', math.sqrt(16))\n").unwrap();
        app.open_path(data, &ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !app.binary.has_file() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        wait_byte_preview(&mut app, &ctx);
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
        for label in ["Python", crate::decoder_ui::RUN_LABEL, "Console"] {
            assert!(labels.contains(&label), "missing {label}");
        }
        assert!(labels.iter().any(|label| label.contains("decoder.py")));
        assert!(!labels.contains(&"Fields") && !labels.contains(&"Steps"));
        assert!(!app.binary.is_decoding());
        assert!(app.binary.showing_byte_preview());
        workspace_click(&mut app, &ctx, crate::decoder_ui::RUN_LABEL);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while app.binary.is_decoding() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline, "{}", app.binary.status().0);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let output = workspace_frame(&mut app, &ctx);
        assert!(!app.binary.showing_byte_preview());
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "amplitude")));
        assert!(app.workspace.dock.find_tab_legacy(&Pane::Table).is_some());
        let (_, editor, _) = app.workspace.dock.find_tab_legacy(&Pane::Layout).unwrap();
        let (_, table, _) = app.workspace.dock.find_tab_legacy(&Pane::Table).unwrap();
        assert_ne!(editor, table);
        workspace_click(&mut app, &ctx, "View");
        workspace_click(&mut app, &ctx, "Signal");
        assert!(app.workspace.dock.find_tab_legacy(&Pane::Plot).is_some());
    }

    #[test]
    fn stop_python_keeps_the_editor_responsive() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
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
        wait_byte_preview(&mut app, &ctx);
        app.binary.load_yaml(script, &ctx);
        workspace_click(&mut app, &ctx, crate::decoder_ui::RUN_LABEL);
        for _ in 0..30 {
            let output = workspace_frame(&mut app, &ctx);
            assert!(!output.shapes.is_empty());
            assert!(app.binary.is_decoding());
        }
        workspace_click(&mut app, &ctx, "Stop");
        assert!(!app.binary.is_decoding());
        assert!(app.binary.showing_byte_preview());
        assert!(app.binary.can_apply_definition());
    }

    fn capture_workspace(app: &mut BinwaveApp, ctx: &Context, name: &str) {
        let output = workspace_frame(app, ctx);
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let atlas = ctx.fonts(|fonts| fonts.image());
        let pixels: Vec<[u8; 4]> = atlas
            .pixels.iter()
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

    #[test]
    #[ignore = "writes an egui CSV workspace capture to /tmp"]
    fn capture_csv_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let path = csv_fixture(dir.path(), 30_000);
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.show_empty_workspace = true;
        for _ in 0..10 {
            workspace_frame(&mut app, &ctx);
        }
        capture_workspace(&mut app, &ctx, "yaml-starter");
        workspace_click(&mut app, &ctx, "Python");
        for _ in 0..10 {
            workspace_frame(&mut app, &ctx);
        }
        capture_workspace(&mut app, &ctx, "python-starter");
        workspace_click(&mut app, &ctx, "YAML");
        app.csv_cache_root = dir.path().join("cache");
        app.open_path(path, &ctx);
        wait_csv(&mut app, &ctx);
        capture_workspace(&mut app, &ctx, "csv-decoder");
        app.show_view(Pane::Plot);
        for _ in 0..20 {
            workspace_frame(&mut app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        capture_workspace(&mut app, &ctx, "csv-plot");
        app.request_csv_link(CsvLinkRequest::Cell(CsvCellSelection {
            row: 42,
            column: Some(1),
        }));
        wait_csv_selection(&mut app, &ctx);
        workspace_click(&mut app, &ctx, "Python");
        for _ in 0..45 {
            workspace_frame(&mut app, &ctx);
        }
        capture_workspace(&mut app, &ctx, "csv-linked-python");
        app.binary.load_python_text("test = 5\nprint(test)", &ctx);
        app.run_active_decoder(&ctx);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.binary.is_decoding() {
            workspace_frame(&mut app, &ctx);
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for _ in 0..10 {
            workspace_frame(&mut app, &ctx);
        }
        capture_workspace(&mut app, &ctx, "python-simple-script");
        app.show_view(Pane::Hex);
        for _ in 0..15 {
            workspace_frame(&mut app, &ctx);
        }
        capture_workspace(&mut app, &ctx, "csv-linked-hex");
    }

    #[test]
    #[ignore = "writes an egui initial byte-plot capture to /tmp"]
    fn capture_initial_byte_plot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.raw");
        let bytes: Vec<_> = (0..8192)
            .map(|index| {
                let phase = index as f64 * std::f64::consts::TAU / 240.;
                (128. + 100. * phase.sin()) as u8
            })
            .collect();
        std::fs::write(&path, bytes).unwrap();
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
        app.open_path(path, &ctx);
        wait_byte_preview(&mut app, &ctx);
        for _ in 0..40 {
            workspace_frame(&mut app, &ctx);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        capture_workspace(&mut app, &ctx, "initial-byte-plot");
    }

    /// A renderer-independent capture for reviewing the actual egui workspace.
    #[test]
    #[ignore = "writes a tessellated UI capture to /tmp; requires the generated preview fixture"]
    fn capture_editing_workspace() {
        let ctx = Context::default();
        let mut app = BinwaveApp::default();
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
        workspace_click(&mut app, &ctx, crate::decoder_ui::RUN_LABEL);
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
        let mut app = BinwaveApp::default();
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
