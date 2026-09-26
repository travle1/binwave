#[cfg(test)]
use crate::egui_compat::ContextRun as _;
pub use crate::source_editor::DecoderLanguage;
use crate::source_editor::YamlLayoutCache;
#[cfg(test)]
use crate::source_editor::{source_layout, toggle_source_comments};
// Workspace state for generic definitions. Parsing runs against immutable byte revisions.
use crate::byte_store::ByteView;
use crate::field_preview::{FieldPreview, PreviewField};
use crate::hex_editor::HexHighlight;
use crate::structure::{
    Action, Define, Endian, Expr, Field, FieldMapping, Node, ParseOutput, Program, Reference,
    Repeat, Structure, TypeChoice, Value,
};
use crate::structure_views::{PlotAction, StructureViews};
use anyhow::{bail, Context as _, Result};
use egui::{Color32, Context, Ui};
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
    Arc,
};
use std::time::{Duration, Instant};

pub use crate::field_edit::{editable_value, encode_value};
use crate::field_edit::primitive_size;

pub fn has_decoder_code(source: &str) -> bool {
    source.lines().any(|line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#')
    })
}

#[derive(Clone)]
struct PythonSource {
    code: String,
    path: Option<PathBuf>,
}
#[derive(Clone)]
enum RunSource {
    Yaml(Program),
    Python(PythonSource),
}
impl RunSource {
    fn language(&self) -> DecoderLanguage {
        match self {
            Self::Yaml(_) => DecoderLanguage::Yaml,
            Self::Python(_) => DecoderLanguage::Python,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Published,
    NoData,
    Failed(String),
    Cancelled,
}
struct Decoded {
    data: Option<(Program, ParseOutput)>,
    stdout: String,
    partial_error: Option<String>,
}
type Parsed = (u64, Result<Decoded, String>);
#[derive(Default)]
pub struct StructureWorkspace {
    program: Option<Program>,
    python_source: Option<PythonSource>,
    python_stdout: String,
    output: Option<ParseOutput>,
    partial_output: Option<ParseOutput>,
    receiver: Option<Receiver<Parsed>>,
    cancel: Arc<AtomicBool>,
    error: Option<String>,
    started: Option<Instant>,
    pending_revision: Option<u64>,
    views: StructureViews,
    builder: Builder,
    selection: Option<Range<usize>>,
    reveal: bool,
    editor_page: EditorPage,
    yaml_layout: YamlLayoutCache,
    steps: StepsEditor,
    formats: FormatEditor,
    decoded_ready: bool,
    explicit_apply_pending: bool,
    pending_source: Option<RunSource>,
    interpretation_check: Option<crate::direct_authoring::Interpretation>,
    last_run_language: Option<DecoderLanguage>,
    run_finished: Option<RunOutcome>,
    source: Option<ByteView>,
    run_requested: bool,
    console_byte_jump: Option<u64>,
}
impl Drop for StructureWorkspace {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl StructureWorkspace {
    pub fn is_generic(yaml: &str) -> bool {
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml)
            .ok()
            .and_then(|v| {
                v.as_mapping().map(|m| {
                    m.contains_key(serde_yaml_ng::Value::String("actions".into()))
                        || m.contains_key(serde_yaml_ng::Value::String("structures".into()))
                })
            })
            .unwrap_or_else(|| {
                yaml.lines()
                    .any(|l| l.starts_with("actions:") || l.starts_with("structures:"))
            })
    }
    #[cfg(test)]
    pub fn has_program(&self) -> bool {
        self.program.is_some() || self.python_source.is_some() || self.pending_source.is_some()
    }
    pub fn has_published_program(&self) -> bool {
        self.program.is_some() || self.python_source.is_some()
    }
    pub fn has_output(&self) -> bool {
        self.output.is_some() || self.partial_output.is_some()
    }
    pub fn has_partial_output(&self) -> bool {
        self.partial_output.is_some()
    }
    pub fn has_successful_output(&self) -> bool {
        self.output.is_some()
    }
    pub fn decode_error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn is_parsing(&self) -> bool {
        self.receiver.is_some()
    }
    pub fn is_ready(&self) -> bool {
        self.output.is_some() && !self.is_parsing()
    }
    /// Stop a worker without changing the last published data or its view state.
    fn stop_worker(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.receiver = None;
        self.started = None;
        self.pending_revision = None;
        self.explicit_apply_pending = false;
        self.pending_source = None;
        self.decoded_ready = false;
        self.run_finished = None;
    }
    pub fn clear_program(&mut self) {
        self.stop_worker();
        self.program = None;
        self.python_source = None;
        self.python_stdout.clear();
        self.last_run_language = None;
        self.output = None;
        self.partial_output = None;
        self.error = None;
        self.views.reset();
    }
    pub fn clear_file(&mut self) {
        *self = Self::default();
    }
    /// Direct edits refresh results without requesting a tab or viewport change.
    pub fn keep_navigation(&mut self) {
        self.explicit_apply_pending = false;
        self.decoded_ready = false;
    }
    pub fn apply_interpretation(&mut self, yaml: &str, source: ByteView, ctx: &Context, check: crate::direct_authoring::Interpretation) -> bool {
        self.interpretation_check = Some(check);
        let result = self.apply(yaml, source, ctx);
        self.interpretation_check = None;
        result
    }
    pub fn apply(&mut self, yaml: &str, source: ByteView, ctx: &Context) -> bool {
        self.stop_worker();
        self.partial_output = None;
        self.invalidate_changed_revision(source.revision);
        self.last_run_language = Some(DecoderLanguage::Yaml);
        match Program::parse(yaml) {
            Ok(program) => {
                self.start_run(RunSource::Yaml(program), source, ctx, true);
                true
            }
            Err(error) => {
                let error = format!("{error:#}");
                self.run_finished = Some(RunOutcome::Failed(error.clone()));
                self.error = Some(error);
                false
            }
        }
    }
    pub fn apply_python(
        &mut self,
        code: &str,
        script_path: Option<&Path>,
        source: ByteView,
        ctx: &Context,
    ) -> bool {
        self.start_run(
            RunSource::Python(PythonSource {
                code: code.into(),
                path: script_path.map(Path::to_path_buf),
            }),
            source,
            ctx,
            true,
        );
        true
    }
    pub fn cancel_decoding(&mut self) {
        let explicit = self.explicit_apply_pending;
        let python = self.last_run_language == Some(DecoderLanguage::Python);
        self.stop_worker();
        self.error = None;
        if python {
            self.python_stdout = "Script stopped.".into();
        }
        if explicit {
            self.run_finished = Some(RunOutcome::Cancelled);
        }
    }
    pub fn reparse(&mut self, source: ByteView, ctx: &Context) {
        // Byte edits rerun only the last published decoder. An interrupted
        // candidate must be run explicitly against the changed bytes.
        let decoder = self
            .python_source
            .clone()
            .map(RunSource::Python)
            .or_else(|| self.program.clone().map(RunSource::Yaml));
        self.invalidate_changed_revision(source.revision);
        if let Some(decoder) = decoder {
            self.start_run(decoder, source, ctx, false);
        }
    }
    fn start_run(&mut self, decoder: RunSource, source: ByteView, ctx: &Context, explicit: bool) {
        self.stop_worker();
        self.partial_output = None;
        self.invalidate_changed_revision(source.revision);
        self.error = None;
        self.last_run_language = Some(decoder.language());
        if decoder.language() == DecoderLanguage::Python {
            self.python_stdout.clear();
        }
        self.pending_source = Some(decoder.clone());
        self.explicit_apply_pending = explicit;
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.started = Some(Instant::now());
        self.pending_revision = Some(source.revision);
        let ctx = ctx.clone();
        let interpretation_check = self.interpretation_check.take();
        std::thread::spawn(move || {
            let result = source
                .validate()
                .and_then(|_| {
                    let decoded = match decoder {
                        RunSource::Python(python) => {
                            let result = crate::python_decoder::run(
                                &python.code,
                                python.path.as_deref(),
                                &source,
                                &cancel,
                            )?;
                            Decoded {
                                data: result.decoded,
                                stdout: bounded_console(&result.stdout),
                                partial_error: None,
                            }
                        }
                        RunSource::Yaml(program) => {
                            let (output, partial_error) = match program.run_with_partial(&source, &cancel) {
                                Ok(output) => (output, None),
                                Err(failure) => (failure.output, Some(bounded_console(&format!("{:#}", failure.error)))),
                            };
                            if partial_error.is_none() {
                                if let Some(check) = &interpretation_check { check.validate(&output)?; }
                            }
                            Decoded {
                                data: Some((program, output)),
                                stdout: String::new(),
                                partial_error,
                            }
                        }
                    };
                    source.validate()?;
                    Ok(decoded)
                })
                .map_err(|error| bounded_console(&format!("{error:#}")));
            if !cancel.load(Ordering::Relaxed) {
                let _ = sender.send((source.revision, result));
                ctx.request_repaint();
            }
        });
    }
    fn invalidate_changed_revision(&mut self, revision: u64) {
        if self
            .pending_revision
            .is_some_and(|pending| pending != revision)
            || self
                .output
                .as_ref()
                .is_some_and(|output| output.revision != revision)
        {
            self.invalidate();
        }
    }
    pub fn invalidate(&mut self) {
        self.stop_worker();
        self.output = None;
        self.partial_output = None;
        self.views.invalidate();
    }
    pub fn tick(&mut self, ctx: &Context, source: ByteView) {
        self.source = Some(source.clone());
        self.invalidate_changed_revision(source.revision);
        if let Some(rx) = &self.receiver {
            match rx.try_recv() {
                Ok((revision, result)) => {
                    let explicit = self.explicit_apply_pending;
                    let decoder = self.pending_source.take();
                    self.receiver = None;
                    self.started = None;
                    self.pending_revision = None;
                    self.explicit_apply_pending = false;
                    if revision == source.revision {
                        match result {
                            Ok(decoded) => {
                                if let Some(error) = decoded.partial_error {
                                    self.partial_output = decoded.data.map(|(_, output)| output);
                                    if let Some(output) = &self.partial_output {
                                        let mut current = output.nodes.last();
                                        let mut in_preview = false;
                                        while let Some(node) = current {
                                            if node.lazy.is_some() {
                                                self.views.expand_first_page(node, output.revision);
                                                in_preview = true;
                                                current = node.children.first();
                                                continue;
                                            }
                                            if node.children.is_empty() { break; }
                                            self.views.expand(&node.path);
                                            current = if in_preview { node.children.first() } else { node.children.last() };
                                        }
                                    }
                                    self.error = Some(error.clone());
                                    self.decoded_ready = false;
                                    if explicit {
                                        self.run_finished = Some(RunOutcome::Failed(error));
                                    }
                                } else {
                                    self.error = None;
                                    self.partial_output = None;
                                    if self.last_run_language == Some(DecoderLanguage::Python) {
                                        self.python_stdout = if decoded.stdout.is_empty() {
                                            "Script finished. No output.".into()
                                        } else {
                                            decoded.stdout
                                        };
                                    }
                                    let outcome = if let Some((program, output)) = decoded.data {
                                        // Explicit successful replacement starts a new view;
                                        // byte-edit refresh retains viewport and selection.
                                        if explicit {
                                            self.views.reset();
                                        }
                                        self.program = Some(program);
                                        self.python_source = match decoder {
                                            Some(RunSource::Python(python)) => Some(python),
                                            _ => None,
                                        };
                                        self.output = Some(output);
                                        self.partial_output = None;
                                        self.decoded_ready = explicit;
                                        RunOutcome::Published
                                    } else {
                                        RunOutcome::NoData
                                    };
                                    if explicit {
                                        self.run_finished = Some(outcome);
                                    }
                                }
                            }
                            Err(error) => {
                                if explicit {
                                    self.run_finished = Some(RunOutcome::Failed(error.clone()));
                                }
                                self.error = Some(error);
                            }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    let explicit = self.explicit_apply_pending;
                    self.stop_worker();
                    let error = "Definition worker stopped before returning a result.".to_owned();
                    if explicit {
                        self.run_finished = Some(RunOutcome::Failed(error.clone()));
                    }
                    self.error = Some(error);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(30))
                }
            }
        }
        self.views.tick(ctx, source);
        if let Some(output) = self.partial_output.as_ref().or(self.output.as_ref()) {
            self.views.select_bytes(output, self.selection.clone());
        }
    }
    /// Completion of an explicit attempt, independently of whether it published data.
    pub fn take_run_finished(&mut self) -> Option<RunOutcome> {
        self.run_finished.take()
    }
    pub fn begin_definition(&mut self, range: Range<usize>) {
        self.formats = FormatEditor::default();
        self.builder = Builder::from_range(range.clone());
        self.selection = Some(range);
        self.editor_page = EditorPage::Structures;
    }
    /// One successful explicit Apply requests the decoded-data view; background refreshes do not.
    pub fn take_decoded(&mut self) -> bool {
        std::mem::take(&mut self.decoded_ready)
    }
    #[cfg(test)]
    pub fn is_describing(&self) -> bool {
        self.editor_page == EditorPage::Structures && self.builder.active
    }
    pub fn take_run_requested(&mut self) -> bool {
        std::mem::take(&mut self.run_requested)
    }
    pub fn take_console_byte_jump(&mut self) -> Option<u64> {
        self.console_byte_jump.take()
    }
    pub fn show_steps(&mut self) {
        self.editor_page = EditorPage::Steps;
    }
    pub fn show_yaml(&mut self) {
        self.editor_page = EditorPage::Yaml;
    }
    pub fn authoring_dirty(&self) -> bool {
        self.builder.dirty() || self.steps.dirty() || self.formats.dirty()
    }
    pub fn show_authoring_draft(&mut self) {
        if self.steps.dirty() {
            self.editor_page = EditorPage::Steps;
        } else if self.builder.dirty() || self.formats.dirty() {
            self.editor_page = EditorPage::Structures;
        }
    }
    /// Called only after an explicitly approved replacement of the YAML document.
    pub fn reset_authoring(&mut self) {
        self.builder = Builder::default();
        self.steps = StepsEditor::default();
        self.formats = FormatEditor::default();
        self.error = None;
    }
    pub fn take_reveal(&mut self) -> bool {
        std::mem::take(&mut self.reveal)
    }
    pub fn select_bytes(&mut self, range: Range<usize>) {
        self.selection = Some(range.clone());
        if let Some(output) = self.partial_output.as_ref().or(self.output.as_ref()) {
            self.views.select_bytes(output, Some(range));
        }
    }
    pub fn highlights(&self) -> Vec<HexHighlight> {
        self.partial_output
            .as_ref()
            .or(self.output.as_ref())
            .map(|o| self.views.highlights(o))
            .unwrap_or_default()
    }
    fn status(&mut self, ui: &mut Ui) {
        if self.is_parsing() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!(
                    "Decoding structure… {:.1}s",
                    self.started
                        .map(|at| at.elapsed().as_secs_f64())
                        .unwrap_or(0.)
                ));
                if ui.small_button("Cancel").clicked() {
                    self.cancel_decoding();
                }
            });
        }
        if let Some(error) = &self.error {
            if self.last_run_language == Some(DecoderLanguage::Python) {
                ui.colored_label(
                    Color32::LIGHT_RED,
                    error.lines().last().unwrap_or("Python decoder failed."),
                );
            } else {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
        }
    }
    pub fn tree(&mut self, ui: &mut Ui, selection: Option<Range<usize>>, names: &mut dyn FnMut(&mut Ui, &Node) -> egui::Response) -> Option<Range<usize>> {
        self.selection = selection.clone();
        if self.has_partial_output() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Partial decode · completed sections remain inspectable. The Console shows where parsing stopped.",
            );
        }
        let selected = self
            .partial_output
            .as_ref()
            .or(self.output.as_ref())
            .and_then(|output| self.views.tree(ui, output, selection, names));
        if let Some(range) = &selected {
            self.selection = Some(range.clone());
        }
        selected
    }
    pub fn table(&mut self, ui: &mut Ui, source: ByteView) -> Option<Range<usize>> {
        let selected = self
            .output
            .as_ref()
            .and_then(|output| self.views.table(ui, output, source));
        if let Some(range) = &selected {
            self.selection = Some(range.clone());
        }
        selected
    }
    pub fn plot(&mut self, ui: &mut Ui, source: ByteView) -> Option<Range<usize>> {
        let selected = self
            .output
            .as_ref()
            .and_then(|output| self.views.plot(ui, output, source));
        if let Some(range) = &selected {
            self.selection = Some(range.clone());
        }
        selected
    }
    pub fn take_plot_action(&mut self) -> Option<PlotAction> {
        self.views.take_plot_action()
    }
    pub fn expand_root(&mut self, name: &str) {
        if let Some(node) = self.output.as_ref().and_then(|o| o.nodes.iter().find(|n| n.name == name)) { self.views.expand(&node.path); }
    }
    pub fn root_range(&self, name: &str) -> Option<Range<usize>> {
        self.output.as_ref()?.nodes.iter().find(|n| n.name == name).map(|n| n.range.clone())
    }
    pub fn code_action(&mut self, action: &'static str) { self.yaml_layout.command(action); }
    pub fn editor_tab_width(&self) -> usize { self.yaml_layout.tab_width() }
    pub fn set_editor_tab_width(&mut self, width: usize) {
        self.yaml_layout.set_tab_width(width);
    }
    pub fn selected_node(&self) -> Option<Node> {
        self.partial_output
            .as_ref()
            .or(self.output.as_ref())
            .and_then(|output| self.views.selected_node(output))
    }

    pub fn selected_sample(&self) -> Option<(usize, Range<usize>)> {
        let field = self.selected_node()?;
        crate::structure_selection::sample_for_node(&self.partial_output.as_ref().or(self.output.as_ref())?.nodes, &field)
    }

    /// Detailed parser information stays behind the inspector's disclosure control.
    pub fn inspector(&mut self, ui: &mut Ui, language: DecoderLanguage) {
        let Some(node) = self.selected_node() else {
            return;
        };
        let definition = self.partial_output.as_ref().or(self.output.as_ref()).map(|output|
            (output.consumed as u64, output.definitions.len()));
        let field = crate::workspace_inspector::FieldInfo::from_node(&node, None);
        crate::workspace_inspector::field_details(ui, &field, definition);
        if language == DecoderLanguage::Python {
            return;
        }
        if matches!(node.value, Some(Value::Signed(_) | Value::Unsigned(_)))
            && ui.button("Use this value for…").clicked()
        {
            self.builder.reference = node.path.clone();
            self.builder.active = true;
            self.editor_page = EditorPage::Structures;
            self.reveal = true;
        }
        if (node.lazy.is_some() || !node.children.is_empty())
            && ui.button("Build a type from these descriptions…").clicked()
        {
            self.builder = Builder::from_range(node.range.end..node.range.end);
            self.builder.name = "Discovered".into();
            self.builder.instance = "payload".into();
            self.builder.mapping.from = node.path.clone();
            self.builder.mapping_type = true;
            self.builder.active = true;
            self.editor_page = EditorPage::Structures;
            self.reveal = true;
        }
    }
    /// Returns new YAML only after an explicit form action. The caller owns the editable draft.
    pub fn editor(
        &mut self,
        ui: &mut Ui,
        yaml: &mut String,
        range: Option<Range<usize>>,
        language: DecoderLanguage,
    ) -> Option<String> {
        if language == DecoderLanguage::Python {
            self.python_editor(ui, yaml);
            return None;
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .selectable_label(self.editor_page == EditorPage::Yaml, "Source")
                .clicked()
            {
                self.show_yaml();
            }
            ui.selectable_value(&mut self.editor_page, EditorPage::Structures, "Fields");
            ui.selectable_value(&mut self.editor_page, EditorPage::Steps, "Steps");
            ui.separator();
            ui.weak(crate::decoder_ui::RUN_LABEL);
        });
        if self.last_run_language != Some(DecoderLanguage::Python)
            && self.editor_page != EditorPage::Yaml
        {
            self.status(ui);
        }
        ui.separator();
        if self.editor_page == EditorPage::Yaml {
            let active_yaml = self.last_run_language == Some(DecoderLanguage::Yaml);
            let error = if active_yaml { self.error.clone() } else { None };
            let action = crate::decoder_ui::source_console(
                ui,
                "",
                error.as_deref(),
                active_yaml && self.is_parsing(),
                |ui| self.yaml_layout.editor(ui, yaml, DecoderLanguage::Yaml),
            );
            if action.copy {
                if let Some(error) = error {
                    ui.ctx().copy_text(error);
                }
            }
            if let Some(line) = action.line {
                self.yaml_layout.go_to_line(line);
            }
            if let Some(byte) = action.byte {
                self.console_byte_jump = Some(byte);
            }
            return None;
        }
        if self.editor_page == EditorPage::Steps {
            return egui::ScrollArea::vertical()
                .id_salt("definition_steps")
                .auto_shrink([false, false])
                .show(ui, |ui| self.steps.ui(ui, yaml))
                .inner;
        }
        self.fields_editor(ui, yaml, range)
    }

    fn python_editor(&mut self, ui: &mut Ui, code: &mut String) {
        let active_python = self.last_run_language == Some(DecoderLanguage::Python);
        let action = crate::decoder_ui::source_console(
            ui,
            if active_python { &self.python_stdout } else { "" },
            if active_python { self.error.as_deref() } else { None },
            active_python && self.is_parsing(),
            |ui| self.yaml_layout.editor(ui, code, DecoderLanguage::Python),
        );
        if action.copy {
            let mut text = self.python_stdout.clone();
            if let Some(error) = &self.error {
                if !text.is_empty() { text.push('\n'); }
                text.push_str(error);
            }
            ui.ctx().copy_text(text);
        }
        if let Some(line) = action.line {
            self.yaml_layout.go_to_line(line);
        }
        if let Some(byte) = action.byte {
            self.console_byte_jump = Some(byte);
        }
    }

    fn fields_editor(
        &mut self,
        ui: &mut Ui,
        yaml: &str,
        range: Option<Range<usize>>,
    ) -> Option<String> {
        let names = draft_program(yaml)
            .map(|program| program.structures.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let selection = range.filter(|range| !range.is_empty());
        let dirty = self.builder.dirty() || self.formats.dirty();
        let mut edit = None;
        let mut use_format = None;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    selection.is_some() && !dirty,
                    egui::Button::new("New from selection"),
                )
                .on_disabled_hover_text(
                    "Save or discard the current fields before starting another structure.",
                )
                .clicked()
            {
                self.begin_definition(selection.clone().unwrap());
            }
            ui.add_enabled_ui(!dirty && !names.is_empty(), |ui| {
                egui::ComboBox::from_id_salt("edit_reusable_format")
                    .selected_text(
                        self.formats
                            .editing
                            .as_ref()
                            .map(|editing| editing.name.as_str())
                            .unwrap_or("Edit format…"),
                    )
                    .show_ui(ui, |ui| {
                        for name in &names {
                            if ui.selectable_label(false, name).clicked() {
                                edit = Some(name.clone());
                            }
                        }
                    });
            })
            .response
            .on_disabled_hover_text(
                "Save or discard the current fields before opening another format.",
            );
            if let Some(editing) = &self.formats.editing {
                if ui
                    .add_enabled(
                        selection.is_some() && !dirty,
                        egui::Button::new("Use at selection"),
                    )
                    .clicked()
                {
                    use_format = Some(editing.name.clone());
                }
            }
        });
        if let Some(name) = edit {
            match self.formats.begin(&name, yaml) {
                Ok(()) => self.builder = Builder::default(),
                Err(error) => self.formats.error = Some(format!("{error:#}")),
            }
        }
        if let Some(name) = use_format {
            self.begin_definition(selection.clone().unwrap());
            self.builder.reuse = true;
            self.builder.reuse_type = name.clone();
            self.builder.instance = name.to_lowercase();
        }
        if let Some(error) = self.formats.error.as_ref().or(self.builder.error.as_ref()) {
            ui.colored_label(Color32::LIGHT_RED, error);
        }
        ui.separator();
        if !self.builder.active && self.formats.editing.is_none() {
            ui.weak("Select bytes in Hex, or open a format above.");
            return None;
        }
        egui::ScrollArea::vertical()
            .id_salt("definition_fields")
            .max_height((ui.available_height() - 44.).max(60.))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.builder.active {
                    self.builder.ui(ui, yaml, self.source.clone());
                } else {
                    self.formats.ui(ui, yaml);
                }
            });
        ui.separator();
        if self.builder.active {
            let mut generated = None;
            ui.horizontal_wrapped(|ui| {
                generated = self.builder.footer(ui, yaml);
                if ui.button("Discard fields").clicked() {
                    self.builder = Builder::default();
                }
            });
            if generated.is_some() {
                self.run_requested = true;
            }
            generated
        } else {
            self.formats.footer(ui, yaml)
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum EditorPage {
    Structures,
    Steps,
    #[default]
    Yaml,
}

/// Edits one reusable structure without rebuilding or dropping the rest of the program.
#[derive(Default)]
struct FormatEditor {
    editing: Option<EditingFormat>,
    error: Option<String>,
}
struct EditingFormat {
    name: String,
    base: String,
    initial: String,
    structure: Structure,
    fields: Vec<ReadStep>,
}
impl EditingFormat {
    fn build(&self) -> Result<Structure> {
        let mut structure = self.structure.clone();
        structure.fields = self
            .fields
            .iter()
            .map(ReadStep::build)
            .collect::<Result<_>>()?;
        Ok(structure)
    }
    fn dirty(&self) -> bool {
        self.build()
            .and_then(|structure| Ok(serde_yaml_ng::to_string(&structure)?))
            .map_or(true, |current| current != self.initial)
    }
    fn save(&self, yaml: &str) -> Result<String> {
        anyhow::ensure!(
            self.base == yaml,
            "YAML changed while these fields were open. Discard the fields and reopen the format."
        );
        let mut program = draft_program(yaml)?;
        *program
            .structures
            .get_mut(&self.name)
            .context("This format is no longer in the decoder")? = self.build()?;
        let next = serde_yaml_ng::to_string(&program)?;
        parse_authoring(&next).context("The updated format is invalid")?;
        Ok(next)
    }
}
impl FormatEditor {
    fn begin(&mut self, name: &str, yaml: &str) -> Result<()> {
        anyhow::ensure!(
            !self.dirty(),
            "Update or discard these fields before opening another format."
        );
        let structure = draft_program(yaml)?
            .structures
            .remove(name)
            .context("This format is no longer in the decoder")?;
        self.editing = Some(EditingFormat {
            name: name.into(),
            base: yaml.into(),
            initial: serde_yaml_ng::to_string(&structure)?,
            fields: structure.fields.iter().map(ReadStep::from_field).collect(),
            structure,
        });
        self.error = None;
        Ok(())
    }
    fn dirty(&self) -> bool {
        self.editing.as_ref().is_some_and(EditingFormat::dirty)
    }
    fn ui(&mut self, ui: &mut Ui, yaml: &str) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.strong(&editing.name);
            egui::ComboBox::from_id_salt("format_byte_order")
                .selected_text(match editing.structure.endian {
                    None => "Inherit byte order",
                    Some(Endian::Little) => "Little endian",
                    Some(Endian::Big) => "Big endian",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut editing.structure.endian, None, "Inherit byte order");
                    ui.selectable_value(
                        &mut editing.structure.endian,
                        Some(Endian::Little),
                        "Little endian",
                    );
                    ui.selectable_value(
                        &mut editing.structure.endian,
                        Some(Endian::Big),
                        "Big endian",
                    );
                });
        });
        if editing.base != yaml {
            ui.colored_label(
                Color32::YELLOW,
                "YAML changed. Discard these fields and reopen the format.",
            );
        }
        let types = available_types(yaml);
        let mut remove = None;
        let mut move_field = None;
        let count = editing.fields.len();
        let mut references = Vec::new();
        for (index, field) in editing.fields.iter_mut().enumerate() {
            ui.push_id((&editing.name, index), |ui| {
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.weak((index + 1).to_string());
                        ui.add(
                            egui::TextEdit::singleline(&mut field.name)
                                .desired_width((ui.available_width() - 245.).clamp(70., 180.))
                                .hint_text("Field name"),
                        );
                        if field.conditional {
                            ui.label("Type by value");
                        } else {
                            egui::ComboBox::from_id_salt("format_field_type")
                                .width(80.)
                                .selected_text(&field.kind)
                                .show_ui(ui, |ui| {
                                    for name in &types {
                                        ui.selectable_value(&mut field.kind, name.clone(), name);
                                    }
                                });
                        }
                        if ui
                            .add_enabled(index > 0, egui::Button::new("Up").small())
                            .clicked()
                        {
                            move_field = Some((index, index - 1));
                        }
                        if ui
                            .add_enabled(index + 1 < count, egui::Button::new("Down").small())
                            .clicked()
                        {
                            move_field = Some((index, index + 1));
                        }
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                    let options_id = ui.make_persistent_id("field_options");
                    egui::collapsing_header::CollapsingState::load_with_default_open(
                        ui.ctx(),
                        options_id,
                        false,
                    )
                    .show_header(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.weak("Options");
                            if matches!(field.kind.as_str(), "bytes" | "string")
                                || !field.length.text.is_empty()
                            {
                                ui.label("Length");
                                ui.add(
                                    egui::TextEdit::singleline(&mut field.length.text)
                                        .desired_width(55.)
                                        .hint_text("bytes"),
                                );
                            } else if let Some(width) = primitive_size(&field.kind) {
                                ui.weak(format!("{width} bytes"));
                            }
                            if field.repeat_mode == 1 {
                                ui.weak(format!("repeat {}", field.repeat.text));
                            } else if field.repeat_mode == 2 {
                                ui.weak("repeat until end");
                            }
                            if let Some(endian) = field.original.endian {
                                ui.weak(match endian {
                                    Endian::Little => "little endian",
                                    Endian::Big => "big endian",
                                });
                            }
                            if let Some(expected) = &field.original.expect {
                                let text = match expected {
                                    crate::structure::Literal::Unsigned(value) => value.to_string(),
                                    crate::structure::Literal::Signed(value) => value.to_string(),
                                    crate::structure::Literal::Float(value) => value.to_string(),
                                    crate::structure::Literal::Text(value) => format!("{value:?}"),
                                };
                                let short: String = text.chars().take(32).collect();
                                ui.weak(format!(
                                    "expect {short}{}",
                                    if text.chars().count() > 32 { "…" } else { "" }
                                ))
                                .on_hover_text(text);
                            }
                        });
                    })
                    .body(|ui| {
                        ui.checkbox(&mut field.conditional, "Choose type by value");
                        if field.conditional {
                            field.choice.ui(ui, &types, &references);
                        }
                        field.options_ui(ui, &references);
                    });
                });
            });
            references.push(field.name.clone());
        }
        if let Some(index) = remove {
            editing.fields.remove(index);
        }
        if let Some((index, target)) = move_field {
            editing.fields.swap(index, target);
        }
        if ui.button("Add field").clicked() {
            let mut suffix = editing.fields.len() + 1;
            while editing
                .fields
                .iter()
                .any(|field| field.name == format!("field_{suffix}"))
            {
                suffix += 1;
            }
            editing.fields.push(ReadStep::from_field(&Field::scalar(
                format!("field_{suffix}"),
                "u16",
            )));
        }
        if !editing.structure.actions.is_empty() {
            ui.separator();
            ui.label(format!(
                "{} actions in YAML",
                editing.structure.actions.len()
            ));
            for action in &editing.structure.actions {
                ui.weak(action_label(action));
            }
        }
    }
    fn footer(&mut self, ui: &mut Ui, yaml: &str) -> Option<String> {
        let editing = self.editing.as_ref()?;
        let can_save = editing.base == yaml && editing.dirty();
        let mut next = None;
        let mut discard = false;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    can_save,
                    egui::Button::new("Update format")
                        .fill(ui.visuals().selection.bg_fill)
                        .min_size(egui::vec2(140., 32.)),
                )
                .clicked()
            {
                match editing.save(yaml) {
                    Ok(updated) => next = Some(updated),
                    Err(error) => self.error = Some(format!("{error:#}")),
                }
            }
            if ui
                .button(if editing.dirty() {
                    "Discard changes"
                } else {
                    "Close format"
                })
                .clicked()
            {
                discard = true;
            }
        });
        if discard {
            self.editing = None;
            self.error = None;
        } else if let Some(next) = &next {
            let name = self.editing.as_ref().unwrap().name.clone();
            self.editing = None;
            // Keep the updated format in place with a fresh baseline for further editing.
            if let Err(error) = self.begin(&name, next) {
                self.error = Some(format!("{error:#}"));
            }
        }
        next
    }
}

