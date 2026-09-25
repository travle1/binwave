//! Shared selection and bounded, asynchronous pages for decoded structure views.
#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use crate::structure_selection::{covering, sample_indices_for_bytes, sample_bytes};
use crate::byte_store::ByteView;
use crate::hex_editor::HexHighlight;
use crate::plot_trace::decoded_channel;
use crate::structure::{LazyArray, Node, ParseOutput, Value};
use crate::structure_plot::{PlotQuality, PlotReader};
use egui::{Color32, Ui};
use egui_plot::{Plot, PlotBounds, PlotPoint, PlotTransform, Points};
use std::collections::HashSet;
use std::ops::Range;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

const PAGE_SIZE: usize = 128;
const INITIAL_PLOT_SAMPLES: usize = 1024;
const MAX_HIGHLIGHTS: usize = 256;


#[derive(Clone, Debug, PartialEq, Eq)]
struct PageKey {
    path: String,
    start: usize,
    revision: u64,
}
#[derive(Clone)]
struct Request {
    key: PageKey,
    array: LazyArray,
}
struct Page {
    key: PageKey,
    result: Result<Vec<Node>, String>,
}

/// Explicit plot actions. Ordinary selection remains in the current pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlotAction {
    InspectBytes(Range<usize>),
    ExportBytes(Range<usize>),
}

#[derive(Clone, Copy)]
struct RangeZoom {
    origin: egui::Pos2,
    transform: PlotTransform,
}

#[derive(Clone)]
struct TableAnchor {
    target: String,
    revision: u64,
    bytes: Range<usize>,
}

