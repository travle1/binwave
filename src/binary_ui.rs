//! egui presentation and background jobs for the binary workspace.
use crate::binary::{
    self, BinaryDocument, ByteSnapshot, Layout, Original, PlotData, ResolvedLayout, Sample,
    ValueIdentity,
};
use crate::byte_store::{ByteStore, ByteView};
use crate::cache::LoadProgress;
use crate::file_dialog::{FileKind, FileLocations, NativeFileDialog};
use crate::file_source::FileSource;
use crate::hex_editor::{HexAction, HexEditor, HexHighlight};
use crate::structure::Node;
use crate::structure_ui::{
    editable_value, encode_value, has_decoder_code, DecoderLanguage, RunOutcome, StructureWorkspace,
};
use crate::structure_views::PlotAction;
use anyhow::{ensure, Result};
use egui::{Color32, Context, RichText, Ui};
use egui_plot::{Line, Plot, PlotBounds, PlotPoint, Points};
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const ROWS_PER_PAGE: usize = 100;

enum Action {
    Open(PathBuf),
    Demo,
    SimpleDemo,
    Apply(ResolvedLayout, String),
    LoadYaml(PathBuf),
    ReplaceYaml(String),
    ExampleYaml(String),
    ExamplePython(String),
    NewDecoder(DecoderLanguage),
    Close,
}

enum FileDialogAction {
    #[cfg(test)]
    Open,
    LoadYaml,
    Save(SaveContent),
}

enum SaveContent {
    Binary(ByteSnapshot),
    Yaml(String, DecoderLanguage),
    Bytes(ByteView, Range<usize>, bool),
}
#[derive(Debug, Clone)]
pub enum OpenedFile {
    Path(PathBuf),
    Demo,
}

enum IoResult {
    Opened(Arc<FileSource>),
    SimpleDemo(Arc<Original>),
    Validated(ResolvedLayout, String, Arc<Original>),
    Loaded(Arc<Original>, Option<PathBuf>, bool),
    SavedBinary(ByteSnapshot, PathBuf),
    SavedYaml(String, PathBuf, DecoderLanguage),
    SavedBytes(ByteView, PathBuf, Range<usize>, bool),
}

/// The inactive language keeps its own document, including its save baseline.
#[derive(Default)]
struct DecoderDraft {
    source: String,
    saved: String,
    path: Option<PathBuf>,
    example: bool,
    demo: bool,
    error: Option<String>,
}
impl DecoderDraft {
    fn starter(language: DecoderLanguage) -> Self {
        Self {
            source: language.starter().into(),
            saved: language.starter().into(),
            ..Self::default()
        }
    }
    fn dirty(&self) -> bool {
        self.source != self.saved
    }
}

struct GenericValueEdit {
    node: Node,
    revision: u64,
    original: String,
    text: String,
    error: Option<String>,
}

pub struct BinaryWorkspace {
    original: Option<Arc<Original>>,
    raw_source: Option<Arc<FileSource>>,
    bytes: Option<ByteStore>,
    hex_editor: HexEditor,
    authoring: crate::direct_authoring::DirectAuthoring,
    author_in_structure: bool,
    structure_ui: StructureWorkspace,
    generic_value_edit: Option<GenericValueEdit>,
    bytes_changed: bool,
    reveal_definition: bool,
    reveal_hex: bool,
    interpretation_stale: bool,
    reparse_at: Option<Instant>,
    opened_file: Option<OpenedFile>,
    layout_applied: bool,
    initial_plot_pending: bool,
    initial_plot_ready: bool,
    validation_is_refresh: bool,
    document: Option<BinaryDocument>,
    source: Option<PathBuf>,
    yaml: String,
    language: DecoderLanguage,
    other_draft: DecoderDraft,
    applied_language: Option<DecoderLanguage>,
    submitted_decoder: Option<(DecoderLanguage, String)>,
    direct_pending: Option<(String, crate::direct_authoring::Interpretation)>,
    interpretation_ready: bool,
    completed_python: Option<String>,
    saved_yaml: String,
    yaml_path: Option<PathBuf>,
    yaml_is_example: bool,
    decode_error: Option<String>,
    decoding_layout: bool,
    applied_yaml: Option<String>,
    demo_yaml: bool,
    message: String,
    error: bool,
    x: Option<usize>,
    y: usize,
    selected: Option<ValueIdentity>,
    table_anchor: Option<usize>,
    edit_text: String,
    edit_original: String,
    page: usize,
    scroll_to_selection: bool,
    plot_data: Option<PlotData>,
    plot_rx: Option<Receiver<Result<PlotData>>>,
    plot_cancel: Arc<AtomicBool>,
    plot_reset: bool,
    plot_bounds: Option<PlotBounds>,
    navigation: Option<PlotBounds>,
    refresh_at: Option<Instant>,
    connect: bool,
    io_rx: Option<Receiver<Result<IoResult>>>,
    file_dialog: NativeFileDialog<FileDialogAction>,
    dialog_template: rfd::AsyncFileDialog,
    file_locations: FileLocations,
    external_dialog_pending: bool,
    pending: Option<Action>,
    overwrite: Option<(PathBuf, SaveContent)>,
    allow_close: bool,
    load_progress: Option<Arc<LoadProgress>>,
    plot_cache: Arc<Mutex<binary::PlotCache>>,
    #[cfg(test)]
    last_transform: Option<egui_plot::PlotTransform>,
}

impl Default for BinaryWorkspace {
    fn default() -> Self {
        Self {
            original: None,
            raw_source: None,
            bytes: None,
            hex_editor: HexEditor::default(),
            authoring: Default::default(),
            author_in_structure: false,
            structure_ui: StructureWorkspace::default(),
            generic_value_edit: None,
            bytes_changed: false,
            reveal_definition: false,
            reveal_hex: false,
            interpretation_stale: false,
            reparse_at: None,
            opened_file: None,
            layout_applied: false,
            initial_plot_pending: false,
            initial_plot_ready: false,
            validation_is_refresh: false,
            document: None,
            source: None,
            yaml: DecoderLanguage::Yaml.starter().into(),
            language: DecoderLanguage::Yaml,
            other_draft: DecoderDraft::starter(DecoderLanguage::Python),
            applied_language: None,
            submitted_decoder: None,
            direct_pending: None,
            interpretation_ready: false,
            completed_python: None,
            saved_yaml: DecoderLanguage::Yaml.starter().into(),
            yaml_path: None,
            yaml_is_example: false,
            decode_error: None,
            decoding_layout: false,
            applied_yaml: None,
            demo_yaml: false,
            message:
                "Open any file to inspect its bytes. Drag panel titles to arrange the workspace."
                    .into(),
            error: false,
            x: None,
            y: 0,
            selected: None,
            table_anchor: None,
            edit_text: String::new(),
            edit_original: String::new(),
            page: 0,
            scroll_to_selection: false,
            plot_data: None,
            plot_rx: None,
            plot_cancel: Arc::new(AtomicBool::new(false)),
            plot_reset: true,
            plot_bounds: None,
            navigation: None,
            refresh_at: None,
            connect: true,
            io_rx: None,
            file_dialog: NativeFileDialog::default(),
            dialog_template: rfd::AsyncFileDialog::new(),
            file_locations: FileLocations::default(),
            external_dialog_pending: false,
            pending: None,
            overwrite: None,
            allow_close: false,
            load_progress: None,
            plot_cache: Arc::new(Mutex::new(binary::PlotCache::default())),
            #[cfg(test)]
            last_transform: None,
        }
    }
}

impl Drop for BinaryWorkspace {
    fn drop(&mut self) {
        self.plot_cancel.store(true, Ordering::Relaxed);
        if let Some(progress) = &self.load_progress {
            progress.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

impl BinaryWorkspace {
    #[cfg(test)]
    pub fn python_workspace() -> Self {
        let mut workspace = Self::default();
        workspace.switch_language(DecoderLanguage::Python);
        workspace
    }
    fn source_file_kind(language: DecoderLanguage) -> FileKind {
        match language {
            DecoderLanguage::Yaml => FileKind::Yaml,
            DecoderLanguage::Python => FileKind::Python,
        }
    }
    pub fn set_file_locations(&mut self, locations: FileLocations) {
        self.file_locations = locations;
    }
    fn decoder_save_directory(&self) -> Option<PathBuf> {
        let kind = Self::source_file_kind(self.language);
        self.file_locations
            .remembered_directory(kind)
            .or_else(|| {
                self.yaml_path
                    .as_deref()
                    .and_then(Path::parent)
                    .filter(|directory| directory.is_dir())
                    .map(Path::to_path_buf)
            })
            .or_else(|| self.file_locations.directory(kind))
    }
    fn language_for_path(path: &Path) -> DecoderLanguage {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("py"))
        {
            DecoderLanguage::Python
        } else {
            DecoderLanguage::Yaml
        }
    }
    fn switch_language(&mut self, language: DecoderLanguage) {
        if self.language == language {
            return;
        }
        let current = DecoderDraft {
            source: std::mem::take(&mut self.yaml),
            saved: std::mem::take(&mut self.saved_yaml),
            path: self.yaml_path.take(),
            example: self.yaml_is_example,
            demo: self.demo_yaml,
            error: self.decode_error.take(),
        };
        let next = std::mem::replace(&mut self.other_draft, current);
        self.yaml = next.source;
        self.saved_yaml = next.saved;
        self.yaml_path = next.path;
        self.yaml_is_example = next.example;
        self.demo_yaml = next.demo;
        self.decode_error = next.error;
        self.language = language;
        self.error = false;
        self.message = match language {
            DecoderLanguage::Python => "Python draft selected.",
            DecoderLanguage::Yaml => "YAML draft selected.",
        }
        .into();
        self.reveal_definition = true;
    }
    fn active_authoring_dirty(&self) -> bool {
        self.language == DecoderLanguage::Yaml && self.structure_ui.authoring_dirty()
    }
    fn draft_dirty(&self, language: DecoderLanguage) -> bool {
        let source_dirty = if language == self.language {
            self.yaml != self.saved_yaml
        } else {
            self.other_draft.dirty()
        };
        source_dirty || (language == DecoderLanguage::Yaml && self.structure_ui.authoring_dirty())
    }
    fn draft_source(&self, language: DecoderLanguage) -> &str {
        if language == self.language {
            &self.yaml
        } else {
            &self.other_draft.source
        }
    }
    fn set_draft_error(&mut self, language: DecoderLanguage, error: Option<String>) {
        if language == self.language {
            self.decode_error = error;
        } else {
            self.other_draft.error = error;
        }
    }
    fn finish_source_save(&mut self, source: String, path: &Path, language: DecoderLanguage) {
        self.file_locations
            .remember_file(Self::source_file_kind(language), path);
        if language == self.language {
            if self.yaml == source {
                self.demo_yaml = false;
            }
            self.saved_yaml = source;
            self.yaml_path = Some(path.into());
            self.yaml_is_example = false;
        } else {
            if self.other_draft.source == source {
                self.other_draft.demo = false;
            }
            self.other_draft.saved = source;
            self.other_draft.path = Some(path.into());
            self.other_draft.example = false;
        }
    }

    pub fn is_python(&self) -> bool {
        self.language == DecoderLanguage::Python
    }
    pub fn is_decoding(&self) -> bool {
        self.structure_ui.is_parsing() || self.decoding_layout
    }
    pub fn stop_decoding(&mut self) {
        self.initial_plot_pending = false;
        self.initial_plot_ready = false;
        self.structure_ui.cancel_decoding();
        if self.decoding_layout {
            if let Some(progress) = &self.load_progress {
                progress.cancelled.store(true, Ordering::Relaxed);
            }
        }
    }
    fn validate_layout(&mut self, ctx: &Context) {
        self.validate_layout_for(ctx, false);
    }
    fn validate_layout_for(&mut self, ctx: &Context, refresh: bool) {
        self.direct_pending = None;
        if !refresh {
            self.initial_plot_pending = false;
            self.initial_plot_ready = false;
        }
        self.validation_is_refresh = refresh;
        self.decode_error = None;
        self.decoding_layout = false;
        if self.is_python() {
            if self.cell_dirty() {
                self.report(Err(anyhow::anyhow!(
                    "Commit or cancel the table value before running the decoder."
                )));
                return;
            }
            if let Some(bytes) = &self.bytes {
                if !refresh {
                    self.reparse_at = None;
                }
                self.submitted_decoder = Some((self.language, self.yaml.clone()));
                self.completed_python = None;
                if self.structure_ui.apply_python(
                    &self.yaml,
                    self.yaml_path.as_deref(),
                    bytes.view(),
                    ctx,
                ) {
                    self.report(Ok("Running Python script…".into()));
                }
            }
            return;
        }
        if StructureWorkspace::is_generic(&self.yaml) {
            if self.cell_dirty() {
                self.report(Err(anyhow::anyhow!(
                    "Commit or cancel the table value before decoding again."
                )));
                return;
            }
            if let Some(bytes) = &self.bytes {
                if !refresh {
                    self.reparse_at = None;
                }
                self.submitted_decoder = Some((self.language, self.yaml.clone()));
                if self.structure_ui.apply(&self.yaml, bytes.view(), ctx) {
                    self.report(Ok("Decoding file… Hex remains available.".into()));
                } else if let Some(error) = self.structure_ui.decode_error().map(str::to_owned) {
                    self.decode_error = Some(error.clone());
                    self.report(Err(anyhow::anyhow!(error)));
                }
            }
            return;
        }
        let layout = match Layout::parse(&self.yaml) {
            Ok(layout) => layout,
            Err(err) => {
                self.decode_error = Some(format!("{err:#}"));
                self.report(Err(err));
                return;
            }
        };
        let existing = self
            .document
            .as_ref()
            .map(|doc| doc.bytes.clone())
            .or_else(|| {
                self.original.clone().map(|original| ByteSnapshot {
                    original,
                    patches: Default::default(),
                })
            });
        let raw_source = self.raw_source.clone();
        let working_patches = self
            .bytes
            .as_ref()
            .map(|s| (*s.view().patches).clone())
            .unwrap_or_default();
        if existing.is_none() && raw_source.is_none() {
            return;
        }
        let yaml = self.yaml.clone();
        let progress = Arc::new(LoadProgress::default());
        self.load_progress = Some(progress.clone());
        self.decoding_layout = true;
        self.report(Ok("Decoding file… Checking section boundaries.".into()));
        self.start_io(ctx, move || {
            let bytes = if let Some(mut bytes) = existing {
                bytes.patches = working_patches;
                bytes
            } else {
                let source = raw_source.unwrap();
                source.validate()?;
                let original = Original::open(&source.path, &progress)?;
                source.validate()?;
                ByteSnapshot {
                    original,
                    patches: working_patches,
                }
            };
            let resolved =
                layout.resolve(bytes.original.bytes().len(), |i| bytes.byte(i), &progress)?;
            Ok(IoResult::Validated(resolved, yaml, bytes.original))
        });
        ctx.request_repaint();
    }

    fn reset_section_view(&mut self) {
        let doc = self.document.as_ref().unwrap();
        self.x = None;
        self.y = doc
            .layout
            .fields
            .iter()
            .position(|f| f.name == "temperature")
            .unwrap_or(0);
        self.selected = None;
        self.table_anchor = None;
        self.edit_text.clear();
        self.edit_original.clear();
        self.page = 0;

        self.plot_data = None;
        self.plot_bounds = None;
        self.navigation = None;
    }

    fn change_section(&mut self, section: usize, ctx: &Context) {
        if self.cell_dirty() {
            self.report(Err(anyhow::anyhow!(
                "Commit or cancel the table value before changing section."
            )));
            return;
        }
        let result = self.document.as_mut().unwrap().select_section(section);
        if let Err(err) = result {
            self.report(Err(err));
            return;
        }
        self.reset_section_view();
        self.refresh_plot(ctx, true);
    }

    fn section_controls(&mut self, ui: &mut Ui) {
        let Some(doc) = &self.document else {
            return;
        };
        let mut section = doc.section;
        egui::ComboBox::from_id_source("binary_section")
            .selected_text(format!("Section: {}", doc.layout.name))
            .show_ui(ui, |ui| {
                for (i, entry) in doc.structure.sections.iter().enumerate() {
                    ui.selectable_value(&mut section, i, &entry.record.name);
                }
            });
        egui::CollapsingHeader::new("Layout summary")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_source("section_summary")
                    .max_height(120.)
                    .show(ui, |ui| {
                        if doc.structure.legacy {
                            ui.small("Legacy explicit-offset layout");
                        }
                        for entry in &doc.structure.sections {
                            let r = &entry.record;
                            ui.small(format!(
                                "{}: 0x{:X}..0x{:X} · {}",
                                r.name,
                                entry.physical.start,
                                entry.physical.end,
                                if entry.opaque {
                                    format!(
                                        "{} uninterpreted bytes",
                                        entry.physical.end - entry.marker.end
                                    )
                                } else {
                                    format!(
                                        "{} records × {} B · data 0x{:X}",
                                        r.records, r.record_size, r.data_offset
                                    )
                                }
                            ));
                            if !entry.marker.is_empty() {
                                ui.small(format!(
                                    "  Marker: {} bytes at 0x{:X}",
                                    entry.marker.len(),
                                    entry.marker.start
                                ));
                            }
                        }
                        for gap in &doc.structure.gaps {
                            ui.small(format!(
                                "Gap: {} bytes · 0x{:X}..0x{:X}",
                                gap.len(),
                                gap.start,
                                gap.end
                            ));
                        }
                        let tail = &doc.structure.trailing;
                        ui.small(format!(
                            "Trailing: {} uninterpreted bytes · 0x{:X}..0x{:X}",
                            tail.len(),
                            tail.start,
                            tail.end
                        ));
                    });
            });
        if section != doc.section {
            self.change_section(section, &ui.ctx().clone());
        }
    }

    fn report(&mut self, result: Result<String>) {
        match result {
            Ok(message) => {
                self.message = message;
                self.error = false;
            }
            Err(error) => {
                self.message = format!("{error:#}");
                self.error = true;
            }
        }
    }

    pub fn busy(&self) -> bool {
        self.io_rx.is_some()
    }
    pub fn dialog_pending(&self) -> bool {
        self.file_dialog.pending()
    }
    pub fn set_external_dialog_pending(&mut self, pending: bool) {
        self.external_dialog_pending = pending;
    }
    pub fn set_dialog_template(&mut self, dialog: rfd::AsyncFileDialog) {
        self.dialog_template = dialog;
    }
    fn any_dialog_pending(&self) -> bool {
        self.dialog_pending() || self.external_dialog_pending
    }
    fn cell_dirty(&self) -> bool {
        self.edit_text != self.edit_original
    }
    fn unsaved(&self) -> bool {
        self.bytes.as_ref().is_some_and(ByteStore::is_dirty)
            || self.document.as_ref().is_some_and(BinaryDocument::is_dirty)
            || self.cell_dirty()
            || self.yaml != self.saved_yaml
            || self.other_draft.dirty()
            || self.structure_ui.authoring_dirty()
    }

    pub fn open(&mut self, path: PathBuf, ctx: &Context) {
        self.request(Action::Open(path), ctx);
    }
    pub fn open_simple_demo(&mut self, ctx: &Context) {
        self.request(Action::SimpleDemo, ctx);
    }
    pub fn open_demo(&mut self, ctx: &Context) {
        self.request(Action::Demo, ctx);
    }
    pub fn take_opened_file(&mut self) -> Option<OpenedFile> {
        self.opened_file.take()
    }
    pub fn take_interpretation_ready(&mut self) -> bool { std::mem::take(&mut self.interpretation_ready) }
    pub fn take_layout_applied(&mut self) -> bool {
        std::mem::take(&mut self.layout_applied)
    }
    pub fn has_file(&self) -> bool {
        self.original.is_some() || self.raw_source.is_some()
    }
    #[cfg(test)]
    pub fn has_layout(&self) -> bool {
        self.document.is_some() || self.structure_ui.has_published_program()
    }
    pub fn source_path(&self) -> Option<&Path> {
        self.source.as_deref()
    }
    pub fn file_len(&self) -> usize {
        self.original
            .as_ref()
            .map(|o| o.bytes().len())
            .or_else(|| self.raw_source.as_ref().map(|s| s.len))
            .unwrap_or(0)
    }
    pub fn status(&self) -> (&str, bool) {
        (&self.message, self.error)
    }
    pub fn has_byte_edits(&self) -> bool {
        self.cell_dirty() || self.bytes.as_ref().is_some_and(ByteStore::has_changes)
    }
    pub fn take_bytes_changed(&mut self) -> bool {
        std::mem::take(&mut self.bytes_changed)
    }
    pub fn take_reveal_definition(&mut self) -> bool {
        self.structure_ui.take_reveal() | std::mem::take(&mut self.reveal_definition)
    }
    pub fn take_reveal_hex(&mut self) -> bool {
        std::mem::take(&mut self.reveal_hex)
    }
    pub fn is_dirty(&self) -> bool {
        self.unsaved()
    }

    pub fn load_yaml(&mut self, path: PathBuf, ctx: &Context) {
        self.request(Action::LoadYaml(path), ctx);
    }

    fn request(&mut self, action: Action, ctx: &Context) {
        if !self.commands_available() {
            return;
        }
        let needs_confirmation = match &action {
            Action::LoadYaml(path) => self.draft_dirty(Self::language_for_path(path)),
            Action::ExampleYaml(_) => self.draft_dirty(DecoderLanguage::Yaml),
            Action::ExamplePython(_) => self.draft_dirty(DecoderLanguage::Python),
            Action::NewDecoder(language) => self.draft_dirty(*language),
            Action::ReplaceYaml(_) => self.draft_dirty(self.language),
            Action::Apply(_, yaml) => self.applied_yaml.as_ref() != Some(yaml) && self.cell_dirty(),
            _ => self.unsaved(),
        };
        if needs_confirmation {
            self.pending = Some(action);
        } else {
            self.execute(action, ctx);
        }
    }