/// Highlighting is cosmetic and cached by the draft and theme; the parser only runs on Decode.
fn bounded_console(text: &str) -> String {
    const LIMIT: usize = 64 * 1024;
    if text.len() <= LIMIT {
        return text.into();
    }
    let mut start = text.len() - LIMIT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("… earlier output truncated …\n{}", &text[start..])
}

#[cfg(test)]
fn yaml_layout(
    text: &str,
    font: egui::FontId,
    dark: bool,
    foreground: Color32,
) -> egui::text::LayoutJob {
    source_layout(text, font, dark, foreground, DecoderLanguage::Yaml)
}

/// A numeric input may retain an existing compound expression without inventing a text language.
#[derive(Clone, Default)]
struct ValueInput {
    text: String,
    original: Option<Expr>,
    initial: String,
}
impl ValueInput {
    fn from_expr(value: Option<Expr>) -> Self {
        let text = value.as_ref().map(expression_label).unwrap_or_default();
        Self {
            initial: text.clone(),
            text,
            original: value,
        }
    }
    fn optional(&self) -> Result<Option<Expr>> {
        if self.text == self.initial {
            Ok(self.original.clone())
        } else {
            optional_expr(&self.text)
        }
    }
    fn required(&self) -> Result<Expr> {
        self.optional()?
            .context("Enter a number or choose an earlier value")
    }
    fn ui(&mut self, ui: &mut Ui, label: &str, hint: &str, references: &[String]) {
        ui.label(label);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.text)
                    .hint_text(hint)
                    .desired_width((ui.available_width() - 45.).max(80.)),
            );
            reference_menu(ui, &mut self.text, references);
        });
        if self.original.as_ref().is_some_and(compound_expression) && self.text == self.initial {
            ui.small("Existing calculation is preserved. Use a Calculate step to build named calculations.");
        }
    }
}
fn compound_expression(value: &Expr) -> bool {
    matches!(
        value,
        Expr::Choose { .. }
            | Expr::Add { .. }
            | Expr::Subtract { .. }
            | Expr::Multiply { .. }
            | Expr::Divide { .. }
    )
}
fn expression_label(value: &Expr) -> String {
    match value {
        Expr::Unsigned(n) => n.to_string(),
        Expr::Signed(n) => n.to_string(),
        Expr::Keyword(keyword) => keyword.clone(),
        Expr::Ref { reference } => reference.clone(),
        Expr::Choose { choose } => format!("choose by {}", choose.reference),
        Expr::Add { add } => expression_list("add", add),
        Expr::Subtract { subtract } => expression_list("subtract", subtract),
        Expr::Multiply { multiply } => expression_list("multiply", multiply),
        Expr::Divide { divide } => expression_list("divide", divide),
    }
}
fn expression_list(operation: &str, values: &[Expr]) -> String {
    format!(
        "{operation}({})",
        values
            .iter()
            .map(expression_label)
            .collect::<Vec<_>>()
            .join(", ")
    )
}
fn reference_menu(ui: &mut Ui, value: &mut String, references: &[String]) {
    ui.menu_button("↗", |ui| {
        ui.label("Earlier values");
        if references.is_empty() {
            ui.small("Read a header or calculate a value in an earlier step first.");
        }
        egui::ScrollArea::vertical()
            .max_height(240.)
            .show(ui, |ui| {
                for path in references {
                    if ui.button(path).clicked() {
                        *value = path.clone();
                        ui.close();
                    }
                }
            });
    })
    .response
    .on_hover_text("Choose a value decoded by an earlier step");
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Read,
    Skip,
    Align,
    Calculate,
    Define,
    Choose,
}
impl Operation {
    const ALL: [Self; 6] = [
        Self::Read,
        Self::Skip,
        Self::Align,
        Self::Calculate,
        Self::Define,
        Self::Choose,
    ];
    fn name(self) -> &'static str {
        match self {
            Self::Read => "Read data",
            Self::Skip => "Skip bytes",
            Self::Align => "Align position",
            Self::Calculate => "Calculate a value",
            Self::Define => "Build type from descriptions",
            Self::Choose => "Choose by value",
        }
    }
    fn help(self) -> &'static str {
        match self {
            Self::Read => "Read a primitive or reusable structure. Repeat by a count, an earlier value, or to the end of a bounded region.",
            Self::Skip => "Advance the current position by a number of bytes, for example over reserved header bytes.",
            Self::Align => "Advance to the next multiple of the alignment, for example 2-byte boundaries for padded chunks.",
            Self::Calculate => "Give a number a name. Add, subtract, multiply, or divide constants and earlier values for use in later steps.",
            Self::Define => "Turn a collection of descriptions into a reusable type. Map generic name and type properties to fields.",
            Self::Choose => "Compare an earlier value with cases and run the matching steps. Use this for section IDs or format versions.",
        }
    }
    fn draft(self) -> StepDraft {
        let action = match self {
            Self::Read => Action::Read(Field::scalar("samples", "u16")),
            Self::Skip => Action::Skip(Expr::Unsigned(1)),
            Self::Align => Action::Align(Expr::Unsigned(2)),
            Self::Calculate => Action::Calculate(crate::structure::Calculate {
                name: "payload_size".into(),
                value: Expr::Multiply {
                    multiply: vec![
                        Expr::Ref {
                            reference: "header.count".into(),
                        },
                        Expr::Unsigned(2),
                    ],
                },
            }),
            Self::Define => Action::Define(Define {
                name: "Payload".into(),
                from: "header.descriptions".into(),
                fields: FieldMapping {
                    name: Some(Reference {
                        reference: "name".into(),
                        enum_name: None,
                    }),
                    kind: TypeChoice {
                        reference: "code".into(),
                        enum_name: None,
                        cases: BTreeMap::new(),
                        otherwise: None,
                    },
                    repeat: None,
                    items_per_group: None,
                    length: None,
                    at: None,
                    relative: None,
                },
            }),
            Self::Choose => Action::Choose(crate::structure::ChooseActions {
                reference: "header.section_id".into(),
                cases: BTreeMap::new(),
                otherwise: Vec::new(),
            }),
        };
        StepDraft::from_action(&action)
    }
}
#[derive(Clone)]
struct TypeChoiceDraft {
    reference: String,
    enum_name: String,
    cases: Vec<(String, String)>,
    otherwise: String,
}
impl TypeChoiceDraft {
    fn from_choice(choice: &TypeChoice) -> Self {
        Self {
            reference: choice.reference.clone(),
            enum_name: choice.enum_name.clone().unwrap_or_default(),
            cases: choice
                .cases
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            otherwise: choice.otherwise.clone().unwrap_or_default(),
        }
    }
    fn build(&self) -> Result<TypeChoice> {
        if !valid_reference(self.reference.trim()) {
            bail!("Choose a field path for the type code");
        }
        let mut cases = BTreeMap::new();
        for (value, kind) in &self.cases {
            if value.trim().is_empty() || kind.trim().is_empty() {
                bail!("Every type case needs a value and a type");
            }
            if cases
                .insert(value.trim().to_owned(), kind.clone())
                .is_some()
            {
                bail!("Duplicate type case {}", value.trim());
            }
        }
        Ok(TypeChoice {
            reference: self.reference.trim().into(),
            enum_name: (!self.enum_name.trim().is_empty()).then(|| self.enum_name.trim().into()),
            cases,
            otherwise: (!self.otherwise.is_empty()).then(|| self.otherwise.clone()),
        })
    }
    fn ui(&mut self, ui: &mut Ui, types: &[String], references: &[String]) {
        ui.label("Type code from");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.reference);
            reference_menu(ui, &mut self.reference, references);
        });
        labeled(ui, "Named enum (optional)", &mut self.enum_name);
        let mut remove = None;
        for (index, (value, kind)) in self.cases.iter_mut().enumerate() {
            ui.push_id(("type_case", index), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("When value is");
                    ui.add(egui::TextEdit::singleline(value).desired_width(60.));
                    ui.label("use");
                    type_picker(ui, "type", kind, types);
                    if ui.small_button("×").on_hover_text("Remove case").clicked() {
                        remove = Some(index);
                    }
                });
            });
        }
        if let Some(index) = remove {
            self.cases.remove(index);
        }
        if ui.small_button("Add type case").clicked() {
            self.cases.push((String::new(), "u16".into()));
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("Other values");
            egui::ComboBox::from_id_salt("default_type")
                .selected_text(if self.otherwise.is_empty() {
                    if self.cases.is_empty() {
                        "Value is the type name"
                    } else {
                        "Report unknown type code"
                    }
                } else {
                    &self.otherwise
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.otherwise,
                        String::new(),
                        if self.cases.is_empty() {
                            "Value is the type name"
                        } else {
                            "Report unknown type code"
                        },
                    );
                    for kind in types {
                        ui.selectable_value(&mut self.otherwise, kind.clone(), kind);
                    }
                });
        });
        ui.small("Without cases, the property must contain a known type name. With cases, unmapped codes report an error unless an alternate type is selected.");
    }
}
#[derive(Clone)]
struct ReadStep {
    original: Field,
    name: String,
    kind: String,
    conditional: bool,
    choice: TypeChoiceDraft,
    repeat_mode: usize,
    repeat: ValueInput,
    length: ValueInput,
    at: ValueInput,
    relative: ValueInput,
    align: ValueInput,
}
impl ReadStep {
    fn from_field(field: &Field) -> Self {
        let (kind, conditional, choice) = match &field.kind {
            crate::structure::TypeSpec::Name(kind) => (
                kind.clone(),
                false,
                TypeChoiceDraft {
                    reference: String::new(),
                    enum_name: String::new(),
                    cases: Vec::new(),
                    otherwise: String::new(),
                },
            ),
            crate::structure::TypeSpec::Select { choose } => {
                ("u16".into(), true, TypeChoiceDraft::from_choice(choose))
            }
        };
        let (repeat_mode, repeat) = match &field.repeat {
            None => (0, None),
            Some(Repeat::Count(value)) => (1, Some(value.clone())),
            Some(Repeat::Until(_)) => (2, None),
        };
        Self {
            original: field.clone(),
            name: field.name.clone(),
            kind,
            conditional,
            choice,
            repeat_mode,
            repeat: ValueInput::from_expr(repeat),
            length: ValueInput::from_expr(field.length.clone()),
            at: ValueInput::from_expr(field.at.clone()),
            relative: ValueInput::from_expr(field.relative.clone()),
            align: ValueInput::from_expr(field.align.clone()),
        }
    }
    fn build(&self) -> Result<Field> {
        let mut field = self.original.clone();
        field.name = self.name.trim().into();
        field.kind = if self.conditional {
            crate::structure::TypeSpec::Select {
                choose: self.choice.build()?,
            }
        } else {
            crate::structure::TypeSpec::Name(self.kind.clone())
        };
        field.repeat = match self.repeat_mode {
            1 => Some(Repeat::Count(self.repeat.required()?)),
            2 => Some(Repeat::Until("until end".into())),
            _ => None,
        };
        field.length = self.length.optional()?;
        field.at = self.at.optional()?;
        field.relative = self.relative.optional()?;
        field.align = self.align.optional()?;
        Ok(field)
    }
    fn ui(&mut self, ui: &mut Ui, types: &[String], references: &[String]) {
        labeled(ui, "Name in decoded data", &mut self.name);
        ui.checkbox(
            &mut self.conditional,
            "Choose the type using an earlier value",
        );
        if self.conditional {
            self.choice.ui(ui, types, references);
        } else {
            ui.horizontal(|ui| {
                ui.label("Read as");
                type_picker(ui, "read_type", &mut self.kind, types);
            });
        }
        self.options_ui(ui, references);
    }
    fn options_ui(&mut self, ui: &mut Ui, references: &[String]) {
        ui.checkbox(&mut self.original.preview, "Show value or decoded type on collapsed parent");
        ui.horizontal_wrapped(|ui| {
            ui.label("Read");
            egui::ComboBox::from_id_salt("read_repeat")
                .selected_text(
                    ["One item", "A number of items", "Until region ends"][self.repeat_mode],
                )
                .show_ui(ui, |ui| {
                    for (index, name) in ["One item", "A number of items", "Until region ends"]
                        .iter()
                        .enumerate()
                    {
                        ui.selectable_value(&mut self.repeat_mode, index, *name);
                    }
                });
        });
        if self.repeat_mode == 1 {
            self.repeat
                .ui(ui, "Item count", "e.g. header.count", references);
        }
        self.at.ui(
            ui,
            "File offset",
            "blank = continue at current position",
            references,
        );
        self.length.ui(
            ui,
            if self.repeat_mode == 0 {
                "Byte length / region limit"
            } else {
                "Byte length of each item"
            },
            "blank = use the type's size",
            references,
        );
        if self.repeat_mode != 0 {
            ui.small("To limit the whole collection, put the repeated field inside a structure and set that structure's byte length.");
        }
        ui.collapsing("Position and byte order", |ui| {
            self.relative.ui(
                ui,
                "Offset relative to containing structure",
                "blank = sequential",
                references,
            );
            self.align.ui(
                ui,
                "Align before reading",
                "blank = no alignment",
                references,
            );
            ui.horizontal_wrapped(|ui| {
                ui.label("Byte order");
                ui.selectable_value(&mut self.original.endian, None, "Inherit");
                ui.selectable_value(&mut self.original.endian, Some(Endian::Little), "Little");
                ui.selectable_value(&mut self.original.endian, Some(Endian::Big), "Big");
            });
            if self.original.expect.is_some() {
                ui.small("The existing expected-value check is preserved; edit its value in YAML.");
            }
        });
    }
}
#[derive(Clone)]
struct CalculateStep {
    name: String,
    left: ValueInput,
    operation: usize,
    right: ValueInput,
}
impl CalculateStep {
    fn from_calculation(value: &crate::structure::Calculate) -> Self {
        let (operation, operands) = match &value.value {
            Expr::Add { add } if add.len() == 2 => (1, Some(add)),
            Expr::Subtract { subtract } if subtract.len() == 2 => (2, Some(subtract)),
            Expr::Multiply { multiply } if multiply.len() == 2 => (3, Some(multiply)),
            Expr::Divide { divide } if divide.len() == 2 => (4, Some(divide)),
            _ => (0, None),
        };
        Self {
            name: value.name.clone(),
            operation,
            left: ValueInput::from_expr(Some(
                operands
                    .map(|v| v[0].clone())
                    .unwrap_or_else(|| value.value.clone()),
            )),
            right: ValueInput::from_expr(operands.map(|v| v[1].clone())),
        }
    }
    fn build(&self) -> Result<crate::structure::Calculate> {
        let left = self.left.required()?;
        let value = if self.operation == 0 {
            left
        } else {
            let values = vec![left, self.right.required()?];
            match self.operation {
                1 => Expr::Add { add: values },
                2 => Expr::Subtract { subtract: values },
                3 => Expr::Multiply { multiply: values },
                _ => Expr::Divide { divide: values },
            }
        };
        Ok(crate::structure::Calculate {
            name: self.name.trim().into(),
            value,
        })
    }
    fn ui(&mut self, ui: &mut Ui, references: &[String]) {
        labeled(ui, "Name for the result", &mut self.name);
        self.left
            .ui(ui, "Start with", "number or earlier value", references);
        let names = [
            "Keep this value",
            "Add",
            "Subtract",
            "Multiply by",
            "Divide by",
        ];
        egui::ComboBox::from_id_salt("calculate_operation")
            .selected_text(names[self.operation])
            .show_ui(ui, |ui| {
                for (index, name) in names.iter().enumerate() {
                    ui.selectable_value(&mut self.operation, index, *name);
                }
            });
        if self.operation != 0 {
            self.right
                .ui(ui, "Value", "number or earlier value", references);
        }
        ui.small("Later steps can use the result by name. Calculations do not consume bytes.");
    }
}
#[derive(Clone)]
struct DefineStep {
    name: String,
    from: String,
    label: String,
    label_enum: String,
    choice: TypeChoiceDraft,
    repeat: ValueInput,
    items_per_group: ValueInput,
    length: ValueInput,
    at: ValueInput,
    relative: ValueInput,
}
impl DefineStep {
    fn from_definition(value: &Define) -> Self {
        Self {
            name: value.name.clone(),
            from: value.from.clone(),
            label: value
                .fields
                .name
                .as_ref()
                .map(|r| r.reference.clone())
                .unwrap_or_default(),
            label_enum: value.fields.name.as_ref()
                .and_then(|r| r.enum_name.clone()).unwrap_or_default(),
            choice: TypeChoiceDraft::from_choice(&value.fields.kind),
            repeat: ValueInput::from_expr(value.fields.repeat.clone()),
            items_per_group: ValueInput::from_expr(value.fields.items_per_group.clone()),
            length: ValueInput::from_expr(value.fields.length.clone()),
            at: ValueInput::from_expr(value.fields.at.clone()),
            relative: ValueInput::from_expr(value.fields.relative.clone()),
        }
    }
    fn build(&self) -> Result<Define> {
        if !valid_reference(self.from.trim()) {
            bail!("Choose a collection path containing the descriptions");
        }
        if !self.label.trim().is_empty() && !valid_reference(self.label.trim()) {
            bail!("Choose a description property for field names");
        }
        Ok(Define {
            name: self.name.trim().into(),
            from: self.from.trim().into(),
            fields: FieldMapping {
                name: (!self.label.trim().is_empty()).then(|| Reference {
                    reference: self.label.trim().into(),
                    enum_name: (!self.label_enum.trim().is_empty()).then(|| self.label_enum.trim().into()),
                }),
                kind: self.choice.build()?,
                repeat: self.repeat.optional()?,
                items_per_group: self.items_per_group.optional()?,
                length: self.length.optional()?,
                at: self.at.optional()?,
                relative: self.relative.optional()?,
            },
        })
    }
    fn ui(&mut self, ui: &mut Ui, types: &[String], references: &[String]) {
        labeled(ui, "New type name", &mut self.name);
        ui.label("Description collection");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.from);
            reference_menu(ui, &mut self.from, references);
        });
        ui.small(
            "Each item describes one field. The property paths below are relative to that item.",
        );
        labeled(ui, "Field name property (optional)", &mut self.label);
        labeled(ui, "Field name enum (optional)", &mut self.label_enum);
        self.choice.ui(ui, types, &[]);
        self.repeat
            .ui(ui, "Item count property (optional)", "e.g. count", &[]);
        self.items_per_group
            .ui(ui, "Items per group (optional)", "e.g. samples_per_group", &[]);
        self.length
            .ui(ui, "Byte length property (optional)", "e.g. length", &[]);
        self.relative.ui(
            ui,
            "Relative offset property (optional)",
            "e.g. offset",
            &[],
        );
        self.at.ui(
            ui,
            "Absolute offset property (optional)",
            "e.g. position",
            &[],
        );
        ui.small("Add a Read step after this step to decode the payload using this type.");
    }
}
#[derive(Clone)]
struct ChoiceStep {
    reference: String,
    cases: Vec<(String, Vec<StepDraft>)>,
    otherwise: Vec<StepDraft>,
}
#[derive(Clone)]
enum StepDraft {
    Read(ReadStep),
    Skip(ValueInput),
    Align(ValueInput),
    Calculate(CalculateStep),
    Define(DefineStep),
    Choose(ChoiceStep),
}
impl StepDraft {
    fn from_action(action: &Action) -> Self {
        match action {
            Action::Read(field) => Self::Read(ReadStep::from_field(field)),
            Action::Skip(value) => Self::Skip(ValueInput::from_expr(Some(value.clone()))),
            Action::Align(value) => Self::Align(ValueInput::from_expr(Some(value.clone()))),
            Action::Calculate(value) => Self::Calculate(CalculateStep::from_calculation(value)),
            Action::Define(value) => Self::Define(DefineStep::from_definition(value)),
            Action::Choose(value) => Self::Choose(ChoiceStep {
                reference: value.reference.clone(),
                cases: value
                    .cases
                    .iter()
                    .map(|(k, v)| (k.clone(), v.iter().map(Self::from_action).collect()))
                    .collect(),
                otherwise: value.otherwise.iter().map(Self::from_action).collect(),
            }),
        }
    }
    fn operation(&self) -> Operation {
        match self {
            Self::Read(_) => Operation::Read,
            Self::Skip(_) => Operation::Skip,
            Self::Align(_) => Operation::Align,
            Self::Calculate(_) => Operation::Calculate,
            Self::Define(_) => Operation::Define,
            Self::Choose(_) => Operation::Choose,
        }
    }
    fn build(&self) -> Result<Action> {
        Ok(match self {
            Self::Read(value) => Action::Read(value.build()?),
            Self::Skip(value) => Action::Skip(value.required()?),
            Self::Align(value) => Action::Align(value.required()?),
            Self::Calculate(value) => Action::Calculate(value.build()?),
            Self::Define(value) => Action::Define(value.build()?),
            Self::Choose(value) => {
                if !valid_reference(value.reference.trim()) {
                    bail!("Choose an earlier field to compare");
                }
                let mut cases = BTreeMap::new();
                for (key, branch) in &value.cases {
                    if key.trim().is_empty() {
                        bail!("Every branch needs a match value");
                    }
                    if cases
                        .insert(
                            key.trim().into(),
                            branch.iter().map(Self::build).collect::<Result<Vec<_>>>()?,
                        )
                        .is_some()
                    {
                        bail!("Duplicate branch value {}", key.trim());
                    }
                }
                Action::Choose(crate::structure::ChooseActions {
                    reference: value.reference.trim().into(),
                    cases,
                    otherwise: value
                        .otherwise
                        .iter()
                        .map(Self::build)
                        .collect::<Result<Vec<_>>>()?,
                })
            }
        })
    }
    fn ui(&mut self, ui: &mut Ui, types: &[String], references: &[String]) {
        ui.small(self.operation().help());
        match self {
            Self::Read(value) => value.ui(ui, types, references),
            Self::Skip(value) => value.ui(
                ui,
                "Bytes to skip",
                "e.g. 16 or header.reserved_length",
                references,
            ),
            Self::Align(value) => value.ui(ui, "Alignment in bytes", "e.g. 2, 4, or 8", references),
            Self::Calculate(value) => value.ui(ui, references),
            Self::Define(value) => value.ui(ui, types, references),
            Self::Choose(value) => {
                ui.label("Compare value from");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut value.reference);
                    reference_menu(ui, &mut value.reference, references);
                });
                let mut remove = None;
                for (index, (key, branch)) in value.cases.iter_mut().enumerate() {
                    ui.push_id(("branch", index), |ui| {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label("When equal to");
                                ui.text_edit_singleline(key);
                                if ui.small_button("×").clicked() {
                                    remove = Some(index);
                                }
                            });
                            nested_steps(ui, branch, types, references);
                        });
                    });
                }
                if let Some(index) = remove {
                    value.cases.remove(index);
                }
                if ui.small_button("Add case").clicked() {
                    value.cases.push((String::new(), Vec::new()));
                }
                ui.collapsing("Otherwise", |ui| {
                    nested_steps(ui, &mut value.otherwise, types, references);
                });
            }
        }
    }
}
fn nested_steps(ui: &mut Ui, steps: &mut Vec<StepDraft>, types: &[String], references: &[String]) {
    let mut remove = None;
    let mut move_up = None;
    for (index, step) in steps.iter_mut().enumerate() {
        ui.push_id(index, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{}. {}", index + 1, step.operation().name()));
                if index > 0 && ui.small_button("↑").clicked() {
                    move_up = Some(index);
                }
                if ui.small_button("×").clicked() {
                    remove = Some(index);
                }
            });
            ui.collapsing("Edit operation", |ui| {
                step.ui(ui, types, references);
            });
        });
    }
    if let Some(index) = remove {
        steps.remove(index);
    } else if let Some(index) = move_up {
        steps.swap(index, index - 1);
    }
    ui.menu_button("Add operation", |ui| {
        for operation in Operation::ALL {
            if ui
                .button(operation.name())
                .on_hover_text(operation.help())
                .clicked()
            {
                steps.push(operation.draft());
                ui.close();
            }
        }
    });
}
#[derive(Default)]
struct StepsEditor {
    editing: Option<EditingStep>,
    error: Option<String>,
}
struct EditingStep {
    index: Option<usize>,
    base: String,
    initial: String,
    draft: StepDraft,
}
enum StepChange {
    Save {
        index: Option<usize>,
        action: Action,
    },
    Remove(usize),
    Move {
        index: usize,
        target: usize,
    },
}
impl StepsEditor {
    fn dirty(&self) -> bool {
        self.editing.as_ref().is_some_and(|editing| {
            editing.index.is_none()
                || editing
                    .draft
                    .build()
                    .and_then(|action| Ok(serde_yaml_ng::to_string(&action)?))
                    .map_or(true, |current| current != editing.initial)
        })
    }
    fn begin(&mut self, index: Option<usize>, action: &Action, yaml: &str) {
        self.editing = Some(EditingStep {
            index,
            base: yaml.to_owned(),
            initial: serde_yaml_ng::to_string(action).unwrap_or_default(),
            draft: StepDraft::from_action(action),
        });
        self.error = None;
    }
    fn ui(&mut self, ui: &mut Ui, yaml: &str) -> Option<String> {
        ui.heading("Decode steps");
        let program = match draft_program(yaml) {
            Ok(program) => program,
            Err(error) => {
                ui.colored_label(Color32::LIGHT_RED, format!("{error:#}"));
                ui.label("Your YAML text is preserved. Open YAML to fix it, then return to Steps.");
                return None;
            }
        };
        let mut change = None;
        let mut begin = None;
        for (index, action) in program.actions.iter().enumerate() {
            ui.push_id(("step", index), |ui| {
                ui.group(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(format!("{}", index + 1));
                        ui.label(action_label(action));
                    });
                    ui.horizontal(|ui| {
                        if ui.small_button("Edit").clicked() {
                            begin = Some((Some(index), action.clone()));
                        }
                        if ui
                            .add_enabled(index > 0, egui::Button::new("↑").small())
                            .on_hover_text("Move earlier")
                            .clicked()
                        {
                            change = Some(StepChange::Move {
                                index,
                                target: index - 1,
                            });
                        }
                        if ui
                            .add_enabled(
                                index + 1 < program.actions.len(),
                                egui::Button::new("↓").small(),
                            )
                            .on_hover_text("Move later")
                            .clicked()
                        {
                            change = Some(StepChange::Move {
                                index,
                                target: index + 1,
                            });
                        }
                        if ui.small_button("Remove").clicked() {
                            change = Some(StepChange::Remove(index));
                        }
                    });
                });
            });
        }
        if program.actions.is_empty() {
            ui.small("No steps yet. Start by reading a header or defining a structure from selected bytes.");
        }
        ui.menu_button("Add step", |ui| {
            for operation in Operation::ALL {
                if ui
                    .button(operation.name())
                    .on_hover_text(operation.help())
                    .clicked()
                {
                    begin = operation.draft().build().ok().map(|action| (None, action));
                    ui.close();
                }
            }
        });
        if let Some((index, action)) = begin {
            if self.editing.is_some() {
                self.error =
                    Some("Finish or cancel the current step before editing another step.".into());
            } else {
                self.begin(index, &action, yaml);
            }
        }
        let mut cancel = false;
        let mut saved = false;
        if let Some(editing) = &mut self.editing {
            ui.separator();
            ui.strong(match editing.index {
                Some(index) => format!("Edit step {}", index + 1),
                None => format!("New step: {}", editing.draft.operation().name()),
            });
            let references =
                earlier_references(&program, editing.index.unwrap_or(program.actions.len()));
            editing.draft.ui(ui, &available_types(yaml), &references);
            let changed_outside = editing.base != yaml;
            if changed_outside {
                ui.colored_label(Color32::YELLOW, "The definition changed while this form was open. Cancel and reopen the step to use the new version.");
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !changed_outside,
                        egui::Button::new(if editing.index.is_some() {
                            "Save step"
                        } else {
                            "Add this step"
                        }),
                    )
                    .clicked()
                {
                    match editing.draft.build() {
                        Ok(action) => {
                            change = Some(StepChange::Save {
                                index: editing.index,
                                action,
                            });
                            saved = true;
                        }
                        Err(error) => self.error = Some(format!("{error:#}")),
                    }
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        }
        if cancel {
            self.editing = None;
            self.error = None;
        }
        let result = if let Some(change) = change {
            if self.editing.is_some() && !saved {
                self.error = Some(
                    "Finish or cancel the current step before changing the step order.".into(),
                );
                None
            } else {
                match update_steps(yaml, change) {
                    Ok(next) => {
                        self.error = None;
                        if saved {
                            self.editing = None;
                        }
                        Some(next)
                    }
                    Err(error) => {
                        self.error = Some(format!("{error:#}"));
                        None
                    }
                }
            }
        } else {
            None
        };
        if let Some(error) = &self.error {
            ui.colored_label(Color32::LIGHT_RED, error);
        }
        result
    }
}
fn draft_program(yaml: &str) -> Result<Program> {
    if !has_decoder_code(yaml) {
        Ok(Program {
            endian: Endian::Little,
            structures: BTreeMap::new(),
            enums: Vec::new(),
            actions: Vec::new(),
        })
    } else if StructureWorkspace::is_generic(yaml) {
        parse_authoring(yaml).context("Fix the definition syntax before editing steps")
    } else {
        bail!("This is a legacy layout. It remains editable in YAML; create a new definition to use guided steps.")
    }
}
// An empty step list is a valid editing draft. Running it still uses Program::parse,
// which explains that at least one action is required.
fn parse_authoring(yaml: &str) -> Result<Program> {
    if yaml.len() > 256 * 1024 {
        bail!("Definition exceeds 256 KiB");
    }
    let mut program: Program = serde_yaml_ng::from_str(yaml)?;
    if program.actions.is_empty() {
        program.actions.push(Action::Skip(Expr::Unsigned(0)));
        Program::parse(&serde_yaml_ng::to_string(&program)?)?;
        program.actions.clear();
        Ok(program)
    } else {
        Program::parse(yaml)
    }
}
fn update_steps(yaml: &str, change: StepChange) -> Result<String> {
    let mut program = draft_program(yaml)?;
    match change {
        StepChange::Save {
            index: Some(index),
            action,
        } => {
            *program
                .actions
                .get_mut(index)
                .context("The step is no longer present")? = action;
        }
        StepChange::Save {
            index: None,
            action,
        } => program.actions.push(action),
        StepChange::Remove(index) => {
            if index >= program.actions.len() {
                bail!("The step is no longer present");
            }
            program.actions.remove(index);
        }
        StepChange::Move { index, target } => {
            if index >= program.actions.len() || target >= program.actions.len() {
                bail!("The step is no longer present");
            }
            let action = program.actions.remove(index);
            program.actions.insert(target, action);
        }
    }
    let next = serde_yaml_ng::to_string(&program)?;
    parse_authoring(&next).context("This change would make the definition invalid")?;
    Ok(next)
}
fn action_label(action: &Action) -> String {
    match action {
        Action::Read(field) => {
            let kind = match &field.kind {
                crate::structure::TypeSpec::Name(name) => name.clone(),
                crate::structure::TypeSpec::Select { choose } => {
                    format!("type chosen by {}", choose.reference)
                }
            };
            let repeat = match &field.repeat {
                None => String::new(),
                Some(Repeat::Count(value)) => format!(", {} items", expression_label(value)),
                Some(Repeat::Until(_)) => ", until region ends".into(),
            };
            let position = field
                .at
                .as_ref()
                .map(|value| format!(" at {}", expression_label(value)))
                .unwrap_or_default();
            format!("Read {} as {}{}{}", field.name, kind, repeat, position)
        }
        Action::Skip(value) => format!("Skip {} bytes", expression_label(value)),
        Action::Align(value) => format!("Align position to {} bytes", expression_label(value)),
        Action::Calculate(value) => format!(
            "Calculate {} = {}",
            value.name,
            expression_label(&value.value)
        ),
        Action::Define(value) => format!("Build type {} from {}", value.name, value.from),
        Action::Choose(value) => format!(
            "Choose by {} ({} cases)",
            value.reference,
            value.cases.len()
        ),
    }
}
fn earlier_references(program: &Program, before: usize) -> Vec<String> {
    fn fields(
        program: &Program,
        kind: &crate::structure::TypeSpec,
        prefix: &str,
        paths: &mut Vec<String>,
        depth: usize,
    ) {
        if depth > 5 || paths.len() >= 256 {
            return;
        }
        if let crate::structure::TypeSpec::Name(name) = kind {
            if let Some(structure) = program.structures.get(name) {
                for field in &structure.fields {
                    if paths.len() >= 256 {
                        break;
                    }
                    let path = format!("{prefix}.{}", field.name);
                    paths.push(path.clone());
                    // A repeated value is a collection, not an object with direct field paths.
                    // Keep its path for Define; individual indexed values may not be materialized.
                    if field.repeat.is_none() {
                        fields(program, &field.kind, &path, paths, depth + 1);
                    }
                }
            }
        }
    }
    let mut paths = Vec::new();
    for action in program.actions.iter().take(before) {
        if paths.len() >= 256 {
            break;
        }
        match action {
            Action::Read(field) => {
                paths.push(field.name.clone());
                if field.repeat.is_none() {
                    fields(program, &field.kind, &field.name, &mut paths, 0);
                }
            }
            Action::Calculate(value) => paths.push(value.name.clone()),
            _ => {}
        }
    }
    paths
}