#[derive(Default)]
pub struct StructureViews {
    selected: Option<String>,
    last_hex_selection: Option<Range<usize>>,
    pending_hex_selection: bool,
    reveal_selected_row: bool,
    expanded: HashSet<String>,
    reveal_tree: Option<String>,
    table_target: String,
    table_anchor: Option<TableAnchor>,
    table_jump: usize,
    navigation_selection: Option<Range<usize>>,
    target_initialized: bool,
    definition_index: usize,
    desired: Option<Request>,
    page: Option<Page>,
    rx: Option<mpsc::Receiver<Page>>,
    running: Option<PageKey>,
    cancel: Arc<AtomicBool>,
    source_revision: Option<u64>,
    repaint_context: Option<egui::Context>,
    plot_reader: PlotReader,
    presentation: crate::plot_presentation::Presentation,
    plot_hidden: HashSet<String>,
    plot_target: String,
    plot_sources: std::collections::BTreeSet<String>,
    plot_sources_initialized: bool,
    extra_plot_readers: std::collections::BTreeMap<String, PlotReader>,
    plot_view: Option<Range<usize>>,
    plot_axis_width: f32,
    plot_force_view: Option<Range<usize>>,
    plot_fit_y: bool,
    plot_free_y: bool,
    plot_range_zoom: Option<RangeZoom>,
    plot_zoom_y: Option<(f64, f64)>,
    plot_follow_selection: bool,
    plot_refine: bool,
    plot_anchor: Option<usize>,
    plot_jump: usize,
    plot_range_first: usize,
    plot_range_last: usize,
    plot_action: Option<PlotAction>,
}
impl Drop for StructureViews {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl StructureViews {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn take_plot_action(&mut self) -> Option<PlotAction> {
        self.plot_action.take()
    }
    pub fn invalidate(&mut self) {
        self.plot_reader.invalidate();
        for reader in self.extra_plot_readers.values_mut() { reader.invalidate(); }
        self.plot_action = None;
        self.plot_range_zoom = None;
        self.plot_zoom_y = None;
        self.navigation_selection = None;
        // Byte edits replace the decode output, but preserve the user's viewport.
        // reset() still starts fresh for a different file or definition.
        self.plot_force_view = self.plot_view.clone();
        self.plot_fit_y = true;
        self.plot_refine = false;
        self.plot_anchor = None;
        self.table_anchor = None;
        self.cancel.store(true, Ordering::Relaxed);
        self.desired = None;
        self.page = None;
        self.source_revision = None;
        self.last_hex_selection = None;
        self.selected = None;
        self.pending_hex_selection = false;
    }
    pub fn tick(&mut self, ctx: &egui::Context, source: ByteView) {
        self.repaint_context = Some(ctx.clone());
        self.plot_reader.tick(ctx);
        if self
            .source_revision
            .is_some_and(|revision| revision != source.revision)
        {
            self.invalidate();
        }
        self.source_revision = Some(source.revision);
        let completed = self.rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(page) => Some(page),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => self.running.clone().map(|key| Page {
                key,
                result: Err("Collection reader stopped unexpectedly.".into()),
            }),
        });
        if let Some(page) = completed {
            self.rx = None;
            self.running = None;
            if page.key.revision == source.revision
                && self
                    .desired
                    .as_ref()
                    .is_some_and(|request| request.key == page.key)
            {
                self.page = Some(page);
                self.resolve_pending_selection();
            }
        }
        let next = self
            .desired
            .as_ref()
            .filter(|request| {
                request.key.revision == source.revision
                    && !self
                        .page
                        .as_ref()
                        .is_some_and(|page| page.key == request.key)
            })
            .cloned();
        if let Some(request) = next {
            if self.running.as_ref().is_some_and(|key| key != &request.key) {
                self.cancel.store(true, Ordering::Relaxed);
            }
            if self.rx.is_none() {
                let (tx, rx) = mpsc::channel();
                self.rx = Some(rx);
                self.running = Some(request.key.clone());
                let cancel = Arc::new(AtomicBool::new(false));
                self.cancel = cancel.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let result = (|| -> anyhow::Result<Vec<Node>> {
                        source.validate()?;
                        let nodes =
                            request
                                .array
                                .page(&source, &cancel, request.key.start, PAGE_SIZE)?;
                        source.validate()?;
                        anyhow::ensure!(
                            !cancel.load(Ordering::Relaxed),
                            "collection request cancelled"
                        );
                        Ok(nodes)
                    })()
                    .map_err(|error| format!("{error:#}"));
                    let _ = tx.send(Page {
                        key: request.key,
                        result,
                    });
                    ctx.request_repaint();
                });
            }
        }
        if self.rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn request(&mut self, node: &Node, start: usize, revision: u64, ctx: &egui::Context) {
        self.queue_page(node, start, revision);
        ctx.request_repaint();
    }
    fn queue_page(&mut self, node: &Node, start: usize, revision: u64) {
        let Some(array) = &node.lazy else {
            return;
        };
        let start = start.min(array.count.saturating_sub(1)) / PAGE_SIZE * PAGE_SIZE;
        let key = PageKey {
            path: node.path.clone(),
            start,
            revision,
        };
        if self
            .desired
            .as_ref()
            .is_some_and(|request| request.key == key)
        {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.desired = Some(Request {
            key,
            array: array.clone(),
        });
        self.page = None;
        if let Some(ctx) = &self.repaint_context {
            ctx.request_repaint();
        }
    }
    fn select(&mut self, node: &Node) -> Range<usize> {
        self.table_anchor = None;
        self.selected = Some(node.path.clone());
        self.plot_follow_selection = true;
        self.pending_hex_selection = false;
        self.last_hex_selection = Some(node.range.clone());
        node.range.clone()
    }
    fn select_table_node(
        &mut self,
        output: &ParseOutput,
        row: &Node,
        field: &Node,
        extend: bool,
    ) -> Range<usize> {
        let anchor = if extend {
            self.table_anchor
                .as_ref()
                .filter(|anchor| {
                    anchor.target == self.table_target && anchor.revision == output.revision
                })
                .map(|anchor| anchor.bytes.clone())
                .or_else(|| {
                    let collection = find_node(&output.nodes, &self.table_target)?;
                    let selected =
                        sample_indices_for_bytes(collection, self.last_hex_selection.as_ref()?)?;
                    sample_bytes(collection, selected.start..selected.start + 1)
                })
        } else {
            None
        }
        .unwrap_or_else(|| row.range.clone());
        let range = if extend {
            anchor.start.min(row.range.start)..anchor.end.max(row.range.end)
        } else {
            field.range.clone()
        };
        self.select(if extend { row } else { field });
        self.plot_anchor = find_node(&output.nodes, &self.table_target)
            .and_then(|collection| sample_indices_for_bytes(collection, &anchor))
            .map(|samples| samples.start);
        self.table_anchor = Some(TableAnchor {
            target: self.table_target.clone(),
            revision: output.revision,
            bytes: anchor,
        });
        self.last_hex_selection = Some(range.clone());
        // Keep the clicked page visible while Hex and Plot receive the full range.
        self.reveal_selected_row = false;
        range
    }

    fn stale(&self, output: &ParseOutput) -> bool {
        self.source_revision
            .is_some_and(|revision| revision != output.revision)
    }
    fn page_nodes(&self, path: &str, revision: u64) -> Option<&[Node]> {
        self.page
            .as_ref()
            .filter(|page| page.key.path == path && page.key.revision == revision)
            .and_then(|page| page.result.as_ref().ok())
            .map(Vec::as_slice)
    }
    pub fn selected_node(&self, output: &ParseOutput) -> Option<Node> {
        if self.stale(output) {
            return None;
        }
        let path = self.selected.as_ref()?;
        find_node(&output.nodes, path)
            .or_else(|| {
                self.page
                    .as_ref()
                    .filter(|page| page.key.revision == output.revision)
                    .and_then(|page| page.result.as_ref().ok())
                    .and_then(|nodes| find_node(nodes, path))
            })
            .cloned()
    }

    /// Byte selection resolves its containing sample without depending on any visible pane.
    pub fn select_bytes(&mut self, output: &ParseOutput, selection: Option<Range<usize>>) {
        if self.stale(output) || selection == self.last_hex_selection {
            return;
        }
        self.table_anchor = None;
        self.last_hex_selection = selection.clone();
        self.plot_follow_selection = true;
        self.selected = None;
        self.pending_hex_selection = false;
        self.reveal_selected_row = true;
        let Some(range) = selection.filter(|range| !range.is_empty()) else {
            return;
        };
        let mut best = covering(&output.nodes, &range);
        if let Some(node) = best.filter(|node| node.lazy.is_some()) {
            let index = sample_indices_for_bytes(node, &range).map_or(0, |indices| indices.start);
            self.table_target = node.path.clone();
            self.table_jump = index;
            self.target_initialized = true;
            self.queue_page(node, index, output.revision);
            self.pending_hex_selection = true;
        } else if let Some(collection) = containing_collection(&output.nodes, &range) {
            self.table_target = collection.path.clone();
            self.target_initialized = true;
        }
        if let Some(nodes) = self
            .page
            .as_ref()
            .filter(|page| page.key.revision == output.revision)
            .and_then(|page| page.result.as_ref().ok())
        {
            if let Some(candidate) = covering(nodes, &range) {
                if best.is_none_or(|node| candidate.range.len() <= node.range.len()) {
                    best = Some(candidate);
                    self.pending_hex_selection = false;
                }
            }
        }
        if let Some(node) = best {
            self.selected = Some(node.path.clone());
            expand_ancestors(&node.path, &mut self.expanded);
        }
    }
    fn resolve_pending_selection(&mut self) {
        if !self.pending_hex_selection {
            return;
        }
        let Some(range) = self.last_hex_selection.as_ref() else {
            return;
        };
        let selected = self
            .page
            .as_ref()
            .and_then(|page| page.result.as_ref().ok())
            .and_then(|nodes| covering(nodes, range));
        if let Some(node) = selected {
            self.selected = Some(node.path.clone());
            expand_ancestors(&node.path, &mut self.expanded);
            self.reveal_selected_row = true;
        }
        self.pending_hex_selection = false;
    }

    pub fn expand(&mut self, path: &str) { self.expanded.insert(path.into()); self.reveal_tree = Some(path.into()); }
    pub fn expand_first_page(&mut self, node: &Node, revision: u64) {
        self.expand(&node.path);
        if node.children.is_empty() {
            self.queue_page(node, 0, revision);
        }
    }
    pub fn tree(
        &mut self,
        ui: &mut Ui,
        output: &ParseOutput,
        selection: Option<Range<usize>>,
        names: &mut dyn FnMut(&mut Ui, &Node) -> egui::Response,
    ) -> Option<Range<usize>> {
        if self.stale(output) {
            ui.colored_label(
                Color32::YELLOW,
                "Bytes changed. Press ▶ Run (F5) to refresh these elements.",
            );
            return None;
        }
        self.select_bytes(output, selection);
        ui.weak("Expand an item to inspect its fields. Click a row to highlight its bytes in Hex; hover a number for decimal, hex, and binary.");
        let mut clicked = None;
        self.definition_details(ui, output, &mut clicked);
        if let Some(request) = self.desired.clone().filter(|request| {
            self.expanded.contains(&request.key.path)
                && self
                    .selected
                    .as_deref()
                    .is_some_and(|path| contains_path(&request.key.path, path))
        }) {
            if let Some(node) = find_node(&output.nodes, &request.key.path) {
                self.page_controls(ui, node, output.revision);
            }
        }
        let mut rows = Vec::new();
        flatten_visible(
            &output.nodes,
            0,
            &self.expanded,
            self.page.as_ref(),
            output.revision,
            &mut rows,
        );
        let row_height = ui.text_style_height(&egui::TextStyle::Body).max(18.) + 4.;
        let mut toggle = None;
        let mut node_click = None;
        let mut scroll = egui::ScrollArea::both();
        if let Some(path) = self.reveal_tree.take() {
            if let Some(index) = rows.iter().position(|(node, _)| node.path == path) { scroll = scroll.vertical_scroll_offset(index as f32 * row_height); }
        }
        scroll
            .id_salt("decoded_structure_tree")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                for index in range {
                    let (node, depth) = rows[index];
                    ui.push_id(&node.path, |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space((depth.min(32) * 14) as f32);
                            if node.lazy.is_some() || !node.children.is_empty() {
                                let open = self.expanded.contains(&node.path);
                                let (_, response) = ui.allocate_exact_size(
                                    egui::vec2(18., 18.),
                                    egui::Sense::click(),
                                );
                                egui::collapsing_header::paint_default_icon(
                                    ui,
                                    if open { 1_f32 } else { 0_f32 },
                                    &response,
                                );
                                if response
                                    .on_hover_text(if open { "Collapse" } else { "Expand" })
                                    .clicked()
                                {
                                    toggle = Some(node.clone());
                                }
                            } else {
                                ui.add_space(18.);
                            }
                            let response = names(ui, node);
                            let tooltip = format!(
                                "{}\n{} · 0x{:X}..0x{:X}\n{}",
                                node.path,
                                node.kind,
                                node.range.start,
                                node.range.end,
                                node.references.join("\n")
                            );
                            let tooltip = node.numeric_tooltip().map_or(tooltip.clone(), |number| format!("{number}\n\n{tooltip}"));
                            let value_response = ui.selectable_label(self.selected.as_deref() == Some(&node.path), format!("· {}", tree_display(node, 90)))
                                .on_hover_text(&tooltip);
                            response.clone().on_hover_text(&tooltip);
                            if response.clicked() || value_response.clicked() {
                                node_click = Some(node.clone());
                            }
                        });
                    });
                }
            });
        if let Some(node) = toggle {
            if is_collection(&node) && find_node(&output.nodes, &node.path).is_some() {
                self.table_target = node.path.clone();
                self.target_initialized = true;
            }
            if !self.expanded.remove(&node.path) {
                self.expanded.insert(node.path.clone());
                self.request(&node, 0, output.revision, ui.ctx());
            }
            clicked = Some(self.select(&node));
        }
        if let Some(node) = node_click {
            if is_collection(&node) && find_node(&output.nodes, &node.path).is_some() {
                self.table_target = node.path.clone();
                self.target_initialized = true;
            }
            self.request(&node, 0, output.revision, ui.ctx());
            clicked = Some(self.select(&node));
        }
        clicked.or_else(|| self.navigation_selection.take())
    }

    fn definition_details(
        &mut self,
        ui: &mut Ui,
        output: &ParseOutput,
        clicked: &mut Option<Range<usize>>,
    ) {
        ui.collapsing(
            format!("Decoder type definitions ({})", output.definitions.len()),
            |ui| {
                if output.definitions.is_empty() {
                    ui.weak("No resolved types.");
                    return;
                }
                self.definition_index = self.definition_index.min(output.definitions.len() - 1);
                egui::ComboBox::from_id_salt("decoded_definition")
                    .selected_text(&output.definitions[self.definition_index].name)
                    .show_ui(ui, |ui| {
                        for (index, definition) in output.definitions.iter().enumerate() {
                            let name = format!(
                                "{}{}",
                                definition.name,
                                if definition.origin.is_some() {
                                    " · from bytes"
                                } else {
                                    ""
                                }
                            );
                            ui.selectable_value(&mut self.definition_index, index, name);
                        }
                    });
                let definition = &output.definitions[self.definition_index];
                if let Some(origin) = &definition.origin {
                    if ui
                        .link(format!(
                            "Defined by 0x{:X}..0x{:X}",
                            origin.start, origin.end
                        ))
                        .clicked()
                    {
                        *clicked = Some(origin.clone());
                        self.select_bytes(output, Some(origin.clone()));
                    }
                }
                egui::ScrollArea::vertical()
                    .id_salt("resolved_definition_fields")
                    .max_height(160.)
                    .show_rows(ui, 20., definition.fields.len(), |ui, range| {
                        for index in range {
                            let field = &definition.fields[index];
                            let details = [
                                field.repeat.as_ref().map(|v| format!("repeat {v}")),
                                field.length.as_ref().map(|v| format!("length {v}")),
                            ]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · ");
                            ui.label(format!("{}: {} {}", field.name, field.kind, details));
                        }
                    });
            },
        );
        ui.separator();
    }

    fn initialize_target(&mut self, output: &ParseOutput) {
        if !self.table_target.is_empty() && find_node(&output.nodes, &self.table_target).is_none() {
            self.target_initialized = false;
        }
        if self.target_initialized {
            return;
        }
        self.target_initialized = true;
        let mut collections = Vec::new();
        collect_collections(&output.nodes, &mut collections);
        self.table_target = collections
            .into_iter()
            .max_by_key(|node| collection_score(node, output))
            .map_or_else(String::new, |node| node.path.clone());
    }
    fn target_controls(&mut self, ui: &mut Ui, output: &ParseOutput) {
        self.initialize_target(output);
        let mut collections = Vec::new();
        collect_collections(&output.nodes, &mut collections);
        let previous = self.table_target.clone();
        ui.horizontal_wrapped(|ui| {
            ui.label("Data");
            egui::ComboBox::from_id_salt("structure_collection")
                .width(ui.available_width().min(420.).max(120.))
                .selected_text(if self.table_target.is_empty() {
                    "File elements".to_owned()
                } else if let Some(node) = find_node(&output.nodes, &self.table_target) {
                    collection_label(node)
                } else {
                    self.table_target.clone()
                })
                .show_ui(ui, |ui| {
                    for node in &collections {
                        ui.selectable_value(
                            &mut self.table_target,
                            node.path.clone(),
                            collection_label(node),
                        )
                        .on_hover_text(&node.path);
                    }
                    if !collections.is_empty() {
                        ui.separator();
                    }
                    ui.selectable_value(&mut self.table_target, String::new(), "File elements");
                });
        });
        if previous != self.table_target {
            self.table_anchor = None;
            self.reveal_selected_row = false;
        }
        if let Some(node) = find_node(&output.nodes, &self.table_target) {
            if node.lazy.is_some() {
                self.page_controls(ui, node, output.revision);
            } else {
                ui.weak(format!(
                    "{} samples · 0x{:X}–0x{:X}",
                    collection_count(node),
                    node.range.start,
                    node.range.end
                ));
            }
        } else if collections.is_empty() {
            ui.weak("No repeated samples are defined. In Decoder, repeat a structure by count or until end to create a sample collection.");
        }
    }
    fn page_controls(&mut self, ui: &mut Ui, node: &Node, revision: u64) {
        let Some(array) = &node.lazy else {
            return;
        };
        let mut start = self
            .desired
            .as_ref()
            .filter(|request| request.key.path == node.path && request.key.revision == revision)
            .map_or(0, |request| request.key.start);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(start > 0, egui::Button::new("Previous"))
                .clicked()
            {
                start = start.saturating_sub(PAGE_SIZE);
                self.table_jump = start;
            }
            ui.label(format!(
                "Samples {}–{} of {}",
                start,
                start
                    .saturating_add(PAGE_SIZE)
                    .min(array.count)
                    .saturating_sub(1),
                array.count
            ));
            if ui
                .add_enabled(
                    start.saturating_add(PAGE_SIZE) < array.count,
                    egui::Button::new("Next"),
                )
                .clicked()
            {
                start = start.saturating_add(PAGE_SIZE);
                self.table_jump = start;
            }
            ui.label("Go to sample");
            let entry = ui.add(
                egui::DragValue::new(&mut self.table_jump)
                    .range(0..=array.count.saturating_sub(1)),
            );
            if ui.button("Go").clicked()
                || entry.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
            {
                start = self.table_jump.min(array.count.saturating_sub(1));
                if let Some(bytes) = sample_bytes(node, start..start.saturating_add(1)) {
                    self.table_anchor = None;
                    self.navigation_selection = Some(bytes.clone());
                    self.last_hex_selection = Some(bytes);
                    self.selected = Some(format!("{}[{start}]", node.path));
                    self.pending_hex_selection = true;
                    self.plot_follow_selection = true;
                    self.reveal_selected_row = true;
                }
            }
        });
        self.request(node, start, revision, ui.ctx());
        if let Some(page) = &self.page {
            if page.key.path == node.path {
                if let Err(error) = &page.result {
                    ui.colored_label(Color32::LIGHT_RED, error);
                }
            }
        } else {
            ui.weak("Reading this page…");
        }
    }
    fn target_rows<'a>(&'a self, output: &'a ParseOutput) -> &'a [Node] {
        match find_node(&output.nodes, &self.table_target) {
            Some(node) if node.lazy.is_some() => {
                self.page_nodes(&node.path, output.revision).unwrap_or(&[])
            }
            Some(node) => &node.children,
            None => &output.nodes,
        }
    }

    pub fn table(
        &mut self,
        ui: &mut Ui,
        output: &ParseOutput,
        source: ByteView,
    ) -> Option<Range<usize>> {
        self.tick(ui.ctx(), source);
        if self.stale(output) {
            ui.label("Bytes changed. Press ▶ Run (F5) to refresh the table.");
            return None;
        }
        self.target_controls(ui, output);
        ui.weak("Click a sample or value · Shift-click to select a range of whole samples.");
        let rows = self.target_rows(output);
        if rows.is_empty() {
            if self.table_target.is_empty() {
                ui.label("No decoded elements.");
            }
            return self.navigation_selection.take();
        }
        let columns = table_columns(rows);
        let selected = self.selected.as_deref();
        let selected_bytes = self.last_hex_selection.as_ref();
        let selected_row = rows.iter().position(|node| {
            selected_bytes.is_some_and(|range| ranges_overlap(range, &node.range))
                || selected.is_some_and(|path| contains_path(&node.path, path))
        });
        let reveal = self.reveal_selected_row && selected_row.is_some();
        let first = self
            .page
            .as_ref()
            .filter(|page| {
                page.key.path == self.table_target && page.key.revision == output.revision
            })
            .map_or(0, |page| page.key.start);
        let sample_collection = !self.table_target.is_empty();
        let mut chosen = None;
        let row_height = 26.;
        let metadata = [
            if sample_collection {
                "Sample"
            } else {
                "Element"
            },
            "Byte offset",
            "Bytes",
        ];
        let widths = [96., 120., 72.];
        let available = ui.available_width();
        let value_width =
            ((available - widths.iter().sum::<f32>()) / columns.len().max(1) as f32).max(116.);
        let table_width =
            (widths.iter().sum::<f32>() + value_width * columns.len() as f32).max(available);
        egui::ScrollArea::horizontal()
            .id_salt("decoded_table_horizontal")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(table_width);
                ui.spacing_mut().item_spacing = egui::vec2(0., 0.);
                ui.horizontal(|ui| {
                    for (label, width) in metadata.into_iter().zip(widths) {
                        table_cell(ui, label, width, row_height, false, false, true);
                    }
                    for name in &columns {
                        table_cell(
                            ui,
                            if name.is_empty() { "Value" } else { name },
                            value_width,
                            row_height,
                            false,
                            false,
                            true,
                        )
                        .on_hover_text(name);
                    }
                });
                ui.separator();
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt(("decoded_table_vertical", &self.table_target))
                    .auto_shrink([false, false]);
                if reveal {
                    scroll =
                        scroll.vertical_scroll_offset(selected_row.unwrap() as f32 * row_height);
                }
                scroll.show_rows(ui, row_height, rows.len(), |ui, range| {
                    for index in range {
                        let node = &rows[index];
                        ui.push_id(&node.path, |ui| {
                            let row_selected = selected_bytes
                                .is_some_and(|range| ranges_overlap(range, &node.range));
                            let row_rect = egui::Rect::from_min_size(
                                ui.cursor().min,
                                egui::vec2(table_width, row_height),
                            );
                            if row_selected {
                                ui.painter().rect_filled(
                                    row_rect,
                                    0.,
                                    ui.visuals().selection.bg_fill.gamma_multiply(0.35),
                                );
                            } else if index % 2 == 1 {
                                ui.painter()
                                    .rect_filled(row_rect, 0., ui.visuals().faint_bg_color);
                            }
                            ui.horizontal(|ui| {
                                let label = if sample_collection {
                                    (first + index).to_string()
                                } else {
                                    node.name.clone()
                                };
                                if table_cell(
                                    ui,
                                    &label,
                                    widths[0],
                                    row_height,
                                    selected == Some(&node.path),
                                    true,
                                    false,
                                )
                                .on_hover_text(format!(
                                    "Select whole sample\n{}\n0x{:X}..0x{:X}",
                                    node.path, node.range.start, node.range.end
                                ))
                                .clicked()
                                {
                                    chosen = Some((node.clone(), node.clone()));
                                }
                                if table_cell(
                                    ui,
                                    &format!("0x{:X}", node.range.start),
                                    widths[1],
                                    row_height,
                                    false,
                                    true,
                                    false,
                                )
                                .clicked()
                                {
                                    chosen = Some((node.clone(), node.clone()));
                                }
                                if table_cell(
                                    ui,
                                    &node.range.len().to_string(),
                                    widths[2],
                                    row_height,
                                    false,
                                    true,
                                    false,
                                )
                                .clicked()
                                {
                                    chosen = Some((node.clone(), node.clone()));
                                }
                                for name in &columns {
                                    if let Some(field) = field_at(node, name) {
                                        if table_cell(
                                            ui,
                                            &display(field, 180),
                                            value_width,
                                            row_height,
                                            selected == Some(&field.path),
                                            true,
                                            false,
                                        )
                                        .on_hover_text(format!(
                                            "{}\n{} · {} bytes · 0x{:X}..0x{:X}\n{}",
                                            field.path,
                                            field.kind,
                                            field.range.len(),
                                            field.range.start,
                                            field.range.end,
                                            display(field, 600)
                                        ))
                                        .clicked()
                                        {
                                            chosen = Some((node.clone(), field.clone()));
                                        }
                                    } else {
                                        table_cell(
                                            ui,
                                            "—",
                                            value_width,
                                            row_height,
                                            false,
                                            false,
                                            false,
                                        );
                                    }
                                }
                            });
                        });
                    }
                });
            });
        if reveal {
            self.reveal_selected_row = false;
        }
        chosen
            .map(|(row, field)| {
                self.select_table_node(output, &row, &field, ui.input(|i| i.modifiers.shift))
            })
            .or_else(|| self.navigation_selection.take())
    }

    fn initialize_plot(&mut self, output: &ParseOutput) {
        if self.plot_target.is_empty() || find_node(&output.nodes, &self.plot_target).is_none() {
            self.plot_view = None;
            self.plot_force_view = None;
            self.initialize_target(output);
            self.plot_target = self.table_target.clone();
            if self.plot_target.is_empty() {
                let mut collections = Vec::new();
                collect_collections(&output.nodes, &mut collections);
                self.plot_target = collections
                    .into_iter()
                    .max_by_key(|node| collection_score(node, output))
                    .map_or_else(String::new, |node| node.path.clone());
            }
        }
        if self.plot_follow_selection {
            self.plot_follow_selection = false;
            if let Some(range) = &self.last_hex_selection {
                if let Some(collection) = containing_collection(&output.nodes, range) {
                    if !self.plot_sources.contains(&collection.path) && self.plot_sources_initialized {
                        return;
                    }
                    if let Some(samples) = sample_indices_for_bytes(collection, range) {
                        self.plot_jump = samples.start;
                        self.plot_range_first = samples.start;
                        self.plot_range_last = samples.end.saturating_sub(1);
                        if self.plot_view.is_none() {
                            self.plot_fit_y = true;
                        }
                        let view = selection_view(
                            self.plot_view.as_ref(),
                            &samples,
                            collection_count(collection),
                        );
                        if self.plot_view.as_ref() != Some(&view) {
                            self.plot_force_view = Some(view.clone());
                            self.plot_view = Some(view);
                            self.plot_refine = false;
                        }
                    }
                }
            }
        }
        if let Some(node) = find_node(&output.nodes, &self.plot_target) {
            if let Some(view) = &self.plot_view {
                let count = collection_count(node);
                if view.end > count || view.is_empty() {
                    let width = view.len().max(1).min(count);
                    let start = view.start.min(count.saturating_sub(width));
                    let clamped = start..start + width;
                    self.plot_view = Some(clamped.clone());
                    self.plot_force_view = Some(clamped);
                }
            }
        }
        if self.plot_view.is_none() {
            if let Some(node) = find_node(&output.nodes, &self.plot_target) {
                let view = 0..collection_count(node).min(INITIAL_PLOT_SAMPLES);
                self.plot_view = Some(view.clone());
                self.plot_force_view = Some(view);
                self.plot_fit_y = true;
            }
        }
    }

    fn choose_plot_bytes(
        &mut self,
        output: &ParseOutput,
        bytes: Range<usize>,
        path: Option<String>,
    ) {
        // Resolve the table page through the same route as a Hex selection, then preserve
        // an exact plotted field path while that page is being read.
        self.select_bytes(output, Some(bytes));
        if let Some(path) = path {
            self.selected = Some(path);
        }
        self.plot_follow_selection = false;
        self.reveal_selected_row = true;
    }

    pub fn plot(
        &mut self,
        ui: &mut Ui,
        output: &ParseOutput,
        source: ByteView,
    ) -> Option<Range<usize>> {
        self.tick(ui.ctx(), source.clone());
        if self.stale(output) {
            ui.label("Bytes changed. Press ▶ Run (F5) to refresh the plot.");
            return None;
        }
        self.initialize_plot(output);
        let mut collections = Vec::new();
        collect_collections(&output.nodes, &mut collections);
        self.plot_sources.retain(|path| collections.iter().any(|node| &node.path == path));
        if !self.plot_sources_initialized {
            self.plot_sources_initialized = true;
            self.plot_sources.extend(collections.iter().filter(|n| collection_score(n, output).0).map(|n| n.path.clone()));
        }
        let mut changed = false;
        if self.plot_sources.is_empty() {
            if collections.is_empty() {
                crate::decoder_ui::empty_state(
                    ui,
                    "No sample series yet",
                    "Define a repeated structure or numeric field, then press ▶ Run (F5).",
                );
                return None;
            }
            self.extra_plot_readers.clear();
            ui.menu_button("Series", |ui| {plot_collection_choices(ui,&collections,&mut self.plot_sources);});
            ui.label("Choose a collection in Series. Toggle individual fields in the legend.");
            return None;
        }
        let primary = collections.iter().filter(|node| self.plot_sources.contains(&node.path))
            .max_by_key(|node| collection_count(node)).unwrap().path.clone();
        if primary != self.plot_target {
            self.plot_target = primary;
            self.plot_reader.invalidate();
            changed = true;
        }
        if changed { self.plot_fit_y = true; }
        self.extra_plot_readers.retain(|path, _| self.plot_sources.contains(path) && path != &self.plot_target);
        let Some(node) = find_node(&output.nodes, &self.plot_target) else {
            ui.label("No sample collection. Repeat a numeric field or structure in the definition to plot it.");
            return None;
        };
        let count = collection_count(node);
        if count == 0 {
            ui.label("This collection contains no samples.");
            return None;
        }
        let selected_samples = self
            .last_hex_selection
            .as_ref()
            .and_then(|bytes| collections.iter().filter(|n| self.plot_sources.contains(&n.path)).find_map(|n| sample_indices_for_bytes(n, bytes)));
        let selected_bytes = self
            .last_hex_selection
            .clone()
            .filter(|_| selected_samples.is_some());
        let mut picked = None;
        let mut force_view = self.plot_force_view.take();
        let zoom_y = self.plot_zoom_y.take();
        let view = self
            .plot_view
            .clone()
            .unwrap_or(0..count.min(INITIAL_PLOT_SAMPLES));
        let ranges: Vec<_> = std::iter::once((&self.plot_target, &self.plot_reader)).chain(self.extra_plot_readers.iter())
            .flat_map(|(path,reader)| reader.data().into_iter().flat_map(move |d| d.series.iter().filter_map(move |(name,points)| {
                numeric_bounds(points.iter().map(|p|p.value)).map(|(min,max)| (format!("{path} / {name}"),min,max))
            }))).collect();
        let visible_series = ranges
            .iter()
            .filter(|(name, _, _)| !self.plot_hidden.contains(name))
            .count();
        let series_label = if ranges.is_empty() {
            "Series".to_owned()
        } else {
            format!("Series {visible_series}/{}", ranges.len())
        };
        ui.horizontal_wrapped(|ui| {
            ui.menu_button(series_label, |ui| {
                ui.strong("Collections");
                changed |= plot_collection_choices(ui,&collections,&mut self.plot_sources);
                if !ranges.is_empty() {
                    ui.separator();
                    ui.strong("Fields shown");
                    egui::ScrollArea::vertical().max_height(220.).show(ui, |ui| {
                        for (name, _, _) in &ranges {
                            let mut shown = !self.plot_hidden.contains(name);
                            if ui.checkbox(&mut shown, name).changed() {
                                if shown {
                                    self.plot_hidden.remove(name);
                                } else {
                                    self.plot_hidden.insert(name.clone());
                                }
                                self.plot_fit_y = true;
                            }
                        }
                    });
                }
                if self.presentation.menu(ui,&ranges) { self.plot_fit_y=true; }
            });
            if ui.button("Fit").on_hover_text("Show the full sample range and fit the visible fields").clicked() { force_view=Some(0..count); self.plot_fit_y=true; }
            ui.menu_button("Go to sample…", |ui| {
                ui.label("Sample index (starts at 0)");
                self.plot_jump=self.plot_jump.min(count-1);
                ui.add(egui::DragValue::new(&mut self.plot_jump).range(0..=count-1));
                if ui.button("Go").clicked() {
                    force_view=Some(centered_view(self.plot_jump,view.len().min(INITIAL_PLOT_SAMPLES).max(64),count));
                    if let Some(bytes)=sample_bytes(node,self.plot_jump..self.plot_jump+1) {picked=Some((bytes,None));}
                    ui.close();
                }
            });
            ui.menu_button("Plot options", |ui| {
            let mut lock_y=!self.plot_free_y;
            if ui.checkbox(&mut lock_y,"Lock vertical zoom").changed() {self.plot_free_y=!lock_y;}
            if ui.button("Fit Y").clicked() {self.plot_fit_y=true;ui.close();}
            if ui.add_enabled(selected_samples.is_some(),egui::Button::new("Fit selection")).clicked() {
                if let Some(samples)=&selected_samples {force_view=Some(padded_selection(samples,count));self.plot_fit_y=true;}
                ui.close();
            }
            ui.weak("Drag a trace vertically to position it. Drag empty space to pan.");
            ui.weak("Wheel/pinch zooms. Right-drag selects a zoom range.");
            ui.separator();
            let mut first = view.start;
            let mut shown = view.len().max(1);
            ui.label("View from");
            let first_changed = ui.add(egui::DragValue::new(&mut first).range(0..=count - 1)).changed();
            ui.label("Count");
            let count_changed = ui.add(egui::DragValue::new(&mut shown).range(1..=count)).changed();
            if first_changed || count_changed {
                force_view = Some(first..first.saturating_add(shown).min(count));
            }
            ui.separator();
            ui.menu_button("Select sample range…", |ui| {
                ui.label("Whole samples, including every field");
                ui.horizontal(|ui| {
                    ui.label("First");
                    ui.add(egui::DragValue::new(&mut self.plot_range_first).range(0..=count - 1));
                });
                ui.horizontal(|ui| {
                    ui.label("Last (inclusive)");
                    ui.add(egui::DragValue::new(&mut self.plot_range_last).range(0..=count - 1));
                });
                if ui.button("Select bytes").clicked() {
                    let first = self.plot_range_first.min(self.plot_range_last).min(count - 1);
                    let last = self.plot_range_first.max(self.plot_range_last).min(count - 1);
                    if let Some(bytes) = sample_bytes(node, first..last + 1) {
                        self.plot_anchor = Some(first);
                        picked = Some((bytes, None));
                    }
                    ui.close();
                }
            });
            if ui.add_enabled(selected_bytes.is_some(), egui::Button::new("Open in Inspector")).clicked() {
                self.plot_action = selected_bytes.clone().map(PlotAction::InspectBytes);
            }
            if ui.add_enabled(selected_bytes.is_some(), egui::Button::new("Export selected bytes…")).clicked() {
                self.plot_action = selected_bytes.clone().map(PlotAction::ExportBytes);
            }
            if let (Some(bytes), Some(samples)) = (&selected_bytes, &selected_samples) {
                let label = if samples.len() == 1 {
                    format!("Sample {}", samples.start)
                } else {
                    format!("Samples {}–{}", samples.start, samples.end.saturating_sub(1))
                };
                let unit = if bytes.len() == 1 { "byte" } else { "bytes" };
                ui.weak(format!("{label} · {} {unit} selected", bytes.len()))
                    .on_hover_text(format!("0x{:X}..0x{:X} (end exclusive). Whole-sample export includes all interleaved fields.", bytes.start, bytes.end));
            }
        });
        });
        if ui.available_height() > 260. {
            ui.weak("Drag to pan · wheel or pinch to zoom · click a point to link Hex and Inspector");
        }
        if changed { self.plot_fit_y=true; }
        if let Some(forced) = &force_view {
            if self.plot_view.as_ref() != Some(forced) {
                self.plot_refine = false;
            }
            self.plot_view = Some(forced.clone());
        }
        let requested = self.plot_view.clone().unwrap_or(view);
        // Keep the last measured axis gutter, so resizing uses the new canvas
        // width immediately. Reduction follows physical pixels, including DPI.
        let canvas_width = (ui.available_width() - self.plot_axis_width).max(1.);
        let pixel_width = (canvas_width * ui.ctx().pixels_per_point()).ceil() as usize;
        self.plot_reader.request(
            ui.ctx(),
            source.clone(),
            node.clone(),
            requested.clone(),
            pixel_width,
            self.plot_refine,
        );
        self.plot_reader.tick(ui.ctx());
        for path in &self.plot_sources {
            if path == &self.plot_target { continue; }
            if let Some(extra) = find_node(&output.nodes, path) {
                let count = collection_count(extra);
                let reader = self.extra_plot_readers.entry(path.clone()).or_default();
                reader.request(ui.ctx(), source.clone(), extra.clone(),
                    requested.start.min(count)..requested.end.min(count), pixel_width, self.plot_refine);
                reader.tick(ui.ctx());
                if let Some(error) = reader.error() { ui.colored_label(Color32::LIGHT_RED, format!("{path}: {error}")); }
                if reader.is_loading() { ui.spinner(); }
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "Samples {}–{} of {}",
                requested.start,
                requested.end.saturating_sub(1),
                count
            ));
            if let Some(data) = self.plot_reader.data() {
                ui.weak(data.quality.label()).on_hover_text(format!(
                    "Data covers sample indices {}–{}. Dense cycles are shown as amplitude bands; zoom in to see individual samples.",
                    data.range.start,
                    data.range.end.saturating_sub(1)
                ));
                if matches!(data.quality, PlotQuality::SampledPreview) {
                    ui.weak("Bands show observed ranges from sampled windows.");
                    if ui.button("Scan for all peaks").clicked() {
                        self.plot_refine = true;
                    }
                }
                if !data.complete && data.total > 0 {
                    ui.add(
                        egui::ProgressBar::new(data.scanned as f32 / data.total as f32)
                            .desired_width(120.)
                            .text(format!(
                                "{:.1}% scanned",
                                data.scanned as f64 * 100. / data.total as f64
                            )),
                    );
                }
            }
            if self.plot_reader.is_loading() {
                ui.spinner();
                ui.weak("Reading visible range…");
                if self.plot_refine && ui.button("Stop scan").clicked() {
                    self.plot_refine = false;
                }
            }
        });
        if let Some(error) = self.plot_reader.error() {
            ui.colored_label(Color32::LIGHT_RED, error);
        }
        let mut combined = self.plot_reader.data().cloned();
        {
            if let Some(data) = &mut combined {
                data.series = std::mem::take(&mut data.series).into_iter()
                    .map(|(name, points)| (format!("{} / {name}", self.plot_target), points)).collect();
                data.envelopes = std::mem::take(&mut data.envelopes).into_iter()
                    .map(|(name, bands)| (format!("{} / {name}", self.plot_target), bands)).collect();
                for (path, reader) in &self.extra_plot_readers {
                    if let Some(extra) = reader.data() {
                        for (name, points) in &extra.series { data.series.insert(format!("{path} / {name}"), points.clone()); }
                        for (name, bands) in &extra.envelopes { data.envelopes.insert(format!("{path} / {name}"), bands.clone()); }
                        if extra.quality == PlotQuality::SampledPreview { data.quality = PlotQuality::SampledPreview; }
                        data.complete &= extra.complete;
                    }
                }
            }
        }
        if let Some(data)=&mut combined {
            for (name,points) in &mut data.series { let p=self.presentation.placement(name); for point in points {point.value=p.display(point.value);} }
            for (name,bands) in &mut data.envelopes {let p=self.presentation.placement(name);for band in bands {band.min.value=p.display(band.min.value);band.max.value=p.display(band.max.value);}}
        }
        let placements=self.presentation.series.clone();
        let data = combined.as_ref();
        let plot_id = egui::Id::new(("decoded_structure_plot", "overlay", output.revision));
        let hidden = self.plot_hidden.clone();
        let y_fit = if self.plot_fit_y && self.plot_range_zoom.is_none() {
            data.and_then(|data| {
                numeric_bounds(
                    data.series
                        .iter()

                        .filter(|(name, _)| !hidden.contains(*name))
                        .flat_map(|(_, values)| values)
                        .map(|point| point.value),
                )
            })
        } else {
            None
        };
        if y_fit.is_some() && data.is_some_and(|data| data.complete) {
            self.plot_fit_y = false;
        }
        let mut wheel_zoomed = false;
        let plot_ctx = ui.ctx().clone();
        let plot_outer = ui.available_rect_before_wrap();
        let plot_clip = ui.clip_rect();
        let plot_layer = ui.layer_id();
        let mut axis_hover = None;
        let response = Plot::new(plot_id)
            .id(plot_id)
            .auto_bounds(egui::Vec2b::new(false, false))
            .allow_double_click_reset(false)
            .allow_boxed_zoom(false)
            .allow_drag(egui::Vec2b::new(!self.presentation.is_dragging(), self.plot_free_y && !self.presentation.is_dragging()))
            .allow_zoom(egui::Vec2b::new(true, self.plot_free_y))
            .allow_scroll(false)
            .label_formatter(move |position| {
                if let egui_plot::HoverPosition::NearDataPoint { plot_name, position, .. } = position {
                    let p = placements.get(*plot_name).copied().unwrap_or_default();
                    Some(format!("{plot_name}\nSample: {:.0}\nValue: {}", position.x, p.original(position.y)))
                } else {
                    None
                }
            })
            .y_axis_label(if self.presentation.series.is_empty() {"Value"} else {"Display position"})
            .x_axis_label("Sample index")
            .show(ui, |plot_ui| {
                if force_view.is_some() || y_fit.is_some() {
                    let old = plot_ui.plot_bounds();
                    let (x0, x1) = force_view
                        .as_ref()
                        .map_or((old.min()[0], old.max()[0]), |range| {
                            (range.start as f64 - 0.5, range.end as f64 - 0.5)
                        });
                    let (y0, y1) = zoom_y.or(y_fit).unwrap_or((old.min()[1], old.max()[1]));
                    plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                        [x0, y0],
                        [x1.max(x0 + 1.), y1.max(y0 + f64::EPSILON)],
                    ));
                }
                if let Some(data) = data {
                    for (name, values) in data.series.iter() {
                        if hidden.contains(name) {
                            continue;
                        }
                        let points: Vec<[f64; 2]> = values
                            .iter()
                            .map(|point| [point.index as f64, point.value])
                            .collect();
                        let color = field_color(name);
                        let bands = (data.quality == PlotQuality::SampledPreview).then(|| {
                            data.envelopes.get(name).into_iter().flatten().map(|band| [
                                band.range.start as f64 - 0.5, band.range.end as f64 - 0.5,
                                band.min.value, band.max.value,
                            ]).collect::<Vec<_>>()
                        });
                        decoded_channel(plot_ui, name, color, points.clone(), bands.as_deref());
                        if data.quality == PlotQuality::Exact && values.len() <= canvas_width as usize / 5 {
                            plot_ui.points(Points::new(name, points).color(color).radius(2.5_f32));
                        }
                    }
                }
                let inner = plot_ui.response().rect;
                let axis_rects = [
                    egui::Rect::from_min_max(
                        egui::pos2(inner.left(), inner.bottom()),
                        egui::pos2(inner.right(), plot_outer.bottom()),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(plot_outer.left(), inner.top()),
                        egui::pos2(inner.left(), inner.bottom()),
                    ),
                ];
                axis_hover = axis_rects
                    .iter()
                    .enumerate()
                    .find(|(_, rect)| {
                        plot_ctx.rect_contains_pointer(plot_layer, rect.intersect(plot_clip))
                    })
                    .map(|(axis, rect)| (axis, *rect));
                if force_view.is_none()
                    && self.plot_range_zoom.is_none()
                    && (axis_hover.is_some() || plot_ui.response().hovered())
                {
                    let (scroll, shift, pinch, pointer) = plot_ctx.input(|input| {
                        let scroll = input.raw.events.iter().filter_map(|event| match event {
                            egui::Event::MouseWheel { delta, .. } => Some(*delta),
                            _ => None,
                        }).fold(egui::Vec2::ZERO, |sum, delta| sum + delta);
                        let shift = input.modifiers.shift;
                        (
                            scroll,
                            shift,
                            if shift && scroll != egui::Vec2::ZERO {
                                egui::Vec2::splat(1.)
                            } else {
                                input.zoom_delta_2d()
                            },
                            input.pointer.hover_pos(),
                        )
                    });
                    if let Some(pointer) = pointer {
                        let old = plot_ui.plot_bounds();
                        let transform = PlotTransform::new(inner, old, false);
                        let center = transform.value_from_position(pointer);
                        if let Some((axis, _)) =
                            axis_hover.filter(|_| pinch != egui::Vec2::splat(1.))
                        {
                            // Built-in pinch/Ctrl-wheel zoom only sees the canvas.
                            let factor = if axis == 0 { pinch.x } else { pinch.y };
                            plot_ui.zoom_bounds(
                                if axis == 0 {
                                    egui::vec2(factor, 1.)
                                } else {
                                    egui::vec2(1., factor)
                                },
                                center,
                            );
                            wheel_zoomed = factor != 1.;
                        } else if pinch == egui::Vec2::splat(1.) {
                            // egui-winit remaps Shift+wheel into horizontal scroll.
                            let delta = if shift {
                                scroll.x + scroll.y
                            } else if scroll.y != 0. {
                                scroll.y
                            } else {
                                scroll.x
                            };
                            if delta != 0. {
                                if shift {
                                    let axis = axis_hover.map_or(0, |(axis, _)| axis);
                                    let mut min = old.min();
                                    let mut max = old.max();
                                    let distance = if axis == 0 {
                                        -f64::from(delta) * old.width()
                                            / f64::from(inner.width().max(1.))
                                    } else {
                                        f64::from(delta) * old.height()
                                            / f64::from(inner.height().max(1.))
                                    };
                                    min[axis] += distance;
                                    max[axis] += distance;
                                    plot_ui.set_plot_bounds(PlotBounds::from_min_max(min, max));
                                } else {
                                    let factor = (delta * 0.005).clamp(-1., 1.).exp();
                                    let axes = axis_hover.map_or(
                                        egui::Vec2b::new(true, self.plot_free_y),
                                        |(axis, _)| egui::Vec2b::new(axis == 0, axis == 1),
                                    );
                                    plot_ui.zoom_bounds(
                                        egui::vec2(
                                            if axes.x { factor } else { 1. },
                                            if axes.y { factor } else { 1. },
                                        ),
                                        center,
                                    );
                                }
                                wheel_zoomed = true;
                            }
                        }
                    }
                }
            });
        self.plot_axis_width = (plot_outer.width() - response.transform.frame().width()).max(0.);
        if let Some((axis, rect)) = axis_hover {
            ui.interact(
                rect.intersect(plot_clip),
                plot_id.with(("axis_wheel", axis)),
                egui::Sense::hover(),
            )
            .on_hover_text(if axis == 0 {
                "X axis: wheel to zoom · Shift+wheel to pan"
            } else {
                "Y axis: wheel to zoom · Shift+wheel to pan"
            });
        }
        if response.response.dragged()
            || wheel_zoomed
            || response.response.hovered() && ui.input(|input| input.zoom_delta() != 1.0)
        {
            self.plot_fit_y = false;
        }
        let bounds = response.transform.bounds();
        let actual = viewport_samples(bounds.min()[0], bounds.max()[0], count);
        if bounds.min()[0] < -0.5 || bounds.max()[0] > count as f64 - 0.5 {
            let visible_width = (bounds.width().ceil().max(1.) as usize).min(count);
            let edge = if bounds.min()[0] < -0.5 {
                0..visible_width
            } else {
                count - visible_width..count
            };
            self.plot_force_view = Some(edge);
            ui.ctx().request_repaint();
        }
        if self.plot_view.as_ref() != Some(&actual) {
            self.plot_view = Some(actual.clone());
            self.plot_refine = false;
            ui.ctx().request_repaint();
        }
        let frame = *response.transform.frame();
        let painter = ui.painter().with_clip_rect(frame.intersect(ui.clip_rect()));
        if response
            .response
            .drag_started_by(egui::PointerButton::Secondary)
        {
            if let Some(origin) = ui
                .input(|input| input.pointer.press_origin())
                .filter(|pos| frame.contains(*pos))
            {
                self.plot_range_zoom = Some(RangeZoom {
                    origin,
                    transform: response.transform,
                });
                self.plot_fit_y = false;
            }
        }
        if let Some(zoom) = self.plot_range_zoom {
            let (position, released, down, cancelled) = ui.input(|input| {
                (
                    input.pointer.latest_pos(),
                    input
                        .pointer
                        .button_released(egui::PointerButton::Secondary),
                    input.pointer.button_down(egui::PointerButton::Secondary),
                    input.key_pressed(egui::Key::Escape) || !input.focused,
                )
            });
            if cancelled || (!down && !released) {
                self.plot_range_zoom = None;
            } else if let Some(position) = position {
                let right = position.x.clamp(
                    zoom.transform.frame().left(),
                    zoom.transform.frame().right(),
                );
                if released {
                    self.plot_range_zoom = None;
                    if (right - zoom.origin.x).abs() >= 4. {
                        let start = zoom.transform.value_from_position(zoom.origin).x;
                        let end = zoom
                            .transform
                            .value_from_position(egui::pos2(right, zoom.origin.y))
                            .x;
                        self.plot_force_view =
                            Some(viewport_samples(start.min(end), start.max(end), count));
                        self.plot_zoom_y = Some((
                            zoom.transform.bounds().min()[1],
                            zoom.transform.bounds().max()[1],
                        ));
                        self.plot_fit_y = false;
                        self.plot_refine = false;
                        ui.ctx().request_repaint();
                    }
                } else {
                    let band = egui::Rect::from_min_max(
                        egui::pos2(zoom.origin.x.min(right), frame.top()),
                        egui::pos2(zoom.origin.x.max(right), frame.bottom()),
                    );
                    painter.rect_filled(
                        band,
                        0.,
                        ui.visuals().selection.bg_fill.gamma_multiply(0.3),
                    );
                    painter.rect_stroke(
                        band,
                        0.,
                        egui::Stroke::new(1_f32, ui.visuals().selection.stroke.color),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }
        if let Some(samples) = &selected_samples {
            let left = response
                .transform
                .position_from_point(&PlotPoint::new(samples.start as f64 - 0.5, 0.))
                .x;
            let right = response
                .transform
                .position_from_point(&PlotPoint::new(samples.end as f64 - 0.5, 0.))
                .x;
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(left, frame.top()),
                    egui::pos2(right.max(left + 2.), frame.bottom()),
                ),
                0.,
                ui.visuals().selection.bg_fill.gamma_multiply(0.18),
            );
            let cursor = response
                .transform
                .position_from_point(&PlotPoint::new(samples.start as f64, 0.))
                .x;
            painter.line_segment(
                [
                    egui::pos2(cursor, frame.top()),
                    egui::pos2(cursor, frame.bottom()),
                ],
                egui::Stroke::new(1_f32, ui.visuals().selection.stroke.color),
            );
            if let (Some(data), Some(bytes)) = (data, &selected_bytes) {
                for (_, values) in data
                    .series
                    .iter()

                    .filter(|(name, _)| !hidden.contains(*name) && samples.len() == 1)
                {
                    for point in values
                        .iter()
                        .filter(|point| ranges_overlap(&point.bytes, bytes))
                    {
                        let pos = response
                            .transform
                            .position_from_point(&PlotPoint::new(point.index as f64, point.value));
                        painter.circle_stroke(pos, 5., egui::Stroke::new(1.5_f32, Color32::WHITE));
                    }
                }
            }
        }
        if data.is_none() {
            painter.text(
                frame.center(),
                egui::Align2::CENTER_CENTER,
                if self.plot_reader.is_loading() {
                    "Reading samples for this view…"
                } else {
                    "No numeric samples in this range"
                },
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().text_color(),
            );
        }
        self.presentation.drag(ui,&response.transform,data.into_iter().flat_map(|d|d.series.iter())
            .filter(|(name,_)| !hidden.contains(*name))
            .flat_map(|(name,points)|points.iter().map(move |p|(name.clone(),[p.index as f64,p.value]))));
        if response.response.clicked() {
            if let Some(pointer) = response
                .response
                .interact_pointer_pos()
                .filter(|pos| frame.contains(*pos))
            {
                let coord = response.transform.value_from_position(pointer);
                let index = (coord.x.round().max(0.) as usize).min(count - 1);
                if ui.input(|input| input.modifiers.shift) {
                    let anchor = self
                        .plot_anchor
                        .or_else(|| selected_samples.as_ref().map(|samples| samples.start))
                        .unwrap_or(index);
                    if let Some(bytes) =
                        sample_bytes(node, anchor.min(index)..anchor.max(index) + 1)
                    {
                        picked = Some((bytes, None));
                    }
                } else {
                    let nearest =
                        data.into_iter()
                            .flat_map(|data| data.series.iter())
                            .filter(|(name, _)| !hidden.contains(*name))
                            .flat_map(|(_, values)| values)
                            .filter_map(|point| {
                                let screen = response.transform.position_from_point(
                                    &PlotPoint::new(point.index as f64, point.value),
                                );
                                let distance = screen.distance_sq(pointer);
                                (distance < 144.).then_some((distance, point))
                            })
                            .min_by(|a, b| a.0.total_cmp(&b.0));
                    if let Some((_, point)) = nearest {
                        self.plot_anchor = Some(point.index);
                        picked = Some((point.bytes.clone(), Some(point.path.clone())));
                    } else if let Some(bytes) = sample_bytes(node, index..index + 1) {
                        self.plot_anchor = Some(index);
                        picked = Some((bytes, None));
                    }
                }
            }
        }
        response.response.context_menu(|ui| {
            ui.menu_button("Series",|ui| {if self.presentation.menu(ui,&ranges) {self.plot_fit_y=true;}});
            if let Some(bytes) = &selected_bytes {
                if ui.button("Inspect / edit selected bytes").clicked() {
                    self.plot_action = Some(PlotAction::InspectBytes(bytes.clone()));
                    ui.close();
                }
                if ui.button("Export selected bytes…").clicked() {
                    self.plot_action = Some(PlotAction::ExportBytes(bytes.clone()));
                    ui.close();
                }
            } else {
                ui.label("Select a point or sample range first.");
            }
        });
        // The plot owns its range request. Paging the table never changes this range.
        self.plot_reader.request(
            ui.ctx(),
            source,
            node.clone(),
            actual,
            (response.transform.frame().width() * ui.ctx().pixels_per_point()).ceil() as usize,
            self.plot_refine,
        );
        if let Some((bytes, path)) = picked {
            if let Some(samples) = sample_indices_for_bytes(node, &bytes) {
                self.plot_range_first = samples.start;
                self.plot_range_last = samples.end.saturating_sub(1);
                self.plot_jump = samples.start;
            }
            self.choose_plot_bytes(output, bytes.clone(), path);
            return Some(bytes);
        }
        None
    }

    pub fn highlights(&self, output: &ParseOutput) -> Vec<HexHighlight> {
        let Some(selected) = self.selected_node(output) else {
            return Vec::new();
        };
        // Selecting an entire collection must not paint every sample or the whole file.
        if is_collection(&selected) {
            return Vec::new();
        }
        let mut fields = Vec::new();
        highlight_fields(&selected, &mut fields);
        fields
            .into_iter()
            .take(MAX_HIGHLIGHTS)
            .map(|node| HexHighlight {
                range: node.range.clone(),
                name: format!("{} · {}", node.path, node.kind),
                color: field_color(&node.path),
            })
            .collect()
    }
}