    fn start_io(
        &mut self,
        ctx: &Context,
        task: impl FnOnce() -> Result<IoResult> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        self.io_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(task());
            ctx.request_repaint();
        });
    }

    fn execute(&mut self, action: Action, ctx: &Context) {
        match action {
            Action::Open(path) => {
                self.report(Ok(format!("Opening {}…", path.display())));
                self.start_io(ctx, move || {
                    Ok(IoResult::Opened(Arc::new(FileSource::open(&path)?)))
                });
            }
            Action::SimpleDemo => {
                self.report(Ok("Opening simple wave demo…".into()));
                self.start_io(ctx, || Ok(IoResult::SimpleDemo(Original::from_bytes(
                    include_bytes!("../samples/quickstart/simple_wave.bin"),
                )?)));
            }
            Action::Demo => {
                self.report(Ok("Opening embedded sensor demo…".into()));
                self.start_io(ctx, || {
                    Ok(IoResult::Loaded(
                        Original::from_bytes(binary::DEMO_BYTES)?,
                        None,
                        true,
                    ))
                });
            }
            Action::Apply(layout, yaml) => {
                if self.applied_language == Some(DecoderLanguage::Yaml)
                    && self.applied_yaml.as_ref() == Some(&yaml)
                    && !self.interpretation_stale
                {
                    self.layout_applied = !self.validation_is_refresh;
                    return;
                }
                let result = if let Some(document) = &mut self.document {
                    document.apply_resolved(layout);
                    Ok(())
                } else if let Some(original) = &self.original {
                    BinaryDocument::from_resolved(original.clone(), layout)
                        .map(|doc| self.document = Some(doc))
                } else {
                    return;
                };
                match result {
                    Ok(()) => {
                        self.applied_yaml = Some(yaml);
                        self.applied_language = Some(DecoderLanguage::Yaml);
                        self.structure_ui.clear_program();
                        self.interpretation_stale = false;
                        if let (Some(doc), Some(store)) = (&mut self.document, &mut self.bytes) {
                            doc.bytes.patches = (*store.view().patches).clone();
                        }
                        self.layout_applied = !self.validation_is_refresh;
                        if !self.validation_is_refresh {
                            if let Some(doc) = &mut self.document {
                                if let Some((index, _)) = doc
                                    .structure
                                    .sections
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, section)| {
                                        !section.record.fields.is_empty()
                                            && section.record.records > 1
                                    })
                                    .max_by_key(|(_, section)| section.record.records)
                                {
                                    let _ = doc.select_section(index);
                                }
                            }
                        }
                        self.reset_section_view();
                        self.refresh_plot(ctx, true);
                        self.report(Ok(
                            "Decoded. Select a sample or value to inspect and edit it.".into(),
                        ));
                    }
                    Err(err) => self.report(Err(err)),
                }
            }
            Action::ReplaceYaml(yaml) => {
                self.yaml = yaml;
                self.demo_yaml = false;
                self.decode_error = None;
                self.reveal_definition = true;
                self.report(Ok(
                    "Decoder updated. Click Decode file to view the results.".into(),
                ));
            }
            Action::ExampleYaml(yaml) => {
                self.switch_language(DecoderLanguage::Yaml);
                self.execute(Action::ReplaceYaml(yaml), ctx);
                self.yaml_path = None;
                self.saved_yaml.clear();
                self.yaml_is_example = true;
                self.structure_ui.reset_authoring();
                self.show_yaml();
            }
            Action::ExamplePython(code) => {
                self.switch_language(DecoderLanguage::Python);
                self.execute(Action::ReplaceYaml(code), ctx);
                self.yaml_path = None;
                self.saved_yaml.clear();
                self.yaml_is_example = true;
                self.report(Ok("Python decoder ready to edit.".into()));
            }
            Action::NewDecoder(language) => {
                self.switch_language(language);
                self.yaml = language.starter().into();
                self.saved_yaml.clone_from(&self.yaml);
                self.yaml_path = None;
                self.yaml_is_example = false;
                self.demo_yaml = false;
                self.decode_error = None;
                self.completed_python = None;
                if language == DecoderLanguage::Yaml {
                    self.structure_ui.reset_authoring();
                    self.show_yaml();
                }
                self.reveal_definition = true;
                self.report(Ok("New decoder ready to edit.".into()));
            }
            Action::LoadYaml(path) => {
                let result = (|| -> Result<String> {
                    let file = std::fs::File::open(&path)?;
                    ensure!(file.metadata()?.is_file(), "select a regular decoder file");
                    let mut yaml = String::new();
                    file.take(256 * 1024 + 1).read_to_string(&mut yaml)?;
                    ensure!(
                        yaml.len() <= 256 * 1024,
                        "decoder exceeds the 256 KiB loading limit"
                    );
                    self.switch_language(Self::language_for_path(&path));
                    self.yaml = yaml;
                    self.saved_yaml.clone_from(&self.yaml);
                    self.demo_yaml = false;
                    self.yaml_path = Some(path.clone());
                    self.yaml_is_example = false;
                    self.decode_error = None;
                    if !self.is_python() {
                        self.structure_ui.reset_authoring();
                        self.show_yaml();
                    }
                    self.reveal_definition = true;
                    self.file_locations
                        .remember_file(Self::source_file_kind(self.language), &path);
                    Ok(if self.is_python() {
                        "Python loaded. Ready to run."
                    } else {
                        "YAML loaded. Ready to decode."
                    }
                    .into())
                })();
                self.report(result);
            }
            Action::Close => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn refresh_plot(&mut self, ctx: &Context, reset: bool) {
        self.plot_cancel.store(true, Ordering::Relaxed);
        self.refresh_at = None;
        let Some(doc) = &self.document else {
            return;
        };
        self.plot_rx = None;
        if doc.layout.fields.is_empty() {
            self.plot_data = None;
            return;
        }
        let bytes = doc.bytes.clone();
        let layout = doc.layout.clone();
        let (x, y) = (self.x, self.y);
        let view = if reset {
            None
        } else {
            self.plot_bounds.map(|b| [b.min()[0], b.max()[0]])
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.plot_cancel = cancel.clone();
        self.plot_reset = reset;
        let (tx, rx) = mpsc::channel();
        self.plot_rx = Some(rx);
        let ctx = ctx.clone();
        let cache = self.plot_cache.clone();
        std::thread::spawn(move || {
            let result = cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .query(&bytes, &layout, x, y, view, 4096, &cancel);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn finish_open(
        &mut self,
        original: Option<Arc<Original>>,
        source: Option<PathBuf>,
        raw: Option<Arc<FileSource>>,
        demo: bool,
        cache_hit: bool,
    ) {
        self.plot_cancel.store(true, Ordering::Relaxed);
        self.plot_rx = None;
        self.plot_cache = Arc::new(Mutex::new(binary::PlotCache::default()));
        self.plot_data = None;
        self.plot_bounds = None;
        self.navigation = None;
        self.refresh_at = None;
        self.bytes = original
            .as_ref()
            .map(|o| ByteStore::from_snapshot(o.clone()))
            .or_else(|| raw.as_ref().map(|r| ByteStore::from_file(r.clone())));
        self.original = original;
        self.raw_source = raw;
        self.hex_editor.reset();
        self.reveal_hex = false;
        self.structure_ui.clear_file();
        self.generic_value_edit = None;
        self.interpretation_stale = false;
        self.reparse_at = None;
        self.bytes_changed = false;
        self.layout_applied = false;
        self.initial_plot_pending = false;
        self.initial_plot_ready = false;
        self.opened_file = Some(if demo {
            OpenedFile::Demo
        } else {
            OpenedFile::Path(source.clone().unwrap())
        });
        self.source = source;
        if !demo {
            if let Some(path) = &self.source {
                self.file_locations.remember_file(FileKind::Data, path);
            }
        }
        self.document = None;
        self.applied_yaml = None;
        self.applied_language = None;
        self.submitted_decoder = None;
        self.direct_pending = None;
        self.completed_python = None;
        self.decode_error = None;
        self.other_draft.error = None;
        self.decoding_layout = false;
        self.selected = None;
        self.table_anchor = None;
        self.edit_text.clear();
        self.edit_original.clear();

        if demo {
            self.switch_language(DecoderLanguage::Yaml);
            self.yaml = binary::DEMO_YAML.into();
            self.saved_yaml.clone_from(&self.yaml);
            self.yaml_path = None;
            self.yaml_is_example = true;
            self.show_yaml();
        } else if self.demo_yaml && self.yaml == binary::DEMO_YAML {
            // Only discard the untouched example supplied by Open demo.
            // Explicitly loaded or edited layouts remain available for reuse.
            self.yaml = DecoderLanguage::Yaml.starter().into();
            self.saved_yaml.clone_from(&self.yaml);
            self.yaml_path = None;
            self.yaml_is_example = false;
            self.demo_yaml = false;
        }
        if !demo && self.other_draft.demo && self.other_draft.source == binary::DEMO_YAML {
            self.other_draft = DecoderDraft::starter(DecoderLanguage::Yaml);
        }
        self.demo_yaml = demo;
        let ready = if cache_hit {
            "Reused cached binary snapshot."
        } else {
            "File open."
        };
        self.report(Ok(format!("{ready} Open or create a decoder.")));
    }

    pub fn tick(&mut self, ctx: &Context) {
        // Native dialogs run independently of the event loop. Poll even while the
        // rest of the workspace is rendering so its window keeps answering pings.
        if let Some((action, path)) = self.file_dialog.poll(ctx) {
            self.finish_file_dialog(action, path, ctx);
        }
        if let Some(result) = self.io_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.io_rx = None;
            let was_decoding_layout = std::mem::take(&mut self.decoding_layout);
            let cache_hit = self
                .load_progress
                .take()
                .is_some_and(|p| p.cache_hit.load(Ordering::Relaxed));
            match result {
                Ok(IoResult::Validated(layout, yaml, original)) => {
                    if let Some(store) = &mut self.bytes {
                        if let Err(error) = store.use_snapshot(original.clone()) {
                            self.report(Err(error));
                            return;
                        }
                    }
                    self.original = Some(original);
                    self.request(Action::Apply(layout, yaml), ctx)
                }
                Ok(IoResult::SimpleDemo(original)) => {
                    self.finish_open(Some(original), None, None, true, false);
                    self.yaml = DecoderLanguage::Yaml.starter().into();
                    self.saved_yaml.clone_from(&self.yaml);
                    self.apply_definition(ctx);
                }
                Ok(IoResult::Opened(source)) => {
                    let path = source.path.clone();
                    self.finish_open(None, Some(path), Some(source), false, false);
                }
                Ok(IoResult::Loaded(original, source, demo)) => {
                    self.finish_open(Some(original), source, None, demo, cache_hit);
                }
                Ok(IoResult::SavedBinary(snapshot, path)) => {
                    self.file_locations.remember_file(FileKind::Exports, &path);
                    if let Some(doc) = &mut self.document {
                        doc.mark_saved(&snapshot);
                    }
                    if let Some(store) = &mut self.bytes {
                        let mut view = store.view();
                        view.patches = Arc::new(snapshot.patches.clone());
                        store.mark_saved(&view);
                    }
                    self.report(Ok(format!("Saved edited copy: {}", path.display())));
                }
                Ok(IoResult::SavedBytes(view, path, range, mark_saved)) => {
                    self.file_locations.remember_file(FileKind::Exports, &path);
                    if mark_saved {
                        if let Some(store) = &mut self.bytes {
                            store.mark_saved(&view);
                        }
                        if let Some(doc) = &mut self.document {
                            let mut snapshot = doc.bytes.clone();
                            snapshot.patches = (*view.patches).clone();
                            doc.mark_saved(&snapshot);
                        }
                    }
                    self.report(Ok(if mark_saved {
                        format!("Saved edited copy: {}", path.display())
                    } else {
                        format!(
                            "Exported {} bytes (0x{:X}..0x{:X}, end exclusive): {}",
                            range.len(),
                            range.start,
                            range.end,
                            path.display()
                        )
                    }));
                }
                Ok(IoResult::SavedYaml(yaml, path, language)) => {
                    self.finish_source_save(yaml, &path, language);
                    self.report(Ok(format!("Saved decoder: {}", path.display())));
                }
                Err(error) => {
                    if was_decoding_layout {
                        self.set_draft_error(DecoderLanguage::Yaml, Some(format!("{error:#}")));
                    }
                    self.report(Err(error));
                }
            }
        }
        if let Some(result) = self.plot_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.plot_rx = None;
            match result {
                Ok(data) => {
                    if self.plot_reset {
                        self.navigation = Some(data_bounds(&data));
                    }
                    self.plot_data = Some(data);
                }
                Err(error) => self.report(Err(error)),
            }
        }
        if self.refresh_at.is_some_and(|at| Instant::now() >= at) {
            self.refresh_plot(ctx, false);
        }
        if self.io_rx.is_some() || self.plot_rx.is_some() || self.refresh_at.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }

        let was_decoding = self.structure_ui.is_parsing();
        if let Some(bytes) = &self.bytes {
            self.structure_ui.tick(ctx, bytes.view());
        }
        if let Some(outcome) = self.structure_ui.take_run_finished() {
            let initial_plot = std::mem::take(&mut self.initial_plot_pending);
            if let Some((language, source)) = self.submitted_decoder.take() {
                let published = matches!(&outcome, RunOutcome::Published);
                if let Some((before, interpretation)) = self.direct_pending.take() {
                    if published && self.yaml == before {
                        self.yaml = source.clone();
                        if let Some(range) = self.structure_ui.root_range(&interpretation.name) {
                            self.hex_editor.select_range(range.clone(), true);
                            self.structure_ui.select_bytes(range);
                        }
                        self.structure_ui.expand_root(&interpretation.name);
                        self.authoring.created(interpretation.name);
                        self.author_in_structure = true;
                        self.interpretation_ready = true;
                    }
                }
                match outcome {
                    RunOutcome::Published => {
                        self.initial_plot_ready = initial_plot;
                        self.applied_language = Some(language);
                        self.applied_yaml = Some(source);
                        self.interpretation_stale = false;
                        self.set_draft_error(language, None);
                        self.plot_cancel.store(true, Ordering::Relaxed);
                        self.plot_rx = None;
                        self.plot_data = None;
                        self.refresh_at = None;
                        self.report(Ok(if self.showing_byte_preview() {
                            "Byte preview ready. Edit the decoder and press F5 to refine it.".into()
                        } else {
                            "Decoded. Select a sample or value to inspect and edit it.".into()
                        }));
                    }
                    RunOutcome::NoData => {
                        if language == DecoderLanguage::Python {
                            self.completed_python = Some(source);
                        }
                        self.set_draft_error(language, None);
                        self.report(Ok("Script finished.".into()));
                    }
                    RunOutcome::Failed(error) => {
                        if self.draft_source(language) == source {
                            self.set_draft_error(language, Some(error.clone()));
                        }
                        self.report(Err(anyhow::anyhow!(error)));
                    }
                    RunOutcome::Cancelled => {
                        self.set_draft_error(language, None);
                        self.report(Ok(if language == DecoderLanguage::Python {
                            "Script stopped."
                        } else {
                            "Decoding stopped."
                        }
                        .into()));
                    }
                }
                if !published && self.interpretation_stale && self.applied_yaml.is_some() {
                    self.reparse_at = Some(Instant::now() + Duration::from_millis(250));
                }
            }
        } else if was_decoding && !self.structure_ui.is_parsing() {
            // A background refresh belongs to the last successful decoder,
            // independently of whichever source is currently being edited.
            if let Some(language) = self.applied_language {
                if let Some(error) = self.structure_ui.decode_error().map(str::to_owned) {
                    self.set_draft_error(language, Some(error.clone()));
                    self.report(Err(anyhow::anyhow!(error)));
                } else if self.structure_ui.is_ready() {
                    self.interpretation_stale = false;
                    self.set_draft_error(language, None);
                    if !self.busy() {
                        self.report(Ok(
                            "Decoded. Select a sample or value to inspect and edit it.".into(),
                        ));
                    }
                }
            }
        }
        if self.structure_ui.take_decoded() {
            self.layout_applied = true;
        }
        if self.reparse_at.is_some_and(|time| Instant::now() >= time)
            && !self.busy()
            && !self.any_dialog_pending()
        {
            self.reparse_at = None;
            if self.structure_ui.has_published_program() {
                if let Some(store) = &self.bytes {
                    self.structure_ui.reparse(store.view(), ctx);
                }
            } else if let Some(yaml) = self.applied_yaml.clone() {
                // Revalidate the applied definition even when the editor has a newer draft.
                let draft = std::mem::replace(&mut self.yaml, yaml);
                let language = self.language;
                let draft_error = self.decode_error.take();
                self.language = DecoderLanguage::Yaml;
                self.validate_layout_for(ctx, true);
                let decode_error = self.decode_error.take();
                self.language = language;
                self.yaml = draft;
                self.decode_error = draft_error;
                self.set_draft_error(DecoderLanguage::Yaml, decode_error);
            }
        }
        if self.reparse_at.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }

        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && (self.unsaved() || self.busy() || self.any_dialog_pending())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.any_dialog_pending() {
                self.report(Ok("Finish or cancel the file dialog before closing.".into()));
            } else if self.busy() {
                self.report(Ok("Wait for file I/O to finish before closing.".into()));
            } else {
                self.pending = Some(Action::Close);
            }
        }
        self.dialogs(ctx);
    }

    fn replacement_prompt(language: DecoderLanguage) -> &'static str {
        match language {
            DecoderLanguage::Python => "Replace the Python draft and discard its unsaved code?",
            DecoderLanguage::Yaml => {
                "Replace the YAML draft and discard its unsaved source and field edits?"
            }
        }
    }

    fn dialogs(&mut self, ctx: &Context) {
        if let Some(action) = &self.pending {
            let (message, button) = match action {
                Action::Apply(..) => ("Decode with the new YAML and discard uncommitted cell text? Edited bytes and shared undo/redo history are kept.", "Decode and keep bytes"),
                Action::LoadYaml(path) => (Self::replacement_prompt(Self::language_for_path(path)), "Replace decoder"),
                Action::ReplaceYaml(_) => (Self::replacement_prompt(self.language), "Replace decoder"),
                Action::ExampleYaml(_) => (Self::replacement_prompt(DecoderLanguage::Yaml), "Replace decoder"),
                Action::ExamplePython(_) => (Self::replacement_prompt(DecoderLanguage::Python), "Replace decoder"),
                Action::NewDecoder(language) => (Self::replacement_prompt(*language), "Replace decoder"),
                _ => ("There are unsaved byte, decoder, or field edits. Discard them and continue?", "Discard and continue"),
            };
            let mut decision = None;
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0., 0.])
                .show(ctx, |ui| {
                    ui.set_max_width(430.);
                    ui.label(message);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            decision = Some(false);
                        }
                        if ui.button(button).clicked() {
                            decision = Some(true);
                        }
                    });
                });
            if let Some(proceed) = decision {
                let action = self.pending.take().unwrap();
                if proceed {
                    self.execute(action, ctx);
                }
            }
        }
        if let Some((path, _)) = &self.overwrite {
            let mut decision = None;
            egui::Window::new("Confirm overwrite")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0., 0.])
                .show(ctx, |ui| {
                    ui.label(format!("Replace existing file?\n{}", path.display()));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            decision = Some(false);
                        }
                        if ui.button("Overwrite this file").clicked() {
                            decision = Some(true);
                        }
                    });
                });
            if let Some(proceed) = decision {
                let (path, content) = self.overwrite.take().unwrap();
                if proceed {
                    self.save(ctx, path, content, true);
                }
            }
        }
    }

    fn finish_file_dialog(
        &mut self,
        action: FileDialogAction,
        path: Option<PathBuf>,
        ctx: &Context,
    ) {
        let Some(path) = path else {
            return;
        };
        match action {
            #[cfg(test)]
            FileDialogAction::Open => self.open(path, ctx),
            FileDialogAction::LoadYaml => self.load_yaml(path, ctx),
            FileDialogAction::Save(content) => {
                // Preserve the snapshot captured before opening the dialog, and
                // keep the existing overwrite and source-alias protections.
                if path.symlink_metadata().is_ok() {
                    self.overwrite = Some((path, content));
                } else {
                    self.save(ctx, path, content, false);
                }
            }
        }
    }

    fn save(&mut self, ctx: &Context, path: PathBuf, content: SaveContent, overwrite: bool) {
        self.report(Ok(format!("Saving {}…", path.display())));
        self.start_io(ctx, move || match content {
            SaveContent::Bytes(view, range, mark_saved) => {
                view.export(range.clone(), &path, overwrite)?;
                Ok(IoResult::SavedBytes(view, path, range, mark_saved))
            }
            SaveContent::Binary(bytes) => {
                binary::write_copy(&bytes, &path, overwrite)?;
                Ok(IoResult::SavedBinary(bytes, path))
            }
            SaveContent::Yaml(yaml, language) => {
                let bytes = ByteSnapshot {
                    original: Original::from_bytes(yaml.as_bytes())?,
                    patches: Default::default(),
                };
                binary::write_copy(&bytes, &path, overwrite)?;
                Ok(IoResult::SavedYaml(yaml, path, language))
            }
        });
    }

    /// Capture the current byte revision so an export includes the exact sparse edits
    /// visible when it was requested. Exporting part of a file does not mark it saved.
    fn selection_export(&self, range: Range<usize>) -> Result<SaveContent> {
        ensure!(
            !self.cell_dirty(),
            "Commit or cancel the table value before exporting bytes."
        );
        let store = self
            .bytes
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Open a file before exporting bytes."))?;
        let view = store.view();
        ensure!(
            range.start < range.end && range.end <= view.len(),
            "Select a nonempty byte range within the file."
        );
        view.validate()?;
        Ok(SaveContent::Bytes(view, range, false))
    }

    fn choose_export(&mut self, ctx: &Context, range: Range<usize>) {
        if !self.commands_available() {
            return;
        }
        let content = match self.selection_export(range.clone()) {
            Ok(content) => content,
            Err(error) => {
                self.report(Err(error));
                return;
            }
        };
        let stem = self
            .source
            .as_ref()
            .and_then(|path| path.file_stem())
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "selection".into());
        let dialog = self.file_locations.configure(
            FileKind::Exports,
            self.dialog_template
                .clone()
                .set_title(format!("Export {} selected bytes", range.len()))
                .set_file_name(format!("{stem}-0x{:X}-0x{:X}.bin", range.start, range.end)),
        );
        self.file_dialog
            .begin_save(dialog, FileDialogAction::Save(content), ctx);
    }

    fn plot_action(&mut self, action: PlotAction, ctx: &Context) {
        match action {
            PlotAction::InspectBytes(range) => {
                if !self
                    .bytes
                    .as_ref()
                    .is_some_and(|store| range.start < range.end && range.end <= store.view().len())
                {
                    self.report(Err(anyhow::anyhow!(
                        "The selected bytes are outside the open file."
                    )));
                    return;
                }
                self.structure_ui.select_bytes(range.clone());
                self.hex_editor.select_range(range.clone(), true);
                self.reveal_hex = true;
                ctx.request_repaint();
                self.report(Ok(format!(
                    "Selected {} bytes in Hex (0x{:X}..0x{:X}, end exclusive). Use Overwrite to edit or Selection → Extract selection to save this range.",
                    range.len(), range.start, range.end
                )));
            }
            PlotAction::ExportBytes(range) => self.choose_export(ctx, range),
        }
    }

    fn choose_save(&mut self, ctx: &Context, yaml: bool) {
        if !self.commands_available() || (yaml && !self.can_save_definition()) {
            return;
        }
        let content = if yaml {
            // A definition is also an editable document: save unfinished drafts verbatim.
            // Applying it performs syntax and byte-boundary validation.
            SaveContent::Yaml(self.yaml.clone(), self.language)
        } else {
            if self.cell_dirty() {
                self.report(Err(anyhow::anyhow!(
                    "Commit or cancel the table value before saving."
                )));
                return;
            }
            if let Some(store) = &self.bytes {
                let view = store.view();
                let len = view.len();
                SaveContent::Bytes(view, 0..len, true)
            } else {
                let Some(doc) = &self.document else {
                    return;
                };
                SaveContent::Binary(doc.bytes.clone())
            }
        };
        let suggested = if yaml {
            self.yaml_path
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    if self.is_python() {
                        "decoder.py".into()
                    } else {
                        "decoder.yaml".into()
                    }
                })
        } else {
            self.source
                .as_ref()
                .map(|path| {
                    format!(
                        "{}-edited.{}",
                        path.file_stem().unwrap_or_default().to_string_lossy(),
                        path.extension().unwrap_or_default().to_string_lossy()
                    )
                })
                .unwrap_or("sensors-edited.bin".into())
        };
        let kind = if yaml {
            Self::source_file_kind(self.language)
        } else {
            FileKind::Exports
        };
        let mut dialog = self
            .file_locations
            .configure(kind, self.dialog_template.clone().set_file_name(&suggested));
        if yaml {
            if let Some(directory) = self.decoder_save_directory() {
                dialog = dialog.set_directory(directory);
            }
        }
        self.file_dialog
            .begin_save(dialog, FileDialogAction::Save(content), ctx);
    }

    pub fn commands_available(&self) -> bool {
        !self.busy()
            && !self.any_dialog_pending()
            && self.pending.is_none()
            && self.overwrite.is_none()
    }
    pub fn has_selection(&self) -> bool {
        self.hex_editor.selection().is_some()
    }
    pub fn has_definition(&self) -> bool {
        has_decoder_code(&self.yaml)
    }
    pub fn can_apply_definition(&self) -> bool {
        self.has_file()
            && self.has_definition()
            && !self.structure_ui.is_parsing()
            && !self.active_authoring_dirty()
    }
    pub fn save_copy(&mut self, ctx: &Context) {
        if self.commands_available() {
            self.choose_save(ctx, false);
        }
    }
    pub fn can_save_definition(&self) -> bool {
        !self.yaml.trim().is_empty() && !self.active_authoring_dirty()
    }
    pub fn save_definition(&mut self, ctx: &Context) {
        if self.commands_available() && self.can_save_definition() {
            self.choose_save(ctx, true);
        }
    }
    pub fn apply_definition(&mut self, ctx: &Context) {
        if self.commands_available() && self.can_apply_definition() {
            self.validate_layout(ctx);
        }
    }
    pub fn choose_definition(&mut self, ctx: &Context) {
        if !self.commands_available() {
            return;
        }
        let mut dialog = self.dialog_template.clone().set_title("Open decoder");
        dialog = if self.is_python() {
            dialog.add_filter("Python", &["py"])
        } else {
            dialog.add_filter("YAML", &["yaml", "yml"])
        };
        let dialog = self.file_locations.configure(
            Self::source_file_kind(self.language),
            dialog.add_filter("All decoders", &["py", "yaml", "yml"]),
        );
        self.file_dialog
            .begin_open(dialog, FileDialogAction::LoadYaml, ctx);
    }
    pub fn load_definition_text(&mut self, yaml: &str, ctx: &Context) {
        self.request(Action::ExampleYaml(yaml.to_owned()), ctx);
    }
    pub fn load_python_text(&mut self, code: &str, ctx: &Context) {
        self.request(Action::ExamplePython(code.to_owned()), ctx);
    }
    /// Register only the built-in byte interpretation, independently of user drafts.
    pub fn start_byte_preview(&mut self, ctx: &Context) -> bool {
        if !self.has_file()
            || self.file_len() == 0
            || self.is_decoding()
            || self.applied_yaml.is_some()
        {
            return false;
        }
        let Some(bytes) = self.bytes.as_ref().map(ByteStore::view) else {
            return false;
        };
        for language in [DecoderLanguage::Yaml, DecoderLanguage::Python] {
            if language == DecoderLanguage::Yaml && self.structure_ui.authoring_dirty() {
                continue;
            }
            if language == self.language {
                if self.yaml_path.is_none()
                    && self.yaml == language.starter()
                    && self.saved_yaml == language.starter()
                {
                    self.yaml = language.byte_preview().into();
                    self.saved_yaml.clone_from(&self.yaml);
                }
            } else if self.other_draft.path.is_none()
                && self.other_draft.source == language.starter()
                && self.other_draft.saved == language.starter()
            {
                self.other_draft.source = language.byte_preview().into();
                self.other_draft.saved.clone_from(&self.other_draft.source);
            }
        }
        let yaml = DecoderLanguage::Yaml.byte_preview();
        self.submitted_decoder = Some((DecoderLanguage::Yaml, yaml.into()));
        self.initial_plot_pending = true;
        self.initial_plot_ready = false;
        if self.structure_ui.apply(yaml, bytes, ctx) {
            self.report(Ok("Preparing byte preview…".into()));
            true
        } else {
            self.initial_plot_pending = false;
            self.submitted_decoder = None;
        self.direct_pending = None;
            if let Some(error) = self.structure_ui.decode_error() {
                self.report(Err(anyhow::anyhow!(error.to_owned())));
            }
            false
        }
    }
    pub fn showing_byte_preview(&self) -> bool {
        self.decoded_ready()
            && self.applied_language.is_some_and(|language| {
                self.applied_yaml.as_deref() == Some(language.byte_preview())
            })
    }
    pub fn take_initial_plot_ready(&mut self) -> bool {
        std::mem::take(&mut self.initial_plot_ready)
    }

    pub fn new_python(&mut self, ctx: &Context) {
        self.request(Action::NewDecoder(DecoderLanguage::Python), ctx);
    }
    pub fn new_yaml(&mut self, ctx: &Context) {
        self.request(Action::NewDecoder(DecoderLanguage::Yaml), ctx);
    }
    pub fn show_steps(&mut self) {
        self.structure_ui.show_steps();
    }
    pub fn show_yaml(&mut self) {
        self.structure_ui.show_yaml();
    }
    pub fn open_decoder(&mut self) {
        self.show_yaml();
        self.reveal_definition = true;
    }
    pub fn decoded_ready(&self) -> bool {
        !self.interpretation_stale
            && (self.structure_ui.has_output()
                || (!self.structure_ui.has_published_program() && self.document.is_some()))
    }
    fn decoder_status(&self) -> &'static str {
        if self.initial_plot_pending {
            "Preparing byte preview…"
        } else if self.structure_ui.is_parsing() || self.decoding_layout {
            if self
                .submitted_decoder
                .as_ref()
                .is_some_and(|(language, _)| *language == DecoderLanguage::Python)
            {
                "Running script…"
            } else {
                "Decoding…"
            }
        } else if self.active_authoring_dirty() {
            "Finish editing fields before decoding"
        } else if self.decode_error.is_some() {
            if self.is_python() {
                "Script failed"
            } else {
                "Decode failed"
            }
        } else if self.is_python() && self.completed_python.as_ref() == Some(&self.yaml) {
            "Script finished"
        } else if !self.has_file() {
            "Open a data file to decode"
        } else if self.showing_byte_preview() {
            if self.yaml == self.language.byte_preview() {
                "Byte preview"
            } else {
                "Draft not applied · byte preview"
            }
        } else if self
            .applied_language
            .is_some_and(|language| language != self.language)
            && self.decoded_ready()
        {
            if self.is_python() {
                "Python not run · YAML results"
            } else {
                "YAML not decoded · Python results"
            }
        } else if !self.has_definition() {
            if self.is_python() {
                "New or open a Python decoder"
            } else {
                "Load or write YAML, then decode the file"
            }
        } else if self
            .applied_language
            .is_some_and(|language| language != self.language)
        {
            if self.is_python() {
                "Ready to run"
            } else {
                "Ready to decode"
            }
        } else if self
            .applied_yaml
            .as_ref()
            .is_some_and(|yaml| yaml != &self.yaml)
        {
            if self.is_python() {
                "Changes not run"
            } else {
                "Changes not decoded"
            }
        } else if self.decoded_ready()
            && self.applied_language == Some(self.language)
            && self.applied_yaml.as_ref() == Some(&self.yaml)
        {
            "Decoded"
        } else {
            if self.is_python() {
                "Ready to run"
            } else {
                "Ready to decode"
            }
        }
    }
    pub fn define_selection(&mut self, ctx: &Context) {
        if let Some(range) = self.hex_editor.selection() {
            self.hex_actions(vec![HexAction::Define(range)], ctx);
        }
    }
    pub fn jump_to_byte(&mut self) {
        self.hex_editor.show_jump();
    }
    pub fn find_bytes(&mut self) {
        self.hex_editor.show_find();
    }
    pub fn replace_selected_bytes(&mut self) {
        self.hex_editor.show_replace();
    }
    pub fn menu_history(&self) -> (bool,bool) { (self.bytes.as_ref().is_some_and(ByteStore::can_undo),self.bytes.as_ref().is_some_and(ByteStore::can_redo)) }
    pub fn menu_action(&mut self, command: crate::app_menu::Command, ctx: &Context) {
        use crate::app_menu::Command::*;
        if let Some(action)=command.editor_action() { self.show_yaml(); self.structure_ui.code_action(action); return; }
        match command {
            Undo => self.history(ctx,false), Redo => self.history(ctx,true),
            CopyHex => if let Some(bytes)=&self.bytes { self.hex_editor.copy_hex(&bytes.view()); },
            Extract => if let Some(range)=self.hex_editor.selection() { self.choose_export(ctx,range); },
            _ => {}
        }
    }
    #[cfg(test)]
    pub fn file_toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.add_enabled_ui(self.commands_available(), |ui| {
            if ui.button("Open file…").clicked() {
                self.file_dialog.begin_open(
                    self.file_locations
                        .configure(FileKind::Data, self.dialog_template.clone()),
                    FileDialogAction::Open,
                    &ctx,
                );
            }
            if ui.button("Open demo").clicked() {
                self.open_demo(&ctx);
            }
        });
    }

    #[cfg(test)]
    pub fn edit_toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let enabled = self.commands_available();
        ui.add_enabled_ui(enabled, |ui| {
            if ui
                .add_enabled(
                    self.bytes.as_ref().is_some_and(ByteStore::can_undo) && !self.cell_dirty(),
                    egui::Button::new("Undo"),
                )
                .clicked()
            {
                self.history(&ctx, false);
            }
            if ui
                .add_enabled(
                    self.bytes.as_ref().is_some_and(ByteStore::can_redo) && !self.cell_dirty(),
                    egui::Button::new("Redo"),
                )
                .clicked()
            {
                self.history(&ctx, true);
            }
            if ui
                .add_enabled(self.has_file(), egui::Button::new("Save edited copy…"))
                .clicked()
            {
                self.choose_save(&ctx, false);
            }
        });
    }

    pub fn select_language(&mut self, language: DecoderLanguage) {
        if self.commands_available() {
            self.switch_language(language);
        }
    }

    pub fn decoder_controls(&mut self, ui: &mut Ui) {
        self.controls_inner(ui, false);
    }

    #[cfg(test)]
    pub fn controls(&mut self, ui: &mut Ui) {
        self.controls_inner(ui, true);
    }

    fn controls_inner(&mut self, ui: &mut Ui, show_language: bool) {
        let ctx = ui.ctx().clone();
        let enabled = self.commands_available();
        ui.horizontal_wrapped(|ui| {
            if show_language {
                ui.add_enabled_ui(enabled, |ui| {
                    let mut language = self.language;
                    for (value, label) in [
                        (DecoderLanguage::Yaml, "YAML"),
                        (DecoderLanguage::Python, "Python"),
                    ] {
                        ui.selectable_value(&mut language, value, label)
                            .on_hover_text("Separate drafts; switching does not convert source.");
                    }
                    self.switch_language(language);
                });
            }
            let name = self
                .yaml_path
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    if self.saved_yaml == self.language.byte_preview() {
                        "Byte preview decoder".into()
                    } else if self.yaml_is_example {
                        "Example decoder".into()
                    } else {
                        "Untitled decoder".into()
                    }
                });
            let title = if self.yaml != self.saved_yaml || self.active_authoring_dirty() {
                format!("{name} · Modified")
            } else {
                name
            };
            let response = ui.strong(title);
            if let Some(path) = &self.yaml_path {
                response.on_hover_text(path.display().to_string());
            }
        });
        if show_language {
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("New").clicked() {
                    if self.is_python() {
                        self.new_python(&ctx);
                    } else {
                        self.new_yaml(&ctx);
                    }
                }
                if ui.button("Open decoder…").clicked() {
                    self.choose_definition(&ctx);
                }
                if ui
                    .add_enabled(
                        self.can_save_definition(),
                        egui::Button::new(if self.is_python() {
                            "Save Python…"
                        } else {
                            "Save YAML…"
                        }),
                    )
                    .on_hover_text("Save this decoder")
                    .clicked()
                {
                    self.choose_save(&ctx, true);
                }
                if ui
                    .add_enabled(
                        self.can_apply_definition(),
                        egui::Button::new(
                            RichText::new(if self.is_python() {
                                "Run script"
                            } else {
                                "Decode file"
                            })
                            .strong(),
                        )
                        .fill(ui.visuals().selection.bg_fill),
                    )
                    .on_hover_text("Run this decoder against the open file (F5)")
                    .clicked()
                {
                    self.apply_definition(&ctx);
                }
                if self.is_decoding() && ui.button("Stop").clicked() {
                    self.stop_decoding();
                }
            });
        });
        } else if let Some(command)=crate::app_menu::decoder_controls(ui,self.can_apply_definition() && enabled,self.is_decoding()) {
            match command { crate::app_menu::Command::Apply=>self.apply_definition(&ctx),crate::app_menu::Command::Cancel=>self.stop_decoding(),_=>{} }
        }
        ui.horizontal_wrapped(|ui| {
            if self.structure_ui.is_parsing() || self.decoding_layout {
                ui.spinner();
            }
            let color = if self.decode_error.is_some() {
                Color32::LIGHT_RED
            } else if matches!(self.decoder_status(), "Decoded" | "Script finished") {
                Color32::LIGHT_GREEN
            } else {
                ui.visuals().weak_text_color()
            };
            ui.colored_label(color, self.decoder_status());
            if self.active_authoring_dirty() && ui.button("Review changes").clicked() {
                self.structure_ui.show_authoring_draft();
            }
        });
        if let Some(progress) = &self.load_progress {
            let done = progress.done.load(Ordering::Relaxed);
            let total = progress.total.load(Ordering::Relaxed);
            ui.add(
                egui::ProgressBar::new(if total > 0 {
                    done as f32 / total as f32
                } else {
                    0.
                })
                .text(format!(
                    "{:.1} / {:.1} MiB",
                    done as f64 / 1048576.,
                    total as f64 / 1048576.
                )),
            );
            let label = if self.decoding_layout {
                "Cancel decoding"
            } else {
                "Cancel loading"
            };
            if ui.button(label).clicked() {
                progress.cancelled.store(true, Ordering::Relaxed);
            }
        }
        if self.error && self.decode_error.is_none() && self.structure_ui.decode_error().is_none() {
            ui.colored_label(Color32::LIGHT_RED, &self.message);
        }
        if !self.is_python()
            && (self.applied_language != Some(self.language)
                || self.structure_ui.decode_error().is_none())
        {
            if let Some(error) = &self.decode_error {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
        }
        if !self.structure_ui.has_published_program()
            && self.document.is_some()
            && !self.interpretation_stale
        {
            egui::CollapsingHeader::new("Record layout")
                .default_open(false)
                .show(ui, |ui| self.section_controls(ui));
        }
        ui.add_enabled_ui(enabled, |ui| {
            let previous = self.yaml.clone();
            let replacement = self.structure_ui.editor(
                ui,
                &mut self.yaml,
                self.hex_editor.selection(),
                self.language,
            );
            if self.yaml != previous {
                self.demo_yaml = false;
                self.decode_error = None;
            }
            if let Some(yaml) = replacement {
                // The builder extends the current draft; it never drops existing definitions.
                self.execute(Action::ReplaceYaml(yaml), &ctx);
                if self.structure_ui.take_run_requested() {
                    self.validate_layout(&ctx);
                }
            }
        });
    }

    fn select(&mut self, record: usize, field: usize) {
        self.select_field(record, field, true);
    }
    fn select_field(&mut self, record: usize, field: usize, sync_hex: bool) {
        if self.cell_dirty() {
            self.report(Err(anyhow::anyhow!(
                "Commit or cancel the table value before changing selection."
            )));
            return;
        }
        let Some(doc) = &self.document else {
            return;
        };
        if let Ok(text) = doc.text(record, field) {
            self.table_anchor = None;
            self.selected = doc.layout.identity(record, field).ok();
            self.edit_original = text.clone();
            self.edit_text = text;
            self.page = record / ROWS_PER_PAGE;
            if let Some(identity) = self.selected.filter(|_| sync_hex) {
                self.hex_editor.select_range(
                    identity.byte_offset..identity.byte_offset + identity.byte_length,
                    true,
                );
            }
            self.scroll_to_selection = true;
        }
        self.reveal_selected();
    }

    fn select_table_record(&mut self, record: usize, field: usize, whole: bool, extend: bool) {
        if self.cell_dirty() {
            self.report(Err(anyhow::anyhow!(
                "Commit or cancel the table value before changing selection."
            )));
            return;
        }
        let Some(doc) = &self.document else {
            return;
        };
        let anchor = if extend {
            self.table_anchor
                .or_else(|| {
                    let bytes = self.hex_editor.selection()?;
                    let start = doc.layout.record_offset(0).ok()?;
                    let end = doc.layout.record_offset(doc.records.checked_sub(1)?).ok()?
                        .checked_add(doc.layout.record_size)?;
                    (!bytes.is_empty() && bytes.start >= start && bytes.end <= end)
                        .then(|| (bytes.start - start) / doc.layout.record_size)
                })
                .or_else(|| self.selected.map(|value| value.record))
                .unwrap_or(record)
        } else {
            record
        };
        let range = if whole || extend {
            let (Ok(first), Ok(last)) = (
                doc.layout.record_offset(anchor.min(record)),
                doc.layout.record_offset(anchor.max(record)),
            ) else {
                return;
            };
            Some(first..last + doc.layout.record_size)
        } else {
            None
        };
        self.select_field(record, field, range.is_none());
        self.table_anchor = Some(anchor);
        if let Some(range) = range {
            self.hex_editor.select_range(range, true);
        }
    }

    fn history(&mut self, ctx: &Context, redo: bool) {
        if !self.commands_available() || self.cell_dirty() {
            return;
        }
        let Some(store) = &mut self.bytes else {
            return;
        };
        let result = if redo { store.redo() } else { store.undo() };
        match result {
            Ok(Some(range)) => {
                let reinterpret = self.interpretation_stale
                    || self.structure_ui.has_published_program()
                    || self.document.as_ref().is_some_and(|d| {
                        !d.structure.sections.iter().any(|section| {
                            let layout = &section.record;
                            layout.fields.iter().enumerate().any(|(field, _)| {
                                range.start >= layout.data_offset
                                    && layout.record_size > 0
                                    && layout
                                        .field_range(
                                            (range.start - layout.data_offset) / layout.record_size,
                                            field,
                                        )
                                        .is_ok_and(|r| r == range)
                            })
                        })
                    });
                self.after_byte_change(ctx, reinterpret);
                if !reinterpret {
                    self.edit_text.clone_from(&self.edit_original);
                    let old_section = self.document.as_ref().map(|d| d.section);
                    self.select_byte_range(range.clone(), true);
                    let changed_section = old_section != self.document.as_ref().map(|d| d.section);
                    self.update_selected_sample();
                    self.refresh_plot(ctx, changed_section);
                } else {
                    self.structure_ui.select_bytes(range.clone());
                    self.hex_editor.select_range(range, true);
                }
                self.report(Ok(if redo { "Edit redone." } else { "Edit undone." }.into()));
            }
            Ok(None) => (),
            Err(error) => self.report(Err(error)),
        }
    }

    fn commit(&mut self, ctx: &Context) {
        if !self.commands_available() {
            return;
        }
        if !self.cell_dirty() {
            self.report(Ok("Value unchanged; source bytes preserved.".into()));
            return;
        }
        let Some(ValueIdentity { record, field, .. }) = self.selected else {
            return;
        };
        let result = (|| -> Result<String> {
            let doc = self.document.as_ref().unwrap();
            let range = doc.layout.field_range(record, field)?;
            let values = doc.layout.encode(field, &self.edit_text)?;
            let store = self
                .bytes
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("No byte source is open"))?;
            store.replace(range.start, &values)?;
            let patches = (*store.view().patches).clone();
            let doc = self.document.as_mut().unwrap();
            doc.bytes.patches = patches;
            doc.text(record, field)
        })();
        match result {
            Ok(actual) => {
                self.after_byte_change(ctx, false);
                self.edit_text = actual.clone();
                self.edit_original = actual.clone();
                self.report(Ok(format!("Encoded and decoded value: {actual}")));
                self.update_selected_sample();
                self.refresh_plot(ctx, false);
            }
            Err(err) => self.report(Err(err)),
        }
    }

    fn update_selected_sample(&mut self) {
        if let (Some(doc), Some(data), Some(ValueIdentity { record, .. })) =
            (&self.document, &mut self.plot_data, self.selected)
        {
            data.samples.retain(|s| s.record != record);
            if let Ok(Some(sample)) = Sample::read(&doc.bytes, &doc.layout, record, self.x, self.y)
            {
                data.samples.push(sample);
                data.samples.sort_unstable_by_key(|s| s.record);
            }
        }
        self.reveal_selected();
    }

    fn reveal_selected(&mut self) {
        let (Some(doc), Some(ValueIdentity { record, .. }), Some(bounds)) =
            (&self.document, self.selected, self.plot_bounds)
        else {
            return;
        };
        let Ok(Some(sample)) = Sample::read(&doc.bytes, &doc.layout, record, self.x, self.y) else {
            return;
        };
        let mut min = bounds.min();
        let mut max = bounds.max();
        let mut changed = false;
        for axis in 0..2 {
            let value = sample.point[axis];
            let margin = ((max[axis] * 0.5 - min[axis] * 0.5).abs() * 0.1)
                .max(value.abs() * 1e-12)
                .max(1e-6);
            if value < min[axis] {
                min[axis] = if (value - margin).is_finite() {
                    value - margin
                } else {
                    value
                };
                changed = true;
            }
            if value > max[axis] {
                max[axis] = if (value + margin).is_finite() {
                    value + margin
                } else {
                    value
                };
                changed = true;
            }
        }
        if changed {
            self.navigation = Some(PlotBounds::from_min_max(min, max));
        }
    }

    #[cfg(test)]
    pub fn content(&mut self, ui: &mut Ui) {
        if self.document.is_none() {
            if self.has_file() {
                ui.label("File open. Inspect the bytes below; load or write YAML and click Decode file to plot and edit records.");
                self.hex(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open a binary file to inspect its bytes, or open the demo.")
                });
            }
            return;
        }
        egui::TopBottomPanel::bottom("binary_inspector")
            .resizable(true)
            .default_height(320.)
            .min_height(180.)
            .show_inside(ui, |ui| {
                egui::SidePanel::right("binary_hex")
                    .resizable(true)
                    .default_width(410.)
                    .min_width(260.)
                    .show_inside(ui, |ui| self.hex(ui));
                egui::CentralPanel::default().show_inside(ui, |ui| self.table(ui));
            });
        if self.document.as_ref().unwrap().layout.fields.is_empty() {
            ui.label("This section contains uninterpreted bytes; select a section with numeric fields to plot or edit.");
        } else {
            self.plot(ui);
        }
    }

    pub fn plot(&mut self, ui: &mut Ui) {
        if self.showing_byte_preview() {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Byte preview");
                ui.label("Unsigned 8-bit · Offset 0 · One byte per sample");
            });
        }
        if self.structure_ui.has_published_program() {
            if let Some(store) = &self.bytes {
                if let Some(range) = self.structure_ui.plot(ui, store.view()) {
                    self.hex_editor.select_range(range, true);
                }
            }
            if let Some(action) = self.structure_ui.take_plot_action() {
                self.plot_action(action, ui.ctx());
            }
            return;
        }
        if self.interpretation_stale {
            ui.label("Refreshing decoded data…");
            return;
        }
        if self.document.is_none() {
            ui.label("Run a decoder to plot its values.");
            return;
        }
        if self.document.as_ref().unwrap().layout.fields.is_empty() {
            ui.label(
                "This section has no numeric fields. Inspect it in Hex or select another section.",
            );
            return;
        }
        let ctx = ui.ctx().clone();
        let fields: Vec<_> = self
            .document
            .as_ref()
            .unwrap()
            .layout
            .fields
            .iter()
            .map(|f| f.name.clone())
            .collect();
        let previous = (self.x, self.y);
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_source("binary_x")
                .selected_text(format!(
                    "X: {}",
                    self.x.map(|i| fields[i].as_str()).unwrap_or("Record index")
                ))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.x, None, "Record index");
                    for (i, field) in fields.iter().enumerate() {
                        ui.selectable_value(&mut self.x, Some(i), field);
                    }
                });
            egui::ComboBox::from_id_source("binary_y")
                .selected_text(format!("Y: {}", fields[self.y]))
                .show_ui(ui, |ui| {
                    for (i, field) in fields.iter().enumerate() {
                        ui.selectable_value(&mut self.y, i, field);
                    }
                });
            ui.checkbox(&mut self.connect, "Connect records");
            if ui.button("Full range").clicked() {
                self.refresh_plot(&ctx, true);
            }
            if let (Some(bounds), Some(data)) = (self.plot_bounds, &self.plot_data) {
                let width = bounds.width();
                for (label, left) in [
                    ("Jump to start", data.full_x[0]),
                    ("Jump to end", data.full_x[1] - width),
                    ("<", bounds.min()[0] - width * 0.2),
                    (">", bounds.min()[0] + width * 0.2),
                ] {
                    if ui.button(label).clicked() {
                        self.navigation = Some(PlotBounds::from_min_max(
                            [left, bounds.min()[1]],
                            [left + width, bounds.max()[1]],
                        ));
                    }
                }
            }
        });
        if previous != (self.x, self.y) {
            self.connect = self.x.is_none();
            self.plot_data = None;
            self.plot_bounds = None;
            self.refresh_plot(&ctx, true);
        }
        if self.plot_rx.is_some() {
            ui.label("Updating plot… table and selected bytes remain available.");
        }
        if let Some(data) = &self.plot_data {
            ui.small(format!("{} finite records · {} displayed samples · click a point to inspect · drag to pan, scroll to zoom, right-drag to box zoom", data.finite_records, data.samples.len()));
        }
        let navigation = self.navigation.take();
        let selected_sample = self.selected.and_then(|identity| {
            let record = identity.record;
            let doc = self.document.as_ref()?;
            Sample::read(&doc.bytes, &doc.layout, record, self.x, self.y)
                .ok()
                .flatten()
        });
        let response = Plot::new("binary_plot")
            .height(ui.available_height().max(100.))
            .allow_drag(true)
            .allow_zoom(true)
            .allow_scroll(true)
            .allow_boxed_zoom(true)
            .show(ui, |plot_ui| {
                if let Some(bounds) = navigation {
                    plot_ui.set_plot_bounds(bounds);
                    plot_ui.set_auto_bounds(egui::Vec2b::FALSE);
                }
                if let Some(data) = &self.plot_data {
                    let points: Vec<_> = data.samples.iter().map(|s| s.point).collect();
                    if self.connect {
                        plot_ui.line(
                            Line::new(points.clone())
                                .color(Color32::from_rgb(70, 165, 220))
                                .name(&fields[self.y]),
                        );
                    }
                    plot_ui.points(
                        Points::new(points)
                            .radius(2.5_f32)
                            .color(Color32::from_rgb(70, 165, 220))
                            .name(&fields[self.y]),
                    );
                }
                if let Some(sample) = selected_sample {
                    plot_ui.points(
                        Points::new(vec![sample.point])
                            .radius(7.0_f32)
                            .color(Color32::YELLOW)
                            .name(format!(
                                "record {} @ 0x{:X}",
                                sample.record, sample.byte_offset
                            )),
                    );
                }
            });
        if response.response.clicked() {
            if let Some(pointer) = response.response.interact_pointer_pos() {
                // Screen-space hit testing returns the sample's identity, never a search by X.
                let nearest = self
                    .plot_data
                    .as_ref()
                    .into_iter()
                    .flat_map(|d| d.samples.iter())
                    .copied()
                    .chain(selected_sample)
                    .map(|sample| {
                        let screen = response
                            .transform
                            .position_from_point(&PlotPoint::from(sample.point));
                        (screen.distance_sq(pointer), sample)
                    })
                    .filter(|(distance, _)| *distance <= 144.)
                    .min_by(|a, b| a.0.total_cmp(&b.0));
                if let Some((_, sample)) = nearest {
                    let doc = self.document.as_ref().unwrap();
                    if let Some(identity) = self
                        .plot_data
                        .as_ref()
                        .and_then(|data| data.identities(&sample, &doc.layout).ok().map(|(_, y)| y))
                    {
                        self.select(identity.record, identity.field);
                    }
                }
            }
        }
        let bounds = *response.transform.bounds();
        #[cfg(test)]
        {
            self.last_transform = Some(response.transform);
        }
        let changed = self
            .plot_bounds
            .is_some_and(|old| old.min()[0] != bounds.min()[0] || old.max()[0] != bounds.max()[0]);
        self.plot_bounds = Some(bounds);
        if changed {
            self.refresh_at = Some(Instant::now() + Duration::from_millis(150));
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    pub fn table(&mut self, ui: &mut Ui) {
        if self.structure_ui.has_published_program() {
            if let Some(store) = &self.bytes {
                if let Some(range) = self.structure_ui.table(ui, store.view()) {
                    self.hex_editor.select_range(range, true);
                }
            }
            return;
        }
        if self.interpretation_stale {
            ui.label("Refreshing decoded data…");
            return;
        }
        if self.document.is_none() {
            ui.label("Run a decoder to view its samples.");
            return;
        }
        let mut section = self.document.as_ref().unwrap().section;
        ui.push_id("table_section", |ui| {
            egui::ComboBox::from_id_source("decoded_section")
                .selected_text(&self.document.as_ref().unwrap().layout.name)
                .show_ui(ui, |ui| {
                    for (i, entry) in self
                        .document
                        .as_ref()
                        .unwrap()
                        .structure
                        .sections
                        .iter()
                        .enumerate()
                    {
                        ui.selectable_value(
                            &mut section,
                            i,
                            format!("{} · {} samples", entry.record.name, entry.record.records),
                        );
                    }
                });
        });
        if section != self.document.as_ref().unwrap().section {
            self.change_section(section, ui.ctx());
        }
        ui.heading(format!(
            "Decoded samples · {}",
            self.document.as_ref().unwrap().layout.name
        ));
        let ctx = ui.ctx().clone();
        let doc = self.document.as_ref().unwrap();
        let pages = doc.records.div_ceil(ROWS_PER_PAGE).max(1);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(self.page > 0, egui::Button::new("Previous"))
                .clicked()
            {
                self.page -= 1;
            }
            let mut page = self.page + 1;
            if ui
                .add(
                    egui::DragValue::new(&mut page)
                        .clamp_range(1..=pages)
                        .prefix("Page "),
                )
                .changed()
            {
                self.page = page - 1;
            }
            ui.label(format!("/ {pages}"));
            if ui
                .add_enabled(self.page + 1 < pages, egui::Button::new("Next"))
                .clicked()
            {
                self.page += 1;
            }
        });
        // Table editing stays in this panel, and always shows the actual decoded value after commit.
        let enabled = self.commands_available();
        let mut commit = false;
        if let Some(ValueIdentity { record, field, .. }) = self.selected {
            ui.label(format!(
                "Edit record {record} / {} (displayed value)",
                doc.layout.fields[field].name
            ));
            ui.add_enabled_ui(enabled, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut self.edit_text).desired_width(165.));
                    commit = ui.button("Commit value").clicked()
                        || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if ui.button("Cancel edit").clicked() {
                        self.edit_text.clone_from(&self.edit_original);
                    }
                });
            });
        } else {
            ui.small("Click a cell to select its record and edit its value here.");
        }
        let mut selection = None;
        let selected_bytes = self.hex_editor.selection();
        egui::ScrollArea::both()
            .id_source("record_table")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("binary_table_grid")
                    .striped(true)
                    .min_col_width(95.)
                    .show(ui, |ui| {
                        ui.strong("Record / offset");
                        for field in &doc.layout.fields {
                            ui.strong(&field.name);
                        }
                        ui.end_row();
                        for record in self.page * ROWS_PER_PAGE
                            ..((self.page + 1) * ROWS_PER_PAGE).min(doc.records)
                        {
                            let offset = doc.layout.record_offset(record).unwrap();
                            let selected_row = selected_bytes.as_ref().is_some_and(|range| {
                                range.start < offset + doc.layout.record_size && offset < range.end
                            });
                            let response = ui
                                .selectable_label(selected_row, format!("{record} / 0x{offset:X}"));
                            if response.clicked() {
                                selection = Some((
                                    record,
                                    self.selected.map(|s| s.field).unwrap_or(self.y),
                                    true,
                                ));
                            }
                            if self.selected.is_some_and(|s| s.record == record) && self.scroll_to_selection {
                                response.scroll_to_me(Some(egui::Align::Center));
                                self.scroll_to_selection = false;
                            }
                            for field in 0..doc.layout.fields.len() {
                                let text =
                                    doc.text(record, field).unwrap_or_else(|e| e.to_string());
                                let modified = doc.modified(record, field);
                                let label = if modified {
                                    RichText::new(format!("{text} *"))
                                        .color(Color32::from_rgb(255, 175, 70))
                                } else {
                                    RichText::new(text)
                                };
                                let response = ui
                                    .selectable_label(
                                        selected_row
                                            && self.selected.is_some_and(|s| s.record == record && s.field == field),
                                        label,
                                    )
                                    .on_hover_text(format!(
                                        "{} · {:?} · scale {}{}",
                                        doc.layout.fields[field].name,
                                        doc.layout.fields[field].kind,
                                        doc.layout.fields[field].scale,
                                        if modified {
                                            " · changed from original bytes"
                                        } else {
                                            ""
                                        }
                                    ));
                                if response.clicked() {
                                    selection = Some((record, field, false));
                                }
                            }
                            ui.end_row();
                        }
                    });
            });
        if commit {
            self.commit(&ctx);
        }
        if let Some((record, field, whole)) = selection {
            self.select_table_record(record, field, whole, ui.input(|i| i.modifiers.shift));
        }
    }

    pub fn byte_selection(&self) -> Option<Range<usize>> {
        self.hex_editor.selection()
    }

    pub fn select_source_range(&mut self, range: Range<usize>, reveal: bool) {
        if range.start <= range.end && range.end <= self.file_len() {
            self.hex_editor.select_range(range, reveal);
        }
    }

    pub fn source_selection_inspector(&mut self, ui: &mut Ui) {
        self.hex_editor.set_python_mode(self.is_python());
        let source = self.bytes.as_ref().map(ByteStore::view);
        let actions = self.hex_editor.ui_inspector(ui, source);
        self.hex_actions(actions, ui.ctx());
    }

    pub fn hex_with_source_highlights(&mut self, ui: &mut Ui, highlights: &[HexHighlight]) {
        self.hex_editor.set_python_mode(self.is_python());
        let source = self.bytes.as_ref().map(ByteStore::view);
        let actions = self.hex_editor.ui(ui, source, highlights);
        self.hex_actions(actions, ui.ctx());
    }

    fn direct_controls(&mut self, ui: &mut Ui, in_structure: bool) {
        if in_structure != self.author_in_structure { return; }
        if self.is_python() { return; }
        let range = self.hex_editor.selection();
        let preview = range.clone().and_then(|r| self.hex_editor.interpretation_preview(r));
        let placeholder = self.yaml_path.is_none() && !self.yaml_is_example && self.yaml == self.saved_yaml
            && ((self.yaml == DecoderLanguage::Yaml.starter() && self.applied_yaml.is_none())
                || self.yaml == DecoderLanguage::Yaml.byte_preview());
        let source = if placeholder { "endian: little\nactions: []\n" } else { &self.yaml };
        let enabled = self.commands_available() && !self.is_decoding() && !self.active_authoring_dirty() && !self.cell_dirty();
        let size = self.file_len() as u64;
        let mut edit = None;
        ui.add_enabled_ui(enabled, |ui| {
            edit = self.authoring.ui(ui, source, range.map(|r| r.start as u64..r.end as u64), preview.as_deref(), size);
        });
        if let Some(error) = self.decode_error.as_deref().or_else(|| self.structure_ui.decode_error()) {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        match edit {
            Some(crate::direct_authoring::Edit::Commit(source)) => {
                self.yaml = source;
                let empty = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&self.yaml).ok().is_some_and(|v| v["actions"].as_sequence().is_some_and(|a| a.is_empty()));
                if empty { self.structure_ui.clear_program(); self.applied_yaml = None; self.submitted_decoder = None; }
                else { self.validate_layout(ui.ctx()); self.structure_ui.keep_navigation(); }
            }
            Some(crate::direct_authoring::Edit::Interpret(source, interpretation)) => {
                if let Some(bytes) = &self.bytes {
                    self.direct_pending = Some((self.yaml.clone(), interpretation.clone()));
                    self.submitted_decoder = Some((DecoderLanguage::Yaml, source.clone()));
                    self.structure_ui.apply_interpretation(&source, bytes.view(), ui.ctx(), interpretation);
                    self.structure_ui.keep_navigation();
                }
            }
            Some(crate::direct_authoring::Edit::Code) => { self.show_yaml(); self.reveal_definition = true; }
            None => {}
        }
    }

    pub fn hex(&mut self, ui: &mut Ui) {
        self.direct_controls(ui, false);
        self.hex_editor.set_python_mode(self.is_python());
        let mut highlights = self.structure_ui.highlights();
        if !self.interpretation_stale && !self.structure_ui.has_published_program() {
            if let Some(doc) = &self.document {
                if let Some(selected) = self.selected {
                    if let Ok(start) = doc.layout.record_offset(selected.record) {
                        for (i, field) in doc.layout.fields.iter().enumerate() {
                            let range =
                                start + field.offset..start + field.offset + field.kind.size();
                            highlights.push(HexHighlight {
                                range,
                                name: format!(
                                    "{}[{}].{}",
                                    doc.layout.name, selected.record, field.name
                                ),
                                color: field_color(i),
                            });
                        }
                    }
                }
            }
        }
        let source = self.bytes.as_ref().map(ByteStore::view);
        let actions = self.hex_editor.ui(ui, source, &highlights);
        self.hex_actions(actions, ui.ctx());
    }

    fn hex_actions(&mut self, actions: Vec<HexAction>, ctx: &Context) {
        for action in actions {
            match action {
                HexAction::Replace { offset, bytes } => {
                    if !self.commands_available() {
                        continue;
                    }
                    if self.cell_dirty() {
                        self.report(Err(anyhow::anyhow!(
                            "Commit or cancel the table value before editing bytes."
                        )));
                        continue;
                    }
                    if let Some(store) = &mut self.bytes {
                        match store.replace(offset, &bytes) {
                            Ok(_) => {
                                self.after_byte_change(ctx, true);
                                self.report(Ok(
                                    "Bytes replaced. Undo restores their previous values.".into(),
                                ));
                            }
                            Err(error) => self.report(Err(error)),
                        }
                    }
                }
                HexAction::Undo => self.history(ctx, false),
                HexAction::Redo => self.history(ctx, true),
                HexAction::Define(range) => {
                    if self.is_python() {
                        ctx.output_mut(|output| {
                            output.copied_text =
                                format!("file.read_at(0x{:X}, {})", range.start, range.len())
                        });
                        self.show_yaml();
                        self.reveal_definition = true;
                        continue;
                    }
                    if self.structure_ui.authoring_dirty() {
                        self.structure_ui.show_authoring_draft();
                    } else {
                        // An untouched, unapplied starter is a placeholder when
                        // the user chooses to describe their own selected bytes.
                        if self.yaml == DecoderLanguage::Yaml.starter()
                            && self.saved_yaml == self.yaml
                            && self.yaml_path.is_none()
                            && !self.yaml_is_example
                            && !self.structure_ui.has_published_program()
                        {
                            self.yaml.clear();
                            self.saved_yaml.clear();
                        }
                        self.structure_ui.begin_definition(range);
                    }
                    self.reveal_definition = true;
                }
                HexAction::Extract(range) => self.choose_export(ctx, range),
                HexAction::Selection(range) => {
                    self.author_in_structure = false;
                    self.structure_ui.select_bytes(range.clone());
                    if !self.interpretation_stale && !self.structure_ui.has_published_program() {
                        self.select_byte_range(range, false);
                    }
                }
            }
        }
    }

    fn after_byte_change(&mut self, ctx: &Context, reinterpret: bool) {
        let restart_preview = std::mem::take(&mut self.initial_plot_pending);
        self.initial_plot_ready = false;
        self.submitted_decoder = None;
        self.direct_pending = None;
        self.completed_python = None;
        self.bytes_changed = true;
        if let (Some(doc), Some(store)) = (&mut self.document, &self.bytes) {
            doc.bytes.patches = (*store.view().patches).clone();
        }
        self.structure_ui.invalidate();
        if reinterpret || self.structure_ui.has_published_program() {
            self.interpretation_stale = true;
            self.plot_cancel.store(true, Ordering::Relaxed);
            self.plot_rx = None;
            self.plot_data = None;
            self.reparse_at = Some(Instant::now() + Duration::from_millis(250));
        }
        if restart_preview {
            self.start_byte_preview(ctx);
        }
        ctx.request_repaint();
    }

    fn select_byte_range(&mut self, range: Range<usize>, reveal: bool) {
        self.table_anchor = None;
        let Some(doc) = &mut self.document else {
            return;
        };
        let mut found = None;
        for (section, entry) in doc.structure.sections.iter().enumerate() {
            let layout = &entry.record;
            if layout.record_size == 0
                || range.start < layout.data_offset
                || range.start >= entry.physical.end
            {
                continue;
            }
            let record = (range.start - layout.data_offset) / layout.record_size;
            if record >= layout.records {
                continue;
            }
            for (field, _) in layout.fields.iter().enumerate() {
                if layout
                    .field_range(record, field)
                    .is_ok_and(|r| r.start <= range.start && r.end >= range.end)
                {
                    found = Some((section, record, field));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        if let Some((section, record, field)) = found {
            if section != doc.section {
                let _ = doc.select_section(section);
                self.reset_section_view();
            }
            if !self.cell_dirty() {
                self.select_field(record, field, false);
            }
        } else if !self.cell_dirty() {
            self.selected = None;
            self.edit_text.clear();
            self.edit_original.clear();
        }
        if reveal {
            self.hex_editor.select_range(range, true);
        }
    }

    pub fn structures(&mut self, ui: &mut Ui) {
        self.direct_controls(ui, true);
        if self.structure_ui.has_published_program() {
            let mut edit = None;
            let source = &self.yaml;
            let authoring = &mut self.authoring;
            if let Some(range) = self.structure_ui.tree(ui, self.hex_editor.selection(), &mut |ui, node| {
                let (response, change) = if node.path == node.name { authoring.name_ui(ui, source, &node.name, node.range.start as u64..node.range.end as u64) } else { (ui.selectable_label(false, &node.name), None) };
                if change.is_some() { edit = change; }
                response
            }) {
                self.author_in_structure = true;
                self.hex_editor.select_range(range, true);
            }
            if let Some(crate::direct_authoring::Edit::Commit(source)) = edit {
                self.yaml = source; self.validate_layout(ui.ctx()); self.structure_ui.keep_navigation();
            }
        } else if let Some(doc) = &self.document {
            ui.label("Decoded structure");
            if self.interpretation_stale {
                ui.colored_label(Color32::YELLOW, "Revalidating changed bytes…");
                return;
            }
            let mut selected = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (index, section) in doc.structure.sections.iter().enumerate() {
                    let title = format!(
                        "{} · {} records",
                        section.record.name, section.record.records
                    );
                    if ui.selectable_label(index == doc.section, title).clicked() {
                        selected = Some((index, section.physical.clone()));
                    }
                }
            });
            if let Some((index, range)) = selected {
                self.change_section(index, ui.ctx());
                self.hex_editor.select_range(range, true);
            }
        } else {
            ui.strong("No decoded structure yet");
            ui.weak("Select bytes in Hex, choose an interpretation, and name a field.");
            if ui.button("Open decoder").clicked() {
                self.open_decoder();
            }
        }
    }

    fn prepare_value_edit(&mut self, node: &Node) {
        let Some(revision) = self.bytes.as_ref().map(|store| store.view().revision) else {
            return;
        };
        if self.generic_value_edit.as_ref().is_none_or(|edit| {
            edit.node.path != node.path
                || edit.node.range != node.range
                || edit.node.kind != node.kind
                || edit.node.endian != node.endian
                || edit.revision != revision
        }) {
            let value = node.display();
            self.generic_value_edit = Some(GenericValueEdit {
                node: node.clone(),
                revision,
                original: value.clone(),
                text: value,
                error: None,
            });
        }
    }

    fn commit_value_edit(&mut self, ctx: &Context) {
        if !self.commands_available() || self.cell_dirty() {
            return;
        }
        let Some(edit) = self.generic_value_edit.as_ref() else {
            return;
        };
        let node = edit.node.clone();
        let revision = edit.revision;
        let text = edit.text.clone();
        let result = (|| -> Result<String> {
            let selected = self
                .structure_ui
                .selected_node()
                .ok_or_else(|| anyhow::anyhow!("Wait for this value to finish decoding."))?;
            ensure!(
                selected.path == node.path
                    && selected.range == node.range
                    && selected.kind == node.kind
                    && selected.endian == node.endian,
                "The selected field changed. Select it again before editing."
            );
            let store = self
                .bytes
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("Open a file before editing."))?;
            ensure!(
                store.view().revision == revision,
                "Bytes changed. Wait for the value to refresh before editing."
            );
            let (bytes, displayed) = encode_value(&node, &text)?;
            store.replace(node.range.start, &bytes)?;
            Ok(displayed)
        })();
        match result {
            Ok(displayed) => {
                self.generic_value_edit = None;
                self.after_byte_change(ctx, true);
                self.structure_ui.select_bytes(node.range.clone());
                self.hex_editor.select_range(node.range.clone(), true);
                self.report(Ok(format!(
                    "{} set to {}. Updated {} bytes; Undo restores the previous value.",
                    node.name,
                    displayed,
                    node.range.len()
                )));
            }
            Err(error) => {
                let message = format!("{error:#}");
                if let Some(edit) = &mut self.generic_value_edit {
                    edit.error = Some(message.clone());
                }
                self.report(Err(anyhow::anyhow!(message)));
            }
        }
    }

    pub fn selection_inspector(&mut self, ui: &mut Ui) {
        self.hex_editor.set_python_mode(self.is_python());
        let node = self.structure_ui.selected_node();
        if let Some(node) = node {
            ui.heading(&node.name);
            let sample = self.structure_ui.selected_sample();
            if let Some((index, _)) = &sample {
                ui.weak(format!("Sample {index}"));
            }
            if editable_value(&node) {
                self.prepare_value_edit(&node);
                let enabled = self.commands_available() && !self.cell_dirty();
                let mut apply = false;
                ui.label("Value");
                if let Some(edit) = &mut self.generic_value_edit {
                    ui.add_enabled_ui(enabled, |ui| {
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut edit.text).desired_width(f32::INFINITY),
                        );
                        let changed = edit.text != edit.original;
                        ui.horizontal_wrapped(|ui| {
                            apply = ui
                                .add_enabled(changed, egui::Button::new("Apply value"))
                                .clicked()
                                || (changed
                                    && response.lost_focus()
                                    && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                            if changed && ui.button("Cancel").clicked() {
                                edit.text.clone_from(&edit.original);
                                edit.error = None;
                            }
                        });
                        if response.has_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Escape))
                        {
                            edit.text.clone_from(&edit.original);
                            edit.error = None;
                        }
                    });
                    if let Some(error) = &edit.error {
                        ui.colored_label(Color32::LIGHT_RED, error);
                    }
                }
                if self
                    .hex_editor
                    .selection()
                    .is_some_and(|selection| selection != node.range)
                {
                    ui.small(format!(
                        "Value uses the full {}-byte field.",
                        node.range.len()
                    ));
                }
                if apply {
                    self.commit_value_edit(ui.ctx());
                }
            } else {
                self.generic_value_edit = None;
                ui.label(node.display());
                if node.value.is_some() {
                    ui.weak("Edit this field in Hex.");
                }
            }
            let enabled = self.commands_available();
            ui.add_enabled_ui(enabled, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Show field bytes").clicked() {
                        self.plot_action(PlotAction::InspectBytes(node.range.clone()), ui.ctx());
                    }
                    if ui.button("Export field…").clicked() {
                        self.choose_export(ui.ctx(), node.range.clone());
                    }
                });
                if let Some((index, range)) = sample {
                    ui.menu_button("Sample actions", |ui| {
                        if ui
                            .button(format!("Select all bytes of sample {index}"))
                            .clicked()
                        {
                            self.plot_action(PlotAction::InspectBytes(range.clone()), ui.ctx());
                            ui.close_menu();
                        }
                        if ui.button("Export whole sample…").clicked() {
                            self.choose_export(ui.ctx(), range.clone());
                            ui.close_menu();
                        }
                    });
                }
            });
            ui.add_space(8.);
            egui::CollapsingHeader::new("Technical details").show(ui, |ui| {
                self.structure_ui.inspector(ui, self.language);
                if let Some(selected) = self.hex_editor.selection() {
                    if selected != node.range {
                        ui.label(format!(
                            "{} selected byte(s) within {}: 0x{:X}..0x{:X}",
                            selected.len(),
                            node.name,
                            selected.start,
                            selected.end
                        ));
                    }
                }
            });
            egui::CollapsingHeader::new("Raw bytes").show(ui, |ui| {
                let source = self.bytes.as_ref().map(ByteStore::view);
                let actions = self.hex_editor.ui_inspector(ui, source);
                self.hex_actions(actions, ui.ctx());
            });
        } else {
            self.generic_value_edit = None;
            if self.structure_ui.is_parsing() {
                ui.weak("Refreshing the selected value…");
            }
            let source = self.bytes.as_ref().map(ByteStore::view);
            let actions = self.hex_editor.ui_inspector(ui, source);
            self.hex_actions(actions, ui.ctx());
        }
    }
}