#[derive(Clone, Debug)]
struct DraftField {
    name: String,
    kind: String,
    length: String,
    repeat: String,
    at: String,
}
impl Default for DraftField {
    fn default() -> Self {
        Self {
            name: "value".into(),
            kind: "u32".into(),
            length: String::new(),
            repeat: String::new(),
            at: String::new(),
        }
    }
}
#[derive(Default, Debug)]
struct Mapping {
    from: String,
    label: String,
    code: String,
    cases: String,
    repeat: String,
    length: String,
    relative: String,
}
struct Builder {
    active: bool,
    name: String,
    instance: String,
    offset: String,
    span: String,
    bounded: bool,
    instance_repeat: String,
    endian: Endian,
    fields: Vec<DraftField>,
    reuse: bool,
    reuse_type: String,
    reference: String,
    bind_field: usize,
    bind_property: usize,
    mapping_type: bool,
    mapping: Mapping,
    error: Option<String>,
    preview: FieldPreview,
    name_initialized: bool,
    repeat_selection: bool,
    bound_length: String,
    last_decoded: Option<(String, String, String, bool, bool)>,
    saved_signature: Option<String>,
}
impl Default for Builder {
    fn default() -> Self {
        Self {
            active: false,
            name: "Record".into(),
            instance: "record".into(),
            offset: "0".into(),
            span: "1".into(),
            bounded: false,
            instance_repeat: String::new(),
            endian: Endian::Little,
            fields: vec![DraftField::default()],
            reuse: false,
            reuse_type: "u32".into(),
            reference: String::new(),
            bind_field: 0,
            bind_property: 0,
            mapping_type: false,
            mapping: Mapping::default(),
            error: None,
            preview: FieldPreview::default(),
            name_initialized: false,
            repeat_selection: false,
            bound_length: String::new(),
            last_decoded: None,
            saved_signature: None,
        }
    }
}
impl Builder {
    fn from_range(range: Range<usize>) -> Self {
        let mut builder = Self {
            active: true,
            offset: format!("0x{:X}", range.start),
            span: range.len().to_string(),
            bound_length: range.len().to_string(),
            ..Self::default()
        };
        builder.fields[0].kind = match range.len() {
            1 => "u8",
            2 => "u16",
            4 => "u32",
            8 => "u64",
            _ => "bytes",
        }
        .into();
        if builder.fields[0].kind == "bytes" {
            builder.fields[0].length = range.len().to_string();
        }
        builder
    }
    fn signature(&self) -> String {
        format!(
            "{:?}{:?}",
            (
                &self.name,
                &self.instance,
                &self.offset,
                &self.span,
                self.bounded,
                &self.instance_repeat,
                self.endian,
                &self.fields
            ),
            (
                self.reuse,
                &self.reuse_type,
                self.mapping_type,
                &self.mapping,
                self.repeat_selection,
                &self.bound_length
            )
        )
    }
    fn dirty(&self) -> bool {
        self.active
            && self
                .saved_signature
                .as_ref()
                .is_none_or(|saved| *saved != self.signature())
    }
    fn ui(&mut self, ui: &mut Ui, yaml: &str, source: Option<ByteView>) {
        if !self.active && self.reference.is_empty() {
            return;
        }
        self.initialize_name(yaml);
        let selected_bytes = number(&self.span)
            .ok()
            .and_then(|n| usize::try_from(n).ok());
        let selected_start = number(&self.offset)
            .ok()
            .and_then(|n| usize::try_from(n).ok());
        ui.heading(if self.mapping_type {
            "Create a format from descriptions".into()
        } else if self.reuse {
            format!("Use {}", self.reuse_type)
        } else {
            format!(
                "Describe {} selected bytes",
                selected_bytes
                    .map(|n| n.to_string())
                    .unwrap_or("the".into())
            )
        });
        if let (Some(start), Some(bytes)) = (selected_start, selected_bytes) {
            ui.label(
                egui::RichText::new(format!(
                    "0x{start:X} – 0x{:X}",
                    start.saturating_add(bytes).saturating_sub(1)
                ))
                .monospace()
                .weak(),
            );
        }
        ui.add_space(8.);
        ui.label("Name");
        if ui
            .add(
                egui::TextEdit::singleline(&mut self.instance)
                    .hint_text("e.g. header or sample")
                    .desired_width(f32::INFINITY),
            )
            .changed()
        {
            if !self.reuse {
                self.name = type_name(&self.instance);
            }
        }
        if !self.reference.is_empty() {
            self.reference_controls(ui);
        }
        ui.add_space(12.);
        let available = available_types(yaml);
        if self.mapping_type {
            ui.label("Each entry in this list will become a field.");
            labeled(ui, "Description list", &mut self.mapping.from);
            labeled(ui, "Name in each entry", &mut self.mapping.label);
            labeled(ui, "Format in each entry", &mut self.mapping.code);
            ui.collapsing("Map format codes", |ui| {
                ui.small(
                    "One code = format per line. Leave empty when the file stores format names.",
                );
                ui.add(
                    egui::TextEdit::multiline(&mut self.mapping.cases)
                        .hint_text("1 = u16\n2 = f32")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
            });
            ui.collapsing("More description properties", |ui| {
                labeled(ui, "Count from", &mut self.mapping.repeat);
                labeled(ui, "Byte length from", &mut self.mapping.length);
                labeled(ui, "Relative position from", &mut self.mapping.relative);
            });
        } else if !self.reuse {
            ui.horizontal(|ui| {
                ui.strong("Fields");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::ComboBox::from_id_salt("describe_byte_order").width(110.).selected_text(if self.endian == Endian::Little { "Little endian" } else { "Big endian" }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.endian, Endian::Little, "Little endian");
                        ui.selectable_value(&mut self.endian, Endian::Big, "Big endian");
                    }).response.on_hover_text("The byte order used for numbers. The preview updates when you change it.");
                });
            });
            let specs = self.preview_fields(yaml);
            self.preview.update(ui.ctx(), source, specs.clone());
            let mut remove = None;
            let mut up = None;
            let count = self.fields.len();
            let field_width = (ui.available_width() - 210.).clamp(60., 200.);
            for (index, field) in self.fields.iter_mut().enumerate() {
                ui.push_id(("describe_field",index), |ui| {
                    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut field.name).desired_width(field_width).hint_text("Field name"));
                            let old_kind = field.kind.clone();
                            egui::ComboBox::from_id_salt("format").width(145.).selected_text(format_label(&field.kind)).show_ui(ui, |ui| {
                                for kind in &available { ui.selectable_value(&mut field.kind, kind.clone(), format_label(kind)); }
                            });
                            if old_kind != field.kind { field.change_format(); }
                            ui.menu_button("...", |ui| {
                                if ui.add_enabled(index > 0, egui::Button::new("Move earlier")).clicked() { up = Some(index); ui.close(); }
                                if ui.add_enabled(count > 1, egui::Button::new("Delete field")).clicked() { remove = Some(index); ui.close(); }
                                ui.separator();
                                ui.label("Repeat count"); ui.text_edit_singleline(&mut field.repeat);
                                ui.label("Absolute file offset"); ui.text_edit_singleline(&mut field.at);
                                ui.small("Blank reads once at the next byte. A count or offset can refer to an earlier value.");
                            });
                        });
                        ui.horizontal_wrapped(|ui| {
                            if matches!(field.kind.as_str(), "bytes" | "string") {
                                ui.label("Length");
                                ui.add(egui::TextEdit::singleline(&mut field.length).desired_width(48.));
                                ui.weak("bytes");
                            } else if let Some(width) = primitive_size(&field.kind) { ui.weak(format!("{width} bytes")); }
                            if let Some(spec) = specs.get(index) { ui.weak(format!("at 0x{:X}", spec.offset)); }
                        });
                        let unchanged = specs.get(index).is_some_and(|spec| spec.kind == field.kind &&
                            (field.length.is_empty() || primitive_size(&field.kind).is_some() || number(&field.length).ok() == Some(spec.length as u64)));
                        let value = if specs.get(index).is_none() { "Available after decoding" } else if unchanged { self.preview.value(index) } else { "Updating…" };
                        ui.horizontal_wrapped(|ui| {
                            ui.weak("Value");
                            ui.add(egui::Label::new(egui::RichText::new(value).monospace()).wrap());
                        });
                    });
                });
            }
            if let Some(index) = remove {
                self.fields.remove(index);
            }
            if let Some(index) = up {
                if index < self.fields.len() {
                    self.fields.swap(index, index - 1);
                }
            }
            if ui.button("+ Add field").clicked() {
                self.add_remaining_field();
            }
        } else {
            ui.label("This uses the saved fields. You do not need to describe them again.");
            if let Ok(program) = draft_program(yaml) {
                if let Some(format) = program.structures.get(&self.reuse_type) {
                    for field in &format.fields {
                        ui.label(action_label(&Action::Read(field.clone())));
                    }
                }
            }
        }
        ui.add_space(10.);
        let size = self.record_size(yaml);
        if let (Some(size), Some(span)) = (size, selected_bytes) {
            let fraction = if span > 0 {
                size as f32 / span as f32
            } else {
                0.
            };
            ui.add(
                egui::ProgressBar::new(fraction.min(1.))
                    .desired_height(5.)
                    .fill(if size > span {
                        Color32::LIGHT_RED
                    } else {
                        ui.visuals().selection.bg_fill
                    }),
            );
            if size <= span {
                ui.label(format!(
                    "{size} of {span} bytes described · {} remaining",
                    span - size
                ));
            } else {
                ui.colored_label(
                    Color32::LIGHT_RED,
                    format!("The fields need {size} bytes; only {span} are selected."),
                );
            }
            if size > 0
                && span > size
                && span % size == 0
                && self.instance_repeat.is_empty()
                && !self.mapping_type
                && !self.bounded
            {
                ui.checkbox(
                    &mut self.repeat_selection,
                    format!("Decode all {} records in this selection", span / size),
                );
            } else {
                self.repeat_selection = false;
            }
        } else {
            ui.weak("The byte count will be resolved when the definition runs.");
        }
        ui.add_space(8.);
        ui.collapsing("Advanced", |ui| {
            ui.small("These settings control where this format is used in the file.");
            labeled(ui, "Absolute file offset", &mut self.offset);
            labeled(
                ui,
                "Repeat count or earlier value",
                &mut self.instance_repeat,
            );
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.bounded, "Limit each record to");
                ui.add_enabled(
                    self.bounded,
                    egui::TextEdit::singleline(&mut self.bound_length).desired_width(80.),
                );
                ui.label("bytes");
            });
            if !self.reuse {
                labeled(ui, "Reusable format name", &mut self.name);
            }
        });
    }

    fn footer(&mut self, ui: &mut Ui, yaml: &str) -> Option<String> {
        let size = self.record_size(yaml);
        let selected_bytes = number(&self.span)
            .ok()
            .and_then(|n| usize::try_from(n).ok());
        let fits = match (size, selected_bytes) {
            (Some(size), Some(span)) => {
                size > 0 && (size <= span || span == 0 || self.mapping_type)
            }
            _ => true,
        };
        if ui
            .add_enabled(
                fits && !self.instance.trim().is_empty(),
                egui::Button::new(egui::RichText::new("Decode selection").color(Color32::WHITE))
                    .fill(ui.visuals().selection.bg_fill)
                    .min_size(egui::vec2(150., 32.)),
            )
            .clicked()
        {
            match self.decode_draft(yaml) {
                Ok(next) => {
                    self.error = None;
                    return Some(next);
                }
                Err(error) => self.error = Some(format!("{error:#}")),
            }
        }
        None
    }

    fn reference_controls(&mut self, ui: &mut Ui) {
        ui.group(|ui| {
            ui.label("Use this value");
            ui.monospace(&self.reference);
            let names: Vec<_> = std::iter::once("Record".to_owned())
                .chain(self.fields.iter().map(|f| f.name.clone()))
                .collect();
            self.bind_field = self.bind_field.min(names.len().saturating_sub(1));
            egui::ComboBox::from_id_salt("reference_field")
                .selected_text(&names[self.bind_field])
                .show_ui(ui, |ui| {
                    for (index, name) in names.iter().enumerate() {
                        ui.selectable_value(&mut self.bind_field, index, name);
                    }
                });
            egui::ComboBox::from_id_salt("reference_use")
                .selected_text(["Repeat count", "Byte length", "File offset"][self.bind_property])
                .show_ui(ui, |ui| {
                    for (index, name) in ["Repeat count", "Byte length", "File offset"]
                        .iter()
                        .enumerate()
                    {
                        ui.selectable_value(&mut self.bind_property, index, *name);
                    }
                });
            if ui.button("Use value").clicked() {
                self.link_reference();
            }
        });
    }
    fn initialize_name(&mut self, yaml: &str) {
        if self.name_initialized {
            return;
        }
        self.name_initialized = true;
        let names = available_types(yaml);
        let program = draft_program(yaml).ok();
        let initial = self.instance.clone();
        for suffix in 0..10000 {
            let instance = if suffix == 0 {
                initial.clone()
            } else {
                format!("{initial}_{}", suffix + 1)
            };
            let name = type_name(&instance);
            let used = program.as_ref().is_some_and(|p| {
                p.actions.iter().any(|a| match a {
                    Action::Read(field) => field.name == instance,
                    Action::Calculate(value) => value.name == instance,
                    _ => false,
                })
            });
            if !used && (self.reuse || !names.contains(&name)) {
                self.instance = instance;
                if !self.reuse {
                    self.name = name;
                }
                break;
            }
        }
    }
    fn record_size(&self, yaml: &str) -> Option<usize> {
        if self.mapping_type {
            return None;
        }
        if self.reuse {
            fixed_format_size(&self.reuse_type, &draft_program(yaml).ok()?, 0)
        } else {
            self.static_size().or_else(|| {
                let program = draft_program(yaml).ok()?;
                self.fields.iter().try_fold(0usize, |sum, field| {
                    if !field.at.is_empty() {
                        return None;
                    }
                    let width = if field.length.is_empty() {
                        fixed_format_size(&field.kind, &program, 0)?
                    } else {
                        usize::try_from(number(&field.length).ok()?).ok()?
                    };
                    let count = if field.repeat.is_empty() || field.repeat == "one" {
                        1
                    } else {
                        usize::try_from(number(&field.repeat).ok()?).ok()?
                    };
                    sum.checked_add(width.checked_mul(count)?)
                })
            })
        }
    }
    fn add_remaining_field(&mut self) {
        let remaining = number(&self.span)
            .ok()
            .and_then(|span| usize::try_from(span).ok())
            .and_then(|span| self.static_size().map(|used| span.saturating_sub(used)))
            .unwrap_or(1)
            .max(1);
        let mut index = self.fields.len() + 1;
        while self
            .fields
            .iter()
            .any(|field| field.name == format!("field_{index}"))
        {
            index += 1;
        }
        self.fields.push(DraftField {
            name: format!("field_{index}"),
            kind: "bytes".into(),
            length: remaining.to_string(),
            ..DraftField::default()
        });
    }
    fn preview_fields(&self, yaml: &str) -> Vec<PreviewField> {
        let Some(mut offset) = number(&self.offset)
            .ok()
            .and_then(|n| usize::try_from(n).ok())
        else {
            return Vec::new();
        };
        let program = draft_program(yaml).ok();
        let mut fields = Vec::new();
        for field in self.fields.iter().take(64) {
            if !field.at.is_empty() {
                let Ok(at) = number(&field.at) else {
                    break;
                };
                let Ok(at) = usize::try_from(at) else {
                    break;
                };
                offset = at;
            }
            let width = if !field.length.is_empty() {
                number(&field.length)
                    .ok()
                    .and_then(|n| usize::try_from(n).ok())
            } else {
                primitive_size(&field.kind).or_else(|| {
                    program
                        .as_ref()
                        .and_then(|p| fixed_format_size(&field.kind, p, 0))
                })
            };
            let Some(width) = width else {
                break;
            };
            fields.push(PreviewField {
                name: field.name.clone(),
                kind: field.kind.clone(),
                offset,
                length: primitive_size(&field.kind).unwrap_or(width),
                endian: self.endian,
            });
            let repeat = if field.repeat.is_empty() || field.repeat == "one" {
                Some(1)
            } else {
                number(&field.repeat)
                    .ok()
                    .and_then(|n| usize::try_from(n).ok())
            };
            let Some(next) = repeat
                .and_then(|count| width.checked_mul(count))
                .and_then(|width| offset.checked_add(width))
            else {
                break;
            };
            offset = next;
        }
        fields
    }
    fn static_size(&self) -> Option<usize> {
        self.fields.iter().try_fold(0usize, |sum, field| {
            if !field.at.is_empty() {
                return None;
            }
            let size = if !field.length.is_empty() {
                usize::try_from(number(&field.length).ok()?).ok()?
            } else {
                match field.kind.as_str() {
                    "u8" | "i8" => 1,
                    "u16" | "i16" => 2,
                    "u32" | "i32" | "f32" => 4,
                    "u64" | "i64" | "f64" => 8,
                    _ => return None,
                }
            };
            let repeat = if field.repeat.trim().is_empty() || field.repeat.trim() == "one" {
                1
            } else {
                usize::try_from(number(&field.repeat).ok()?).ok()?
            };
            sum.checked_add(size.checked_mul(repeat)?)
        })
    }
    fn link_reference(&mut self) {
        if self.bind_field == 0 {
            match self.bind_property {
                0 => self.instance_repeat = self.reference.clone(),
                1 => {
                    self.bound_length = self.reference.clone();
                    self.bounded = true;
                }
                _ => self.offset = self.reference.clone(),
            }
        } else if let Some(field) = self.fields.get_mut(self.bind_field - 1) {
            match self.bind_property {
                0 => field.repeat = self.reference.clone(),
                1 => field.length = self.reference.clone(),
                _ => field.at = self.reference.clone(),
            }
        }
        self.active = true;
        self.reference.clear();
    }
    fn decode_draft(&mut self, yaml: &str) -> Result<String> {
        let mut base = yaml.to_owned();
        if let Some((previous, instance, name, reuse, mapping)) = &self.last_decoded {
            if previous != yaml {
                bail!("The definition changed in another editor. Discard this form and start a new selection.");
            }
            let mut program = parse_authoring(yaml)?;
            program.actions.retain(|action| {
                !matches!(action, Action::Read(field) if &field.name == instance)
                    && !(*mapping && matches!(action, Action::Define(value) if &value.name == name))
            });
            if !reuse && !mapping {
                program.structures.remove(name);
            }
            base = serde_yaml_ng::to_string(&program)?;
        }
        let next = self.generate(&base)?;
        self.last_decoded = Some((
            next.clone(),
            self.instance.trim().to_owned(),
            self.name.clone(),
            self.reuse,
            self.mapping_type,
        ));
        self.saved_signature = Some(self.signature());
        Ok(next)
    }
    fn generate(&self, yaml: &str) -> Result<String> {
        let mut program = if !has_decoder_code(yaml) {
            Program {
                endian: self.endian,
                structures: BTreeMap::new(),
                enums: Vec::new(),
                actions: Vec::new(),
            }
        } else if StructureWorkspace::is_generic(yaml) {
            parse_authoring(yaml).context("Fix the current YAML before adding fields")?
        } else {
            bail!("This file uses a legacy layout. Save it, then clear the YAML editor to start a generic definition.")
        };
        if program.actions.iter().any(|action| match action {
            Action::Read(field) => field.name == self.instance.trim(),
            Action::Calculate(value) => value.name == self.instance.trim(),
            _ => false,
        }) {
            bail!(
                "The name {} is already used. Choose another name.",
                self.instance.trim()
            );
        }
        let type_names = available_types(yaml);
        let kind = if self.mapping_type {
            if self.mapping.from.trim().is_empty() || self.mapping.code.trim().is_empty() {
                bail!("Choose a description collection and the field containing its type.");
            }
            if type_names.contains(&self.name) {
                bail!(
                    "Type {} already exists. Choose another name or reuse it.",
                    self.name
                );
            }
            if !valid_reference(self.mapping.from.trim())
                || !valid_reference(self.mapping.code.trim())
                || (!self.mapping.label.trim().is_empty()
                    && !valid_reference(self.mapping.label.trim()))
            {
                bail!("Use named field paths for description properties");
            }
            let mut cases = BTreeMap::new();
            for line in self.mapping.cases.lines().filter(|l| !l.trim().is_empty()) {
                let (key, value) = line
                    .split_once('=')
                    .context("Enter each type mapping as code = type")?;
                if key.trim().is_empty() || value.trim().is_empty() {
                    bail!("Both code and type are required");
                }
                if !type_names.iter().any(|name| name == value.trim()) {
                    bail!(
                        "Type {} is not a defined structure or primitive",
                        value.trim()
                    );
                }
                if cases
                    .insert(key.trim().to_owned(), value.trim().to_owned())
                    .is_some()
                {
                    bail!("Duplicate type code {}", key.trim());
                }
            }
            program.actions.push(Action::Define(Define {
                name: self.name.clone(),
                from: self.mapping.from.trim().into(),
                fields: FieldMapping {
                    name: (!self.mapping.label.trim().is_empty()).then(|| Reference {
                        reference: self.mapping.label.trim().into(),
                        enum_name: None,
                    }),
                    kind: TypeChoice {
                        reference: self.mapping.code.trim().into(),
                        enum_name: None,
                        cases,
                        otherwise: None,
                    },
                    repeat: optional_expr(&self.mapping.repeat)?,
                    items_per_group: None,
                    length: optional_expr(&self.mapping.length)?,
                    at: None,
                    relative: optional_expr(&self.mapping.relative)?,
                },
            }));
            self.name.clone()
        } else if self.reuse {
            if !type_names.contains(&self.reuse_type) {
                bail!("Choose an existing structure or primitive type");
            }
            self.reuse_type.clone()
        } else {
            if type_names.contains(&self.name) {
                bail!(
                    "Type {} already exists. Choose another name or reuse it.",
                    self.name
                );
            }
            if self.fields.is_empty() {
                bail!("Add at least one field");
            }
            let fields = self
                .fields
                .iter()
                .map(|f| {
                    let mut field = Field::scalar(f.name.trim(), f.kind.trim());
                    field.length = optional_expr(&f.length)?;
                    field.at = optional_expr(&f.at)?;
                    field.repeat = repeat_expression(&f.repeat)?;
                    Ok(field)
                })
                .collect::<Result<Vec<_>>>()?;
            program.structures.insert(
                self.name.clone(),
                Structure {
                    fields,
                    actions: Vec::new(),
                    endian: Some(self.endian),
                },
            );
            self.name.clone()
        };
        let mut read = Field::scalar(self.instance.trim(), kind);
        read.at = optional_expr(&self.offset)?;
        read.repeat = repeat_expression(&self.instance_repeat)?;
        if self.repeat_selection {
            if self.bounded {
                bail!("Set an explicit repeat count when limiting the byte length of each record");
            }
            let size = self
                .record_size(yaml)
                .filter(|size| *size > 0)
                .context("The record size must be known before repeating across the selection")?;
            let span = usize::try_from(number(&self.span)?)?;
            if span % size != 0 {
                bail!("The selection does not contain an exact number of records");
            }
            read.repeat = Some(Repeat::Count(Expr::Unsigned((span / size) as u64)));
        }
        if self.bounded {
            read.length = Some(
                expression(&self.bound_length).context("Enter the byte limit for each record")?,
            );
        }
        program.actions.push(Action::Read(read));
        let yaml = serde_yaml_ng::to_string(&program)?;
        Program::parse(&yaml)?;
        Ok(yaml)
    }
}
impl DraftField {
    fn change_format(&mut self) {
        self.length = if matches!(self.kind.as_str(), "bytes" | "string") {
            "1".into()
        } else {
            String::new()
        };
    }
}
fn format_label(kind: &str) -> String {
    match kind {
        "bytes" => "Raw bytes".into(),
        "string" => "String (UTF-8)".into(),
        "cstring" => "Zero-terminated text".into(),
        "varuint" => "Variable integer".into(),
        "u8" => "Unsigned 8-bit".into(),
        "u16" => "Unsigned 16-bit".into(),
        "u32" => "Unsigned 32-bit".into(),
        "u64" => "Unsigned 64-bit".into(),
        "i8" => "Signed 8-bit".into(),
        "i16" => "Signed 16-bit".into(),
        "i32" => "Signed 32-bit".into(),
        "i64" => "Signed 64-bit".into(),
        "f32" => "Float 32-bit".into(),
        "f64" => "Float 64-bit".into(),
        _ => kind.into(),
    }
}
fn type_name(instance: &str) -> String {
    let mut upper = true;
    instance
        .chars()
        .filter_map(|c| {
            if c == '_' || c == '-' || c.is_whitespace() {
                upper = true;
                None
            } else if upper {
                upper = false;
                Some(c.to_ascii_uppercase())
            } else {
                Some(c)
            }
        })
        .collect()
}
fn fixed_format_size(kind: &str, program: &Program, depth: usize) -> Option<usize> {
    if let Some(width) = primitive_size(kind) {
        return Some(width);
    }
    if depth >= 32 {
        return None;
    }
    let structure = program.structures.get(kind)?;
    if !structure.actions.is_empty() {
        return None;
    }
    structure.fields.iter().try_fold(0usize, |sum, field| {
        if field.at.is_some() || field.relative.is_some() || field.align.is_some() {
            return None;
        }
        let width = match &field.length {
            Some(Expr::Unsigned(n)) => usize::try_from(*n).ok()?,
            None => match &field.kind {
                crate::structure::TypeSpec::Name(kind) => {
                    fixed_format_size(kind, program, depth + 1)?
                }
                _ => return None,
            },
            _ => return None,
        };
        let count = match &field.repeat {
            None => 1,
            Some(Repeat::Count(Expr::Unsigned(n))) => usize::try_from(*n).ok()?,
            _ => return None,
        };
        sum.checked_add(width.checked_mul(count)?)
    })
}