fn plot_collection_choices(ui: &mut Ui, collections: &[&Node], selected: &mut std::collections::BTreeSet<String>) -> bool {
    let mut changed=false;
    ui.menu_button("Collections", |ui| {
        for node in collections {
            let mut enabled=selected.contains(&node.path);
            if ui.checkbox(&mut enabled,collection_label(node)).changed() {
                if enabled {selected.insert(node.path.clone());} else {selected.remove(&node.path);}
                changed=true;
            }
        }
    });
    changed
}

fn field_color(path: &str) -> Color32 {
    // A field keeps its color across sample indices and row/cell selection.
    let relative = path
        .rsplit_once(']')
        .map_or(path, |(_, suffix)| suffix)
        .trim_start_matches('.');
    crate::plot_trace::series_color(relative)
}
fn find_node<'a>(nodes: &'a [Node], path: &str) -> Option<&'a Node> {
    nodes.iter().find_map(|node| {
        if node.path == path {
            Some(node)
        } else {
            find_node(&node.children, path)
        }
    })
}
fn expand_ancestors(path: &str, expanded: &mut HashSet<String>) {
    for (index, ch) in path.char_indices() {
        if ch == '.' || ch == '[' {
            expanded.insert(path[..index].to_owned());
        }
    }
}
fn is_collection(node: &Node) -> bool {
    node.lazy.is_some() || node.kind.ends_with("[]")
}
fn collection_count(node: &Node) -> usize {
    node.lazy
        .as_ref()
        .map_or(node.children.len(), |array| array.count)
}
fn collect_collections<'a>(nodes: &'a [Node], output: &mut Vec<&'a Node>) {
    for node in nodes {
        if is_collection(node) {
            output.push(node);
        }
        collect_collections(&node.children, output);
    }
}
fn containing_collection<'a>(nodes: &'a [Node], range: &Range<usize>) -> Option<&'a Node> {
    let mut collections = Vec::new();
    collect_collections(nodes, &mut collections);
    collections
        .into_iter()
        .filter(|node| node.range.start <= range.start && node.range.end >= range.end)
        .min_by_key(|node| node.range.len())
}
fn numeric_kind(kind: &str) -> bool {
    matches!(
        kind.trim_end_matches("[]"),
        "u8" | "u16"
            | "u24"
            | "u32"
            | "u64"
            | "i8"
            | "i16"
            | "i24"
            | "i32"
            | "i64"
            | "f32"
            | "f64"
            | "varuint"
            | "varint"
    )
}
fn has_numeric(node: &Node) -> bool {
    node.numeric().is_some() || node.children.iter().any(has_numeric)
}
fn collection_score(node: &Node, output: &ParseOutput) -> (bool, usize, usize) {
    let numeric = has_numeric(node)
        || numeric_kind(&node.kind)
        || output
            .definitions
            .iter()
            .find(|definition| definition.name == node.kind.trim_end_matches("[]"))
            .is_some_and(|definition| {
                definition
                    .fields
                    .iter()
                    .any(|field| numeric_kind(&field.kind))
            });
    (numeric, collection_count(node), node.range.start)
}
fn contains_path(parent: &str, path: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with('['))
}
fn leaf_columns(node: &Node, prefix: &str, columns: &mut Vec<String>) {
    if node.children.is_empty() || is_collection(node) {
        if !columns.iter().any(|column| column == prefix) {
            columns.push(prefix.to_owned());
        }
        return;
    }
    for child in &node.children {
        let path = if prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{prefix}.{}", child.name)
        };
        leaf_columns(child, &path, columns);
    }
}
fn table_columns(rows: &[Node]) -> Vec<String> {
    let mut columns = Vec::new();
    for node in rows {
        leaf_columns(node, "", &mut columns);
    }
    columns
}
fn field_at<'a>(node: &'a Node, path: &str) -> Option<&'a Node> {
    if path.is_empty() {
        return Some(node);
    }
    let (name, rest) = path.split_once('.').unwrap_or((path, ""));
    node.children
        .iter()
        .find(|child| child.name == name)
        .and_then(|child| field_at(child, rest))
}
fn highlight_fields<'a>(node: &'a Node, fields: &mut Vec<&'a Node>) {
    if fields.len() >= MAX_HIGHLIGHTS || node.range.is_empty() || is_collection(node) {
        return;
    }
    if node.children.is_empty() {
        fields.push(node);
    } else {
        for child in &node.children {
            highlight_fields(child, fields);
        }
    }
}
fn table_cell(
    ui: &mut Ui,
    text: &str,
    width: f32,
    height: f32,
    selected: bool,
    clickable: bool,
    header: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, height),
        if clickable {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, text)
    });
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&response, selected);
        if selected || clickable && response.hovered() {
            ui.painter().rect_filled(
                rect.shrink(1.),
                2.,
                if selected {
                    ui.visuals().selection.bg_fill
                } else {
                    visuals.weak_bg_fill
                },
            );
        }
        let color = if selected {
            ui.visuals().selection.stroke.color
        } else {
            visuals.text_color()
        };
        let font = if header {
            egui::TextStyle::Button.resolve(ui.style())
        } else {
            egui::TextStyle::Monospace.resolve(ui.style())
        };
        ui.painter()
            .with_clip_rect(rect.shrink2(egui::vec2(6., 0.)))
            .text(
                rect.left_center() + egui::vec2(6., 0.),
                egui::Align2::LEFT_CENTER,
                text,
                font,
                color,
            );
    }
    response
}
fn flatten_visible<'a>(
    nodes: &'a [Node],
    depth: usize,
    expanded: &HashSet<String>,
    page: Option<&'a Page>,
    revision: u64,
    rows: &mut Vec<(&'a Node, usize)>,
) {
    for node in nodes {
        rows.push((node, depth));
        if expanded.contains(&node.path) {
            flatten_visible(&node.children, depth + 1, expanded, page, revision, rows);
            if let Some(nodes) = page
                .filter(|page| page.key.path == node.path && page.key.revision == revision)
                .and_then(|page| page.result.as_ref().ok())
            {
                for child in nodes {
                    if !node.children.iter().any(|preview| preview.path == child.path) {
                        flatten_visible(std::slice::from_ref(child), depth + 1, expanded, None, revision, rows);
                    }
                }
            }
        }
    }
}
fn clipped(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}
fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}