#[cfg(test)]
fn hex_offset(text: &str, scope: &std::ops::Range<usize>) -> Result<usize> {
    let text = text.trim();
    let offset = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        usize::from_str_radix(hex, 16)
    } else {
        text.parse::<usize>()
    }
    .map_err(|_| anyhow::anyhow!("Enter a decimal byte offset or hexadecimal with a 0x prefix."))?;
    ensure!(
        scope.contains(&offset),
        "Offset is outside this region (0x{:X}..0x{:X}, end exclusive).",
        scope.start,
        scope.end
    );
    Ok(offset)
}

fn field_color(index: usize) -> Color32 {
    const COLORS: [[u8; 3]; 6] = [
        [80, 160, 220],
        [100, 190, 145],
        [205, 165, 90],
        [170, 135, 220],
        [215, 125, 155],
        [90, 185, 195],
    ];
    let c = COLORS[index % COLORS.len()];
    Color32::from_rgb(c[0], c[1], c[2])
}

fn data_bounds(data: &PlotData) -> PlotBounds {
    fn pad([min, max]: [f64; 2]) -> [f64; 2] {
        let margin = ((max * 0.5 - min * 0.5).abs() * 0.08)
            .max(min.abs() * 1e-12)
            .max(1e-6);
        let low = min - margin;
        let high = max + margin;
        [
            if low.is_finite() { low } else { min },
            if high.is_finite() { high } else { max },
        ]
    }
    let x = pad(data.full_x);
    let y = pad(data.full_y);
    PlotBounds::from_min_max([x[0], y[0]], [x[1], y[1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_simple_demo_opens_and_decodes_without_a_path_or_manual_apply() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_simple_demo(&ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.file_len(), 2048);
        assert!(workspace.source_path().is_none());
        assert!(workspace.decoded_ready(), "{}", workspace.message);
        assert_eq!(workspace.yaml, DecoderLanguage::Yaml.starter());
        assert!(!workspace.is_dirty());
        assert!(workspace.take_layout_applied());
    }

    #[test]
    fn byte_preview_small_files_are_linked_and_both_starters_decode_the_same_values() {
        fn numeric_leaves(nodes: &[Node]) -> Vec<(Range<usize>, f64)> {
            nodes
                .iter()
                .flat_map(|node| {
                    node.numeric()
                        .map(|value| vec![(node.range.clone(), value)])
                        .unwrap_or_else(|| numeric_leaves(&node.children))
                })
                .collect()
        }
        let ctx = Context::default();
        let directory = tempfile::tempdir().unwrap();
        for length in [0, 1, 128, 129] {
            let path = directory.path().join(format!("bytes-{length}.raw"));
            let expected: Vec<u8> = (0..length).map(|index| (index * 37) as u8).collect();
            std::fs::write(&path, &expected).unwrap();
            let mut workspace = BinaryWorkspace::default();
            workspace.open(path, &ctx);
            settle(&mut workspace, &ctx);
            assert_eq!(workspace.start_byte_preview(&ctx), length != 0);
            settle(&mut workspace, &ctx);
            assert!(
                workspace.original.is_none(),
                "Preview must use the original file directly"
            );
            assert!(workspace.document.is_none());
            assert!(
                !workspace.is_dirty(),
                "Generated starters are a clean baseline"
            );
            if length == 0 {
                assert!(!workspace.decoded_ready());
                assert!(!workspace.take_initial_plot_ready());
                continue;
            }
            assert!(workspace.showing_byte_preview(), "{}", workspace.message);
            assert!(workspace.take_initial_plot_ready());
            assert!(
                !workspace.take_initial_plot_ready(),
                "Initial navigation is a one-shot event"
            );
            assert!(
                !workspace.start_byte_preview(&ctx),
                "Do not replace an existing result"
            );
            assert_eq!(workspace.language, DecoderLanguage::Yaml);
            assert_eq!(workspace.yaml, DecoderLanguage::Yaml.byte_preview());
            assert_eq!(
                workspace.other_draft.source,
                DecoderLanguage::Python.byte_preview()
            );
            let source = workspace.bytes.as_ref().unwrap().view();
            let cancel = AtomicBool::new(false);
            let yaml = crate::structure::Program::parse(&workspace.yaml)
                .unwrap()
                .run(&source, &cancel)
                .unwrap();
            let python =
                crate::python_decoder::run(&workspace.other_draft.source, None, &source, &cancel)
                    .unwrap()
                    .decoded
                    .unwrap()
                    .1;
            for output in [yaml, python] {
                assert_eq!(output.consumed, length);
                let collection = &output.nodes[0];
                assert_eq!(collection.range, 0..length);
                let rows = if let Some(array) = &collection.lazy {
                    assert_eq!(array.count, length);
                    assert_eq!(array.stride, 1);
                    array.page(&source, &cancel, 0, length).unwrap()
                } else {
                    collection.children.clone()
                };
                assert_eq!(
                    numeric_leaves(&rows),
                    expected
                        .iter()
                        .enumerate()
                        .map(|(index, value)| (index..index + 1, f64::from(*value)))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, target_pointer_width = "64"))]
    fn byte_preview_large_sparse_source_stays_lazy_and_can_reach_its_last_byte() {
        use std::io::{Seek, SeekFrom, Write};
        let ctx = Context::default();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large-source.raw");
        let length = 40_usize * 1024 * 1024 * 1024;
        let mut file = std::fs::File::create(&path).unwrap();
        crate::cache::set_fixture_len(&file, length as u64).unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(&[231]).unwrap();
        drop(file);
        let mut workspace = BinaryWorkspace::default();
        workspace.open(path, &ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.start_byte_preview(&ctx));
        settle(&mut workspace, &ctx);
        assert!(workspace.showing_byte_preview(), "{}", workspace.message);
        assert_eq!(workspace.file_len(), length);
        assert!(workspace.original.is_none());
        assert!(workspace.raw_source.is_some());
        assert!(workspace.document.is_none());
        let source = workspace.bytes.as_ref().unwrap().view();
        let cancel = AtomicBool::new(false);
        let output = crate::structure::Program::parse(workspace.applied_yaml.as_ref().unwrap())
            .unwrap()
            .run(&source, &cancel)
            .unwrap();
        assert_eq!(output.nodes.len(), 1);
        let collection = &output.nodes[0];
        assert!(
            collection.children.is_empty(),
            "Do not materialize billions of byte nodes"
        );
        let array = collection.lazy.as_ref().unwrap();
        assert_eq!(array.count, length);
        assert_eq!(array.stride, 1);
        let rows = array.page(&source, &cancel, length - 1, 1).unwrap();
        assert_eq!(rows[0].range, length - 1..length);
        assert_eq!(rows[0].children[0].numeric(), Some(231.));
    }

    #[test]
    fn byte_preview_preserves_loaded_edited_cleared_and_inactive_decoder_documents() {
        let ctx = Context::default();
        let directory = tempfile::tempdir().unwrap();
        let data_path = directory.path().join("bytes.raw");
        std::fs::write(&data_path, [1, 2, 3, 4]).unwrap();
        for language in [DecoderLanguage::Yaml, DecoderLanguage::Python] {
            for state in ["loaded", "edited", "cleared", "comments"] {
                let mut workspace = BinaryWorkspace::default();
                workspace.open(data_path.clone(), &ctx);
                settle(&mut workspace, &ctx);
                workspace.switch_language(language);
                match state {
                    "loaded" => {
                        let extension = if language == DecoderLanguage::Yaml {
                            "yaml"
                        } else {
                            "py"
                        };
                        let path = directory.path().join(format!("saved-starter.{extension}"));
                        std::fs::write(&path, language.starter()).unwrap();
                        workspace.load_yaml(path, &ctx);
                    }
                    "edited" => workspace.yaml.push_str("# User notes to retain\n"),
                    "cleared" => workspace.yaml.clear(),
                    "comments" => workspace.yaml = "# My format notes\n".into(),
                    _ => unreachable!(),
                }
                let source = workspace.yaml.clone();
                let saved = workspace.saved_yaml.clone();
                let path = workspace.yaml_path.clone();
                workspace.other_draft.source = "# Keep the other language too\n".into();
                workspace.other_draft.saved = "# Older saved source\n".into();
                workspace.other_draft.path = Some(directory.path().join("other-decoder"));
                let other_source = workspace.other_draft.source.clone();
                let other_saved = workspace.other_draft.saved.clone();
                let other_path = workspace.other_draft.path.clone();
                assert!(workspace.start_byte_preview(&ctx));
                settle(&mut workspace, &ctx);
                assert!(
                    workspace.showing_byte_preview(),
                    "{state}: {}",
                    workspace.message
                );
                assert_eq!(workspace.language, language);
                assert_eq!(workspace.yaml, source, "{language:?}: {state}");
                assert_eq!(workspace.saved_yaml, saved);
                assert_eq!(workspace.yaml_path, path);
                assert_eq!(workspace.other_draft.source, other_source);
                assert_eq!(workspace.other_draft.saved, other_saved);
                assert_eq!(workspace.other_draft.path, other_path);
            }
        }
    }

    #[test]
    fn byte_preview_survives_nonpublishing_python_and_invalid_yaml_attempts() {
        let ctx = Context::default();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bytes.raw");
        std::fs::write(&path, [7, 12, 240]).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(path, &ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.start_byte_preview(&ctx));
        settle(&mut workspace, &ctx);
        assert!(workspace.take_initial_plot_ready());
        workspace.take_layout_applied();
        let applied = workspace.applied_yaml.clone();
        workspace.hex_editor.select_range(1..2, false);
        workspace.switch_language(DecoderLanguage::Python);
        for code in [
            "test = 5",
            "raise ValueError('candidate failed')",
            "while True: pass",
        ] {
            workspace.yaml = code.into();
            workspace.apply_definition(&ctx);
            if code.starts_with("while") {
                workspace.stop_decoding();
            }
            settle(&mut workspace, &ctx);
            assert!(workspace.showing_byte_preview(), "{}", workspace.message);
            assert_eq!(workspace.applied_yaml, applied);
            assert_eq!(workspace.hex_editor.selection(), Some(1..2));
            assert!(!workspace.take_initial_plot_ready());
            assert!(!workspace.take_layout_applied());
        }
        workspace.switch_language(DecoderLanguage::Yaml);
        workspace.yaml = "actions: [{read: {name: broken, type: {structure: MissingType}}}]".into();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.decode_error.is_some());
        assert!(workspace.showing_byte_preview());
        assert_eq!(workspace.applied_yaml, applied);
        assert!(!workspace.take_initial_plot_ready());
        workspace.yaml = "actions: [{read: {name: values, type: u8, repeat: until end}}]".into();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.decoded_ready(), "{}", workspace.message);
        assert!(!workspace.showing_byte_preview());
        assert!(workspace.take_layout_applied());
        assert!(
            !workspace.take_initial_plot_ready(),
            "Explicit decoding has its own navigation event"
        );
    }

    #[test]
    fn byte_preview_refreshes_edits_even_during_start_without_running_dormant_python() {
        let ctx = Context::default();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("editable.raw");
        std::fs::write(&path, [11, 22, 33, 44]).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(path.clone(), &ctx);
        settle(&mut workspace, &ctx);
        workspace.switch_language(DecoderLanguage::Python);
        workspace.yaml = "raise RuntimeError('dormant Python must never run')".into();
        let python = workspace.yaml.clone();
        assert!(workspace.start_byte_preview(&ctx));
        workspace.hex_actions(
            vec![HexAction::Replace {
                offset: 1,
                bytes: vec![99],
            }],
            &ctx,
        );
        settle(&mut workspace, &ctx);
        assert!(workspace.showing_byte_preview(), "{}", workspace.message);
        assert!(!workspace.error, "{}", workspace.message);
        assert_eq!(workspace.yaml, python);
        workspace.take_initial_plot_ready();
        workspace.take_layout_applied();
        workspace.structure_ui.select_bytes(1..2);
        assert_eq!(
            workspace.structure_ui.selected_node().unwrap().numeric(),
            Some(99.)
        );
        workspace.hex_actions(
            vec![HexAction::Replace {
                offset: 1,
                bytes: vec![177],
            }],
            &ctx,
        );
        settle(&mut workspace, &ctx);
        assert!(workspace.showing_byte_preview(), "{}", workspace.message);
        workspace.structure_ui.select_bytes(1..2);
        assert_eq!(
            workspace.structure_ui.selected_node().unwrap().numeric(),
            Some(177.)
        );
        assert_eq!(workspace.yaml, python);
        assert!(
            !workspace.take_initial_plot_ready(),
            "Refreshing bytes must not move the dock focus"
        );
        assert!(!workspace.take_layout_applied());
        assert_eq!(std::fs::read(path).unwrap(), [11, 22, 33, 44]);
    }

    #[test]
    fn byte_preview_cancellation_and_file_replacement_discard_pending_navigation() {
        let ctx = Context::default();
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.raw");
        let second = directory.path().join("second.raw");
        std::fs::write(&first, [1, 2, 3]).unwrap();
        std::fs::write(&second, [210, 211]).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(first.clone(), &ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.start_byte_preview(&ctx));
        workspace.stop_decoding();
        settle(&mut workspace, &ctx);
        assert!(!workspace.decoded_ready());
        assert!(!workspace.take_initial_plot_ready());
        assert!(workspace.start_byte_preview(&ctx));
        workspace.open(second.clone(), &ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.source_path(), Some(second.as_path()));
        assert!(!workspace.decoded_ready());
        assert!(!workspace.take_initial_plot_ready());
        assert!(workspace.start_byte_preview(&ctx));
        settle(&mut workspace, &ctx);
        assert!(workspace.showing_byte_preview());
        workspace.structure_ui.select_bytes(0..1);
        assert_eq!(
            workspace.structure_ui.selected_node().unwrap().numeric(),
            Some(210.)
        );
        // A result that completed before the next open cannot navigate the new file either.
        workspace.open(first, &ctx);
        settle(&mut workspace, &ctx);
        assert!(!workspace.take_initial_plot_ready());
        assert!(!workspace.showing_byte_preview());
    }

    fn pending_dialog(
        workspace: &mut BinaryWorkspace,
        ctx: &Context,
        action: FileDialogAction,
    ) -> mpsc::Sender<Option<PathBuf>> {
        let (tx, rx) = mpsc::channel();
        assert!(workspace.file_dialog.begin_test(
            async move { rx.recv_timeout(Duration::from_secs(5)).unwrap() },
            action,
            ctx,
        ));
        tx
    }

    fn visible_control_text(workspace: &mut BinaryWorkspace, ctx: &Context) -> String {
        // egui hides a new Window for its first sizing frame. Inspect its painted
        // text after that frame, as the click helper also does for popup controls.
        frame(workspace, ctx, vec![]);
        frame(workspace, ctx, vec![])
            .shapes
            .into_iter()
            .filter_map(|shape| {
                if let egui::epaint::Shape::Text(text) = shape.shape {
                    Some(text.galley.text().to_owned())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn successful_file_operations_remember_their_own_directory_and_captured_save_language() {
        let ctx = Context::default();
        let temporary = tempfile::tempdir().unwrap();
        let directories: Vec<_> = ["data", "yaml", "python", "saved-python", "exports"]
            .iter()
            .map(|name| {
                let directory = temporary.path().join(name);
                std::fs::create_dir(&directory).unwrap();
                directory
            })
            .collect();
        let data = directories[0].join("source.raw");
        let yaml = directories[1].join("layout.yaml");
        let python = directories[2].join("decoder.py");
        std::fs::write(&data, [1, 2, 3, 4]).unwrap();
        std::fs::write(&yaml, "actions: []\n").unwrap();
        std::fs::write(&python, "print('ready')\n").unwrap();
        let locations = FileLocations::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.set_file_locations(locations.clone());
        workspace.open(data, &ctx);
        settle(&mut workspace, &ctx);
        workspace.load_yaml(yaml, &ctx);
        workspace.load_yaml(python, &ctx);
        assert_eq!(
            locations.directory(FileKind::Data),
            Some(directories[0].clone())
        );
        assert_eq!(
            locations.directory(FileKind::Yaml),
            Some(directories[1].clone())
        );
        assert_eq!(
            locations.directory(FileKind::Python),
            Some(directories[2].clone())
        );
        let captured = workspace.yaml.clone();
        workspace.switch_language(DecoderLanguage::Yaml);
        workspace.finish_file_dialog(
            FileDialogAction::Save(SaveContent::Yaml(captured.clone(), DecoderLanguage::Python)),
            Some(directories[3].join("saved.py")),
            &ctx,
        );
        settle(&mut workspace, &ctx);
        assert_eq!(
            locations.directory(FileKind::Python),
            Some(directories[3].clone())
        );
        assert_eq!(
            locations.directory(FileKind::Yaml),
            Some(directories[1].clone())
        );
        assert_eq!(
            workspace.decoder_save_directory(),
            Some(directories[1].clone())
        );
        let view = workspace.bytes.as_ref().unwrap().view();
        workspace.finish_file_dialog(
            FileDialogAction::Save(SaveContent::Bytes(view, 0..2, false)),
            Some(directories[4].join("selection.bin")),
            &ctx,
        );
        settle(&mut workspace, &ctx);
        assert_eq!(
            locations.directory(FileKind::Exports),
            Some(directories[4].clone())
        );
        assert_eq!(
            locations.directory(FileKind::Data),
            Some(directories[0].clone())
        );
        assert_eq!(
            std::fs::read_to_string(directories[3].join("saved.py")).unwrap(),
            captured
        );
        assert_eq!(
            std::fs::read(directories[4].join("selection.bin")).unwrap(),
            [1, 2]
        );
    }

    #[test]
    fn canceled_declined_and_failed_dialog_operations_keep_remembered_directories() {
        let ctx = Context::default();
        let temporary = tempfile::tempdir().unwrap();
        let original = temporary.path().join("original");
        let candidate = temporary.path().join("candidate");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&candidate).unwrap();
        let locations = FileLocations::default();
        for kind in [FileKind::Yaml, FileKind::Python, FileKind::Exports] {
            locations.remember_file(kind, &original.join("previous-file"));
        }
        let mut workspace = BinaryWorkspace::default();
        workspace.set_file_locations(locations.clone());
        workspace.finish_file_dialog(FileDialogAction::LoadYaml, None, &ctx);
        workspace.finish_file_dialog(
            FileDialogAction::Save(SaveContent::Yaml("draft".into(), DecoderLanguage::Python)),
            None,
            &ctx,
        );
        let invalid = candidate.join("invalid.yaml");
        std::fs::write(&invalid, [0xff]).unwrap();
        workspace.load_yaml(invalid, &ctx);
        assert!(workspace.error);
        let valid = candidate.join("valid.yaml");
        std::fs::write(&valid, "actions: []").unwrap();
        workspace.yaml = "# unsaved draft".into();
        workspace.load_yaml(valid, &ctx);
        click(&mut workspace, &ctx, "Cancel");
        let existing = candidate.join("existing.py");
        std::fs::write(&existing, "original").unwrap();
        workspace.finish_file_dialog(
            FileDialogAction::Save(SaveContent::Yaml("new".into(), DecoderLanguage::Python)),
            Some(existing.clone()),
            &ctx,
        );
        assert!(workspace.overwrite.is_some());
        click(&mut workspace, &ctx, "Cancel");
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "original");
        workspace.save(
            &ctx,
            candidate.join("missing/fails.py"),
            SaveContent::Yaml("new".into(), DecoderLanguage::Python),
            false,
        );
        settle(&mut workspace, &ctx);
        assert!(workspace.error);
        for kind in [FileKind::Yaml, FileKind::Python, FileKind::Exports] {
            assert_eq!(locations.remembered_directory(kind), Some(original.clone()));
        }
    }

    #[test]
    fn decoder_save_prefers_remembered_folder_then_its_own_path_and_never_the_data_folder() {
        let temporary = tempfile::tempdir().unwrap();
        let data = temporary.path().join("data");
        let current = temporary.path().join("current-yaml");
        let remembered = temporary.path().join("last-yaml");
        let python = temporary.path().join("python");
        for directory in [&data, &current, &remembered, &python] {
            std::fs::create_dir(directory).unwrap();
        }
        let locations = FileLocations::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.set_file_locations(locations.clone());
        workspace.source = Some(data.join("source.raw"));
        workspace.yaml_path = Some(current.join("layout.yaml"));
        assert_eq!(workspace.decoder_save_directory(), Some(current));
        locations.remember_file(FileKind::Yaml, &remembered.join("other.yaml"));
        assert_eq!(workspace.decoder_save_directory(), Some(remembered));
        locations.remember_file(FileKind::Python, &python.join("decoder.py"));
        workspace.switch_language(DecoderLanguage::Python);
        assert_eq!(workspace.decoder_save_directory(), Some(python));
        assert_ne!(workspace.decoder_save_directory(), Some(data));
        assert!(locations.remembered_directory(FileKind::Exports).is_none());
    }

    #[test]
    fn language_selector_preserves_independent_source_paths_and_save_baselines() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.yaml = "actions: []\n# YAML draft\n".into();
        workspace.saved_yaml = "actions: []\n".into();
        workspace.yaml_path = Some(PathBuf::from("layout.yaml"));
        workspace.decode_error = Some("Old YAML diagnostic".into());
        workspace.error = true;
        workspace.message = "Old YAML diagnostic".into();
        let yaml = workspace.yaml.clone();
        click(&mut workspace, &ctx, "Python");
        assert!(workspace.is_python());
        assert_eq!(workspace.yaml, workspace.language.starter());
        assert!(!visible_control_text(&mut workspace, &ctx).contains("Old YAML diagnostic"));
        workspace.yaml = "print('Python draft')\n".into();
        workspace.saved_yaml = "print('saved Python')\n".into();
        workspace.yaml_path = Some(PathBuf::from("decoder.py"));
        let python = workspace.yaml.clone();
        let controls = visible_control_text(&mut workspace, &ctx);
        assert!(controls.contains("Run script"));
        assert!(controls.contains("Save Python…"));
        click(&mut workspace, &ctx, "YAML");
        assert_eq!(workspace.yaml, yaml);
        assert_eq!(workspace.saved_yaml, "actions: []\n");
        assert_eq!(
            workspace.yaml_path.as_deref(),
            Some(Path::new("layout.yaml"))
        );
        assert!(workspace.draft_dirty(DecoderLanguage::Yaml));
        let controls = visible_control_text(&mut workspace, &ctx);
        assert!(controls.contains("Decode file"));
        assert!(controls.contains("Save YAML…"));
        click(&mut workspace, &ctx, "Python");
        assert_eq!(workspace.yaml, python);
        assert_eq!(workspace.saved_yaml, "print('saved Python')\n");
        assert_eq!(
            workspace.yaml_path.as_deref(),
            Some(Path::new("decoder.py"))
        );
        assert!(
            workspace.applied_language.is_none(),
            "Switching never runs or converts source"
        );
    }

    #[test]
    fn decoder_extension_selects_the_matching_draft_and_guards_only_its_replacement() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let python_path = dir.path().join("decoder.PY");
        let yaml_path = dir.path().join("layout.yml");
        let replacement = dir.path().join("replacement.py");
        std::fs::write(&python_path, "print('original')\n").unwrap();
        std::fs::write(&yaml_path, "actions: []\n").unwrap();
        std::fs::write(&replacement, "print('replacement')\n").unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.load_yaml(python_path.clone(), &ctx);
        assert!(workspace.is_python());
        workspace.yaml.push_str("# pending Python edit\n");
        let python = workspace.yaml.clone();
        workspace.load_yaml(yaml_path.clone(), &ctx);
        assert!(
            workspace.pending.is_none(),
            "Opening YAML keeps the Python draft intact"
        );
        assert!(!workspace.is_python());
        assert_eq!(workspace.yaml_path.as_ref(), Some(&yaml_path));
        assert_eq!(workspace.other_draft.source, python);
        workspace.load_yaml(replacement.clone(), &ctx);
        assert!(matches!(&workspace.pending, Some(Action::LoadYaml(path)) if path == &replacement));
        assert!(visible_control_text(&mut workspace, &ctx).contains("Replace the Python draft"));
        click(&mut workspace, &ctx, "Cancel");
        assert!(!workspace.is_python());
        assert_eq!(workspace.other_draft.source, python);
        workspace.load_yaml(replacement, &ctx);
        click(&mut workspace, &ctx, "Replace decoder");
        assert!(workspace.is_python());
        assert_eq!(workspace.yaml, "print('replacement')\n");
        workspace.switch_language(DecoderLanguage::Yaml);
        assert_eq!(workspace.yaml, "actions: []\n");
        assert_eq!(workspace.yaml_path.as_ref(), Some(&yaml_path));
    }

    #[test]
    fn inactive_drafts_remain_part_of_close_and_file_open_guards() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.switch_language(DecoderLanguage::Python);
        workspace.yaml = "# unsaved Python\n".into();
        workspace.switch_language(DecoderLanguage::Yaml);
        assert_eq!(workspace.yaml, workspace.language.starter());
        assert!(workspace.is_dirty());
        workspace.open(PathBuf::from("another-data-file.raw"), &ctx);
        assert!(matches!(workspace.pending, Some(Action::Open(_))));
        workspace.pending = None;
        workspace.request(Action::Close, &ctx);
        assert!(matches!(workspace.pending, Some(Action::Close)));
        workspace.pending = None;
        workspace.other_draft = DecoderDraft::default();
        workspace.structure_ui.begin_definition(0..2);
        workspace.switch_language(DecoderLanguage::Python);
        assert!(
            workspace.is_dirty(),
            "Inactive YAML fields are still an unsaved draft"
        );
        workspace.request(Action::Close, &ctx);
        assert!(matches!(workspace.pending, Some(Action::Close)));
    }

    #[test]
    fn yaml_field_draft_survives_python_run_without_disabling_its_controls() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        workspace.structure_ui.begin_definition(0..2);
        let yaml = workspace.yaml.clone();
        assert!(workspace.active_authoring_dirty());
        workspace.load_python_text("from binwave import Struct\nSample = Struct('Sample', [('value', 'u8')])\nfile.records('samples', Sample)\n", &ctx);
        assert!(
            workspace.pending.is_none(),
            "New Python never replaces the YAML fields"
        );
        assert!(workspace.is_python());
        assert!(workspace.can_apply_definition());
        assert!(workspace.can_save_definition());
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.decoded_ready(), "{}", workspace.message);
        assert_eq!(workspace.applied_language, Some(DecoderLanguage::Python));
        workspace.switch_language(DecoderLanguage::Yaml);
        assert_eq!(workspace.yaml, yaml);
        assert!(workspace.structure_ui.authoring_dirty());
        assert!(workspace.structure_ui.is_describing());
        assert!(!workspace.can_apply_definition());
        assert!(!workspace.can_save_definition());
        assert!(
            workspace.decoded_ready(),
            "Switching drafts keeps the applied Python result"
        );
    }

    #[test]
    fn switching_language_keeps_applied_results_but_does_not_claim_the_other_source_ran() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        workspace.yaml = "actions: [{read: {name: value, type: u8}}]".into();
        workspace.saved_yaml = workspace.yaml.clone();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.decoder_status(), "Decoded");
        let applied = workspace.applied_yaml.clone();
        workspace.switch_language(DecoderLanguage::Python);
        assert_eq!(workspace.yaml, workspace.language.starter());
        assert_eq!(
            workspace.decoder_status(),
            "Python not run · YAML results",
            "An empty draft still identifies the visible result's decoder"
        );
        workspace.yaml = "print('not run')".into();
        assert!(workspace.decoded_ready());
        assert_eq!(workspace.decoder_status(), "Python not run · YAML results");
        assert_eq!(workspace.applied_yaml, applied);
        workspace.switch_language(DecoderLanguage::Yaml);
        assert_eq!(workspace.decoder_status(), "Decoded");
        workspace.yaml = "actions: [{unknown_action: bad}]".into();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.decoded_ready());
        assert_eq!(workspace.applied_yaml, applied);
        workspace.switch_language(DecoderLanguage::Python);
        assert_eq!(workspace.decoder_status(), "Python not run · YAML results");
        workspace.yaml.clear();
        assert_eq!(workspace.decoder_status(), "Python not run · YAML results");
    }

    #[test]
    fn plain_python_completes_without_publishing_and_prints_in_console() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::python_workspace();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        workspace.switch_language(DecoderLanguage::Python);
        let preview = binary::DEMO_BYTES[..2]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        for (code, expected) in [
            ("test = 5", "Script finished. No output."),
            ("test = 5\nprint(test)", "5"),
            ("print(file.read_at(0, 2).hex())", preview.as_str()),
        ] {
            workspace.yaml = code.into();
            workspace.apply_definition(&ctx);
            settle(&mut workspace, &ctx);
            assert_eq!(
                workspace.decoder_status(),
                "Script finished",
                "{}",
                workspace.message
            );
            assert!(!workspace.error);
            assert!(!workspace.take_layout_applied());
            assert!(!workspace.decoded_ready());
            let rendered = visible_control_text(&mut workspace, &ctx);
            assert!(rendered.contains(expected), "{rendered}");
            assert!(!rendered.contains("Traceback"));
        }
        workspace.yaml = "raise ValueError('test failure')".into();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.decoder_status(), "Script failed");
        assert!(visible_control_text(&mut workspace, &ctx).contains("test failure"));
        workspace.yaml = "print('recovered')".into();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.decoder_status(), "Script finished");
        assert!(!workspace.error);
    }

    #[test]
    fn python_attempts_keep_legacy_and_generic_results_and_applied_identity() {
        let generic = "structures: [{name: Sample, fields: [{name: value, type: u8}]}]\nactions: [{read: {name: samples, type: {structure: Sample}, repeat: until end}}]";
        for decoder in [binary::DEMO_YAML, generic] {
            let ctx = Context::default();
            let mut workspace = BinaryWorkspace::default();
            workspace.open_demo(&ctx);
            settle(&mut workspace, &ctx);
            workspace.yaml = decoder.into();
            workspace.apply_definition(&ctx);
            settle(&mut workspace, &ctx);
            assert!(workspace.decoded_ready());
            assert!(workspace.take_layout_applied());
            let applied = workspace.applied_yaml.clone();
            let plot = workspace.plot_data.clone();
            workspace.hex_editor.select_range(16..18, false);
            workspace.switch_language(DecoderLanguage::Python);
            for code in [
                "test = 5",
                "raise RuntimeError('attempt failed')",
                "while True: pass",
            ] {
                workspace.yaml = code.into();
                workspace.apply_definition(&ctx);
                assert!(
                    workspace.decoded_ready(),
                    "valid results stay ready while the script runs"
                );
                if code.starts_with("while") {
                    workspace.stop_decoding();
                }
                settle(&mut workspace, &ctx);
                assert_eq!(workspace.applied_yaml, applied);
                assert_eq!(workspace.applied_language, Some(DecoderLanguage::Yaml));
                assert_eq!(workspace.hex_editor.selection(), Some(16..18));
                assert!(workspace.decoded_ready(), "{}", workspace.message);
                assert!(!workspace.take_layout_applied());
                assert_eq!(workspace.plot_data.is_some(), plot.is_some());
            }
        }
    }

    #[test]
    fn plain_python_after_byte_edit_resumes_the_committed_decoder_refresh() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        let yaml = "actions: [{read: {name: value, type: u8}}]";
        workspace.yaml = yaml.into();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.take_layout_applied());
        workspace.hex_editor.select_range(0..1, false);
        workspace.structure_ui.select_bytes(0..1);
        workspace.hex_actions(
            vec![HexAction::Replace {
                offset: 0,
                bytes: vec![99],
            }],
            &ctx,
        );
        assert!(workspace.interpretation_stale);
        workspace.switch_language(DecoderLanguage::Python);
        workspace.yaml = "test = 5".into();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.decoded_ready(), "{}", workspace.message);
        assert_eq!(workspace.applied_yaml.as_deref(), Some(yaml));
        assert_eq!(
            workspace.structure_ui.selected_node().unwrap().numeric(),
            Some(99.)
        );
        assert!(
            !workspace.take_layout_applied(),
            "refresh does not pretend the plain script published a dataset"
        );
    }

    #[test]
    fn completed_source_save_updates_its_own_language_draft() {
        let mut workspace = BinaryWorkspace::python_workspace();
        workspace.yaml = "captured Python\n".into();
        let captured = workspace.yaml.clone();
        workspace.yaml.push_str("# newer change\n");
        workspace.switch_language(DecoderLanguage::Yaml);
        workspace.yaml = "actions: []\n".into();
        workspace.saved_yaml = workspace.yaml.clone();
        let yaml = workspace.yaml.clone();
        workspace.finish_source_save(
            captured.clone(),
            Path::new("saved.py"),
            DecoderLanguage::Python,
        );
        assert_eq!(workspace.yaml, yaml);
        assert_eq!(workspace.saved_yaml, yaml);
        assert!(workspace.yaml_path.is_none());
        workspace.switch_language(DecoderLanguage::Python);
        assert_eq!(workspace.saved_yaml, captured);
        assert!(workspace.yaml.ends_with("# newer change\n"));
        assert_eq!(workspace.yaml_path.as_deref(), Some(Path::new("saved.py")));
        assert!(workspace.draft_dirty(DecoderLanguage::Python));
    }

    #[test]
    fn new_button_uses_the_selected_language_and_preserves_the_other_draft() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.yaml = "actions: []\n# keep YAML\n".into();
        let yaml = workspace.yaml.clone();
        click(&mut workspace, &ctx, "Python");
        click(&mut workspace, &ctx, "New");
        assert!(workspace.is_python());
        assert_eq!(workspace.yaml, DecoderLanguage::Python.starter());
        assert!(!workspace.has_definition());
        assert!(!workspace.draft_dirty(DecoderLanguage::Python));
        assert!(workspace.pending.is_none());
        let python = workspace.yaml.clone();
        click(&mut workspace, &ctx, "YAML");
        assert_eq!(workspace.yaml, yaml);
        click(&mut workspace, &ctx, "New");
        assert!(visible_control_text(&mut workspace, &ctx).contains("Replace the YAML draft"));
        click(&mut workspace, &ctx, "Replace decoder");
        assert_eq!(workspace.yaml, DecoderLanguage::Yaml.starter());
        assert!(workspace.has_definition(), "The simple YAML starter is ready to apply");
        assert!(!workspace.is_dirty());
        workspace.switch_language(DecoderLanguage::Python);
        assert_eq!(workspace.yaml, python);
    }

    #[test]
    fn loaded_yaml_reveals_source_with_decode_action_and_tracks_state() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample-decoder.yml");
        let yaml = "actions: [{read: {name: version, type: u8}}]\n";
        std::fs::write(&path, yaml).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        workspace.structure_ui.begin_definition(0..1);
        assert!(workspace.structure_ui.is_describing());
        workspace.load_yaml(path.clone(), &ctx);
        if workspace.pending.is_some() {
            click(&mut workspace, &ctx, "Replace decoder");
        }
        assert!(workspace.pending.is_none());
        assert!(workspace.take_reveal_definition());
        assert!(!workspace.structure_ui.is_describing());
        assert_eq!(workspace.yaml_path.as_ref(), Some(&path));
        assert_eq!(workspace.yaml, yaml);
        assert_eq!(workspace.saved_yaml, yaml);
        assert_eq!(workspace.decoder_status(), "Ready to decode");
        let text = visible_control_text(&mut workspace, &ctx);
        for label in [
            "sample-decoder.yml",
            "Open decoder…",
            "Save YAML…",
            "Decode file",
            "Ready to decode",
            "actions:",
        ] {
            assert!(text.contains(label), "missing {label:?} from {text}");
        }
        click(&mut workspace, &ctx, "Decode file");
        settle(&mut workspace, &ctx);
        assert!(workspace.decoded_ready(), "{}", workspace.message);
        assert_eq!(workspace.decoder_status(), "Decoded");
        workspace.yaml.push_str("# changed draft\n");
        assert_eq!(workspace.decoder_status(), "Changes not decoded");
        assert!(visible_control_text(&mut workspace, &ctx).contains("Modified"));
    }

    #[test]
    fn failed_or_cancelled_yaml_load_preserves_source_and_active_builder() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("working.yaml");
        std::fs::write(&path, "actions: []").unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.load_yaml(path.clone(), &ctx);
        assert!(workspace.take_reveal_definition());
        workspace.structure_ui.begin_definition(0..1);
        workspace.finish_file_dialog(FileDialogAction::LoadYaml, None, &ctx);
        assert!(!workspace.take_reveal_definition());
        assert!(workspace.structure_ui.is_describing());
        workspace.execute(Action::LoadYaml(dir.path().join("missing.yaml")), &ctx);
        assert!(workspace.error);
        assert_eq!(workspace.yaml, "actions: []");
        assert_eq!(workspace.saved_yaml, "actions: []");
        assert_eq!(workspace.yaml_path.as_ref(), Some(&path));
        assert!(workspace.structure_ui.is_describing());
        assert!(!workspace.take_reveal_definition());
    }

    #[test]
    fn builder_replacements_keep_builder_but_examples_open_yaml() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.yaml_path = Some(PathBuf::from("working.yaml"));
        workspace.structure_ui.begin_definition(0..1);
        workspace.execute(Action::ReplaceYaml("actions: []".into()), &ctx);
        assert!(workspace.structure_ui.is_describing());
        assert_eq!(
            workspace.yaml_path.as_deref(),
            Some(Path::new("working.yaml"))
        );
        workspace.execute(Action::ExampleYaml("actions: []".into()), &ctx);
        assert!(!workspace.structure_ui.is_describing());
        assert!(workspace.yaml_path.is_none());
        assert!(visible_control_text(&mut workspace, &ctx).contains("Example decoder"));
    }

    #[test]
    fn loading_replacement_yaml_clears_previous_decoder_diagnostic() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("valid.yaml");
        std::fs::write(&path, "actions: [{read: {name: value, type: u8}}]").unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        workspace.yaml = "actions: [{unknown_action: bad}]".into();
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.decoder_status(), "Decode failed");
        assert!(workspace.structure_ui.decode_error().is_some());
        workspace.load_yaml(path.clone(), &ctx);
        click(&mut workspace, &ctx, "Replace decoder");
        assert!(workspace.decode_error.is_none());
        assert!(workspace.structure_ui.decode_error().is_none());
        assert_eq!(workspace.decoder_status(), "Ready to decode");
        assert_eq!(workspace.yaml_path.as_ref(), Some(&path));
        assert!(!workspace.has_layout());
    }

    #[test]
    fn pending_field_draft_blocks_save_and_decode_and_review_restores_it() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        let original_yaml = workspace.yaml.clone();
        workspace.structure_ui.begin_definition(0..1);
        assert!(workspace.structure_ui.authoring_dirty());
        workspace.show_yaml();
        assert!(!workspace.structure_ui.is_describing());
        assert!(workspace.structure_ui.authoring_dirty());
        assert!(!workspace.can_apply_definition());
        assert!(!workspace.can_save_definition());
        workspace.save_definition(&ctx);
        workspace.apply_definition(&ctx);
        assert!(!workspace.dialog_pending());
        assert!(!workspace.busy());
        assert!(!workspace.has_layout());
        let text = visible_control_text(&mut workspace, &ctx);
        assert!(text.contains("Modified"));
        assert!(text.contains("Review changes"));
        click(&mut workspace, &ctx, "Review changes");
        assert!(workspace.structure_ui.is_describing());
        assert_eq!(workspace.yaml, original_yaml);
    }

    #[test]
    fn decoder_status_reports_generic_and_legacy_failures_and_recovers() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        for yaml in [
            "actions: [{unknown_action: bad}]",
            "actions: [{read: {name: value, type: u64, at: 100000}}]",
            "invalid: layout",
        ] {
            workspace.yaml = yaml.into();
            workspace.apply_definition(&ctx);
            settle(&mut workspace, &ctx);
            assert!(workspace.decode_error.is_some(), "{}", workspace.message);
            assert_eq!(workspace.decoder_status(), "Decode failed");
            assert!(workspace.error);
            assert!(!workspace.decoded_ready());
        }
        workspace.yaml = binary::DEMO_YAML.into();
        click(&mut workspace, &ctx, "Decode file");
        settle(&mut workspace, &ctx);
        assert!(workspace.decode_error.is_none(), "{}", workspace.message);
        assert!(workspace.decoded_ready());
        assert_eq!(workspace.decoder_status(), "Decoded");
    }

    #[test]
    fn cancelled_native_dialog_preserves_draft_and_blocks_duplicate_or_close() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.yaml = "unfinished definition".into();
        let completion = pending_dialog(&mut workspace, &ctx, FileDialogAction::LoadYaml);
        assert!(!workspace.commands_available());
        assert!(
            !workspace.busy(),
            "a picker must not claim background IO is running"
        );
        // These public entry points must not create another native dialog or
        // replace the document while its picker is still open.
        workspace.choose_definition(&ctx);
        workspace.save_definition(&ctx);
        workspace.open_demo(&ctx);
        assert!(workspace.pending.is_none());
        assert!(workspace.io_rx.is_none());
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| workspace.tick(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::CancelClose));
        assert!(
            workspace.pending.is_none(),
            "close must not hide a pending picker behind a discard dialog"
        );
        completion.send(None).unwrap();
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.yaml, "unfinished definition");
        assert_eq!(workspace.saved_yaml, DecoderLanguage::Yaml.starter());
        assert!(workspace.commands_available());
        assert!(workspace.is_dirty());
    }

    #[test]
    fn external_file_picker_blocks_binary_dialogs_and_document_replacement() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.set_external_dialog_pending(true);
        assert!(!workspace.commands_available());
        assert!(!workspace.dialog_pending());
        workspace.choose_definition(&ctx);
        workspace.save_definition(&ctx);
        workspace.open_demo(&ctx);
        assert!(!workspace.dialog_pending());
        assert!(workspace.io_rx.is_none());
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| workspace.tick(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::CancelClose));
        workspace.set_external_dialog_pending(false);
        assert!(workspace.commands_available());
    }

    #[test]
    fn definition_dialog_completion_checks_current_unsaved_draft() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("definition.yaml");
        std::fs::write(&path, "actions: []").unwrap();
        let mut workspace = BinaryWorkspace::default();
        let completion = pending_dialog(&mut workspace, &ctx, FileDialogAction::LoadYaml);
        workspace.yaml = "new draft created while picker was pending".into();
        completion.send(Some(path.clone())).unwrap();
        settle(&mut workspace, &ctx);
        assert!(matches!(&workspace.pending, Some(Action::LoadYaml(chosen)) if chosen == &path));
        assert_eq!(workspace.yaml, "new draft created while picker was pending");
        click(&mut workspace, &ctx, "Cancel");
        assert_eq!(workspace.yaml, "new draft created while picker was pending");
    }

    #[test]
    fn save_dialog_preserves_captured_yaml_when_draft_changes() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("definition.yaml");
        let mut workspace = BinaryWorkspace::default();
        workspace.yaml = "captured draft".into();
        let intent = FileDialogAction::Save(SaveContent::Yaml(
            workspace.yaml.clone(),
            workspace.language,
        ));
        let completion = pending_dialog(&mut workspace, &ctx, intent);
        workspace.yaml = "newer draft".into();
        completion.send(Some(path.clone())).unwrap();
        settle(&mut workspace, &ctx);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "captured draft");
        assert_eq!(workspace.yaml_path.as_ref(), Some(&path));
        assert_eq!(workspace.yaml, "newer draft");
        assert_eq!(workspace.saved_yaml, "captured draft");
        assert!(workspace.is_dirty());
    }

    #[test]
    fn save_dialog_keeps_captured_byte_revision_through_overwrite_confirmation() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.raw");
        let path = dir.path().join("edited.raw");
        std::fs::write(&source, [1, 2, 3, 4]).unwrap();
        std::fs::write(&path, b"existing").unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(source.clone(), &ctx);
        settle(&mut workspace, &ctx);
        workspace.bytes.as_mut().unwrap().replace(0, &[9]).unwrap();
        let captured = workspace.bytes.as_ref().unwrap().view();
        let completion = pending_dialog(
            &mut workspace,
            &ctx,
            FileDialogAction::Save(SaveContent::Bytes(captured, 0..4, true)),
        );
        // A later byte revision must neither leak into the requested save nor
        // become marked as saved when the captured revision finishes writing.
        workspace.bytes.as_mut().unwrap().replace(0, &[8]).unwrap();
        completion.send(Some(path.clone())).unwrap();
        settle(&mut workspace, &ctx);
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        assert!(workspace.overwrite.is_some());
        click(&mut workspace, &ctx, "Overwrite this file");
        settle(&mut workspace, &ctx);
        assert_eq!(std::fs::read(path).unwrap(), [9, 2, 3, 4]);
        assert_eq!(std::fs::read(source).unwrap(), [1, 2, 3, 4]);
        assert_eq!(
            workspace.bytes.as_ref().unwrap().view().read(0, 4).unwrap(),
            [8, 2, 3, 4]
        );
        assert!(workspace.bytes.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn generic_value_inspector_edits_full_field_from_partial_byte_selection() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("samples.raw");
        let mut original = Vec::new();
        for index in 0..4u64 {
            original.extend_from_slice(&(10. + index as f64).to_be_bytes());
            original.extend_from_slice(&index.to_be_bytes());
        }
        std::fs::write(&path, &original).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(path.clone(), &ctx);
        settle(&mut workspace, &ctx);
        workspace.yaml = "endian: big\nstructures:\n  - name: Sample\n    fields: [{name: triangle, type: f64}, {name: counter, type: u64}]\nactions: [{read: {name: samples, type: {structure: Sample}, repeat: until end}}]".into();
        workspace.validate_layout(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.message.starts_with("Decoded."));
        workspace.hex_editor.select_range(19..20, true);
        workspace.hex_actions(vec![HexAction::Selection(19..20)], &ctx);
        settle(&mut workspace, &ctx);
        let node = workspace.structure_ui.selected_node().unwrap();
        assert_eq!(node.name, "triangle");
        assert_eq!(node.range, 16..24);
        assert_eq!(node.numeric(), Some(11.));
        assert_eq!(workspace.structure_ui.selected_sample(), Some((1, 16..32)));
        let shown = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| workspace.selection_inspector(ui));
        });
        let labels: Vec<_> = shown
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect();
        assert!(labels.iter().any(|text| text == "triangle"));
        assert!(labels.iter().any(|text| text == "Sample 1"));
        assert!(labels
            .iter()
            .any(|text| text == "Value uses the full 8-byte field."));
        assert!(!labels
            .iter()
            .any(|text| text == "Definition path" || text == "Little endian"));
        let revision = workspace.bytes.as_ref().unwrap().view().revision;
        workspace.generic_value_edit.as_mut().unwrap().text = "NaN".into();
        workspace.commit_value_edit(&ctx);
        assert!(workspace.error);
        assert_eq!(workspace.bytes.as_ref().unwrap().view().revision, revision);
        assert!(!workspace.bytes.as_ref().unwrap().has_changes());
        workspace.generic_value_edit.as_mut().unwrap().text = "-18.25".into();
        workspace.commit_value_edit(&ctx);
        assert_eq!(workspace.hex_editor.selection(), Some(16..24));
        settle(&mut workspace, &ctx);
        assert_eq!(
            workspace.structure_ui.selected_node().unwrap().numeric(),
            Some(-18.25)
        );
        let mut expected = original.clone();
        expected[16..24].copy_from_slice(&(-18.25_f64).to_be_bytes());
        assert_eq!(
            workspace
                .bytes
                .as_ref()
                .unwrap()
                .view()
                .read(0, original.len())
                .unwrap(),
            expected
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
        workspace.history(&ctx, false);
        settle(&mut workspace, &ctx);
        assert_eq!(
            workspace.structure_ui.selected_node().unwrap().numeric(),
            Some(11.)
        );
        workspace.history(&ctx, true);
        settle(&mut workspace, &ctx);
        assert_eq!(
            workspace.structure_ui.selected_node().unwrap().numeric(),
            Some(-18.25)
        );
        assert_eq!(workspace.hex_editor.selection(), Some(16..24));
    }

    #[test]
    fn generic_value_editor_rejects_a_stale_byte_revision() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value.raw");
        std::fs::write(&path, 7u64.to_le_bytes()).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(path, &ctx);
        settle(&mut workspace, &ctx);
        workspace.yaml = "actions: [{read: {name: counter, type: u64}}]".into();
        workspace.validate_layout(&ctx);
        settle(&mut workspace, &ctx);
        workspace.structure_ui.select_bytes(0..8);
        let node = workspace.structure_ui.selected_node().unwrap();
        workspace.prepare_value_edit(&node);
        workspace.generic_value_edit.as_mut().unwrap().text = "18446744073709551615".into();
        workspace
            .bytes
            .as_mut()
            .unwrap()
            .replace(0, &9u64.to_le_bytes())
            .unwrap();
        workspace.commit_value_edit(&ctx);
        assert!(workspace.error);
        assert!(workspace.message.contains("Bytes changed"));
        assert_eq!(
            workspace.bytes.as_ref().unwrap().view().read(0, 8).unwrap(),
            9u64.to_le_bytes()
        );
    }

    #[test]
    fn value_editor_refreshes_when_the_same_bytes_are_redefined() {
        let original = Original::from_bytes(&11f64.to_le_bytes()).unwrap();
        let store = ByteStore::from_snapshot(original);
        let source = store.view();
        let mut workspace = BinaryWorkspace::default();
        workspace.bytes = Some(store);
        for (endian, kind) in [("little", "f64"), ("big", "f64"), ("big", "u64")] {
            let yaml =
                format!("endian: {endian}\nactions: [{{read: {{name: value, type: {kind}}}}}]");
            let output = crate::structure::Program::parse(&yaml)
                .unwrap()
                .run(&source, &AtomicBool::new(false))
                .unwrap();
            let node = &output.nodes[0];
            workspace.prepare_value_edit(node);
            let edit = workspace.generic_value_edit.as_mut().unwrap();
            assert_eq!(edit.text, node.display());
            assert_eq!(edit.node.kind, node.kind);
            assert_eq!(edit.node.endian, node.endian);
            edit.text = "999".into();
        }
        assert_eq!(source.read(0, 8).unwrap(), 11f64.to_le_bytes());
    }

    #[test]
    fn all_file_types_open_as_bytes_without_a_snapshot_or_layout() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let mut workspace = BinaryWorkspace::default();
        for name in [
            "data.csv",
            "data.raw",
            "layout.yaml",
            "config.md",
            "no-extension",
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"TEST\x07\0\0\0").unwrap();
            workspace.open(path.clone(), &ctx);
            settle(&mut workspace, &ctx);
            assert!(workspace.has_file());
            assert!(
                workspace.original.is_none(),
                "raw opening must not copy the file"
            );
            assert!(workspace.document.is_none());
            assert_eq!(workspace.yaml, workspace.language.starter());
            assert!(matches!(workspace.take_opened_file(), Some(OpenedFile::Path(p)) if p == path));
        }
    }

    #[test]
    fn raw_hex_survives_invalid_layout_and_does_not_apply_demo_to_external_files() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("header.raw");
        std::fs::write(&path, b"TEST\x07\0\0\0trailing bytes").unwrap();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        workspace.open(path.clone(), &ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.yaml, workspace.language.starter());
        assert_eq!(
            workspace
                .bytes
                .as_ref()
                .unwrap()
                .view()
                .read(0, 256)
                .unwrap(),
            b"TEST\x07\0\0\0trailing bytes"
        );
        workspace.language = DecoderLanguage::Yaml;
        workspace.yaml = "invalid: layout".into();
        workspace.validate_layout(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.error);
        assert!(workspace.document.is_none());
        assert!(workspace.original.is_none());
        assert!(workspace
            .bytes
            .as_ref()
            .unwrap()
            .view()
            .read(0, 256)
            .is_ok());
        workspace.yaml = "endian: little\nsections:\n  - name: header\n    marker: '54 45 53 54'\n    fields:\n      - version: u32\n".into();
        workspace.saved_yaml = workspace.yaml.clone();
        workspace.validate_layout(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.has_layout(), "{}", workspace.message);
        assert_eq!(
            workspace.document.as_ref().unwrap().text(0, 0).unwrap(),
            "7"
        );
        workspace.select(0, 0);
        workspace.edit_text = "8".into();
        workspace.commit(&ctx);
        settle(&mut workspace, &ctx);
        // Closing and reopening a view changes no document state.
        let mut dock = crate::workspace_layout::WorkspaceLayout::default();
        dock.reveal(crate::workspace_layout::Pane::Table);
        dock.dock.remove_tab(
            dock.dock
                .find_tab(&crate::workspace_layout::Pane::Table)
                .unwrap(),
        );
        dock.reveal(crate::workspace_layout::Pane::Table);
        assert_eq!(
            workspace.document.as_ref().unwrap().text(0, 0).unwrap(),
            "8"
        );
        assert_eq!(workspace.selected.unwrap().byte_offset, 4);
        workspace.history(&ctx, false);
        settle(&mut workspace, &ctx);
        assert_eq!(
            workspace.document.as_ref().unwrap().text(0, 0).unwrap(),
            "7"
        );
        // Reusing a user's saved schema retains it for the next file.
        let yaml = workspace.yaml.clone();
        workspace.open(path, &ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.yaml, yaml);
        assert!(workspace.original.is_none());
        assert!(workspace.document.is_none());
    }

    #[test]
    fn raw_overwrites_keep_the_hex_caret_and_share_undo_without_a_snapshot() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.anything");
        std::fs::write(&path, [1, 2, 3, 4]).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(path.clone(), &ctx);
        settle(&mut workspace, &ctx);
        // Hex owns its caret and advances it before dispatching these actions.
        workspace.hex_editor.select_range(1..2, false);
        workspace.hex_actions(
            vec![
                HexAction::Replace {
                    offset: 0,
                    bytes: vec![0xAB],
                },
                HexAction::Selection(1..2),
            ],
            &ctx,
        );
        assert_eq!(workspace.hex_editor.selection(), Some(1..2));
        assert!(workspace.take_bytes_changed());
        workspace.hex_editor.select_range(2..3, false);
        workspace.hex_actions(
            vec![
                HexAction::Replace {
                    offset: 1,
                    bytes: vec![0xCD],
                },
                HexAction::Selection(2..3),
            ],
            &ctx,
        );
        assert_eq!(workspace.hex_editor.selection(), Some(2..3));
        assert_eq!(
            workspace.bytes.as_ref().unwrap().view().read(0, 4).unwrap(),
            [0xAB, 0xCD, 3, 4]
        );
        assert!(workspace.original.is_none());
        assert!(workspace.document.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), [1, 2, 3, 4]);
        workspace.history(&ctx, false);
        assert_eq!(
            workspace.bytes.as_ref().unwrap().view().read(0, 4).unwrap(),
            [0xAB, 2, 3, 4]
        );
        workspace.history(&ctx, false);
        assert!(!workspace.bytes.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn raw_marker_edits_invalidate_legacy_views_and_undo_revalidates() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.execute(Action::Demo, &ctx);
        settle(&mut workspace, &ctx);
        workspace.validate_layout(&ctx);
        settle(&mut workspace, &ctx);
        workspace.hex_actions(
            vec![HexAction::Replace {
                offset: 2416,
                bytes: vec![0],
            }],
            &ctx,
        );
        assert!(workspace.interpretation_stale);
        assert!(workspace.plot_data.is_none());
        settle(&mut workspace, &ctx);
        assert!(
            workspace.error,
            "the damaged footer marker must fail validation"
        );
        assert!(workspace.interpretation_stale);
        workspace.history(&ctx, false);
        settle(&mut workspace, &ctx);
        assert!(!workspace.error, "{}", workspace.message);
        assert!(!workspace.interpretation_stale);
        assert_eq!(
            workspace
                .bytes
                .as_ref()
                .unwrap()
                .view()
                .read(2416, 4)
                .unwrap(),
            [0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn generic_decoding_and_reinterpretation_use_working_bytes_without_snapshotting() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("described.raw");
        std::fs::write(&path, [2, 10, 20, 30]).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(path, &ctx);
        settle(&mut workspace, &ctx);
        workspace.yaml = "actions:\n  - read: {name: count, type: u8}\n  - read: {name: values, type: u8, repeat: {ref: count}}\n".into();
        workspace.validate_layout(&ctx);
        settle(&mut workspace, &ctx);
        assert!(workspace.structure_ui.has_program());
        assert!(workspace.take_layout_applied());
        assert!(workspace.original.is_none());
        workspace.hex_actions(
            vec![HexAction::Replace {
                offset: 0,
                bytes: vec![3],
            }],
            &ctx,
        );
        assert!(workspace.take_bytes_changed());
        settle(&mut workspace, &ctx);
        assert!(workspace.structure_ui.has_program());
        assert!(workspace.original.is_none());
        let export = dir.path().join("edited.raw");
        let view = workspace.bytes.as_ref().unwrap().view();
        workspace.save(
            &ctx,
            export.clone(),
            SaveContent::Bytes(view, 0..4, true),
            false,
        );
        settle(&mut workspace, &ctx);
        assert_eq!(std::fs::read(&export).unwrap(), [3, 10, 20, 30]);
        assert!(!workspace.bytes.as_ref().unwrap().is_dirty());
        workspace.history(&ctx, false);
        settle(&mut workspace, &ctx);
        assert_eq!(
            workspace.bytes.as_ref().unwrap().view().read(0, 1).unwrap(),
            [2]
        );
        assert!(workspace.bytes.as_ref().unwrap().is_dirty());
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn generic_plot_distant_selection_edit_history_and_streamed_export() {
        use crate::byte_store::READ_LIMIT;
        use crate::structure::Program;
        use std::io::{Seek, SeekFrom, Write};

        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("large.raw");
        let size = 40_000_000_000usize;
        let mut file = std::fs::File::create(&source).unwrap();
        // The logical address is genuinely beyond 32 bits without writing 40 GB in a test.
        crate::cache::set_fixture_len(&file, size as u64).unwrap();
        file.seek(SeekFrom::Start((size - 16) as u64)).unwrap();
        file.write_all(&17u64.to_le_bytes()).unwrap();
        file.write_all(&29u64.to_le_bytes()).unwrap();
        drop(file);
        let mut workspace = BinaryWorkspace::default();
        workspace.open(source.clone(), &ctx);
        settle(&mut workspace, &ctx);
        workspace.yaml = "structures:\n  - name: Sample\n    fields: [{name: a, type: u64}, {name: b, type: u64}]\nactions:\n  - read: {name: samples, type: {structure: Sample}, repeat: until end}\n".into();
        workspace.validate_layout(&ctx);
        settle(&mut workspace, &ctx);
        let view = workspace.bytes.as_ref().unwrap().view();
        let output = Program::parse(&workspace.yaml)
            .unwrap()
            .run(&view, &AtomicBool::new(false))
            .unwrap();
        let array = output.nodes[0].lazy.as_ref().unwrap();
        let last = array
            .page(&view, &AtomicBool::new(false), array.count - 1, 1)
            .unwrap();
        let field = &last[0].children[1];
        assert_eq!(field.range, size - 8..size);
        assert_eq!(field.numeric(), Some(29.));
        workspace.plot_action(PlotAction::InspectBytes(field.range.clone()), &ctx);
        assert_eq!(workspace.hex_editor.selection(), Some(field.range.clone()));
        assert!(workspace.take_reveal_hex());
        assert!(
            !workspace.take_reveal_hex(),
            "inspection opens Hex exactly once"
        );

        workspace.hex_actions(
            vec![HexAction::Replace {
                offset: field.range.start,
                bytes: 73u64.to_le_bytes().to_vec(),
            }],
            &ctx,
        );
        settle(&mut workspace, &ctx);
        assert!(workspace.structure_ui.has_program());
        assert!(
            workspace.original.is_none(),
            "editing must not snapshot the 40 GB file"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !workspace
            .structure_ui
            .highlights()
            .iter()
            .any(|highlight| highlight.range == field.range)
        {
            assert!(
                Instant::now() < deadline,
                "selected sample did not refresh after editing"
            );
            frame(&mut workspace, &ctx, vec![]);
            std::thread::sleep(Duration::from_millis(5));
        }
        workspace.history(&ctx, false);
        settle(&mut workspace, &ctx);
        assert_eq!(
            workspace
                .bytes
                .as_ref()
                .unwrap()
                .view()
                .read(size - 8, 8)
                .unwrap(),
            29u64.to_le_bytes()
        );
        workspace.history(&ctx, true);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.hex_editor.selection(), Some(field.range.clone()));
        assert_eq!(
            workspace
                .bytes
                .as_ref()
                .unwrap()
                .view()
                .read(size - 8, 8)
                .unwrap(),
            73u64.to_le_bytes()
        );

        // A multi-sample selection exceeds the read limit but exports in bounded chunks.
        let selected = size - READ_LIMIT - 32..size;
        workspace.plot_action(PlotAction::InspectBytes(selected.clone()), &ctx);
        let export = dir.path().join("selected.bin");
        let content = workspace.selection_export(selected.clone()).unwrap();
        workspace.save(&ctx, export.clone(), content, false);
        settle(&mut workspace, &ctx);
        assert!(!workspace.error, "{}", workspace.message);
        let actual = std::fs::read(export).unwrap();
        assert_eq!(actual.len(), selected.len());
        assert!(actual[..actual.len() - 16].iter().all(|byte| *byte == 0));
        assert_eq!(
            &actual[actual.len() - 16..actual.len() - 8],
            &17u64.to_le_bytes()
        );
        assert_eq!(&actual[actual.len() - 8..], &73u64.to_le_bytes());
        assert!(
            workspace.bytes.as_ref().unwrap().is_dirty(),
            "exporting a range does not save the document"
        );
        assert_eq!(
            FileSource::open(&source)
                .unwrap()
                .read(size - 8, 8)
                .unwrap(),
            29u64.to_le_bytes()
        );
        assert!(workspace.message.contains("Exported 1048608 bytes"));
    }

    #[test]
    fn plot_range_export_preserves_existing_files_on_errors() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.raw");
        let destination = dir.path().join("destination.raw");
        std::fs::write(&source, b"original bytes").unwrap();
        std::fs::write(&destination, b"keep destination").unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(source.clone(), &ctx);
        settle(&mut workspace, &ctx);
        workspace.hex_actions(
            vec![HexAction::Replace {
                offset: 0,
                bytes: b"EDIT".to_vec(),
            }],
            &ctx,
        );
        settle(&mut workspace, &ctx);
        assert!(workspace.selection_export(0..0).is_err());
        assert!(workspace.selection_export(0..100).is_err());
        let content = workspace.selection_export(0..4).unwrap();
        workspace.save(&ctx, source.clone(), content, true);
        settle(&mut workspace, &ctx);
        assert!(workspace.error);
        assert_eq!(std::fs::read(&source).unwrap(), b"original bytes");
        let content = workspace.selection_export(0..4).unwrap();
        workspace.save(&ctx, destination.clone(), content, false);
        settle(&mut workspace, &ctx);
        assert!(workspace.error);
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep destination");

        // A revision captured for export is still rejected if the on-disk source changes.
        let content = workspace.selection_export(0..4).unwrap();
        std::fs::write(&source, b"changed source length").unwrap();
        workspace.save(&ctx, destination.clone(), content, true);
        settle(&mut workspace, &ctx);
        assert!(workspace.error);
        assert!(workspace.message.contains("Source file changed"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep destination");
        assert!(workspace.selection_export(0..4).is_err());
    }

    #[test]
    fn legacy_table_shift_selection_spans_pages_and_keeps_dirty_edits() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        workspace.apply_definition(&ctx);
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.document.as_ref().unwrap().records, 200);
        workspace.select_table_record(3, 1, true, false);
        assert_eq!(workspace.byte_selection(), Some(52..64));
        workspace.select_table_record(130, 1, true, true);
        assert_eq!(workspace.byte_selection(), Some(52..1588));
        assert_eq!(workspace.table_anchor, Some(3));
        assert_eq!(workspace.page, 130 / ROWS_PER_PAGE);
        workspace.select_table_record(1, 1, true, true);
        assert_eq!(workspace.byte_selection(), Some(28..64));
        workspace.select_table_record(4, 1, false, false);
        assert_eq!(workspace.byte_selection(), Some(68..70));
        workspace.select_table_record(7, 1, false, true);
        assert_eq!(workspace.byte_selection(), Some(64..112));
        workspace.edit_text = "1234".into();
        let selected = workspace.selected;
        workspace.select_table_record(9, 1, true, true);
        assert_eq!(workspace.selected, selected);
        assert_eq!(workspace.byte_selection(), Some(64..112));
        assert_eq!(workspace.table_anchor, Some(4));
        assert!(workspace.error);
        workspace.edit_text.clone_from(&workspace.edit_original);
        workspace.select_byte_range(40..52, true);
        assert!(workspace.selected.is_none());
        workspace.select_table_record(7, 1, true, true);
        assert_eq!(workspace.byte_selection(), Some(40..112));
        workspace.select_byte_range(0..4, true); // Header bytes are outside sample records.
        workspace.select_table_record(5, 1, true, true);
        assert_eq!(workspace.byte_selection(), Some(76..88));
        workspace.reset_section_view();
        assert!(workspace.table_anchor.is_none());
    }

    #[test]
    fn hex_offsets_are_absolute_bounded_and_accept_decimal_or_hex() {
        assert_eq!(hex_offset(" 0x100 ", &(16..512)).unwrap(), 256);
        assert_eq!(hex_offset("256", &(16..512)).unwrap(), 256);
        assert_eq!(hex_offset("0X1FF", &(16..512)).unwrap(), 511);
        for invalid in [
            "-1",
            "15",
            "512",
            "0xGG",
            "",
            "999999999999999999999999999999999",
        ] {
            assert!(hex_offset(invalid, &(16..512)).is_err(), "{invalid}");
        }
        assert!(hex_offset("0", &(0..0)).is_err());
    }

    fn resolve_demo(yaml: &str) -> ResolvedLayout {
        Layout::parse(yaml)
            .unwrap()
            .resolve(
                binary::DEMO_BYTES.len(),
                |i| binary::DEMO_BYTES[i],
                &LoadProgress::default(),
            )
            .unwrap()
    }

    fn frame(
        workspace: &mut BinaryWorkspace,
        ctx: &Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400., 950.),
            )),
            events,
            ..Default::default()
        };
        ctx.run(input, |ctx| {
            workspace.tick(ctx);
            egui::TopBottomPanel::top("test_file_toolbar").show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    workspace.file_toolbar(ui);
                    workspace.edit_toolbar(ui);
                });
            });
            egui::SidePanel::left("test_controls")
                .exact_width(350.)
                .show(ctx, |ui| workspace.controls(ui));
            egui::CentralPanel::default().show(ctx, |ui| workspace.content(ui));
        })
    }

    fn settle(workspace: &mut BinaryWorkspace, ctx: &Context) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            frame(workspace, ctx, vec![]);
            if !workspace.busy()
                && !workspace.dialog_pending()
                && workspace.plot_rx.is_none()
                && workspace.refresh_at.is_none()
                && workspace.reparse_at.is_none()
                && !workspace.structure_ui.is_parsing()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not settle: {}",
                workspace.message
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        frame(workspace, ctx, vec![]);
    }

    fn click_at(workspace: &mut BinaryWorkspace, ctx: &Context, pos: egui::Pos2) {
        frame(workspace, ctx, vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            frame(
                workspace,
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

    fn click(workspace: &mut BinaryWorkspace, ctx: &Context, label: &str) {
        frame(workspace, ctx, vec![]);
        let output = frame(workspace, ctx, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::epaint::Shape::Text(text) = &shape.shape {
                    if text.galley.text() == label {
                        return Some(text.pos + text.galley.rect.center().to_vec2());
                    }
                }
                None
            })
            .unwrap_or_else(|| panic!("UI text {label:?} not found"));
        click_at(workspace, ctx, pos);
    }

    #[test]
    fn egui_demo_point_selection_edit_history_and_export_workflow() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        click(&mut workspace, &ctx, "Open demo");
        settle(&mut workspace, &ctx);
        assert!(workspace.original.is_some());
        assert!(workspace.document.is_none(), "layout must require Apply");
        click(&mut workspace, &ctx, "Decode file");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.document.as_ref().unwrap().records, 200);
        let spike = workspace
            .last_transform
            .as_ref()
            .unwrap()
            .position_from_point(&PlotPoint::new(73., 85.));
        click_at(&mut workspace, &ctx, spike);
        assert_eq!(
            workspace.selected,
            workspace
                .document
                .as_ref()
                .unwrap()
                .layout
                .identity(73, 1)
                .ok()
        );
        assert_eq!(workspace.edit_text, "85");
        assert_eq!(
            workspace
                .document
                .as_ref()
                .unwrap()
                .layout
                .field_range(73, 1)
                .unwrap(),
            896..898
        );

        // Focus the actual table TextEdit, select its contents, and send text input.
        click(&mut workspace, &ctx, "85");
        frame(
            &mut workspace,
            &ctx,
            vec![
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                        ctrl: true,
                        command: true,
                        ..Default::default()
                    },
                },
                egui::Event::Text("23.45".into()),
            ],
        );
        assert_eq!(workspace.edit_text, "23.45");
        click(&mut workspace, &ctx, "Commit value");
        settle(&mut workspace, &ctx);
        assert_eq!(
            workspace.document.as_ref().unwrap().text(73, 1).unwrap(),
            "23.45"
        );
        assert!(workspace
            .plot_data
            .as_ref()
            .unwrap()
            .samples
            .iter()
            .any(|s| s.record == 73 && s.point[1] == 23.45));
        click(&mut workspace, &ctx, "Undo");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.edit_text, "85");
        click(&mut workspace, &ctx, "Redo");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.edit_text, "23.45");

        click(&mut workspace, &ctx, "74 / 0x388");
        assert_eq!(
            workspace.selected,
            workspace
                .document
                .as_ref()
                .unwrap()
                .layout
                .identity(74, 1)
                .ok()
        );
        click(&mut workspace, &ctx, "X: Record index");
        click(&mut workspace, &ctx, "timestamp");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.x, Some(0));
        // Record 73 has the same timestamp as 70 and 72. Restore its spike, then
        // click it using the real plot transform with timestamp on the X axis.
        click(&mut workspace, &ctx, "Undo");
        settle(&mut workspace, &ctx);
        let spike = workspace
            .last_transform
            .as_ref()
            .unwrap()
            .position_from_point(&PlotPoint::new(1700000070., 85.));
        workspace.selected = None;
        click_at(&mut workspace, &ctx, spike);
        assert_eq!(
            workspace.selected,
            workspace
                .document
                .as_ref()
                .unwrap()
                .layout
                .identity(73, 1)
                .ok()
        );
        click(&mut workspace, &ctx, "Redo");
        settle(&mut workspace, &ctx);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edited.bin");
        let snapshot = workspace.document.as_ref().unwrap().bytes.clone();
        // Native picker is deliberately bypassed; exercise the real asynchronous save job.
        workspace.save(&ctx, path.clone(), SaveContent::Binary(snapshot), false);
        settle(&mut workspace, &ctx);
        let mut expected = binary::DEMO_BYTES.to_vec();
        expected[896..898].copy_from_slice(&2345i16.to_le_bytes());
        assert_eq!(std::fs::read(path).unwrap(), expected);
        assert!(!workspace.document.as_ref().unwrap().is_dirty());
    }

    #[test]
    fn egui_sections_opaque_hex_cross_section_undo_and_failed_apply() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        click(&mut workspace, &ctx, "Open demo");
        settle(&mut workspace, &ctx);
        click(&mut workspace, &ctx, "Decode file");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.document.as_ref().unwrap().section, 1);
        workspace.select(73, 1);
        workspace.edit_text = "23.45".into();
        // Pending text prevents a section switch, preserving the exact identity.
        workspace.change_section(2, &ctx);
        assert_eq!(workspace.document.as_ref().unwrap().section, 1);
        assert_eq!(workspace.selected.unwrap().byte_offset, 896);
        workspace.commit(&ctx);
        settle(&mut workspace, &ctx);
        click(&mut workspace, &ctx, "Record layout");
        click(&mut workspace, &ctx, "Section: samples");
        click(&mut workspace, &ctx, "footer");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.document.as_ref().unwrap().section, 2);
        assert!(workspace.selected.is_none());
        assert_eq!(workspace.plot_data.as_ref().unwrap().samples.len(), 1);
        let footer_point = workspace
            .last_transform
            .as_ref()
            .unwrap()
            .position_from_point(&PlotPoint::new(0., 305419896.));
        click_at(&mut workspace, &ctx, footer_point);
        assert_eq!(
            workspace.selected.unwrap(),
            ValueIdentity {
                section: 2,
                record: 0,
                field: 0,
                byte_offset: 2420,
                byte_length: 4,
            }
        );
        workspace.edit_text = "42".into();
        workspace.commit(&ctx);
        settle(&mut workspace, &ctx);
        click(&mut workspace, &ctx, "Undo");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.selected.unwrap().section, 2);
        click(&mut workspace, &ctx, "Undo");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.selected.unwrap().section, 1);
        assert_eq!(workspace.edit_text, "85");
        click(&mut workspace, &ctx, "Redo");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.edit_text, "23.45");
        let selected = workspace.selected;
        let active = workspace.document.as_ref().unwrap().structure.clone();
        workspace.yaml = binary::DEMO_YAML.replace("DE AD BE EF", "00 00 00 00");
        click(&mut workspace, &ctx, "Decode file");
        settle(&mut workspace, &ctx);
        assert!(workspace.error);
        assert!(workspace.message.contains("marker"));
        assert!(
            workspace.pending.is_none(),
            "invalid edits should fail validation before asking to replace a layout"
        );
        assert!(Arc::ptr_eq(
            &active,
            &workspace.document.as_ref().unwrap().structure
        ));
        assert_eq!(workspace.selected, selected);
        assert!(workspace.bytes.as_ref().unwrap().can_undo());
        workspace.yaml =
            "endian: little\nsections: [{name: broken, bytes: 1, compression: gzip}]".into();
        click(&mut workspace, &ctx, "Decode file");
        assert!(workspace
            .message
            .contains("compressed sections are not supported yet"));
        assert_eq!(workspace.selected, selected);
        click(&mut workspace, &ctx, "Section: samples");
        click(&mut workspace, &ctx, "header");
        settle(&mut workspace, &ctx);
        assert_eq!(workspace.document.as_ref().unwrap().section, 0);
        assert!(workspace.plot_data.is_none());
        assert!(workspace.selected.is_none());
        // Hex reads directly from the shared bytes even for an opaque section.
        assert_eq!(
            workspace
                .bytes
                .as_ref()
                .unwrap()
                .view()
                .read(0, 16)
                .unwrap(),
            binary::DEMO_BYTES[..16]
        );
        workspace.hex_editor.select_range(0..1, true);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let output = frame(&mut workspace, &ctx, vec![]);
            if output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.text() == "50")) { break; }
            assert!(
                Instant::now() < deadline,
                "asynchronous Hex should render the opaque header"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        workspace
            .hex_editor
            .select_range(0..binary::DEMO_BYTES.len(), true);
        assert_eq!(
            workspace.hex_editor.selection(),
            Some(0..binary::DEMO_BYTES.len())
        );
    }

    #[test]
    fn replacing_a_file_is_guarded_and_applying_a_layout_keeps_byte_history() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.execute(Action::Demo, &ctx);
        settle(&mut workspace, &ctx);
        workspace.execute(
            Action::Apply(resolve_demo(binary::DEMO_YAML), binary::DEMO_YAML.into()),
            &ctx,
        );
        settle(&mut workspace, &ctx);
        workspace.select(73, 1);
        workspace.edit_text = "23.45".into();
        workspace.commit(&ctx);
        workspace.request(Action::Demo, &ctx);
        assert!(workspace.pending.is_some());
        assert!(!workspace.busy());
        click(&mut workspace, &ctx, "Cancel");
        assert_eq!(
            workspace.document.as_ref().unwrap().text(73, 1).unwrap(),
            "23.45"
        );
        let yaml = binary::DEMO_YAML.replace("temperature", "sensor");
        workspace.request(Action::Apply(resolve_demo(&yaml), yaml), &ctx);
        assert!(
            workspace.pending.is_none(),
            "reinterpreting bytes preserves edits and history"
        );
        settle(&mut workspace, &ctx);
        let doc = workspace.document.as_ref().unwrap();
        assert_eq!(doc.text(73, 1).unwrap(), "23.45");
        assert!(doc.is_dirty());
        assert!(workspace.bytes.as_ref().unwrap().can_undo());
        workspace.history(&ctx, false);
        settle(&mut workspace, &ctx);
        assert_eq!(
            workspace.document.as_ref().unwrap().text(73, 1).unwrap(),
            "85"
        );
    }

    #[test]
    fn uncommitted_values_close_guard_and_overwrite_confirmation() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.execute(Action::Demo, &ctx);
        settle(&mut workspace, &ctx);
        workspace.execute(
            Action::Apply(resolve_demo(binary::DEMO_YAML), binary::DEMO_YAML.into()),
            &ctx,
        );
        settle(&mut workspace, &ctx);
        workspace.select(73, 1);
        workspace.edit_text = "23.45".into();
        workspace.select(74, 1);
        assert_eq!(
            workspace.selected,
            workspace
                .document
                .as_ref()
                .unwrap()
                .layout
                .identity(73, 1)
                .ok(),
            "pending text must not be lost by selection"
        );
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let output = ctx.run(input, |ctx| workspace.tick(ctx));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::CancelClose));
        assert!(matches!(workspace.pending, Some(Action::Close)));
        workspace.pending = None;
        workspace.commit(&ctx);
        settle(&mut workspace, &ctx);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.bin");
        std::fs::write(&path, b"existing contents").unwrap();
        workspace.overwrite = Some((
            path.clone(),
            SaveContent::Binary(workspace.document.as_ref().unwrap().bytes.clone()),
        ));
        click(&mut workspace, &ctx, "Cancel");
        assert_eq!(std::fs::read(&path).unwrap(), b"existing contents");
        workspace.overwrite = Some((
            path.clone(),
            SaveContent::Binary(workspace.document.as_ref().unwrap().bytes.clone()),
        ));
        click(&mut workspace, &ctx, "Overwrite this file");
        settle(&mut workspace, &ctx);
        let mut expected = binary::DEMO_BYTES.to_vec();
        expected[896..898].copy_from_slice(&2345i16.to_le_bytes());
        assert_eq!(std::fs::read(&path).unwrap(), expected);

        let yaml_path = dir.path().join("layout.yaml");
        workspace.save(
            &ctx,
            yaml_path.clone(),
            SaveContent::Yaml(workspace.yaml.clone(), workspace.language),
            false,
        );
        settle(&mut workspace, &ctx);
        assert_eq!(
            std::fs::read_to_string(yaml_path).unwrap(),
            binary::DEMO_YAML
        );
    }
    #[test]
    fn describe_selection_runs_directly_and_opens_decoded_samples() {
        let ctx = Context::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selection.any");
        std::fs::write(&path, [42, 0]).unwrap();
        let mut workspace = BinaryWorkspace::default();
        workspace.open(path, &ctx);
        settle(&mut workspace, &ctx);
        workspace.hex_actions(vec![HexAction::Define(0..2)], &ctx);
        click(&mut workspace, &ctx, "Decode selection");
        settle(&mut workspace, &ctx);
        assert!(workspace.structure_ui.has_program());
        assert!(!workspace.error, "{}", workspace.message);
        assert!(
            workspace.take_layout_applied(),
            "The new samples must be revealed without a separate Apply"
        );
        assert!(
            workspace.original.is_none(),
            "Describing bytes must not snapshot the source"
        );
        let output = crate::structure::Program::parse(&workspace.yaml)
            .unwrap()
            .run(
                &workspace.bytes.as_ref().unwrap().view(),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            output.nodes[0].children[0].value,
            Some(crate::structure::Value::Unsigned(42))
        );
        click(&mut workspace, &ctx, "Decode selection");
        settle(&mut workspace, &ctx);
        assert!(!workspace.error, "{}", workspace.message);
        assert_eq!(
            crate::structure::Program::parse(&workspace.yaml)
                .unwrap()
                .actions
                .len(),
            1
        );
    }
    #[test]
    fn byte_jumps_update_the_shared_decoded_selection_and_history() {
        let ctx = Context::default();
        let mut workspace = BinaryWorkspace::default();
        workspace.open_demo(&ctx);
        settle(&mut workspace, &ctx);
        workspace.validate_layout(&ctx);
        settle(&mut workspace, &ctx);
        let enter = || egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        for (jump, offset, record) in [("0x380", 896, 73), ("+12", 908, 74), ("-12", 896, 73)] {
            workspace.jump_to_byte();
            frame(&mut workspace, &ctx, vec![]);
            frame(&mut workspace, &ctx, vec![egui::Event::Text(jump.into())]);
            frame(&mut workspace, &ctx, vec![enter()]);
            frame(&mut workspace, &ctx, vec![]);
            assert_eq!(
                workspace.hex_editor.selection(),
                Some(offset..offset + 1),
                "{jump}"
            );
            assert_eq!(
                workspace.selected,
                workspace
                    .document
                    .as_ref()
                    .unwrap()
                    .layout
                    .identity(record, 1)
                    .ok(),
                "{jump} must select its decoded field too"
            );
        }
        click(&mut workspace, &ctx, "Back");
        assert_eq!(workspace.hex_editor.selection(), Some(908..909));
        assert_eq!(
            workspace.selected,
            workspace
                .document
                .as_ref()
                .unwrap()
                .layout
                .identity(74, 1)
                .ok()
        );
        click(&mut workspace, &ctx, "Forward");
        assert_eq!(workspace.hex_editor.selection(), Some(896..897));
    }
}