fn number(text: &str) -> Result<u64> {
    if let Some(hex) = text
        .trim()
        .strip_prefix("0x")
        .or_else(|| text.trim().strip_prefix("0X"))
    {
        Ok(u64::from_str_radix(hex, 16)?)
    } else {
        Ok(text.trim().parse()?)
    }
}
fn expression(text: &str) -> Result<Expr> {
    let text = text.trim();
    if text.is_empty() {
        bail!("A value or field path is required");
    }
    if text.starts_with("0x") || text.starts_with("0X") || text.chars().all(|c| c.is_ascii_digit())
    {
        return Ok(Expr::Unsigned(number(text)?));
    }
    if text.starts_with('-') && text[1..].chars().all(|c| c.is_ascii_digit()) {
        return Ok(Expr::Signed(text.parse()?));
    }
    if text == "remaining" {
        return Ok(Expr::Keyword(text.into()));
    }
    if !valid_reference(text) {
        bail!("Use a number or field path. Use a Calculate step for arithmetic.");
    }
    Ok(Expr::Ref {
        reference: text.to_owned(),
    })
}
fn valid_reference(text: &str) -> bool {
    let path = text.strip_prefix('/').unwrap_or(text);
    !path.is_empty()
        && path.split('.').all(|segment| {
            let name_end = segment.find('[').unwrap_or(segment.len());
            let name = &segment[..name_end];
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
            {
                return false;
            }
            let mut indices = &segment[name_end..];
            while !indices.is_empty() {
                let Some(rest) = indices.strip_prefix('[') else {
                    return false;
                };
                let Some(close) = rest.find(']') else {
                    return false;
                };
                if close == 0 || !rest[..close].chars().all(|c| c.is_ascii_digit()) {
                    return false;
                }
                indices = &rest[close + 1..];
            }
            true
        })
}
fn repeat_expression(text: &str) -> Result<Option<Repeat>> {
    match text.trim() {
        "" | "one" => Ok(None),
        "until end" => Ok(Some(Repeat::Until("until end".into()))),
        other => expression(other).map(|value| Some(Repeat::Count(value))),
    }
}
fn optional_expr(text: &str) -> Result<Option<Expr>> {
    if text.trim().is_empty() {
        Ok(None)
    } else {
        expression(text).map(Some)
    }
}
fn labeled(ui: &mut Ui, label: &str, text: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
}
fn available_types(yaml: &str) -> Vec<String> {
    let mut names: Vec<String> = [
        "u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64", "f32", "f64", "string", "cstring",
        "bytes", "varuint",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Ok(program) = draft_program(yaml) {
        names.extend(program.structures.keys().cloned());
        collect_defined_types(&program.actions, &mut names);
        for structure in program.structures.values() {
            collect_defined_types(&structure.actions, &mut names);
        }
    }
    let mut seen = std::collections::HashSet::new();
    names.retain(|name| seen.insert(name.clone()));
    names
}
fn collect_defined_types(actions: &[Action], names: &mut Vec<String>) {
    for action in actions {
        match action {
            Action::Define(definition) => names.push(definition.name.clone()),
            Action::Choose(choice) => {
                for branch in choice.cases.values() {
                    collect_defined_types(branch, names);
                }
                collect_defined_types(&choice.otherwise, names);
            }
            _ => {}
        }
    }
}
fn type_picker(ui: &mut Ui, id: &str, value: &mut String, names: &[String]) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.as_str())
        .show_ui(ui, |ui| {
            for name in names {
                ui.selectable_value(value, name.clone(), name);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric_test_node(kind: &str, endian: Endian) -> Node {
        let source = crate::byte_store::ByteStore::from_snapshot(
            crate::binary::Original::from_bytes(&[0; 8]).unwrap(),
        );
        let yaml = format!(
            "endian: {}\nactions: [{{read: {{name: value, type: {kind}}}}}]",
            if endian == Endian::Little {
                "little"
            } else {
                "big"
            }
        );
        Program::parse(&yaml)
            .unwrap()
            .run(&source.view(), &AtomicBool::new(false))
            .unwrap()
            .nodes
            .remove(0)
    }

    #[test]
    fn value_editor_encodes_exact_integer_limits_and_float_byte_order() {
        for endian in [Endian::Little, Endian::Big] {
            for (kind, text, little) in [
                ("u8", "255", vec![255]),
                ("i8", "-128", vec![128]),
                ("u16", "65535", u16::MAX.to_le_bytes().to_vec()),
                ("i16", "-32768", i16::MIN.to_le_bytes().to_vec()),
                ("u32", "4294967295", u32::MAX.to_le_bytes().to_vec()),
                ("i32", "-2147483648", i32::MIN.to_le_bytes().to_vec()),
                (
                    "u64",
                    "18446744073709551615",
                    u64::MAX.to_le_bytes().to_vec(),
                ),
                (
                    "i64",
                    "-9223372036854775808",
                    i64::MIN.to_le_bytes().to_vec(),
                ),
                ("f32", "-12.5", (-12.5_f32).to_le_bytes().to_vec()),
                ("f64", "-12.5", (-12.5_f64).to_le_bytes().to_vec()),
            ] {
                let node = numeric_test_node(kind, endian);
                let (actual, displayed) = encode_value(&node, text).unwrap();
                let mut expected = little;
                if endian == Endian::Big {
                    expected.reverse();
                }
                assert_eq!(actual, expected, "{kind} {endian:?}");
                assert_eq!(displayed, text);
            }
        }
    }

    #[test]
    fn value_editor_rejects_out_of_range_fractional_and_variable_width_edits() {
        for (kind, invalid) in [
            ("u8", "256"),
            ("i8", "-129"),
            ("u16", "65536"),
            ("i16", "32768"),
            ("u32", "4294967296"),
            ("i32", "2147483648"),
            ("u64", "18446744073709551616"),
            ("u64", "-1"),
            ("u64", "9007199254740993.1"),
            ("i64", "9223372036854775808"),
            ("i64", "-9223372036854775809"),
            ("u32", "1.5"),
            ("f32", "1e100"),
            ("f64", "1e400"),
            ("f64", "NaN"),
            ("f32", "inf"),
        ] {
            assert!(
                encode_value(&numeric_test_node(kind, Endian::Little), invalid).is_err(),
                "{kind} {invalid}"
            );
        }
        let mut node = numeric_test_node("u64", Endian::Little);
        node.range = 0..1;
        assert!(!editable_value(&node));
        assert!(encode_value(&node, "7").is_err());
        node.kind = "varuint".into();
        assert!(encode_value(&node, "7").is_err());
        node.kind = "string".into();
        assert!(encode_value(&node, "7").is_err());
    }

    #[test]
    fn builder_uses_selection_and_preserves_existing_types() {
        let mut builder = Builder::from_range(16..20);
        let yaml = builder.generate("").unwrap();
        let program = Program::parse(&yaml).unwrap();
        assert!(program.structures.contains_key("Record"));
        let Action::Read(read) = &program.actions[0] else {
            panic!()
        };
        assert!(matches!(read.at, Some(Expr::Unsigned(16))));
        builder.name = "Other".into();
        builder.instance = "other".into();
        builder.fields[0].repeat = "record.count".into();
        let updated = builder.generate(&yaml).unwrap();
        let program = Program::parse(&updated).unwrap();
        assert_eq!(program.structures.len(), 2);
        assert_eq!(program.actions.len(), 2);
        assert!(updated.contains("ref: record.count"));
        assert!(builder.generate(&updated).is_err());
    }
    #[test]
    fn mapping_builder_generates_runtime_type_then_reads_it() {
        let mut builder = Builder::from_range(100..108);
        builder.mapping_type = true;
        builder.name = "Payload".into();
        builder.mapping.from = "header.items".into();
        builder.mapping.code = "code".into();
        builder.mapping.label = "label".into();
        builder.mapping.cases = "1 = u16\n2 = f32".into();
        builder.mapping.repeat = "count".into();
        let yaml = builder.generate("").unwrap();
        let p = Program::parse(&yaml).unwrap();
        let Action::Define(d) = &p.actions[0] else {
            panic!()
        };
        assert_eq!(d.from, "header.items");
        assert_eq!(d.fields.kind.cases["2"], "f32");
        assert!(matches!(p.actions[1], Action::Read(_)));
    }
    #[test]
    fn readable_values_do_not_accept_executable_expressions() {
        assert!(matches!(
            expression("header.length").unwrap(),
            Expr::Ref { .. }
        ));
        assert!(matches!(expression("0x10").unwrap(), Expr::Unsigned(16)));
        assert!(expression("system('anything')").is_err());
        assert!(expression("header.count * 2").is_err());
    }
    fn bytes(data: &[u8]) -> crate::byte_store::ByteStore {
        crate::byte_store::ByteStore::from_snapshot(
            crate::binary::Original::from_bytes(data).unwrap(),
        )
    }
    fn settle(workspace: &mut StructureWorkspace, source: ByteView, ctx: &Context) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while workspace.is_parsing() && Instant::now() < deadline {
            workspace.tick(ctx, source.clone());
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!workspace.is_parsing(), "parser worker did not complete");
    }
    #[test]
    fn failed_yaml_run_publishes_inspectable_prefix_without_claiming_success() {
        let source = bytes(&[9, 1, 1, 42, 2, 3, 99]);
        let ctx = Context::default();
        let yaml = r#"
structures:
  - name: Section
    fields:
      - {name: tag, type: u8}
      - {name: size, type: u8}
      - {name: payload, type: bytes, length: {ref: size}}
actions:
  - read: {name: signature, type: u8}
  - read: {name: sections, type: {structure: Section}, repeat: until end}
"#;
        let mut workspace = StructureWorkspace::default();
        assert!(workspace.apply(yaml, source.view(), &ctx));
        settle(&mut workspace, source.view(), &ctx);
        assert!(workspace.has_output());
        assert!(!workspace.is_ready());
        assert!(!workspace.has_published_program());
        assert!(!workspace.take_decoded());
        assert!(matches!(workspace.take_run_finished(), Some(RunOutcome::Failed(_))));
        assert_eq!(workspace.partial_output.as_ref().unwrap().nodes[1].children.len(), 2);
    }
    #[test]
    fn form_links_header_values_to_reused_instances_and_decodes_them() {
        let mut header = Builder::from_range(0..8);
        header.name = "Header".into();
        header.instance = "header".into();
        header.fields = vec![
            DraftField {
                name: "count".into(),
                ..DraftField::default()
            },
            DraftField {
                name: "offset".into(),
                ..DraftField::default()
            },
        ];
        let yaml = header.generate("").unwrap();
        let mut record = Builder::from_range(8..10);
        record.name = "Record".into();
        record.instance = "example".into();
        let yaml = record.generate(&yaml).unwrap();
        let mut repeated = Builder::from_range(8..10);
        repeated.reuse = true;
        repeated.reuse_type = "Record".into();
        repeated.instance = "records".into();
        repeated.reference = "header.count".into();
        repeated.bind_field = 0;
        repeated.bind_property = 0;
        repeated.link_reference();
        repeated.reference = "header.offset".into();
        repeated.bind_property = 2;
        repeated.link_reference();
        let yaml = repeated.generate(&yaml).unwrap();
        let source = bytes(&[3, 0, 0, 0, 8, 0, 0, 0, 1, 0, 2, 0, 3, 0]);
        let output = Program::parse(&yaml)
            .unwrap()
            .run(&source.view(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(output.nodes[2].children.len(), 3);
        assert_eq!(output.nodes[2].range, 8..14);
        assert_eq!(
            output.nodes[2].children[2].children[0].value,
            Some(Value::Unsigned(3))
        );
    }
    #[test]
    fn mapping_form_decodes_later_payload_from_a_nested_description_list() {
        let base = r#"
structures:
  - name: Description
    fields:
      - {name: label, type: string, length: 4}
      - {name: code, type: u8}
      - {name: count, type: u8}
  - name: Header
    fields:
      - {name: items, type: {structure: Description}, repeat: 2}
actions:
  - read: {name: header, type: {structure: Header}}
"#;
        let mut builder = Builder::from_range(12..18);
        builder.mapping_type = true;
        builder.name = "Payload".into();
        builder.instance = "payload".into();
        builder.mapping.from = "header.items".into();
        builder.mapping.code = "code".into();
        builder.mapping.label = "label".into();
        builder.mapping.cases = "1 = u16\n2 = i16".into();
        builder.mapping.repeat = "count".into();
        let yaml = builder.generate(base).unwrap();
        let mut source = bytes(b"left\x01\x02rght\x02\x01\xff\xff\x07\x00\xfe\xff");
        let program = Program::parse(&yaml).unwrap();
        let output = program
            .run(&source.view(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            output.nodes[1].children[0].children[0].value,
            Some(Value::Unsigned(65535))
        );
        source.replace(4, &[2]).unwrap();
        let changed = program
            .run(&source.view(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            changed.nodes[1].children[0].children[0].value,
            Some(Value::Signed(-1))
        );
    }
    #[test]
    fn bad_apply_cancels_prior_work_and_cannot_be_overwritten_by_its_result() {
        let source = bytes(&[1, 2]);
        let ctx = Context::default();
        let mut workspace = StructureWorkspace::default();
        assert!(workspace.apply(
            "actions: [{read: {name: first, type: u8}}]",
            source.view(),
            &ctx
        ));
        let cancellation = workspace.cancel.clone();
        assert!(!workspace.apply(
            "actions: [{read: {name: x, type: {structure: missing}, typo: true}}]",
            source.view(),
            &ctx
        ));
        assert!(cancellation.load(Ordering::Relaxed));
        assert!(!workspace.has_program());
        workspace.tick(&ctx, source.view());
        assert!(workspace.error.is_some());
        assert!(workspace.output.is_none());
    }
    #[test]
    fn edits_invalidate_background_results_and_refresh_selected_values() {
        let mut source = bytes(&[1, 2]);
        let ctx = Context::default();
        let mut workspace = StructureWorkspace::default();
        assert!(workspace.apply(
            "actions: [{read: {name: first, type: u8}}]",
            source.view(),
            &ctx
        ));
        settle(&mut workspace, source.view(), &ctx);
        assert!(workspace.is_ready());
        assert!(workspace.apply(
            "actions: [{read: {name: first, type: u8}}]",
            source.view(),
            &ctx
        ));
        let old_cancel = workspace.cancel.clone();
        source.replace(0, &[9]).unwrap();
        workspace.tick(&ctx, source.view());
        assert!(old_cancel.load(Ordering::Relaxed));
        assert!(workspace.output.is_none());
        assert!(!workspace.is_parsing());
        workspace.select_bytes(0..1);
        workspace.reparse(source.view(), &ctx);
        settle(&mut workspace, source.view(), &ctx);
        let output = workspace.output.as_ref().unwrap();
        assert_eq!(output.revision, source.view().revision);
        assert_eq!(output.nodes[0].value, Some(Value::Unsigned(9)));
        assert_eq!(workspace.views.selected_node(output).unwrap().path, "first");
        workspace.clear_file();
        assert!(!workspace.has_program());
        assert!(workspace.output.is_none());
    }
    #[test]
    fn byte_edits_cancel_unpublished_candidates_without_automatic_retry() {
        let mut source = bytes(&[1, 2]);
        let ctx = Context::default();
        let mut workspace = StructureWorkspace::default();
        workspace.apply(
            "actions: [{read: {name: first, type: u8}}]",
            source.view(),
            &ctx,
        );
        let cancel = workspace.cancel.clone();
        source.replace(0, &[9]).unwrap();
        workspace.tick(&ctx, source.view());
        workspace.reparse(source.view(), &ctx);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!workspace.has_program());
        assert!(!workspace.has_output());
        assert!(!workspace.is_parsing());
        assert!(!workspace.take_decoded());
        assert_eq!(workspace.take_run_finished(), None);
    }
    #[test]
    fn repeat_keywords_paths_and_type_picker_match_engine_syntax() {
        assert!(
            matches!(repeat_expression("until end").unwrap(), Some(Repeat::Until(v)) if v == "until end")
        );
        assert!(repeat_expression("one").unwrap().is_none());
        assert!(matches!(
            expression("/header.my-field[12]").unwrap(),
            Expr::Ref { .. }
        ));
        for invalid in [
            "header..count",
            "header.items[-1]",
            "header.items[]",
            "header[0]tail",
            "/",
        ] {
            assert!(expression(invalid).is_err(), "accepted {invalid}");
        }
        assert!(!available_types("").contains(&"u24".to_owned()));
        let mut builder = Builder::from_range(0..1);
        builder.fields[0].repeat = "until end".into();
        let yaml = builder.generate("").unwrap();
        assert!(yaml.contains("until end"));
        let output = Program::parse(&yaml)
            .unwrap()
            .run(&bytes(&[1, 2, 3]).view(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(output.nodes[0].children[0].children.len(), 3);
    }
    #[test]
    fn guided_steps_decode_a_header_calculation_and_repeated_payload() {
        let mut header = Builder::from_range(0..2);
        header.name = "Header".into();
        header.instance = "header".into();
        header.fields[0].name = "count".into();
        let mut yaml = header.generate("").unwrap();
        let StepDraft::Calculate(mut calculation) = Operation::Calculate.draft() else {
            panic!()
        };
        calculation.name = "metadata_bytes".into();
        for action in [
            Action::Calculate(calculation.build().unwrap()),
            Action::Skip(Expr::Ref {
                reference: "metadata_bytes".into(),
            }),
            Action::Align(Expr::Unsigned(4)),
        ] {
            yaml = update_steps(
                &yaml,
                StepChange::Save {
                    index: None,
                    action,
                },
            )
            .unwrap();
        }
        let StepDraft::Read(mut samples) = Operation::Read.draft() else {
            panic!()
        };
        samples.repeat_mode = 1;
        samples.repeat.text = "header.count".into();
        yaml = update_steps(
            &yaml,
            StepChange::Save {
                index: None,
                action: Action::Read(samples.build().unwrap()),
            },
        )
        .unwrap();
        let program = Program::parse(&yaml).unwrap();
        let references = earlier_references(&program, 4);
        assert!(references.contains(&"header.count".to_owned()));
        assert!(references.contains(&"metadata_bytes".to_owned()));
        assert!(!references.contains(&"samples".to_owned()));
        let output = program
            .run(
                &bytes(&[3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 5, 0, 6, 0]).view(),
                &AtomicBool::new(false),
            )
            .unwrap();
        let samples = output
            .nodes
            .iter()
            .find(|node| node.name == "samples")
            .unwrap();
        assert_eq!(samples.range, 8..14);
        assert_eq!(
            samples
                .children
                .iter()
                .map(|node| node.numeric().unwrap())
                .collect::<Vec<_>>(),
            vec![4., 5., 6.]
        );
    }

    #[test]
    fn editing_forms_preserve_existing_checks_expressions_and_nested_branches() {
        let yaml = r#"
structures:
  - name: Header
    fields: [{name: count, type: u16}]
actions:
  - read: {name: header, type: {structure: Header}, endian: big}
  - read:
      name: bytes
      type: bytes
      length: {multiply: [{ref: header.count}, 2]}
      at: 2
      align: 2
      expect: test
  - define:
      name: Payload
      from: descriptions
      fields:
        name: {ref: label}
        type: {ref: code, cases: {'1': u16}, otherwise: u8}
        repeat: {add: [{ref: count}, 1]}
        relative: {ref: offset}
  - choose:
      ref: header.count
      cases:
        '1':
          - read: {name: value, type: {choose: {ref: header.count, cases: {'1': u16}}}}
          - align: 4
      otherwise: [{skip: 2}]
  - calculate: {name: length, value: {multiply: [{ref: header.count}, 4]}}
"#;
        let original = Program::parse(yaml).unwrap();
        for action in &original.actions {
            let edited = StepDraft::from_action(action).build().unwrap();
            assert_eq!(
                serde_yaml_ng::to_string(action).unwrap(),
                serde_yaml_ng::to_string(&edited).unwrap()
            );
        }
        let mut draft = StepDraft::from_action(&original.actions[1]);
        let StepDraft::Read(read) = &mut draft else {
            panic!()
        };
        read.name = "payload_bytes".into();
        let updated = update_steps(
            yaml,
            StepChange::Save {
                index: Some(1),
                action: draft.build().unwrap(),
            },
        )
        .unwrap();
        let edited = Program::parse(&updated).unwrap();
        let Action::Read(read) = &edited.actions[1] else {
            panic!()
        };
        assert!(matches!(read.length, Some(Expr::Multiply { .. })));
        assert!(read.expect.is_some());
        assert!(read.align.is_some());
        assert_eq!(read.name, "payload_bytes");
    }

    #[test]
    fn choose_form_edits_nested_operations_without_losing_other_branches() {
        let program = Program::parse(
            r#"
actions:
  - read: {name: version, type: u8}
  - choose:
      ref: version
      cases:
        '1': [{read: {name: small, type: u8}}]
        '2': [{skip: 1}, {read: {name: large, type: i16}}]
"#,
        )
        .unwrap();
        let mut draft = StepDraft::from_action(&program.actions[1]);
        let StepDraft::Choose(choice) = &mut draft else {
            panic!()
        };
        let StepDraft::Read(read) = &mut choice.cases[1].1[1] else {
            panic!()
        };
        read.name = "sample".into();
        let yaml = update_steps(
            &serde_yaml_ng::to_string(&program).unwrap(),
            StepChange::Save {
                index: Some(1),
                action: draft.build().unwrap(),
            },
        )
        .unwrap();
        let output = Program::parse(&yaml)
            .unwrap()
            .run(&bytes(&[2, 0, 255, 255]).view(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(output.nodes[1].name, "sample");
        assert_eq!(output.nodes[1].value, Some(Value::Signed(-1)));
        let branch = Program::parse(&yaml)
            .unwrap()
            .run(&bytes(&[1, 7]).view(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(branch.nodes[1].name, "small");
        assert_eq!(branch.nodes[1].value, Some(Value::Unsigned(7)));
    }

    #[test]
    fn step_order_and_empty_drafts_preserve_the_structure_library() {
        let base = Builder::from_range(0..2).generate("").unwrap();
        let appended = update_steps(
            &base,
            StepChange::Save {
                index: None,
                action: Action::Skip(Expr::Unsigned(1)),
            },
        )
        .unwrap();
        let moved = update_steps(
            &appended,
            StepChange::Move {
                index: 1,
                target: 0,
            },
        )
        .unwrap();
        let program = Program::parse(&moved).unwrap();
        assert!(matches!(program.actions[0], Action::Skip(_)));
        assert!(program.structures.contains_key("Record"));
        let one = update_steps(&moved, StepChange::Remove(0)).unwrap();
        let empty = update_steps(&one, StepChange::Remove(0)).unwrap();
        assert!(draft_program(&empty).unwrap().actions.is_empty());
        assert!(available_types(&empty).contains(&"Record".into()));
        assert!(
            Program::parse(&empty).is_err(),
            "An empty draft must not pretend it decodes data"
        );
        let restored = update_steps(
            &empty,
            StepChange::Save {
                index: None,
                action: Action::Read(Field::scalar("record", "Record")),
            },
        )
        .unwrap();
        assert!(Program::parse(&restored)
            .unwrap()
            .structures
            .contains_key("Record"));
        let invalid = "structures: [broken YAML";
        assert!(update_steps(invalid, StepChange::Remove(0)).is_err());
    }

    #[test]
    fn interpretation_failure_preserves_published_structure() {
        let source = bytes(&[3, 65, 66, 67, 0, 0, 0, 0]);
        let ctx = Context::default();
        let mut workspace = StructureWorkspace::default();
        workspace.apply("actions: [{read: {name: previous, type: u8}}]", source.view(), &ctx);
        settle(&mut workspace, source.view(), &ctx);
        let yaml = "structures: [{name: Header, fields: [{name: count, type: u8}, {name: data, type: bytes, length: {ref: count}}]}]\nactions:\n  - read: {name: field, type: {structure: Header}, at: 0}\n";
        workspace.apply_interpretation(yaml, source.view(), &ctx, crate::direct_authoring::Interpretation {name: "field".into(), range: 0..2});
        settle(&mut workspace, source.view(), &ctx);
        assert!(matches!(workspace.take_run_finished(), Some(RunOutcome::Failed(_))));
        assert_eq!(workspace.root_range("previous"), Some(0..1));
        assert_eq!(workspace.root_range("field"), None);
        workspace.apply_interpretation(yaml, source.view(), &ctx, crate::direct_authoring::Interpretation {name: "field".into(), range: 0..8});
        settle(&mut workspace, source.view(), &ctx);
        assert!(matches!(workspace.take_run_finished(), Some(RunOutcome::Published)));
        assert_eq!(workspace.root_range("field"), Some(0..4));
    }

    #[test]
    fn decoded_event_only_fires_for_successful_explicit_apply() {
        let mut source = bytes(&[1, 0]);
        let ctx = Context::default();
        let mut workspace = StructureWorkspace::default();
        let yaml = "actions: [{read: {name: value, type: u16}}]";
        assert!(workspace.apply(yaml, source.view(), &ctx));
        assert!(!workspace.take_decoded());
        settle(&mut workspace, source.view(), &ctx);
        assert!(workspace.take_decoded());
        assert!(!workspace.take_decoded());
        source.replace(0, &[2]).unwrap();
        workspace.reparse(source.view(), &ctx);
        settle(&mut workspace, source.view(), &ctx);
        assert!(!workspace.take_decoded());
        assert!(workspace.apply(yaml, source.view(), &ctx));
        workspace.invalidate();
        assert!(!workspace.take_decoded());
        assert!(workspace.apply(
            "actions: [{read: {name: bad, type: u64}}]",
            source.view(),
            &ctx
        ));
        settle(&mut workspace, source.view(), &ctx);
        assert!(workspace.error.is_some());
        assert!(!workspace.take_decoded());
    }

    fn editor_frame(
        workspace: &mut StructureWorkspace,
        yaml: &mut String,
        ctx: &Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(750., 1000.),
                )),
                events,
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    if let Some(next) =
                        workspace.editor(ui, yaml, Some(0..2), DecoderLanguage::Yaml)
                    {
                        *yaml = next;
                    }
                });
            },
        )
    }
    fn editor_click(
        workspace: &mut StructureWorkspace,
        yaml: &mut String,
        ctx: &Context,
        label: &str,
    ) {
        editor_frame(workspace, yaml, ctx, vec![]);
        let output = editor_frame(workspace, yaml, ctx, vec![]);
        let position = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.pos + text.galley.rect.center().to_vec2())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing {label:?} in definition editor"));
        editor_frame(
            workspace,
            yaml,
            ctx,
            vec![egui::Event::PointerMoved(position)],
        );
        for pressed in [true, false] {
            editor_frame(
                workspace,
                yaml,
                ctx,
                vec![egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
    }
    fn settle_python(workspace: &mut StructureWorkspace, source: ByteView, ctx: &Context) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while workspace.is_parsing() && Instant::now() < deadline {
            workspace.tick(ctx, source.clone());
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!workspace.is_parsing(), "Python decoder did not complete");
    }

    #[test]
    fn plain_python_succeeds_without_creating_a_read_plan() {
        let ctx = Context::default();
        let source = bytes(&[3, 4]);
        let mut workspace = StructureWorkspace::default();
        workspace.apply_python("test = 5", None, source.view(), &ctx);
        settle_python(&mut workspace, source.view(), &ctx);
        assert!(workspace.decode_error().is_none());
        assert_eq!(workspace.take_run_finished(), Some(RunOutcome::NoData));
        assert_eq!(workspace.take_run_finished(), None);
        assert!(!workspace.take_decoded());
        assert!(!workspace.has_program());
        assert!(!workspace.is_ready());
        assert!(workspace.output.is_none());
        assert_eq!(workspace.python_stdout, "Script finished. No output.");

        workspace.apply_python("print(5)", None, source.view(), &ctx);
        settle_python(&mut workspace, source.view(), &ctx);
        assert_eq!(workspace.python_stdout, "5\n");
        assert_eq!(workspace.take_run_finished(), Some(RunOutcome::NoData));
        assert!(!workspace.has_program());
    }

    #[test]
    fn unsuccessful_or_plain_python_preserves_yaml_results_and_selection() {
        let ctx = Context::default();
        let mut source = bytes(&[3, 4]);
        let mut workspace = StructureWorkspace::default();
        workspace.apply(
            "actions: [{read: {name: original, type: u8}}]",
            source.view(),
            &ctx,
        );
        settle(&mut workspace, source.view(), &ctx);
        assert_eq!(workspace.take_run_finished(), Some(RunOutcome::Published));
        assert!(workspace.take_decoded());
        workspace.select_bytes(0..1);
        let original_nodes = workspace.output.as_ref().unwrap().nodes.as_ptr();

        for (code, successful) in [
            ("test = 5", true),
            ("print(file.read_at(0, 1))", true),
            ("raise ValueError('test failure')", false),
        ] {
            workspace.apply_python(code, None, source.view(), &ctx);
            assert_eq!(
                workspace.output.as_ref().unwrap().nodes.as_ptr(),
                original_nodes
            );
            assert_eq!(workspace.selected_node().unwrap().path, "original");
            settle_python(&mut workspace, source.view(), &ctx);
            let outcome = workspace.take_run_finished().unwrap();
            if successful {
                assert_eq!(outcome, RunOutcome::NoData);
                assert!(workspace.error.is_none());
            } else {
                assert!(
                    matches!(outcome, RunOutcome::Failed(ref message) if message.contains("test failure"))
                );
            }
            assert!(!workspace.take_decoded());
            assert!(workspace.is_ready());
            assert!(workspace.python_source.is_none());
            assert_eq!(
                workspace.output.as_ref().unwrap().nodes.as_ptr(),
                original_nodes
            );
            assert_eq!(workspace.selection, Some(0..1));
            assert_eq!(workspace.selected_node().unwrap().path, "original");
        }
        workspace.apply_python("while True: pass", None, source.view(), &ctx);
        workspace.cancel_decoding();
        assert_eq!(workspace.take_run_finished(), Some(RunOutcome::Cancelled));
        assert_eq!(
            workspace.output.as_ref().unwrap().nodes.as_ptr(),
            original_nodes
        );
        assert_eq!(workspace.selected_node().unwrap().path, "original");
        assert!(!workspace.is_parsing());
        assert!(workspace.is_ready());

        assert!(!workspace.apply("actions: [{read: {type: {structure: missing}}}]", source.view(), &ctx));
        assert!(matches!(
            workspace.take_run_finished(),
            Some(RunOutcome::Failed(_))
        ));
        assert_eq!(
            workspace.output.as_ref().unwrap().nodes.as_ptr(),
            original_nodes
        );
        source.replace(0, &[9]).unwrap();
        workspace.reparse(source.view(), &ctx);
        assert!(workspace.output.is_none());
        settle(&mut workspace, source.view(), &ctx);
        assert_eq!(
            workspace.selected_node().unwrap().value,
            Some(Value::Unsigned(9))
        );
        assert_eq!(workspace.selected_node().unwrap().path, "original");
        assert_eq!(workspace.take_run_finished(), None);
    }

    #[test]
    fn byte_edits_refresh_the_last_published_python_after_plain_or_failed_runs() {
        let ctx = Context::default();
        let mut source = bytes(&[3, 4]);
        let mut workspace = StructureWorkspace::default();
        let code = "from binwave import Struct\nfile.read_struct('original', Struct('Record', [('value', 'u8')]))";
        workspace.apply_python(code, Some(Path::new("original.py")), source.view(), &ctx);
        settle_python(&mut workspace, source.view(), &ctx);
        assert_eq!(workspace.take_run_finished(), Some(RunOutcome::Published));
        assert!(workspace.take_decoded());
        workspace.select_bytes(0..1);
        for attempt in ["test = 5", "raise ValueError('test failure')"] {
            workspace.apply_python(attempt, Some(Path::new("scratch.py")), source.view(), &ctx);
            settle_python(&mut workspace, source.view(), &ctx);
            assert!(!workspace.take_decoded());
            assert_eq!(workspace.python_source.as_ref().unwrap().code, code);
            assert_eq!(
                workspace.python_source.as_ref().unwrap().path.as_deref(),
                Some(Path::new("original.py"))
            );
            assert_eq!(
                workspace.selected_node().unwrap().value,
                Some(Value::Unsigned(3))
            );
        }
        // A failing YAML replacement must preserve the Python refresh source too.
        workspace.apply(
            "actions: [{read: {name: too_wide, type: u64}}]",
            source.view(),
            &ctx,
        );
        settle(&mut workspace, source.view(), &ctx);
        assert!(matches!(
            workspace.take_run_finished(),
            Some(RunOutcome::Failed(_))
        ));
        assert_eq!(workspace.python_source.as_ref().unwrap().code, code);
        source.replace(0, &[9]).unwrap();
        workspace.reparse(source.view(), &ctx);
        assert!(workspace.output.is_none());
        settle_python(&mut workspace, source.view(), &ctx);
        assert_eq!(
            workspace.selected_node().unwrap().value,
            Some(Value::Unsigned(9))
        );
        assert_eq!(workspace.take_run_finished(), None);
        assert!(!workspace.take_decoded());
    }

    #[test]
    fn no_data_after_byte_revision_change_never_restores_stale_decoding() {
        let ctx = Context::default();
        let mut source = bytes(&[3]);
        let mut workspace = StructureWorkspace::default();
        let code = "from binwave import Struct\nif file.read_at(0, 1) == bytes([3]):\n    file.read_struct('value', Struct('Record', [('value', 'u8')]))";
        workspace.apply_python(code, None, source.view(), &ctx);
        settle_python(&mut workspace, source.view(), &ctx);
        assert!(workspace.is_ready());
        workspace.take_decoded();
        workspace.take_run_finished();
        source.replace(0, &[4]).unwrap();
        workspace.reparse(source.view(), &ctx);
        assert!(workspace.output.is_none());
        settle_python(&mut workspace, source.view(), &ctx);
        assert!(workspace.error.is_none());
        assert!(workspace.output.is_none());
        assert!(!workspace.is_ready());
        assert!(!workspace.take_decoded());
        assert_eq!(workspace.take_run_finished(), None);
    }

    #[test]
    fn python_decoder_reruns_header_logic_after_byte_edits() {
        let ctx = Context::default();
        let mut data = vec![0; 401];
        data[0] = 200;
        let mut source = bytes(&data);
        let code = r#"from binwave import Struct
Header = Struct('Header', [('count', 'u8')])
header = file.read_struct('header', Header)
Sample = Struct('Sample', [('value', 'u16')])
samples = file.records('samples', Sample, count=header.count)
print(f'{len(samples)} samples')
"#;
        let mut workspace = StructureWorkspace::default();
        assert!(workspace.apply_python(code, None, source.view(), &ctx));
        assert!(workspace.has_program());
        settle_python(&mut workspace, source.view(), &ctx);
        assert!(
            workspace.decode_error().is_none(),
            "{:?}",
            workspace.decode_error()
        );
        assert!(workspace.is_ready());
        assert!(workspace.python_stdout.contains("200 samples"));
        let sample_count = |workspace: &StructureWorkspace| {
            workspace
                .output
                .as_ref()
                .unwrap()
                .nodes
                .iter()
                .find(|node| node.path == "samples")
                .unwrap()
                .lazy
                .as_ref()
                .unwrap()
                .count
        };
        assert_eq!(sample_count(&workspace), 200);
        assert!(workspace.take_decoded());
        assert!(!workspace.take_decoded());
        source.replace(0, &[129]).unwrap();
        workspace.reparse(source.view(), &ctx);
        settle_python(&mut workspace, source.view(), &ctx);
        assert!(
            workspace.decode_error().is_none(),
            "{:?}",
            workspace.decode_error()
        );
        assert_eq!(
            sample_count(&workspace),
            129,
            "Rerun Python instead of retaining the old concrete read plan"
        );
        assert!(workspace.python_stdout.contains("129 samples"));
        assert!(
            !workspace.take_decoded(),
            "Background byte edits do not force workspace navigation"
        );
        assert_eq!(
            workspace.output.as_ref().unwrap().revision,
            source.view().revision
        );
    }

    #[test]
    fn python_traceback_is_visible_beside_source_without_yaml_form_navigation() {
        let ctx = Context::default();
        let source = bytes(&[1, 0]);
        let mut code = "raise ValueError('bad descriptor')\n".to_owned();
        let mut workspace = StructureWorkspace::default();
        workspace.apply_python(
            &code,
            Some(Path::new("example_decoder.py")),
            source.view(),
            &ctx,
        );
        settle_python(&mut workspace, source.view(), &ctx);
        assert!(!workspace.is_ready());
        assert!(workspace.decode_error().unwrap().contains("bad descriptor"));
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(700., 650.),
                )),
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    workspace.editor(ui, &mut code, None, DecoderLanguage::Python);
                });
            },
        );
        let text: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(text.iter().any(|text| *text == code));
        assert!(text.contains(&"Console"));
        assert!(text
            .iter()
            .any(|text| text.contains("bad descriptor") && text.contains("Traceback")));
        for label in ["Source", "Fields", "Steps"] {
            assert!(
                !text.contains(&label),
                "Python does not expose unrelated authoring forms"
            );
        }
        // Inspecting a separate YAML draft must not attribute Python's
        // traceback to that draft, and switching back retains the run output.
        for language in [DecoderLanguage::Yaml, DecoderLanguage::Python] {
            let mut draft = if language == DecoderLanguage::Yaml {
                "structures: []\nactions: []\n".to_owned()
            } else {
                code.clone()
            };
            let rendered = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(700., 650.),
                    )),
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        workspace.editor(ui, &mut draft, None, language);
                    });
                },
            );
            let has_traceback = rendered.shapes.iter().any(|shape| {
                matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text().contains("Traceback"))
            });
            assert_eq!(has_traceback, language == DecoderLanguage::Python);
        }
    }

    #[test]
    fn stopping_python_then_applying_yaml_discards_the_python_worker_result() {
        let ctx = Context::default();
        let source = bytes(&[3]);
        let mut workspace = StructureWorkspace::default();
        workspace.apply_python("while True: pass", None, source.view(), &ctx);
        let cancel = workspace.cancel.clone();
        workspace.cancel_decoding();
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!workspace.is_parsing());
        assert!(!workspace.take_decoded());
        assert!(workspace.apply(
            "actions: [{read: {name: value, type: u8}}]",
            source.view(),
            &ctx
        ));
        settle(&mut workspace, source.view(), &ctx);
        assert!(workspace.is_ready());
        assert!(workspace.python_source.is_none());
        assert_eq!(
            workspace.output.as_ref().unwrap().nodes[0].value,
            Some(Value::Unsigned(3))
        );
    }

    #[test]
    fn python_highlighting_and_console_limits_preserve_unicode() {
        let code = "from binwave import Struct\nname = 'café' # 日本語\nvalue = 0x10\ntext = \"\"\"multiple\nlines 🦀\"\"\"\n";
        let job = source_layout(
            code,
            egui::FontId::monospace(13.),
            true,
            Color32::WHITE,
            DecoderLanguage::Python,
        );
        assert_eq!(job.text, code);
        for section in &job.sections {
            assert!(code.get(section.byte_range.start.0..section.byte_range.end.0).is_some());
        }
        let output = "🦀".repeat(40_000) + "\nValueError: final failure";
        let bounded = bounded_console(&output);
        assert!(bounded.len() < 65_600);
        assert!(bounded.ends_with("ValueError: final failure"));
    }

    #[test]
    fn yaml_is_the_default_editor_and_modes_preserve_uncommitted_fields() {
        let ctx = Context::default();
        let mut workspace = StructureWorkspace::default();
        let mut yaml = "structures: []\nactions: []\n".to_owned();
        assert!(workspace.editor_page == EditorPage::Yaml);
        let output = editor_frame(&mut workspace, &mut yaml, &ctx, vec![]);
        assert!(output.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == yaml)
        }));
        editor_click(&mut workspace, &mut yaml, &ctx, "Fields");
        assert!(
            !workspace.authoring_dirty(),
            "Mode selection must not create a draft"
        );
        editor_click(&mut workspace, &mut yaml, &ctx, "New from selection");
        assert!(workspace.authoring_dirty());
        workspace.builder.fields[0].name = "pending_value".into();
        editor_click(&mut workspace, &mut yaml, &ctx, "Source");
        assert!(!workspace.is_describing());
        assert!(workspace.authoring_dirty());
        assert_eq!(workspace.builder.fields[0].name, "pending_value");
        workspace.show_authoring_draft();
        assert!(workspace.is_describing());
        editor_click(&mut workspace, &mut yaml, &ctx, "Discard fields");
        assert!(!workspace.authoring_dirty());
    }

    #[test]
    fn external_yaml_replacement_resets_forms_and_superseded_diagnostics() {
        let mut workspace = StructureWorkspace::default();
        workspace.begin_definition(0..2);
        workspace.builder.fields[0].name = "old_field".into();
        workspace.error = Some("Old decoder failed".into());
        workspace.show_yaml();
        assert!(
            workspace.authoring_dirty(),
            "Ordinary mode switches keep the draft"
        );
        workspace.reset_authoring();
        workspace.show_yaml();
        assert!(!workspace.authoring_dirty());
        assert!(!workspace.builder.active);
        assert!(workspace.decode_error().is_none());
        assert!(workspace.editor_page == EditorPage::Yaml);
    }

    #[test]
    fn source_comment_toggle_preserves_unicode_indentation_and_line_selection() {
        use egui::text::{CCursor, CCursorRange};

        let original = "  # café\n\t# value\nlast: 3\n";
        let end = "  # café\n\t# value\n".chars().count();
        for reverse in [false, true] {
            let mut text = original.to_owned();
            let selection = if reverse {
                CCursorRange::two(CCursor::new(end), CCursor::new(0))
            } else {
                CCursorRange::two(CCursor::new(0), CCursor::new(end))
            };
            let next = toggle_source_comments(&mut text, selection);
            assert_eq!(text, "  café\n\tvalue\nlast: 3\n");
            assert_eq!(next.sorted_cursors()[0].index.0, 0);
            assert_eq!(next.sorted_cursors()[1].index.0, end - 4);
            assert_eq!(next.primary.index.0 == 0, reverse);
            let restored = toggle_source_comments(&mut text, next);
            assert_eq!(text, original);
            assert_eq!(restored, selection);
        }

        let mut mixed = "  value\n\n  # note\nlast\n".to_owned();
        let selected_end = "  value\n\n  # note\n".chars().count();
        let selected = CCursorRange::two(CCursor::new(0), CCursor::new(selected_end));
        let next = toggle_source_comments(&mut mixed, selected);
        assert_eq!(mixed, "  # value\n\n  # # note\nlast\n");
        toggle_source_comments(&mut mixed, next);
        assert_eq!(mixed, "  value\n\n  # note\nlast\n");

        let mut current = "🦀 = 1\n  value\n".to_owned();
        let selection = CCursorRange::one(CCursor::new(10));
        let next = toggle_source_comments(&mut current, selection);
        assert_eq!(current, "🦀 = 1\n  # value\n");
        assert_eq!(next.primary.index.0, 12);
        assert_eq!(toggle_source_comments(&mut current, next), selection);
        assert_eq!(current, "🦀 = 1\n  value\n");

        let mut blank = "\t\r\nlast".to_owned();
        let cursor = CCursorRange::one(CCursor::new(1));
        let next = toggle_source_comments(&mut blank, cursor);
        assert_eq!(blank, "\t# \r\nlast");
        assert_eq!(next.primary.index.0, 3);
        assert_eq!(toggle_source_comments(&mut blank, next), cursor);
        assert_eq!(blank, "\t\r\nlast");
    }

    #[test]
    fn source_comment_shortcut_requires_focus_and_supports_undo_in_both_languages() {
        for language in [DecoderLanguage::Yaml, DecoderLanguage::Python] {
            let ctx = Context::default();
            let mut cache = YamlLayoutCache::default();
            let mut source = language.header_example().to_owned();
            let original = source.clone();
            let mut frame = |source: &mut String, events: Vec<egui::Event>| {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(700., 400.),
                        )),
                        events,
                        ..Default::default()
                    },
                    |_ctx, ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            cache.editor(ui, source, language);
                        });
                    },
                )
            };
            let key = |key, modifiers| egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            };
            frame(&mut source, vec![]);
            let output = frame(
                &mut source,
                vec![key(egui::Key::Slash, egui::Modifiers::COMMAND)],
            );
            assert_eq!(
                source, original,
                "An unfocused editor must not handle the shortcut"
            );
            let position = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == source => {
                        Some(text.pos + egui::vec2(12., 6.))
                    }
                    _ => None,
                })
                .unwrap();
            frame(&mut source, vec![egui::Event::PointerMoved(position)]);
            for pressed in [true, false] {
                frame(
                    &mut source,
                    vec![egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    }],
                );
            }
            let id = ctx
                .memory(|memory| memory.focused())
                .expect("source editor focus");
            frame(
                &mut source,
                vec![key(egui::Key::A, egui::Modifiers::COMMAND)],
            );
            frame(
                &mut source,
                vec![key(egui::Key::Slash, egui::Modifiers::COMMAND)],
            );
            assert!(has_decoder_code(&source));
            assert!(source.starts_with("# "), "Instructions remain commented");
            let uncommented = source.clone();
            let selection = egui::TextEdit::load_state(&ctx, id)
                .unwrap()
                .cursor
                .char_range()
                .unwrap();
            assert_eq!(selection.sorted_cursors()[0].index.0, 0);
            assert_eq!(selection.sorted_cursors()[1].index.0, source.chars().count());
            frame(
                &mut source,
                vec![key(egui::Key::Z, egui::Modifiers::COMMAND)],
            );
            assert_eq!(source, original, "Toggle is one undoable edit");
            frame(
                &mut source,
                vec![key(
                    egui::Key::Z,
                    egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                )],
            );
            assert_eq!(source, uncommented);

            // The commented header example decodes after the documented keyboard sequence.
            let mut data = 2_u32.to_le_bytes().to_vec();
            for (time, value) in [(100_u32, 1.5_f32), (200, -2.5)] {
                data.extend_from_slice(&time.to_le_bytes());
                data.extend_from_slice(&value.to_le_bytes());
            }
            let data = bytes(&data);
            let cancel = AtomicBool::new(false);
            let parsed = match language {
                DecoderLanguage::Yaml => Program::parse(&source)
                    .unwrap()
                    .run(&data.view(), &cancel)
                    .unwrap(),
                DecoderLanguage::Python => {
                    let output =
                        crate::python_decoder::run(&source, None, &data.view(), &cancel).unwrap();
                    assert_eq!(output.stdout.trim(), "Decoded 2 samples");
                    output.decoded.unwrap().1
                }
            };
            let samples = &parsed.nodes[1];
            assert_eq!(samples.range, 4..20);
            assert_eq!(samples.children.len(), 2);
            assert_eq!(
                samples.children[0].children[0].value,
                Some(Value::Unsigned(100))
            );
            assert_eq!(
                samples.children[0].children[1].value,
                Some(Value::Float(1.5))
            );
            assert_eq!(
                samples.children[1].children[0].value,
                Some(Value::Unsigned(200))
            );
            assert_eq!(
                samples.children[1].children[1].value,
                Some(Value::Float(-2.5))
            );
        }
    }

    #[test]
    fn yaml_highlighting_preserves_utf8_quotes_comments_and_incomplete_drafts() {
        for text in [
            "# 日本語\nname: \"café # value\" # comment\ncount: 0x10\n",
            "name: 'it''s text'\nvalue: {ref: header.length}\n",
            "name: \"unfinished 🦀",
            r#"name: "escaped \"quoted\" text""#,
            "path: /a#b\nflag: true\nunknown: [",
            "",
        ] {
            for dark in [true, false] {
                let job = yaml_layout(text, egui::FontId::monospace(13.), dark, Color32::WHITE);
                assert_eq!(job.text, text);
                assert_eq!(job.wrap.max_width, f32::INFINITY);
                for section in &job.sections {
                    assert!(
                        text.get(section.byte_range.start.0..section.byte_range.end.0).is_some(),
                        "UTF-8 token boundaries"
                    );
                }
            }
        }
        let job = yaml_layout(
            "name: \"a # b\" # comment",
            egui::FontId::monospace(13.),
            true,
            Color32::WHITE,
        );
        let quoted = job
            .sections
            .iter()
            .find(|section| &job.text[section.byte_range.start.0..section.byte_range.end.0] == "\"a # b\"")
            .unwrap();
        let comment = job
            .sections
            .iter()
            .find(|section| &job.text[section.byte_range.start.0..section.byte_range.end.0] == "# comment")
            .unwrap();
        assert_ne!(quoted.format.color, comment.format.color);
    }

    #[test]
    fn updating_existing_format_preserves_other_fields_actions_and_decode_steps() {
        let ctx = Context::default();
        let mut yaml = r#"
endian: big
structures:
  - name: Header
    endian: little
    fields:
      - {name: count, type: u16, endian: big, expect: 2}
      - {name: payload, type: bytes, length: {ref: count}, relative: 2, align: 2}
    actions:
      - calculate: {name: computed, value: {add: [1, 2]}}
  - name: Other
    fields: [{name: trailing, type: u8}]
actions:
  - read: {name: header, type: {structure: Header}}
"#
        .to_owned();
        let original: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
        let mut workspace = StructureWorkspace::default();
        editor_click(&mut workspace, &mut yaml, &ctx, "Fields");
        editor_click(&mut workspace, &mut yaml, &ctx, "Edit format…");
        editor_click(&mut workspace, &mut yaml, &ctx, "Header");
        assert!(!workspace.authoring_dirty());
        workspace.formats.editing.as_mut().unwrap().fields[0].kind = "u32".into();
        assert!(workspace.authoring_dirty());
        editor_click(&mut workspace, &mut yaml, &ctx, "Source");
        assert!(workspace.authoring_dirty());
        workspace.show_authoring_draft();
        editor_click(&mut workspace, &mut yaml, &ctx, "Update format");
        assert!(!workspace.authoring_dirty());
        let updated: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
        let expected_program =
            Program::parse(&serde_yaml_ng::to_string(&original).unwrap()).unwrap();
        let updated_program = Program::parse(&yaml).unwrap();
        let mut expected_header = expected_program.structures["Header"].clone();
        expected_header.fields[0].kind = crate::structure::TypeSpec::Name("u32".into());
        assert_eq!(
            serde_yaml_ng::to_value(&updated_program.structures["Header"]).unwrap(),
            serde_yaml_ng::to_value(&expected_header).unwrap()
        );
        assert_eq!(updated["actions"], original["actions"]);
        assert_eq!(updated["endian"], original["endian"]);
        assert_eq!(
            serde_yaml_ng::to_value(&updated_program.structures["Other"]).unwrap(),
            serde_yaml_ng::to_value(&expected_program.structures["Other"]).unwrap()
        );
        assert_eq!(workspace.formats.editing.as_ref().unwrap().name, "Header");
        assert!(
            !workspace.take_run_requested(),
            "Updating fields changes the draft; Decode runs it explicitly"
        );
    }

    #[test]
    fn reusable_format_editor_rejects_stale_and_invalid_changes() {
        let yaml = "structures: [{name: Header, fields: [{name: value, type: u16}]}]\nactions: []\n";
        let mut editor = FormatEditor::default();
        editor.begin("Header", yaml).unwrap();
        editor.editing.as_mut().unwrap().fields[0].name = "changed".into();
        assert!(editor.dirty());
        assert!(
            editor.begin("Header", yaml).is_err(),
            "Do not discard another pending format"
        );
        assert!(editor
            .editing
            .as_ref()
            .unwrap()
            .save(&format!("{yaml}# newer draft\n"))
            .is_err());
        editor.editing.as_mut().unwrap().fields[0].repeat_mode = 1;
        editor.editing.as_mut().unwrap().fields[0].repeat.text = "invalid expression +".into();
        assert!(editor.editing.as_ref().unwrap().save(yaml).is_err());
    }

    #[test]
    fn user_can_add_and_save_a_read_step_through_the_guided_editor() {
        let ctx = Context::default();
        let mut workspace = StructureWorkspace::default();
        let mut yaml = String::new();
        editor_click(&mut workspace, &mut yaml, &ctx, "Steps");
        editor_click(&mut workspace, &mut yaml, &ctx, "Add step");
        editor_click(&mut workspace, &mut yaml, &ctx, "Read data");
        assert!(workspace.steps.editing.is_some());
        editor_click(&mut workspace, &mut yaml, &ctx, "Add this step");
        assert!(workspace.steps.editing.is_none());
        let program = Program::parse(&yaml).unwrap();
        assert_eq!(program.actions.len(), 1);
        let output = program
            .run(&bytes(&[42, 0]).view(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(output.nodes[0].value, Some(Value::Unsigned(42)));
        editor_click(&mut workspace, &mut yaml, &ctx, "Edit");
        yaml.push_str("\n# another editor updated the draft\n");
        editor_click(&mut workspace, &mut yaml, &ctx, "Save step");
        assert!(
            workspace.steps.editing.is_some(),
            "A stale form must not overwrite the changed draft"
        );
        assert!(yaml.contains("another editor"));
        editor_click(&mut workspace, &mut yaml, &ctx, "Cancel");
        assert!(workspace.steps.editing.is_none());
    }
    #[test]
    fn reference_picker_keeps_collections_without_inventing_scalar_paths() {
        let program = Program::parse(
            r#"
structures:
  - name: Item
    fields: [{name: count, type: u16}]
  - name: Header
    fields: [{name: items, type: {structure: Item}, repeat: 2}]
actions:
  - read: {name: header, type: {structure: Header}}
  - read: {name: later, type: u8}
"#,
        )
        .unwrap();
        let references = earlier_references(&program, 1);
        assert!(references.contains(&"header.items".into()));
        assert!(!references.contains(&"header.items.count".into()));
        assert!(!references.contains(&"later".into()));
    }
    #[test]
    fn describe_fields_use_actual_width_and_unique_names() {
        let mut builder = Builder::from_range(0..15);
        assert_eq!(builder.fields[0].length, "15");
        builder.fields[0].kind = "u16".into();
        builder.fields[0].change_format();
        assert!(builder.fields[0].length.is_empty());
        assert_eq!(builder.record_size(""), Some(2));
        builder.add_remaining_field();
        assert_eq!(builder.fields[1].length, "13");
        assert_eq!(builder.record_size(""), Some(15));
        builder.add_remaining_field();
        builder.fields.remove(1);
        builder.add_remaining_field();
        let names: std::collections::HashSet<_> = builder.fields.iter().map(|f| &f.name).collect();
        assert_eq!(names.len(), builder.fields.len());
    }

    #[test]
    fn describe_repeat_counts_records_without_padding_them_to_the_selection() {
        let mut builder = Builder::from_range(0..15);
        builder.fields = vec![
            DraftField {
                name: "id".into(),
                kind: "u8".into(),
                ..DraftField::default()
            },
            DraftField::default(),
        ];
        builder.repeat_selection = true;
        let yaml = builder.decode_draft("").unwrap();
        let program = Program::parse(&yaml).unwrap();
        let Action::Read(read) = &program.actions[0] else {
            panic!()
        };
        assert!(read.length.is_none());
        let output = program
            .run(&bytes(&[0; 15]).view(), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(output.nodes[0].children.len(), 3);
        assert_eq!(output.nodes[0].children[1].range, 5..10);
        assert_eq!(output.nodes[0].range, 0..15);
        builder.bounded = true;
        builder.bound_length = "8".into();
        builder.repeat_selection = false;
        builder.bound_length.clear();
        assert!(
            builder.decode_draft(&yaml).is_err(),
            "An empty record boundary must not silently use the selected span"
        );
        builder.bound_length = "8".into();
        builder.repeat_selection = true;
        assert!(
            builder.decode_draft(&yaml).is_err(),
            "A record boundary cannot silently change the repeat stride"
        );
    }

    #[test]
    fn describe_rerun_updates_its_own_definition_and_new_selection_gets_a_fresh_name() {
        let base = "actions: [{calculate: {name: record, value: 1}}]";
        let mut first = Builder::from_range(0..2);
        first.initialize_name(base);
        assert_eq!(first.instance, "record_2");
        first.instance = " record_2 ".into();
        let yaml = first.decode_draft(base).unwrap();
        first.fields[0].name = "sample".into();
        let yaml = first.decode_draft(&yaml).unwrap();
        let program = Program::parse(&yaml).unwrap();
        assert_eq!(program.actions.len(), 2);
        assert_eq!(program.structures.len(), 1);
        assert_eq!(program.structures["Record2"].fields[0].name, "sample");
        let mut second = Builder::from_range(2..4);
        second.initialize_name(&yaml);
        assert_eq!(second.instance, "record_3");
        let next = second.decode_draft(&yaml).unwrap();
        assert_eq!(Program::parse(&next).unwrap().structures.len(), 2);
        assert!(
            first.decode_draft(&next).is_err(),
            "An old form cannot replace a later edit"
        );
    }

    #[test]
    fn describe_form_fits_a_narrow_pane_and_decodes_in_one_action() {
        let ctx = Context::default();
        let mut workspace = StructureWorkspace::default();
        workspace.begin_definition(0..15);
        workspace.source = Some(bytes(&[0; 15]).view());
        let mut yaml = String::new();
        let frame = |workspace: &mut StructureWorkspace, yaml: &mut String, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400., 720.),
                    )),
                    events,
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        if let Some(next) =
                            workspace.editor(ui, yaml, Some(0..15), DecoderLanguage::Yaml)
                        {
                            *yaml = next;
                        }
                    });
                },
            )
        };
        frame(&mut workspace, &mut yaml, vec![]);
        let output = frame(&mut workspace, &mut yaml, vec![]);
        let texts: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text),
                _ => None,
            })
            .collect();
        for hidden in [
            "Type name",
            "Instance name",
            "Read at",
            "Repeat instances",
            "From descriptions",
            "Apply definition",
        ] {
            assert!(
                !texts.iter().any(|text| text.galley.text() == hidden),
                "{hidden} must not clutter the initial form"
            );
        }
        for label in [
            "Name",
            "Fields",
            "Raw bytes",
            "15 of 15 bytes described · 0 remaining",
            "Decode selection",
        ] {
            let text = texts
                .iter()
                .find(|text| text.galley.text() == label)
                .unwrap_or_else(|| panic!("Missing {label}"));
            let rect = text.galley.rect.translate(text.pos.to_vec2());
            assert!(
                rect.right() <= 400. && rect.bottom() <= 720.,
                "{label} clipped: {rect:?}"
            );
        }
        let button = texts
            .iter()
            .find(|text| text.galley.text() == "Decode selection")
            .unwrap();
        let pos = button.pos + button.galley.rect.center().to_vec2();
        frame(
            &mut workspace,
            &mut yaml,
            vec![egui::Event::PointerMoved(pos)],
        );
        for pressed in [true, false] {
            frame(
                &mut workspace,
                &mut yaml,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
        assert!(workspace.take_run_requested());
        assert!(!workspace.take_run_requested());
        assert_eq!(Program::parse(&yaml).unwrap().structures.len(), 1);
        assert!(
            workspace.is_describing(),
            "Keep fields available for corrections"
        );
    }
}