fn centered_view(center: usize, width: usize, count: usize) -> Range<usize> {
    let width = width.max(1).min(count);
    let start = center
        .saturating_sub(width / 2)
        .min(count.saturating_sub(width));
    start..start + width
}

fn padded_selection(samples: &Range<usize>, count: usize) -> Range<usize> {
    let padding = (samples.len() / 10).max(8);
    samples.start.saturating_sub(padding)..samples.end.saturating_add(padding).min(count)
}

fn selection_view(
    current: Option<&Range<usize>>,
    samples: &Range<usize>,
    count: usize,
) -> Range<usize> {
    if let Some(current) =
        current.filter(|current| current.start <= samples.start && current.end >= samples.end)
    {
        return current.clone();
    }
    if samples.len() > INITIAL_PLOT_SAMPLES {
        return padded_selection(samples, count);
    }
    let width = current
        .map_or(INITIAL_PLOT_SAMPLES, |current| current.len())
        .clamp(
            samples.len().saturating_add(16),
            INITIAL_PLOT_SAMPLES.max(samples.len().saturating_add(16)),
        );
    centered_view(
        samples.start.saturating_add(samples.len() / 2),
        width,
        count,
    )
}

fn viewport_samples(min: f64, max: f64, count: usize) -> Range<usize> {
    if count == 0 {
        return 0..0;
    }
    if !min.is_finite() || !max.is_finite() || max <= min {
        return 0..count.min(INITIAL_PLOT_SAMPLES);
    }
    let start = (min.max(0.).ceil() as usize).min(count - 1);
    let end = ((max.max(0.).floor() as usize).saturating_add(1))
        .min(count)
        .max(start + 1);
    start..end
}

fn numeric_bounds(values: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    let (min, max) = values
        .filter(|value| value.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), value| {
            (min.min(value), max.max(value))
        });
    if !min.is_finite() {
        return None;
    }
    let margin = ((max * 0.5 - min * 0.5) * 0.1)
        .max(min.abs() * f64::EPSILON * 8.)
        .max(0.01);
    Some(((min - margin).max(-f64::MAX), (max + margin).min(f64::MAX)))
}

fn collection_label(node: &Node) -> String {
    format!("{} · {} samples", node.name, collection_count(node))
}

fn compact_float(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0. {
        return "0".into();
    }
    let magnitude = value.abs();
    if !(0.0001..10_000_000.).contains(&magnitude) {
        return format!("{value:.6e}");
    }
    let decimals = (6. - magnitude.log10().floor()).max(0.) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

fn display(node: &Node, limit: usize) -> String {
    match &node.value {
        Some(Value::Float(value)) if limit <= 180 => compact_float(*value),
        Some(Value::Text(text)) => clipped(text, limit),
        Some(Value::Bytes(bytes)) => {
            let mut text = bytes
                .iter()
                .take(limit / 3)
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(" ");
            if bytes.len() > limit / 3 {
                text.push('…');
            }
            text
        }
        _ => clipped(&node.display(), limit),
    }
}

fn tree_display(node: &Node, limit: usize) -> String {
    if node.value.is_some() || node.lazy.is_some() {
        display(node, limit)
    } else {
        clipped(&node.tree_summary(), limit)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn positioned_series_clicks_keep_original_values_and_bytes() {
        let ctx=egui::Context::default();
        let store=ByteStore::from_snapshot(Original::from_bytes(crate::demos::Demo::Advanced.bytes()).unwrap());
        let output=Program::parse(crate::demos::Demo::Advanced.yaml()).unwrap().run(&store.view(),&AtomicBool::new(false)).unwrap();
        let mut views=StructureViews::default();
        for _ in 0..20 {render_plot(&ctx,&mut views,&output,&store,vec![],Default::default());std::thread::sleep(std::time::Duration::from_millis(5));}
        let raw=views.extra_plot_readers["measurements"].data().unwrap().series["temperature"][10].value;
        views.presentation.series.insert("measurements / temperature".into(),crate::plot_presentation::Placement {scale:0.5,offset:200.});
        views.plot_fit_y=true;
        render_plot(&ctx,&mut views,&output,&store,vec![],Default::default());
        let selected=click_plot(&ctx,&mut views,&output,&store,10.,raw*0.5+200.,false);
        assert_eq!(selected,Some(6240..6244));
        assert_eq!(views.extra_plot_readers["measurements"].data().unwrap().series["temperature"][10].value,raw);
        assert!(!store.is_dirty());
    }
    #[test]
    fn advanced_demo_overlays_collections_and_keeps_them_when_selecting_bytes() {
        let ctx = egui::Context::default();
        let store = ByteStore::from_snapshot(Original::from_bytes(crate::demos::Demo::Advanced.bytes()).unwrap());
        let output = Program::parse(crate::demos::Demo::Advanced.yaml()).unwrap()
            .run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            render_plot(&ctx,&mut views,&output,&store,vec![],Default::default());
            if views.extra_plot_readers.len()==2 && views.extra_plot_readers.values().all(|r|r.data().is_some() && !r.is_loading()) && !views.plot_reader.is_loading() {break;}
            assert!(std::time::Instant::now()<deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(views.plot_sources.len(),3);
        let data=views.extra_plot_readers["measurements"].data().unwrap();
        assert_eq!(data.series["temperature"][0].index,0);
        assert_eq!(data.series["temperature"][0].bytes,6160..6164);
        let sources=views.plot_sources.clone();
        views.select_bytes(&output,Some(6160..6164));
        render_plot(&ctx,&mut views,&output,&store,vec![],Default::default());
        assert_eq!(views.plot_sources,sources);
        assert_eq!(views.plot_target,"waves");
        views.plot_sources.remove("events");
        render_plot(&ctx,&mut views,&output,&store,vec![],Default::default());
        assert_eq!(views.plot_sources.len(),2);
        assert!(!views.extra_plot_readers.contains_key("events"));
    }

    use super::*;
    use crate::binary::Original;
    use crate::byte_store::ByteStore;
    use crate::structure::Program;

    fn array() -> (ByteStore, ParseOutput) {
        let bytes: Vec<_> = (0_u16..1024).flat_map(u16::to_le_bytes).collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let program =
            Program::parse("actions:\n  - read: {name: items, type: u16, repeat: 1024}\n").unwrap();
        let output = program.run(&store.view(), &AtomicBool::new(false)).unwrap();
        (store, output)
    }
    fn settle(views: &mut StructureViews, source: ByteView) {
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            views.tick(&ctx, source.clone());
            if views.rx.is_none() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "page worker timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    #[test]
    fn lazy_pages_are_bounded_and_selectable_without_rendering_the_tree() {
        let (store, output) = array();
        assert!(output.nodes[0].children.is_empty());
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.request(&output.nodes[0], 512, output.revision, &ctx);
        settle(&mut views, store.view());
        let page = views.page_nodes("items", output.revision).unwrap();
        assert_eq!(page.len(), PAGE_SIZE);
        assert_eq!(page[0].numeric(), Some(512.));
        assert_eq!(page[0].range, 1024..1026);
        views.select_bytes(&output, Some(1024..1026));
        let selected = views.selected_node(&output).unwrap();
        assert_eq!(selected.path, "items[512]");
        assert_eq!(views.highlights(&output)[0].range, 1024..1026);
        views.select_bytes(&output, Some(9000..9001));
        assert!(views.selected_node(&output).is_none());
        views.select_bytes(&output, None);
        assert!(views.selected_node(&output).is_none());
    }
    #[test]
    fn changed_bytes_discard_inflight_pages_and_stale_highlights() {
        let (mut store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.request(&output.nodes[0], 0, output.revision, &ctx);
        views.tick(&ctx, store.view());
        assert!(views.rx.is_some());
        store.replace(0, &[255]).unwrap();
        settle(&mut views, store.view());
        assert!(views.page.is_none());
        assert!(views.desired.is_none());
        assert!(views.highlights(&output).is_empty());
        assert!(views.selected_node(&output).is_none());
    }
    #[test]
    fn selecting_root_elements_remains_selected_across_frames() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.tick(&ctx, store.view());
        let _ = ctx.run(Default::default(), |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| views.target_controls(ui, &output));
        });
        assert_eq!(views.table_target, "items");
        views.table_target.clear();
        let _ = ctx.run(Default::default(), |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| views.target_controls(ui, &output));
        });
        assert!(views.table_target.is_empty());
    }
    fn records() -> (ByteStore, ParseOutput) {
        let yaml = r#"
structures:
  - name: Header
    fields: [{name: version, type: u16}, {name: count, type: u16}]
  - name: Descriptor
    fields: [{name: code, type: u8}, {name: size, type: u8}]
  - name: Pair
    fields: [{name: z, type: i16}, {name: a, type: u16}]
  - name: Sample
    fields: [{name: timestamp, type: u32}, {name: pair, type: {structure: Pair}}]
actions:
  - read: {name: header, type: {structure: Header}}
  - read: {name: descriptors, type: {structure: Descriptor}, repeat: 2}
  - read: {name: samples, type: {structure: Sample}, repeat: 256}
"#;
        let bytes = vec![0; 8 + 256 * 8];
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml)
            .unwrap()
            .run(&store.view(), &AtomicBool::new(false))
            .unwrap();
        (store, output)
    }
    #[test]
    fn default_table_chooses_samples_and_keeps_definition_column_order() {
        let (store, output) = records();
        let mut views = StructureViews::default();
        views.initialize_target(&output);
        assert_eq!(views.table_target, "samples");
        let mut collections = Vec::new();
        collect_collections(&output.nodes, &mut collections);
        assert_eq!(
            collections
                .iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            ["descriptors", "samples"]
        );
        views.request(
            &output.nodes[2],
            0,
            output.revision,
            &egui::Context::default(),
        );
        settle(&mut views, store.view());
        let rows = views.target_rows(&output);
        assert_eq!(rows.len(), PAGE_SIZE);
        assert_eq!(table_columns(rows), ["timestamp", "pair.z", "pair.a"]);
        assert_eq!(field_at(&rows[0], "pair.a").unwrap().range, 14..16);
    }
    #[test]
    fn hex_selection_loads_its_sample_page_without_any_view_open() {
        let (store, output) = records();
        let mut views = StructureViews::default();
        views.select_bytes(&output, Some(1614..1616));
        assert_eq!(views.table_target, "samples");
        assert_eq!(views.desired.as_ref().unwrap().key.start, 128);
        assert!(views.highlights(&output).is_empty());
        settle(&mut views, store.view());
        assert_eq!(
            views.selected_node(&output).unwrap().path,
            "samples[200].pair.a"
        );
        assert_eq!(views.highlights(&output).len(), 1);
        assert_eq!(views.highlights(&output)[0].range, 1614..1616);
        assert!(views.reveal_selected_row);
        views.select_bytes(&output, Some(2..4));
        assert_eq!(
            views.table_target, "samples",
            "inspecting the header must not replace the sample table"
        );
        assert_eq!(views.selected_node(&output).unwrap().path, "header.count");
    }
    fn table_frame(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
        events: Vec<egui::Event>,
        modifiers: egui::Modifiers,
    ) -> (egui::FullOutput, Option<Range<usize>>) {
        let mut selection = None;
        let frame = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900., 600.),
                )),
                events: std::iter::once(egui::Event::ModifiersChanged(modifiers)).chain(events).collect(),
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    selection = views.table(ui, output, store.view());
                });
            },
        );
        // The workspace sends the same selection back from Hex on the next tick.
        if let Some(bytes) = &selection {
            views.select_bytes(output, Some(bytes.clone()));
        }
        (frame, selection)
    }

    fn click_table_row(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
        row: usize,
        field: bool,
        shift: bool,
    ) -> Range<usize> {
        let label = row.to_string();
        let (frame, _) = table_frame(ctx, views, output, store, vec![], egui::Modifiers::NONE);
        let pos = frame
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape {
                    if text.galley.text() == label && text.pos.x < 100. {
                        return Some(text.pos + text.galley.size() * 0.5);
                    }
                }
                None
            })
            .expect("sample number should be visible");
        let pos = if field { egui::pos2(750., pos.y) } else { pos };
        let modifiers = egui::Modifiers {
            shift,
            ..Default::default()
        };
        table_frame(
            ctx,
            views,
            output,
            store,
            vec![egui::Event::PointerMoved(pos)],
            modifiers,
        );
        table_frame(
            ctx,
            views,
            output,
            store,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers,
            }],
            modifiers,
        );
        table_frame(
            ctx,
            views,
            output,
            store,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers,
            }],
            modifiers,
        )
        .1
        .expect("table click should select bytes")
    }

    #[test]
    fn rendered_table_shift_click_preserves_anchor_and_selects_whole_samples() {
        let (store, output) = records();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.initialize_target(&output);
        views.request(&output.nodes[2], 0, output.revision, &ctx);
        settle(&mut views, store.view());
        assert_eq!(
            click_table_row(&ctx, &mut views, &output, &store, 3, false, false),
            32..40
        );
        assert_eq!(
            click_table_row(&ctx, &mut views, &output, &store, 7, false, true),
            32..72
        );
        assert_eq!(
            click_table_row(&ctx, &mut views, &output, &store, 1, false, true),
            16..40
        );
        // Ordinary cell clicks still select just that field; Shift selects full rows.
        assert_eq!(
            click_table_row(&ctx, &mut views, &output, &store, 2, true, false),
            30..32
        );
        assert_eq!(
            click_table_row(&ctx, &mut views, &output, &store, 5, true, true),
            24..56
        );
        assert_eq!(
            sample_indices_for_bytes(&output.nodes[2], views.last_hex_selection.as_ref().unwrap()),
            Some(2..6)
        );
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(views.last_hex_selection, Some(24..56));
    }

    #[test]
    fn table_shift_selection_crosses_lazy_pages_and_resets_for_external_selection() {
        let (store, output) = records();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.initialize_target(&output);
        views.request(&output.nodes[2], 0, output.revision, &ctx);
        settle(&mut views, store.view());
        let row = views.target_rows(&output)[3].clone();
        views.select_table_node(&output, &row, &row, false);
        views.request(&output.nodes[2], 128, output.revision, &ctx);
        settle(&mut views, store.view());
        let row = views.target_rows(&output)[2].clone();
        assert_eq!(views.select_table_node(&output, &row, &row, true), 32..1056);
        assert_eq!(views.desired.as_ref().unwrap().key.start, 128);
        // A linked Hex/Plot selection establishes a fresh starting sample.
        views.select_bytes(&output, Some(1046..1048)); // sample 129's last field
        assert_eq!(
            views.select_table_node(&output, &row, &row, true),
            1040..1056
        );
        views.invalidate();
        assert!(views.table_anchor.is_none());
    }

    #[test]
    fn table_shift_anchor_does_not_leak_between_collections() {
        let (store, output) = records();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.initialize_target(&output);
        views.request(&output.nodes[2], 0, output.revision, &ctx);
        settle(&mut views, store.view());
        let row = views.target_rows(&output)[3].clone();
        views.select_table_node(&output, &row, &row, false);
        views.table_target = "descriptors".into();
        let descriptor = output.nodes[1].children.get(1).cloned().unwrap_or_else(|| {
            output.nodes[1]
                .lazy
                .as_ref()
                .unwrap()
                .page(&store.view(), &AtomicBool::new(false), 1, 1)
                .unwrap()
                .remove(0)
        });
        assert_eq!(
            views.select_table_node(&output, &descriptor, &descriptor, true),
            descriptor.range
        );
    }

    #[test]
    fn sample_and_cell_selection_keep_exact_ranges_and_collection() {
        let (store, output) = records();
        let mut views = StructureViews::default();
        views.initialize_target(&output);
        views.request(
            &output.nodes[2],
            0,
            output.revision,
            &egui::Context::default(),
        );
        settle(&mut views, store.view());
        let row = views.target_rows(&output)[3].clone();
        let selected = views.select(&row);
        views.select_bytes(&output, Some(selected.clone()));
        assert_eq!(selected, 32..40);
        assert_eq!(views.selected_node(&output).unwrap().path, "samples[3]");
        assert_eq!(
            views
                .highlights(&output)
                .iter()
                .map(|highlight| highlight.range.clone())
                .collect::<Vec<_>>(),
            [32..36, 36..38, 38..40]
        );
        let field = field_at(&row, "pair.a").unwrap();
        let selected = views.select(field);
        views.select_bytes(&output, Some(selected.clone()));
        assert_eq!(selected, 38..40);
        assert_eq!(views.highlights(&output).len(), 1);
        assert_eq!(views.highlights(&output)[0].range, 38..40);
        assert_eq!(views.table_target, "samples");
    }
    #[test]
    fn highlights_do_not_paint_unselected_hierarchies_or_entire_collections() {
        let (_, output) = records();
        let mut views = StructureViews::default();
        assert!(views.highlights(&output).is_empty());
        views.select(&output.nodes[2]);
        assert!(views.highlights(&output).is_empty());
        views.select(&output.nodes[0]);
        let highlights = views.highlights(&output);
        assert_eq!(
            highlights
                .iter()
                .map(|highlight| highlight.range.clone())
                .collect::<Vec<_>>(),
            [0..2, 2..4]
        );
    }
    #[test]
    fn eager_numeric_samples_are_preferred_over_large_non_numeric_collections() {
        let bytes = vec![0; 140];
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse("actions: [{read: {name: strings, type: string, length: 1, repeat: 100}}, {read: {name: measurements, type: u16, repeat: 20}}]")
            .unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        views.initialize_target(&output);
        assert_eq!(views.table_target, "measurements");
        assert_eq!(views.target_rows(&output).len(), 20);
    }
    #[test]
    fn field_colors_are_stable_between_samples_and_row_or_cell_selection() {
        let (store, output) = records();
        let mut views = StructureViews::default();
        views.request(
            &output.nodes[2],
            0,
            output.revision,
            &egui::Context::default(),
        );
        settle(&mut views, store.view());
        let row = views.page_nodes("samples", output.revision).unwrap()[3].clone();
        views.select(&row);
        let color = views.highlights(&output)[2].color;
        views.select(field_at(&row, "pair.a").unwrap());
        assert_eq!(views.highlights(&output)[0].color, color);
        assert_eq!(field_color("samples[4].pair.a"), color);
    }
    #[test]
    fn sample_table_retains_more_than_thirty_two_fields() {
        let fields = (0..40)
            .map(|index| format!("{{name: value_{index}, type: u8}}"))
            .collect::<Vec<_>>()
            .join(",");
        let yaml = format!("structures: [{{name: Sample, fields: [{fields}]}}]\nactions: [{{read: {{name: samples, type: {{structure: Sample}}, repeat: 2}}}}]");
        let store = ByteStore::from_snapshot(Original::from_bytes(&[0; 80]).unwrap());
        let output = Program::parse(&yaml)
            .unwrap()
            .run(&store.view(), &AtomicBool::new(false))
            .unwrap();
        let columns = table_columns(&output.nodes[0].children);
        assert_eq!(columns.len(), 40);
        assert_eq!(columns[39], "value_39");
    }
    #[test]
    fn rendered_table_exposes_sample_numbers_and_keeps_headers_visible_when_scrolling() {
        fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
            fn collect(shape: &egui::Shape, result: &mut Vec<String>) {
                match shape {
                    egui::Shape::Text(text) => result.push(text.galley.text().to_owned()),
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            collect(shape, result);
                        }
                    }
                    _ => (),
                }
            }
            let mut result = Vec::new();
            for shape in shapes {
                collect(&shape.shape, &mut result);
            }
            result
        }
        let (store, output) = records();
        let mut views = StructureViews::default();
        views.initialize_target(&output);
        views.request(
            &output.nodes[2],
            0,
            output.revision,
            &egui::Context::default(),
        );
        settle(&mut views, store.view());
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900., 360.),
            )),
            ..Default::default()
        };
        let mut render = || {
            ctx.run(input.clone(), |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    views.table(ui, &output, store.view());
                });
            })
        };
        let first = render();
        let labels = texts(&first.shapes);
        for label in [
            "Sample",
            "Byte offset",
            "timestamp",
            "pair.z",
            "pair.a",
            "0x8",
        ] {
            assert!(
                labels.iter().any(|text| text == label),
                "missing {label} in {labels:?}"
            );
        }
        views.select_bytes(&output, Some(8 + 100 * 8..8 + 101 * 8));
        let scrolled = ctx.run(input, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                views.table(ui, &output, store.view());
            });
        });
        let labels = texts(&scrolled.shapes);
        for label in ["Sample", "timestamp", "100"] {
            assert!(
                labels.iter().any(|text| text == label),
                "missing {label} after scroll in {labels:?}"
            );
        }
    }
    fn render_plot(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
        events: Vec<egui::Event>,
        modifiers: egui::Modifiers,
    ) -> Option<Range<usize>> {
        let events = events.into_iter().map(|event| match event {
            egui::Event::MouseWheel { unit, delta, phase, .. } =>
                egui::Event::MouseWheel { unit, delta, phase, modifiers },
            other => other,
        });
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200., 700.),
            )),
            events: std::iter::once(egui::Event::ModifiersChanged(modifiers)).chain(events).collect(),
            ..Default::default()
        };
        let mut selection = None;
        let _ = ctx.run(input, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                selection = views.plot(ui, output, store.view());
            });
        });
        selection
    }

    fn settle_plot(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for _ in 0..2 {
            loop {
                render_plot(ctx, views, output, store, Vec::new(), Default::default());
                if !views.plot_reader.is_loading() {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "plot reader timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        assert!(
            views.plot_reader.error().is_none(),
            "{:?}",
            views.plot_reader.error()
        );
    }

    fn click_plot(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
        sample: f64,
        value: f64,
        shift: bool,
    ) -> Option<Range<usize>> {
        let id = egui::Id::new((
            "decoded_structure_plot",
            "overlay",
            output.revision,
        ));
        let memory = egui_plot::PlotMemory::load(ctx, id).unwrap();
        let pos = memory
            .transform()
            .position_from_point(&PlotPoint::new(sample, value));
        assert!(
            memory.transform().frame().contains(pos),
            "click must be inside the plot"
        );
        let modifiers = egui::Modifiers {
            shift,
            ..Default::default()
        };
        render_plot(
            ctx,
            views,
            output,
            store,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers,
                },
            ],
            modifiers,
        );
        render_plot(
            ctx,
            views,
            output,
            store,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers,
            }],
            modifiers,
        )
    }

    #[test]
    fn rendered_plot_reads_full_view_and_pans_independently_of_table_pages() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.initialize_target(&output);
        views.request(&output.nodes[0], 128, output.revision, &ctx);
        settle(&mut views, store.view());
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(views.plot_reader.data().unwrap().range, 0..1024);
        assert_eq!(
            views.plot_reader.data().unwrap().series["value"].len(),
            1024
        );
        assert_eq!(views.desired.as_ref().unwrap().key.start, 128);
        let id = egui::Id::new((
            "decoded_structure_plot",
            "overlay",
            output.revision,
        ));
        let mut memory = egui_plot::PlotMemory::load(&ctx, id).unwrap();
        memory.set_bounds(PlotBounds::from_min_max([511.5, 450.], [767.5, 800.]));
        memory.store(&ctx, id);
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(views.plot_reader.data().unwrap().range, 512..768);
        assert_eq!(
            views.desired.as_ref().unwrap().key.start,
            128,
            "panning must not page the table"
        );
        views.request(&output.nodes[0], 896, output.revision, &ctx);
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(
            views.plot_reader.data().unwrap().range,
            512..768,
            "paging the table must not change the plot"
        );
    }

    #[test]
    fn rendered_point_shift_range_and_hex_selections_share_exact_bytes() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        let selected = click_plot(&ctx, &mut views, &output, &store, 600., 600., false).unwrap();
        assert_eq!(selected, 1200..1202);
        assert_eq!(views.desired.as_ref().unwrap().key.start, 512);
        settle(&mut views, store.view());
        assert_eq!(views.selected_node(&output).unwrap().path, "items[600]");
        let selected = click_plot(&ctx, &mut views, &output, &store, 650., 650., true).unwrap();
        assert_eq!(
            selected,
            1200..1302,
            "Shift-click must include both whole endpoint samples"
        );
        assert_eq!(views.last_hex_selection, Some(selected));
        views.plot_view = Some(512..768);
        views.plot_force_view = Some(512..768);
        settle_plot(&ctx, &mut views, &output, &store);
        views.select_bytes(&output, Some(1800..1802));
        settle_plot(&ctx, &mut views, &output, &store);
        let range = &views.plot_reader.data().unwrap().range;
        assert!(
            range.contains(&900),
            "Hex selection must reveal its sample in the plot"
        );
        assert_eq!(views.desired.as_ref().unwrap().key.start, 896);
        assert_eq!(views.last_hex_selection, Some(1800..1802));
    }

    #[test]
    fn sample_range_mapping_includes_all_fields_and_clamps_selection_views() {
        let (_, output) = records();
        let node = &output.nodes[2];
        assert_eq!(
            sample_indices_for_bytes(node, &(8 + 200 * 8 + 6..8 + 200 * 8 + 8)),
            Some(200..201)
        );
        assert_eq!(sample_bytes(node, 200..203), Some(1608..1632));
        assert_eq!(
            sample_indices_for_bytes(node, &(8 + 200 * 8..8 + 203 * 8)),
            Some(200..203)
        );
        assert!(sample_bytes(node, 255..257).is_none());
        assert!(sample_indices_for_bytes(node, &(2..4)).is_none());
        assert_eq!(
            selection_view(Some(&(100..220)), &(150..151), 256),
            100..220
        );
        let shifted = selection_view(Some(&(0..64)), &(255..256), 256);
        assert_eq!(shifted.end, 256);
        assert!(shifted.contains(&255));
    }

    #[test]
    fn rendered_out_of_file_pan_returns_to_real_samples() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        let id = egui::Id::new((
            "decoded_structure_plot",
            "overlay",
            output.revision,
        ));
        let mut memory = egui_plot::PlotMemory::load(&ctx, id).unwrap();
        memory.set_bounds(PlotBounds::from_min_max([5000., 0.], [5500., 1100.]));
        memory.store(&ctx, id);
        settle_plot(&ctx, &mut views, &output, &store);
        let memory = egui_plot::PlotMemory::load(&ctx, id).unwrap();
        assert!(memory.bounds().max()[0] <= 1023.5);
        assert_eq!(views.plot_reader.data().unwrap().range.end, 1024);
    }

    #[test]
    fn byte_revision_keeps_plot_range_but_discards_old_values_and_actions() {
        let (mut store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.tick(&ctx, store.view());
        views.plot_target = "items".into();
        views.plot_view = Some(500..750);
        views.plot_action = Some(PlotAction::ExportBytes(1000..1002));
        store.replace(1000, &[0xff]).unwrap();
        views.tick(&ctx, store.view());
        assert_eq!(views.plot_view, Some(500..750));
        assert_eq!(views.plot_force_view, Some(500..750));
        assert!(views.plot_reader.data().is_none());
        assert!(views.take_plot_action().is_none());
        assert!(views.stale(&output));
    }
    fn waveform_plot_fixture() -> (ByteStore, ParseOutput) {
        let bytes: Vec<_> = (0..65_536)
            .flat_map(|index| {
                let phase = (index % 64) as f64 / 64.;
                [
                    (phase * std::f64::consts::TAU).sin() * 4.,
                    10. + phase * 4.,
                    20. + if index == 5001 {
                        8.
                    } else if index % 64 < 32 {
                        2.
                    } else {
                        -2.
                    },
                ]
                .into_iter()
                .flat_map(f64::to_le_bytes)
            })
            .collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(
            r#"
structures:
  - name: Sample
    fields: [{name: sine, type: f64}, {name: ramp, type: f64}, {name: square, type: f64}]
actions: [{read: {name: samples, type: {structure: Sample}, repeat: until end}}]
"#,
        )
        .unwrap()
        .run(&store.view(), &AtomicBool::new(false))
        .unwrap();
        (store, output)
    }

    fn waveform_plot_frame(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200., 700.),
                )),
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    views.plot(ui, output, store.view());
                });
            },
        )
    }

    #[test]
    fn zoom_out_keeps_resolved_traces_and_dense_bands_with_source_provenance() {
        let (store, output) = waveform_plot_fixture();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        fn paths(shape: &egui::Shape, color: Color32, traces: &mut Vec<Vec<egui::Pos2>>) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        paths(shape, color, traces);
                    }
                }
                egui::Shape::Path(path)
                    if !path.closed && path.stroke.color == egui::epaint::ColorMode::Solid(color) && path.points.len() > 4 =>
                {
                    traces.push(path.points.clone());
                }
                _ => (),
            }
        }
        for count in [2048, 2049, 2179, 4096, 8193, 10_000, 65_536] {
            views.plot_force_view = Some(0..count);
            views.plot_fit_y = true;
            settle_plot(&ctx, &mut views, &output, &store);
            let rendered = waveform_plot_frame(&ctx, &mut views, &output, &store);
            let data = views.plot_reader.data().unwrap();
            assert_eq!(data.scanned, count);
            assert!(data.complete);
            if count > 8192 {
                assert_eq!(data.quality, PlotQuality::MinMax);
            }
            let transform = navigation_memory(&ctx, &views, &output).transform().clone();
            for name in ["sine", "ramp", "square"] {
                let values = &data.series[name];
                assert!(values.windows(2).all(|pair| pair[0].index < pair[1].index));
                let mut traces = Vec::new();
                for shape in &rendered.shapes {
                    paths(&shape.shape, field_color(&format!("{} / {name}", views.plot_target)), &mut traces);
                }
                if count <= 10_000 {
                    assert_eq!(traces.len(), 1,
                        "{count} samples: {name} cycles fit and must remain a continuous trace");
                    let trace = &traces[0];
                    assert_eq!(trace.len(), values.len());
                    for (screen, sample) in trace.iter().zip(values) {
                        let expected = transform
                            .position_from_point(&PlotPoint::new(sample.index as f64, sample.value));
                        assert!(screen.distance(expected) < 0.01);
                    }
                } else {
                    assert!(traces.is_empty(), "subpixel cycles must become a band");
                    assert!(rendered.shapes.iter().any(|shape| matches!(
                        &shape.shape, egui::Shape::Mesh(mesh)
                        if mesh.vertices.len() > 100
                            && mesh.vertices.iter().any(|v| v.color == field_color(&format!("{} / {name}", views.plot_target)).gamma_multiply(0.45))
                    )), "{name} must render a continuous amplitude mesh");
                }
                for sample in values {
                    assert_eq!(
                        store
                            .view()
                            .read(sample.bytes.start, sample.bytes.len())
                            .unwrap()
                            .as_slice(),
                        sample.value.to_le_bytes()
                    );
                }
            }
            if count > 5001 {
                let spike = data.series["square"]
                    .iter()
                    .find(|sample| sample.index == 5001)
                    .expect("single-sample peak must survive reduction");
                assert_eq!(spike.value, 28.);
                assert_eq!(spike.bytes, 5001 * 24 + 16..5001 * 24 + 24);
            }
        }
        // Selection still resolves to the source sample after traces are reduced.
        views.plot_force_view = Some(0..10_000);
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(
            click_plot(&ctx, &mut views, &output, &store, 5001., 28., false),
            Some(5001 * 24 + 16..5001 * 24 + 24)
        );
        views.plot_force_view = Some(0..65_536);
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(
            click_plot(&ctx, &mut views, &output, &store, 5001., 28., false),
            Some(5001 * 24 + 16..5001 * 24 + 24),
            "selecting a peak inside an amplitude band must retain exact source bytes"
        );
    }

    #[test]
    fn display_scale_changes_reduction_without_changing_sample_range_or_selection() {
        let (store, output) = waveform_plot_fixture();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_force_view = Some(0..4096);
        settle_plot(&ctx, &mut views, &output, &store);
        let selected = click_plot(&ctx, &mut views, &output, &store, 1008., -4., false);
        assert_eq!(selected, Some(1008 * 24..1008 * 24 + 8));
        for (scale, quality) in [(1., PlotQuality::MinMax), (2., PlotQuality::Exact), (1., PlotQuality::MinMax)] {
            ctx.set_pixels_per_point(scale);
            settle_plot(&ctx, &mut views, &output, &store);
            let data = views.plot_reader.data().unwrap();
            assert_eq!(data.quality, quality, "display scale {scale}");
            assert_eq!(data.range, 0..4096);
            assert_eq!(views.last_hex_selection, selected);
        }
    }

    #[test]
    #[ignore = "writes waveform zoom captures to /tmp"]
    fn capture_waveform_zoom_levels() {
        let (store, output) = waveform_plot_fixture();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        for count in [2048, 2179, 10_000] {
            views.plot_force_view = Some(0..count);
            views.plot_fit_y = true;
            settle_plot(&ctx, &mut views, &output, &store);
            let rendered = waveform_plot_frame(&ctx, &mut views, &output, &store);
            write_waveform_capture(&ctx, rendered,
                &format!("/tmp/binwave-waveform-{count}.json"));
        }
    }

    fn write_waveform_capture(ctx: &egui::Context, rendered: egui::FullOutput, path: &str) {
        let primitives = ctx.tessellate(rendered.shapes, rendered.pixels_per_point);
        let atlas = ctx.fonts(|fonts| fonts.image());
        let pixels: Vec<[u8; 4]> = atlas
            .pixels.iter()
            .map(|pixel| pixel.to_array())
            .collect();
        let meshes: Vec<_> = primitives.into_iter().filter_map(|primitive| {
            let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive else { return None; };
            Some(serde_json::json!({
                "clip": [primitive.clip_rect.min.x, primitive.clip_rect.min.y, primitive.clip_rect.max.x, primitive.clip_rect.max.y],
                "vertices": mesh.vertices.iter().map(|v| (v.pos.x, v.pos.y, v.uv.x, v.uv.y, v.color.to_array())).collect::<Vec<_>>(),
                "indices": mesh.indices,
            }))
        }).collect();
        let capture = serde_json::json!({"size": [1200, 700], "atlas_size": atlas.size, "atlas": pixels, "meshes": meshes});
        std::fs::write(path, serde_json::to_vec(&capture).unwrap()).unwrap();
    }

    #[test]
    #[ignore = "requires generated 2 GB demo and writes captures to /tmp"]
    fn capture_demo_dense_zoom() {
        let source = crate::file_source::FileSource::open(std::path::Path::new(
            "samples/data/waveform_demo_2gb.bin",
        )).unwrap();
        let store = ByteStore::from_file(Arc::new(source));
        let output = Program::parse(include_str!("../samples/decoders/yaml/waveform.yaml"))
            .unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        for count in [2179, 12_496, 32_768] {
            views.plot_force_view = Some(28_358..28_358 + count);
            views.plot_fit_y = true;
            settle_plot(&ctx, &mut views, &output, &store);
            assert_eq!(views.plot_reader.data().unwrap().scanned, count);
            let rendered = waveform_plot_frame(&ctx, &mut views, &output, &store);
            write_waveform_capture(&ctx, rendered,
                &format!("/tmp/binwave-demo-density-{count}.json"));
        }
    }

    #[test]
    fn flat_overview_bands_produce_only_finite_geometry() {
        let count = 100_000;
        let store = ByteStore::from_snapshot(Original::from_bytes(&vec![7; count]).unwrap());
        let output =
            Program::parse("actions: [{read: {name: samples, type: u8, repeat: until end}}]")
                .unwrap()
                .run(&store.view(), &AtomicBool::new(false))
                .unwrap();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_force_view = Some(0..count);
        settle_plot(&ctx, &mut views, &output, &store);
        let data = views.plot_reader.data().unwrap();
        assert_eq!(data.quality, PlotQuality::SampledPreview);
        assert!(data.envelopes["value"]
            .iter()
            .all(|band| band.min.value == 7. && band.max.value == 7.));
        let rendered = waveform_plot_frame(&ctx, &mut views, &output, &store);
        let primitives = ctx.tessellate(rendered.shapes, rendered.pixels_per_point);
        assert!(!primitives.is_empty());
        for primitive in primitives {
            if let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive {
                assert!(
                    mesh.vertices
                        .iter()
                        .all(|vertex| vertex.pos.is_finite() && vertex.uv.is_finite()),
                    "flat overview must not send invalid triangles to the GPU"
                );
            }
        }
    }

    #[test]
    fn rendered_overview_uses_amplitude_bands_instead_of_sample_dots() {
        let count = 100_000;
        let bytes: Vec<_> = (0..count)
            .flat_map(|index| {
                ((index % 64) as f64 * std::f64::consts::TAU / 64.)
                    .sin()
                    .to_le_bytes()
            })
            .collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output =
            Program::parse("actions: [{read: {name: samples, type: f64, repeat: until end}}]")
                .unwrap()
                .run(&store.view(), &AtomicBool::new(false))
                .unwrap();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_force_view = Some(0..count);
        views.plot_fit_y = true;
        settle_plot(&ctx, &mut views, &output, &store);
        let data = views.plot_reader.data().unwrap();
        assert_eq!(data.quality, PlotQuality::SampledPreview);
        assert!(data.scanned <= 65_536 && data.scanned < count);
        let envelopes = &data.envelopes["value"];
        assert!(!envelopes.is_empty());
        assert!(envelopes
            .iter()
            .all(|band| band.min.value < -0.99 && band.max.value > 0.99));
        let rendered = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200., 700.),
                )),
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    views.plot(ui, &output, store.view());
                });
            },
        );
        fn count_shapes(shape: &egui::Shape, bands: &mut usize, dots: &mut usize) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        count_shapes(shape, bands, dots);
                    }
                }
                egui::Shape::Path(path)
                    if path.closed
                        && path.points.len() == 4
                        && path.fill != Color32::TRANSPARENT =>
                {
                    *bands += 1
                }
                egui::Shape::Circle(circle) if (circle.radius - 2.5).abs() < 0.01 => *dots += 1,
                _ => (),
            }
        }
        let (mut bands, mut dots) = (0, 0);
        for shape in &rendered.shapes {
            count_shapes(&shape.shape, &mut bands, &mut dots);
        }
        assert!(
            bands >= 64,
            "overview must paint the observed ranges, found {bands} bands"
        );
        assert_eq!(dots, 0, "overview must not render isolated sample markers");
        fn series_color(shape: &egui::Shape, color: Color32) -> bool {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().any(|shape| series_color(shape, color)),
                egui::Shape::Path(path) => {
                    path.stroke.color == egui::epaint::ColorMode::Solid(color) || path.fill == color.gamma_multiply(0.23)
                }
                _ => false,
            }
        }
        assert!(
            rendered
                .shapes
                .iter()
                .any(|shape| series_color(&shape.shape, field_color("samples / value"))),
            "band fill and outline must use the stable series color, rather than neutral gray"
        );
    }

    fn navigation_memory(
        ctx: &egui::Context,
        _views: &StructureViews,
        output: &ParseOutput,
    ) -> egui_plot::PlotMemory {
        let id = egui::Id::new((
            "decoded_structure_plot",
            "overlay",
            output.revision,
        ));
        egui_plot::PlotMemory::load(ctx, id).expect("rendered plot memory")
    }

    fn navigation_button(position: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn navigation_start_drag(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
        from: egui::Pos2,
        to: egui::Pos2,
    ) {
        assert!(navigation_memory(ctx, views, output)
            .transform()
            .frame()
            .contains(from));
        render_plot(
            ctx,
            views,
            output,
            store,
            vec![
                egui::Event::PointerMoved(from),
                navigation_button(from, true),
            ],
            egui::Modifiers::NONE,
        );
        render_plot(
            ctx,
            views,
            output,
            store,
            vec![egui::Event::PointerMoved(to)],
            egui::Modifiers::NONE,
        );
        assert!(
            views.plot_range_zoom.is_some(),
            "secondary drag must start the range gesture"
        );
    }

    fn navigation_end_drag(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
        position: egui::Pos2,
    ) {
        let selection = render_plot(
            ctx,
            views,
            output,
            store,
            vec![
                egui::Event::PointerMoved(position),
                navigation_button(position, false),
            ],
            egui::Modifiers::NONE,
        );
        assert!(
            selection.is_none(),
            "range zoom must not edit the byte selection"
        );
        settle_plot(ctx, views, output, store);
        assert!(views.plot_range_zoom.is_none());
        assert!(
            views.plot_zoom_y.is_none(),
            "saved Y bounds must be consumed after zoom"
        );
    }

    fn navigation_assert_y(before: PlotBounds, after: PlotBounds) {
        for (expected, actual) in [
            (before.min()[1], after.min()[1]),
            (before.max()[1], after.max()[1]),
        ] {
            assert!(
                (expected - actual).abs() <= 1e-10 * expected.abs().max(1.),
                "vertical scale changed from {before:?} to {after:?}",
            );
        }
    }

    #[test]
    fn right_drag_range_zoom_preserves_y_for_horizontal_reverse_and_unlocked_drags() {
        // One-pixel vertical motion reproduced the original almost-flat Y box;
        // zero vertical motion must work too, including with Y unlocked.
        for (reverse, vertical_motion, free_y) in [
            (false, 1., false),
            (false, 0., false),
            (true, 1., false),
            (false, 60., true),
        ] {
            let (store, output) = array();
            let ctx = egui::Context::default();
            let mut views = StructureViews::default();
            views.plot_free_y = free_y;
            settle_plot(&ctx, &mut views, &output, &store);
            let memory = navigation_memory(&ctx, &views, &output);
            let before = *memory.bounds();
            let transform = memory.transform();
            let y = transform.frame().center().y;
            let left = egui::pos2(
                transform.position_from_point(&PlotPoint::new(255.5, 0.)).x,
                y,
            );
            let right = egui::pos2(
                transform.position_from_point(&PlotPoint::new(767.5, 0.)).x,
                y,
            );
            let (from, mut to) = if reverse {
                (right, left)
            } else {
                (left, right)
            };
            to.y += vertical_motion;
            navigation_start_drag(&ctx, &mut views, &output, &store, from, to);
            navigation_end_drag(&ctx, &mut views, &output, &store, to);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            navigation_assert_y(before, after);
            assert_eq!(views.plot_reader.data().unwrap().range, 256..768);
            assert_eq!(views.plot_view, Some(256..768));
            assert!(after.width() < before.width());
            assert!(views.last_hex_selection.is_none());
            // A further refresh must retain the axis instead of fitting the new
            // sample subset (whose values occupy only half the original Y range).
            settle_plot(&ctx, &mut views, &output, &store);
            navigation_assert_y(before, *navigation_memory(&ctx, &views, &output).bounds());
        }
    }

    #[test]
    fn vertical_only_right_drag_does_not_change_plot_bounds_or_selection() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        let memory = navigation_memory(&ctx, &views, &output);
        let before = *memory.bounds();
        let from = memory.transform().frame().center();
        let to = from + egui::vec2(2., 80.);
        navigation_start_drag(&ctx, &mut views, &output, &store, from, to);
        navigation_end_drag(&ctx, &mut views, &output, &store, to);
        assert_eq!(*navigation_memory(&ctx, &views, &output).bounds(), before);
        assert_eq!(views.plot_reader.data().unwrap().range, 0..1024);
        assert!(views.last_hex_selection.is_none());
    }

    #[test]
    fn escape_cancels_right_drag_without_committing_on_button_release() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        let memory = navigation_memory(&ctx, &views, &output);
        let before = *memory.bounds();
        let frame = *memory.transform().frame();
        let from = egui::pos2(frame.left() + frame.width() * 0.25, frame.center().y);
        let to = egui::pos2(frame.left() + frame.width() * 0.75, frame.center().y + 1.);
        navigation_start_drag(&ctx, &mut views, &output, &store, from, to);
        render_plot(
            &ctx,
            &mut views,
            &output,
            &store,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            egui::Modifiers::NONE,
        );
        assert!(views.plot_range_zoom.is_none());
        navigation_end_drag(&ctx, &mut views, &output, &store, to);
        assert_eq!(*navigation_memory(&ctx, &views, &output).bounds(), before);
        assert_eq!(views.plot_reader.data().unwrap().range, 0..1024);
        assert!(views.last_hex_selection.is_none());
    }

    #[test]
    fn right_drag_released_outside_plot_clamps_x_and_preserves_y() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        let memory = navigation_memory(&ctx, &views, &output);
        let before = *memory.bounds();
        let transform = memory.transform();
        let frame = *transform.frame();
        let from = egui::pos2(
            transform.position_from_point(&PlotPoint::new(511.5, 0.)).x,
            frame.center().y,
        );
        let inside = egui::pos2(frame.right() - 10., frame.center().y + 1.);
        let outside = egui::pos2(frame.right() + 30., frame.center().y + 80.);
        navigation_start_drag(&ctx, &mut views, &output, &store, from, inside);
        navigation_end_drag(&ctx, &mut views, &output, &store, outside);
        let after = *navigation_memory(&ctx, &views, &output).bounds();
        navigation_assert_y(before, after);
        assert_eq!(views.plot_reader.data().unwrap().range, 512..1024);
        assert_eq!(after.max()[0], 1023.5);
        assert!(views.last_hex_selection.is_none());
    }

    #[test]
    fn ordinary_wheel_zooms_x_with_y_locked_and_both_axes_when_unlocked() {
        for free_y in [false, true] {
            let (store, output) = array();
            let ctx = egui::Context::default();
            let mut views = StructureViews::default();
            views.plot_free_y = free_y;
            settle_plot(&ctx, &mut views, &output, &store);
            let memory = navigation_memory(&ctx, &views, &output);
            let before = *memory.bounds();
            let pointer = memory.transform().frame().center();
            render_plot(
                &ctx,
                &mut views,
                &output,
                &store,
                vec![egui::Event::PointerMoved(pointer)],
                egui::Modifiers::NONE,
            );
            render_plot(
                &ctx,
                &mut views,
                &output,
                &store,
                vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0., 80.), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE }],
                egui::Modifiers::NONE,
            );
            settle_plot(&ctx, &mut views, &output, &store);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            assert!(
                after.width() < before.width() * 0.99,
                "ordinary wheel must zoom horizontally: {before:?} -> {after:?}"
            );
            if free_y {
                assert!(
                    after.height() < before.height() * 0.99,
                    "unlocked wheel should zoom both axes"
                );
            } else {
                navigation_assert_y(before, after);
            }
            assert!(views.last_hex_selection.is_none());
        }
    }
    fn axis_navigation_fixture() -> (egui::Context, ByteStore, ParseOutput, StructureViews) {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.initialize_plot(&output);
        views.plot_view = Some(256..768);
        views.plot_force_view = Some(256..768);
        settle_plot(&ctx, &mut views, &output, &store);
        // Retain a real linked byte/sample selection and table page throughout
        // axis navigation, rather than testing only an empty selection state.
        let selected = click_plot(&ctx, &mut views, &output, &store, 600., 600., false);
        assert_eq!(selected, Some(1200..1202));
        settle(&mut views, store.view());
        settle_plot(&ctx, &mut views, &output, &store);
        assert!(!views.plot_free_y, "Y locking is the default");
        (ctx, store, output, views)
    }

    fn axis_navigation_position(memory: &egui_plot::PlotMemory, y_axis: bool) -> egui::Pos2 {
        let frame = *memory.transform().frame();
        let pointer = if y_axis {
            egui::pos2(frame.left() - 8., frame.center().y)
        } else {
            egui::pos2(frame.center().x, frame.bottom() + 8.)
        };
        assert!(
            !frame.contains(pointer),
            "axis gesture must originate outside the canvas"
        );
        pointer
    }

    fn axis_navigation_assert_x(before: PlotBounds, after: PlotBounds) {
        for (expected, actual) in [
            (before.min()[0], after.min()[0]),
            (before.max()[0], after.max()[0]),
        ] {
            assert!(
                (expected - actual).abs() <= 1e-10 * expected.abs().max(1.),
                "horizontal bounds changed from {before:?} to {after:?}"
            );
        }
    }

    fn axis_navigation_send(
        ctx: &egui::Context,
        views: &mut StructureViews,
        output: &ParseOutput,
        store: &ByteStore,
        position: egui::Pos2,
        event: egui::Event,
        modifiers: egui::Modifiers,
    ) {
        render_plot(
            ctx,
            views,
            output,
            store,
            vec![egui::Event::PointerMoved(position)],
            modifiers,
        );
        let picked = render_plot(ctx, views, output, store, vec![event], modifiers);
        assert!(
            picked.is_none(),
            "wheel gestures must not select another sample"
        );
    }

    #[test]
    fn wheel_over_each_axis_zooms_only_that_axis_even_with_y_locked() {
        for y_axis in [false, true] {
            let (ctx, store, output, mut views) = axis_navigation_fixture();
            let selected = views.last_hex_selection.clone();
            let page = views.desired.as_ref().unwrap().key.clone();
            let memory = navigation_memory(&ctx, &views, &output);
            let before = *memory.bounds();
            let pointer = axis_navigation_position(&memory, y_axis);
            axis_navigation_send(
                &ctx,
                &mut views,
                &output,
                &store,
                pointer,
                egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0., 48.), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE },
                egui::Modifiers::NONE,
            );
            settle_plot(&ctx, &mut views, &output, &store);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            if y_axis {
                axis_navigation_assert_x(before, after);
                assert!(
                    after.height() < before.height() * 0.99,
                    "explicit Y-axis zoom must work while Lock Y is enabled"
                );
            } else {
                navigation_assert_y(before, after);
                assert!(
                    after.width() < before.width() * 0.99,
                    "bottom-axis wheel must zoom X"
                );
            }
            assert!(
                !views.plot_free_y,
                "an explicit Y gesture must not toggle Lock Y"
            );
            assert_eq!(views.last_hex_selection, selected);
            assert_eq!(
                views.desired.as_ref().unwrap().key,
                page,
                "axis navigation must not page the decoded sample table"
            );
        }
    }

    #[test]
    fn shift_wheel_pans_only_hovered_axis_without_changing_scale_or_selection() {
        for y_axis in [false, true] {
            let (ctx, store, output, mut views) = axis_navigation_fixture();
            let selected = views.last_hex_selection.clone();
            let page = views.desired.as_ref().unwrap().key.clone();
            let memory = navigation_memory(&ctx, &views, &output);
            let before = *memory.bounds();
            let pointer = axis_navigation_position(&memory, y_axis);
            let shift = egui::Modifiers {
                shift: true,
                ..Default::default()
            };
            // egui-winit remaps Shift+wheel into the horizontal component.
            axis_navigation_send(
                &ctx,
                &mut views,
                &output,
                &store,
                pointer,
                egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(48., 0.), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE },
                shift,
            );
            let immediately_after = *navigation_memory(&ctx, &views, &output).bounds();
            assert!((immediately_after.width() - before.width()).abs() < 1e-8);
            assert!((immediately_after.height() - before.height()).abs() < 1e-8);
            if y_axis {
                axis_navigation_assert_x(before, immediately_after);
                assert!(
                    immediately_after.min()[1] > before.min()[1],
                    "positive Y scroll pans to higher values"
                );
            } else {
                navigation_assert_y(before, immediately_after);
                assert!(
                    immediately_after.min()[0] < before.min()[0],
                    "positive X scroll pans to earlier samples"
                );
            }
            // Releasing Shift and moving onto the other axis must not turn a
            // smoothed tail of this pan into a zoom or an operation on that axis.
            let other_axis =
                axis_navigation_position(&navigation_memory(&ctx, &views, &output), !y_axis);
            render_plot(
                &ctx,
                &mut views,
                &output,
                &store,
                vec![egui::Event::PointerMoved(other_axis)],
                egui::Modifiers::NONE,
            );
            settle_plot(&ctx, &mut views, &output, &store);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            axis_navigation_assert_x(immediately_after, after);
            navigation_assert_y(immediately_after, after);
            assert_eq!(views.last_hex_selection, selected);
            assert_eq!(views.desired.as_ref().unwrap().key, page);
        }
    }

    #[test]
    fn ctrl_wheel_or_pinch_over_axis_zoom_only_that_axis() {
        for y_axis in [false, true] {
            let (ctx, store, output, mut views) = axis_navigation_fixture();
            let selected = views.last_hex_selection.clone();
            let page = views.desired.as_ref().unwrap().key.clone();
            let memory = navigation_memory(&ctx, &views, &output);
            let before = *memory.bounds();
            let pointer = axis_navigation_position(&memory, y_axis);
            let control = egui::Modifiers {
                ctrl: true,
                ..Default::default()
            };
            axis_navigation_send(
                &ctx,
                &mut views,
                &output,
                &store,
                pointer,
                egui::Event::Zoom(1.4),
                control,
            );
            settle_plot(&ctx, &mut views, &output, &store);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            if y_axis {
                axis_navigation_assert_x(before, after);
                assert!(
                    (after.height() - before.height() / 1.4).abs() < 1e-4,
                    "axis zoom factor must be applied exactly once"
                );
            } else {
                navigation_assert_y(before, after);
                assert!(
                    (after.width() - before.width() / 1.4).abs() < 1e-4,
                    "axis zoom factor must be applied exactly once"
                );
            }
            assert_eq!(views.last_hex_selection, selected);
            assert_eq!(views.desired.as_ref().unwrap().key, page);
        }
    }

    #[test]
    fn plot_axis_corner_does_not_zoom_or_pan_either_axis() {
        let (ctx, store, output, mut views) = axis_navigation_fixture();
        let selected = views.last_hex_selection.clone();
        let page = views.desired.as_ref().unwrap().key.clone();
        let memory = navigation_memory(&ctx, &views, &output);
        let before = *memory.bounds();
        let frame = *memory.transform().frame();
        // This corner belongs to neither the bottom X strip nor the left Y strip.
        let corner = egui::pos2(frame.left() - 8., frame.bottom() + 8.);
        for (event, modifiers) in [
            (
                egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0., 48.), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE },
                egui::Modifiers::NONE,
            ),
            (
                egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(48., 0.), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE },
                egui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            ),
            (
                egui::Event::Zoom(1.4),
                egui::Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            ),
        ] {
            axis_navigation_send(&ctx, &mut views, &output, &store, corner, event, modifiers);
            settle_plot(&ctx, &mut views, &output, &store);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            axis_navigation_assert_x(before, after);
            navigation_assert_y(before, after);
            assert_eq!(views.last_hex_selection, selected);
            assert_eq!(views.desired.as_ref().unwrap().key, page);
        }
    }

    #[test]
    fn completed_axis_wheel_does_not_continue_on_new_hover_target() {
        for y_axis in [false, true] {
            let (ctx, store, output, mut views) = axis_navigation_fixture();
            let memory = navigation_memory(&ctx, &views, &output);
            let pointer = axis_navigation_position(&memory, y_axis);
            axis_navigation_send(
                &ctx,
                &mut views,
                &output,
                &store,
                pointer,
                egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0., 80.), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE },
                egui::Modifiers::NONE,
            );
            let immediately_after = *navigation_memory(&ctx, &views, &output).bounds();
            let other_axis =
                axis_navigation_position(&navigation_memory(&ctx, &views, &output), !y_axis);
            render_plot(
                &ctx,
                &mut views,
                &output,
                &store,
                vec![egui::Event::PointerMoved(other_axis)],
                egui::Modifiers::NONE,
            );
            settle_plot(&ctx, &mut views, &output, &store);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            axis_navigation_assert_x(immediately_after, after);
            navigation_assert_y(immediately_after, after);
        }
    }
}
