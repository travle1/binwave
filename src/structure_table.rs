//! The Table panel: one decoded collection (or one item) at a time.
//!
//! A collection picker lists every collection in decoder order; a breadcrumb walks
//! nested collections; singletons show a vertical field list. Large lazy collections
//! scroll virtually through a window of rows, like the Hex view, and read their pages
//! on a background thread with their own cache so Structure's paging is unaffected.
use super::*;
use crate::hex_editor::{recenter_scroll_window, window_start, WINDOW_ROWS};
use egui_extras::{Column, TableBuilder};
use std::collections::VecDeque;

/// Pages of rows kept per collection; each page is at most 128 rows or 8 MiB.
const PAGE_CACHE: usize = 24;

#[derive(Default)]
pub(super) struct TableState {
    rows: TableRows,
    memory: HashMap<String, TableMemory>,
    /// Collections and items opened from rows or the tree that are not in the
    /// decoded output itself (they live inside lazily read pages).
    pub(super) known: HashMap<String, Node>,
    /// The target drawn last frame; a change restores that target's memory.
    shown: Option<String>,
}

/// What the Table remembers per collection or item.
#[derive(Default)]
struct TableMemory {
    window_start: usize,
    scroll: f32,
    pending_scroll: Option<f32>,
    go_to: String,
    /// Selected row, restored when returning to this collection from the Table.
    selected_row: Option<usize>,
    pending_select: Option<usize>,
    columns: Option<Vec<TableColumn>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum TableColumn {
    /// A leaf field shared by every row, by relative path.
    Field(String),
    /// A field whose type differs between rows (a variant payload): one summary column.
    Payload(String),
}

#[derive(Default)]
struct TableRows {
    key: Option<(String, u64)>,
    page_size: usize,
    pages: HashMap<usize, Arc<Vec<Node>>>,
    order: VecDeque<usize>,
    pending: Option<PendingRows>,
    error: Option<String>,
}

struct PendingRows {
    start: usize,
    rx: mpsc::Receiver<Result<Vec<Node>, String>>,
    cancel: Arc<AtomicBool>,
}

impl TableRows {
    fn target(&mut self, path: &str, revision: u64, array: &LazyArray) {
        let key = (path.to_owned(), revision);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Relaxed);
        }
        *self = Self { key: Some(key), page_size: inspector_page_size(array), ..Self::default() };
    }

    fn poll(&mut self) {
        let Some(pending) = &self.pending else { return };
        let result = match pending.rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("Table reader stopped unexpectedly.".into()),
        };
        let start = pending.start;
        self.pending = None;
        match result {
            Ok(rows) => {
                self.pages.insert(start, Arc::new(rows));
                self.order.push_back(start);
                while self.order.len() > PAGE_CACHE {
                    if let Some(old) = self.order.pop_front() {
                        self.pages.remove(&old);
                    }
                }
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn row(&self, index: usize) -> Option<&Node> {
        let size = self.page_size.max(1);
        let start = index / size * size;
        self.pages.get(&start)?.get(index - start)
    }

    /// Read the page holding `index` unless it is loaded or already being read.
    fn request(&mut self, array: &LazyArray, index: usize, source: &ByteView, ctx: &egui::Context) {
        let size = self.page_size.max(1);
        let start = index / size * size;
        if self.pages.contains_key(&start) || self.pending.as_ref().is_some_and(|p| p.start == start) {
            return;
        }
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Relaxed);
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (array, source, worker_cancel, ctx) = (array.clone(), source.clone(), cancel.clone(), ctx.clone());
        let count = size.min(array.count.saturating_sub(start));
        std::thread::spawn(move || {
            let result = array.page(&source, &worker_cancel, start, count).map_err(|error| format!("{error:#}"));
            if !worker_cancel.load(Ordering::Relaxed) {
                let _ = tx.send(result);
                ctx.request_repaint();
            }
        });
        self.pending = Some(PendingRows { start, rx, cancel });
    }
}

/// One entry of the collection picker.
pub(super) struct Chip {
    pub(super) label: String,
    target: String,
    hover: String,
    /// Channels discovered inside a repeated item (for example one per sensor),
    /// shown as one chip and chosen in the breadcrumb.
    pub(super) channels: Vec<Channel>,
}

pub(super) struct Channel {
    pub(super) label: String,
    /// Member collections; a channel with several has an `[n]` selector.
    pub(super) members: Vec<(Option<usize>, String)>,
}

impl Chip {
    fn contains(&self, target: &str) -> bool {
        self.channels.iter().any(|channel| channel.members.iter().any(|(_, path)| path == target))
    }
}

impl StructureViews {
    pub fn table(
        &mut self,
        ui: &mut Ui,
        output: &ParseOutput,
        program: Option<&Program>,
        source: ByteView,
    ) -> Option<Range<usize>> {
        self.tick(ui.ctx(), source.clone());
        self.discover_flattened_collections(output);
        if self.stale(output) {
            ui.label("Bytes changed. Press ▶ Run (F5) to refresh the table.");
            return None;
        }
        self.start_nested_discovery(output, source.clone(), ui.ctx());
        if let Some(error) = &self.discovery_error {
            ui.colored_label(crate::theme::DARK.color.status.warning, error);
        }
        self.initialize_target(output);
        if self.table.shown.as_deref() != Some(self.table_target.as_str()) {
            self.table.shown = Some(self.table_target.clone());
        }
        let chips = self.table_chips(output);
        let mut open = None;
        self.picker(ui, &chips, &mut open);
        self.breadcrumb(ui, output, &chips, &mut open);
        let mut chosen = None;
        match self.table_target_node(output) {
            Some(node) if is_collection(&node) => {
                self.collection_table(ui, output, program, &node, &source, &mut chosen, &mut open);
            }
            Some(node) => self.item_table(ui, program, std::slice::from_ref(&node), Some(&node), &mut chosen, &mut open),
            None if self.table_target.is_empty() => {
                let fields: Vec<Node> = output.nodes.iter().filter(|node| !is_collection(node)).cloned().collect();
                self.item_table(ui, program, &fields, None, &mut chosen, &mut open);
            }
            None => {
                ui.weak("This item is not loaded. Choose its collection above to reach it.");
            }
        }
        if let Some((target, node)) = open {
            self.open_table_target(target, node, true);
        }
        let selection = chosen.map(|(row, field, extend)| self.select_table_row(output, &row, &field, extend));
        selection.or_else(|| self.navigation_selection.take())
    }

    /// Switch the Table to another collection or item, remembering where the old one was.
    pub(super) fn open_table_target(&mut self, target: String, node: Option<Node>, restore_selection: bool) {
        if let Some(node) = node {
            self.table.known.insert(target.clone(), node);
        }
        // An explicit choice, even of the current target, is not replaced by the default.
        self.target_initialized = true;
        if target == self.table_target {
            return;
        }
        self.table_target = target;
        self.table_anchor = None;
        self.reveal_selected_row = false;
        let memory = self.table.memory.entry(self.table_target.clone()).or_default();
        memory.pending_scroll = Some(memory.scroll);
        if restore_selection {
            memory.pending_select = memory.selected_row;
        }
        // A combined sensor collection drives the Signal plot, as before.
        if self.flattened_collections.iter()
            .any(|node| node.path == self.table_target && sensor_group_key(node).is_some()) {
            self.plot_sources.clear();
            self.plot_sources.insert(self.table_target.clone());
            self.plot_sources_initialized = true;
            self.plot_target = self.table_target.clone();
            self.plot_catalog_target.clear();
            self.plot_view = None;
            self.plot_force_view = None;
            self.plot_fit_y = true;
            self.plot_reader.invalidate();
            self.extra_plot_readers.clear();
            self.group_plot_readers.clear();
            self.group_plan_cache = None;
        }
    }

    fn table_target_node(&self, output: &ParseOutput) -> Option<Node> {
        if self.table_target.is_empty() {
            return None;
        }
        self.collection_node(output, &self.table_target).cloned()
            .or_else(|| self.table.known.get(&self.table_target).cloned())
    }

    /// Collections in decoder order; sensor groups collapse into one chip.
    pub(super) fn table_chips(&self, output: &ParseOutput) -> Vec<Chip> {
        let mut chips = Vec::new();
        if output.nodes.iter().any(|node| !is_collection(node)) {
            chips.push(Chip { label: "File header".into(), target: String::new(),
                hover: "Top-level fields".into(), channels: Vec::new() });
        }
        let mut collections = Vec::new();
        collect_collections(&output.nodes, &mut collections);
        collections.extend(self.flattened_collections.iter());
        // Channel keys look like `container.CHANNEL[i].rest` or `container.CHANNEL.sensors`:
        // dropping the last two segments gives the container they all live in.
        let container_of = |key: &str| key.rsplitn(3, '.').nth(2).map(str::to_owned);
        let keyed: Vec<(&Node, Option<(String, usize)>)> = collections.iter()
            .map(|node| (*node, sensor_group_key(node))).collect();
        let containers: HashSet<String> = keyed.iter()
            .filter_map(|(_, key)| key.as_ref().and_then(|(key, _)| container_of(key))).collect();
        let channel_parents: HashSet<String> = keyed.iter()
            .filter_map(|(_, key)| key.as_ref().and_then(|(key, _)| key.rsplit_once('.').map(|(parent, _)| parent.to_owned())))
            .collect();
        let mut chip_of_container: HashMap<String, usize> = HashMap::new();
        for (node, key) in keyed {
            // A discovered collection directly inside a channel container is one more channel.
            let (container, channel, index) = match &key {
                Some((key, index)) => match container_of(key) {
                    Some(container) => (container, key.rsplit('.').nth(1).unwrap_or(key).to_owned(), Some(*index)),
                    None => continue,
                },
                None => match node.path.rsplit_once('.').filter(|(parent, _)| containers.contains(*parent)) {
                    Some((parent, name)) => (parent.to_owned(), name.to_owned(), None),
                    None => {
                        if channel_parents.contains(&node.path) {
                            continue;
                        }
                        let name = node.name.split_once(" across ").map_or(node.name.as_str(), |(name, _)| name);
                        chips.push(Chip {
                            label: format!("{} ({})", humanize(name.rsplit('.').next().unwrap_or(name)),
                                crate::format::count(collection_count(node))),
                            target: node.path.clone(),
                            hover: node.path.clone(),
                            channels: Vec::new(),
                        });
                        continue;
                    }
                },
            };
            let chip = *chip_of_container.entry(container.clone()).or_insert_with(|| {
                chips.push(Chip { label: humanize(container.rsplit('.').next().unwrap_or(&container)),
                    target: node.path.clone(), hover: container.clone(), channels: Vec::new() });
                chips.len() - 1
            });
            let channels = &mut chips[chip].channels;
            match channels.iter_mut().find(|existing| existing.label == channel) {
                Some(existing) => existing.members.push((index, node.path.clone())),
                None => channels.push(Channel { label: channel, members: vec![(index, node.path.clone())] }),
            }
        }
        for &chip in chip_of_container.values() {
            let chip = &mut chips[chip];
            for channel in &mut chip.channels {
                channel.members.sort_by_key(|(index, _)| *index);
            }
            chip.target = chip.channels[0].members[0].1.clone();
            chip.label = format!("{} ({})", chip.label,
                crate::format::plural(chip.channels.len() as u64, "channel", "channels"));
        }
        // Equal names in different places keep the chips apart.
        let labels: Vec<String> = chips.iter().map(|chip| chip.label.clone()).collect();
        for chip in &mut chips {
            if labels.iter().filter(|label| **label == chip.label).count() > 1 {
                if let Some((parent, _)) = chip.hover.rsplit_once('.') {
                    chip.label = format!("{} · {parent}", chip.label);
                }
            }
        }
        chips
    }

    fn picker(&mut self, ui: &mut Ui, chips: &[Chip], open: &mut Option<(String, Option<Node>)>) {
        let target = self.table_target.clone();
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(egui_phosphor::regular::INFO)
                    .font(egui::FontId::new(14., egui::FontFamily::Name("phosphor".into())))
                    .color(crate::theme::DARK.color.text.muted))
                    .on_hover_text("Click a row or value to select its bytes in Hex and Structure.\n\
                        Shift-click selects a range of whole rows.\n\
                        Click a Payload or collection cell to open it here.");
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true), |ui| {
                    for chip in chips {
                        let active = chip.target == target || chip.contains(&target)
                            || (!chip.target.is_empty() && (target.starts_with(&format!("{}[", chip.target))
                                || target.starts_with(&format!("{}.", chip.target))));
                        if ui.selectable_label(active, &chip.label).on_hover_text(&chip.hover).clicked() && !active {
                            *open = Some((chip.target.clone(), None));
                        }
                    }
                });
            });
        });
    }

    /// `Sections › [2] › records`: every segment opens that collection or item.
    fn breadcrumb(&mut self, ui: &mut Ui, output: &ParseOutput, chips: &[Chip],
        open: &mut Option<(String, Option<Node>)>) {
        let target = self.table_target.clone();
        let group = chips.iter().find(|chip| chip.contains(&target));
        let segments = breadcrumb_segments(&target);
        if segments.len() < 2 && group.is_none() {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            if let Some(group) = group {
                // Container › channel › [n]
                let channel = group.channels.iter()
                    .find(|channel| channel.members.iter().any(|(_, path)| *path == target)).unwrap();
                ui.weak(group.label.split(" (").next().unwrap_or(&group.label)).on_hover_text(&group.hover);
                ui.weak("›");
                egui::ComboBox::from_id_salt("table_channel")
                    .selected_text(&channel.label)
                    .height(ui.available_height().max(240.))
                    .show_ui(ui, |ui| {
                        for option in &group.channels {
                            let label = if option.members.len() > 1 {
                                format!("{} ({})", option.label, option.members.len())
                            } else {
                                option.label.clone()
                            };
                            if ui.selectable_label(option.label == channel.label, label).clicked() {
                                *open = Some((option.members[0].1.clone(), None));
                            }
                        }
                    });
                if channel.members.len() > 1 {
                    ui.weak("›");
                    let index = channel.members.iter().find(|(_, path)| *path == target).and_then(|(index, _)| *index);
                    egui::ComboBox::from_id_salt("table_channel_member")
                        .selected_text(index.map_or_else(String::new, |index| format!("[{index}]")))
                        .show_ui(ui, |ui| {
                            for (member, path) in &channel.members {
                                let text = member.map_or_else(String::new, |member| format!("[{member}]"));
                                if ui.selectable_label(*path == target, text).clicked() {
                                    *open = Some((path.clone(), None));
                                }
                            }
                        });
                }
                return;
            }
            let last = segments.len() - 1;
            for (index, (label, path)) in segments.into_iter().enumerate() {
                if index > 0 {
                    ui.weak("›");
                }
                if index == last {
                    ui.strong(label);
                } else if ui.link(label).on_hover_text(&path).clicked() {
                    let node = self.collection_node(output, &path).cloned()
                        .or_else(|| self.table.known.get(&path).cloned())
                        .or_else(|| self.loaded_item(&path));
                    *open = Some((path, node));
                }
            }
        });
    }

    /// An item of the collection currently read into the Table, if its page is loaded.
    fn loaded_item(&self, path: &str) -> Option<Node> {
        let (collection, index) = path.strip_suffix(']')?.rsplit_once('[')?;
        let index: usize = index.parse().ok()?;
        let key = self.table.rows.key.as_ref()?;
        (key.0 == collection).then(|| self.table.rows.row(index).cloned()).flatten()
    }

    /// Vertical Field | Value | Offset | Size list for one item or the file header.
    /// `item` is the collection row these fields belong to; the file header has none.
    fn item_table(&mut self, ui: &mut Ui, program: Option<&Program>, fields: &[Node],
        item: Option<&Node>, chosen: &mut Option<(Node, Node, bool)>, open: &mut Option<(String, Option<Node>)>) {
        let mut lines = Vec::new();
        for field in fields {
            flatten_item_fields(field, 0, None, &mut lines);
        }
        if lines.is_empty() {
            ui.weak("No fields decoded yet.");
            return;
        }
        let selected = self.selected.clone();
        let selected_bytes = self.last_hex_selection.clone();
        let mono = egui::TextStyle::Monospace.resolve(ui.style());
        TableBuilder::new(ui)
            .id_salt(("table_item", &self.table_target))
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(crate::theme::DARK.size.table_item_column).at_least(80.).clip(true))
            .column(Column::initial(crate::theme::DARK.size.table_item_column).at_least(60.).clip(true))
            .column(Column::initial(crate::theme::DARK.size.table_offset_column).at_least(60.).clip(true))
            .column(Column::remainder().at_least(80.).clip(true))
            .header(crate::theme::DARK.size.table_row, |mut header| {
                for name in ["Field", "Value", "Offset", "Size"] {
                    header.col(|ui| { ui.strong(name); });
                }
            })
            .body(|body| {
                body.rows(crate::theme::DARK.size.table_row, lines.len(), |mut table_row| {
                    let (depth, node, parent_kind) = &lines[table_row.index()];
                    table_row.set_selected(selected.as_deref() == Some(node.path.as_str())
                        || selected_bytes.as_ref().is_some_and(|range| *range == node.range));
                    let (_, name) = table_row.col(|ui| {
                        ui.add_space(*depth as f32 * crate::theme::DARK.spacing.tree_indent);
                        ui.add(egui::Label::new(&node.name).truncate().selectable(false));
                    });
                    let value = tree_column_value(node, parent_kind.as_deref(), program);
                    let (_, value_cell) = table_row.col(|ui| {
                        let text = egui::RichText::new(&value).font(mono.clone());
                        ui.add(egui::Label::new(text).truncate().selectable(false));
                    });
                    let (_, offset) = table_row.col(|ui| numeric_cell(ui, &crate::format::offset(node.range.start), &mono));
                    let size = node.range.end.saturating_sub(node.range.start);
                    let (_, size_cell) = table_row.col(|ui| numeric_cell(ui, &crate::format::size(size), &mono));
                    let hover = format!("{}\n{}\n{}", node.path, node.kind,
                        crate::format::byte_range(node.range.start, node.range.end));
                    let response = name.union(value_cell).union(offset).union(size_cell).on_hover_text(hover);
                    if response.clicked() {
                        if is_collection(node) {
                            *open = Some((node.path.clone(), Some(node.clone())));
                        } else {
                            *chosen = Some((item.unwrap_or(node).clone(), node.clone(), false));
                        }
                    }
                });
            });
    }

    #[allow(clippy::too_many_arguments)]
    fn collection_table(&mut self, ui: &mut Ui, output: &ParseOutput, program: Option<&Program>, node: &Node,
        source: &ByteView, chosen: &mut Option<(Node, Node, bool)>, open: &mut Option<(String, Option<Node>)>) {
        let count = collection_count(node);
        let path = node.path.clone();
        if let Some(array) = &node.lazy {
            self.table.rows.target(&path, output.revision, array);
            self.table.rows.poll();
        }
        let samples = self.samples_path(output).as_deref() == Some(path.as_str());
        // A Hex or Structure selection inside this collection scrolls to its row.
        if self.reveal_selected_row {
            let index = self.last_hex_selection.as_ref().and_then(|bytes| {
                if node.lazy.is_some() {
                    sample_indices_for_bytes(node, bytes).map(|rows| rows.start)
                } else {
                    node.children.iter().position(|row| ranges_overlap(bytes, &row.range))
                }
            });
            if let Some(index) = index {
                self.scroll_table_to(&path, index, count, (row_pitch(ui), ui.available_height()), false);
            }
            self.reveal_selected_row = false;
        }
        let mut go_to = None;
        ui.horizontal(|ui| {
            let memory = self.table.memory.entry(path.clone()).or_default();
            ui.weak(format!("{} {}", crate::format::count(count), row_noun(samples, count)));
            ui.add_space(12.);
            ui.label("Go to #");
            let entry = ui.add(egui::TextEdit::singleline(&mut memory.go_to).desired_width(96.)
                .hint_text(format!("0–{}", crate::format::count(count.saturating_sub(1)))));
            if entry.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                let digits: String = memory.go_to.chars().filter(char::is_ascii_digit).collect();
                go_to = digits.parse::<usize>().ok().filter(|index| *index < count);
            }
            if let Some(error) = &self.table.rows.error {
                ui.colored_label(crate::theme::DARK.color.status.warning, error);
            }
        });
        if let Some(index) = go_to {
            self.scroll_table_to(&path, index, count, (row_pitch(ui), ui.available_height()), true);
        }
        if self.table.memory.get(&path).is_none_or(|memory| memory.columns.is_none()) {
            // Any loaded rows will do: a selection can open the Table far from row 0.
            let sample: Vec<Node> = if node.lazy.is_some() {
                self.table.rows.pages.values().flat_map(|rows| rows.iter())
                    .chain(self.page_for(&path, output.revision)
                        .and_then(|page| page.result.as_ref().ok()).into_iter().flatten())
                    .take(64).cloned().collect()
            } else {
                node.children.iter().take(256).cloned().collect()
            };
            if !sample.is_empty() {
                self.table.memory.entry(path.clone()).or_default().columns = Some(table_layout(&sample));
            }
        }
        let memory = self.table.memory.entry(path.clone()).or_default();
        let columns = memory.columns.clone().unwrap_or_default();
        let window = memory.window_start.min(count.saturating_sub(1));
        let window_rows = WINDOW_ROWS.min(count.saturating_sub(window));
        let pending_scroll = memory.pending_scroll.take();
        let pending_select = memory.pending_select;
        let selected = self.selected.clone();
        let selected_bytes = self.last_hex_selection.clone();
        let mono = egui::TextStyle::Monospace.resolve(ui.style());
        let mut missing: Option<usize> = None;
        let mut selected_now = None;
        let rows = &self.table.rows;
        let page_size = rows.page_size.max(1);
        // Rows Structure has already read are used as they are.
        let tree_row = |index: usize| {
            let start = index / page_size * page_size;
            self.page_for_key(&PageKey { path: path.clone(), start, revision: output.revision })
                .and_then(|page| page.result.as_ref().ok())
                .and_then(|rows| rows.get(index - start))
        };
        // Many fields scroll sideways; TableBuilder itself only scrolls vertically.
        let output_scroll = egui::ScrollArea::horizontal()
            .id_salt(("table_columns", &path))
            .auto_shrink([false, false])
            .show(ui, |ui| {
            let mut builder = TableBuilder::new(ui)
                .id_salt(("table_rows", &path))
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .auto_shrink([false, false])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(crate::theme::DARK.size.table_index_column).at_least(40.).clip(true))
                .column(Column::initial(crate::theme::DARK.size.table_offset_column).at_least(60.).clip(true))
                .column(Column::initial(crate::theme::DARK.size.table_size_column).at_least(56.).clip(true));
            for _ in &columns {
                builder = builder.column(Column::initial(crate::theme::DARK.size.table_field_column).at_least(40.).clip(true));
            }
            if let Some(offset) = pending_scroll {
                builder = builder.vertical_scroll_offset(offset);
            }
            builder
                .header(crate::theme::DARK.size.table_row, |mut header| {
                    header.col(|ui| { ui.strong("#"); });
                    header.col(|ui| { ui.strong("Offset"); });
                    header.col(|ui| { ui.strong("Size"); });
                    for column in &columns {
                        let (name, hover) = match column {
                            TableColumn::Field(name) if name.is_empty() => ("Value", "Value".to_owned()),
                            TableColumn::Field(name) => (name.as_str(), name.clone()),
                            TableColumn::Payload(name) => (name.as_str(), format!("{name}\nSummary of a field whose type differs between rows")),
                        };
                        header.col(|ui| { ui.add(egui::Label::new(egui::RichText::new(name).strong()).truncate().selectable(false)).on_hover_text(hover); });
                    }
                })
                .body(|body| {
                    body.rows(crate::theme::DARK.size.table_row, window_rows, |mut table_row| {
                        let index = window + table_row.index();
                        let row = if node.lazy.is_some() { rows.row(index).or_else(|| tree_row(index)) } else { node.children.get(index) };
                        let Some(row) = row else {
                            missing = Some(missing.map_or(index, |first| first.min(index)));
                            table_row.col(|ui| numeric_cell(ui, &crate::format::count(index), &mono));
                            table_row.col(|ui| { ui.weak("…"); });
                            return;
                        };
                        if pending_select == Some(index) {
                            *chosen = Some((row.clone(), row.clone(), false));
                        }
                        let row_selected = selected.as_deref().is_some_and(|path| contains_path(&row.path, path))
                            || selected_bytes.as_ref().is_some_and(|range| ranges_overlap(range, &row.range));
                        if row_selected {
                            selected_now = Some(index);
                        }
                        table_row.set_selected(row_selected);
                        let row_hover = format!("{}\n{}", row.path, crate::format::byte_range(row.range.start, row.range.end));
                        let size = row.range.end.saturating_sub(row.range.start);
                        let (_, a) = table_row.col(|ui| numeric_cell(ui, &crate::format::count(index), &mono));
                        let (_, b) = table_row.col(|ui| numeric_cell(ui, &crate::format::offset(row.range.start), &mono));
                        let (_, c) = table_row.col(|ui| numeric_cell(ui, &crate::format::size(size), &mono));
                        let metadata = a.union(b).union(c).on_hover_text(row_hover);
                        let mut cell_clicked = false;
                        if metadata.clicked() {
                            cell_clicked = true;
                            *chosen = Some((row.clone(), row.clone(), metadata.ctx.input(|i| i.modifiers.shift)));
                        }
                        for column in &columns {
                            let (field, text, numeric) = match column {
                                TableColumn::Field(name) => {
                                    let Some(field) = field_at(row, name) else {
                                        table_row.col(|ui| { ui.weak("—"); });
                                        continue;
                                    };
                                    let parent = name.rsplit_once('.').and_then(|(parent, _)| field_at(row, parent)).unwrap_or(row);
                                    let text = tree_enum_meaning(field, Some(parent.kind.as_str()), program)
                                        .map_or_else(|| display(field, 180), |meaning| format!("{} · {meaning}", display(field, 180)));
                                    (field, text, field.value.as_ref().and_then(Value::numeric).is_some())
                                }
                                TableColumn::Payload(name) => {
                                    let Some(field) = field_at(row, name) else {
                                        table_row.col(|ui| { ui.weak("—"); });
                                        continue;
                                    };
                                    (field, payload_summary(field).unwrap_or_else(|| field.kind.clone()), false)
                                }
                            };
                            let (_, cell) = table_row.col(|ui| {
                                if numeric {
                                    numeric_cell(ui, &text, &mono);
                                } else {
                                    let text = egui::RichText::new(&text).font(mono.clone());
                                    ui.add(egui::Label::new(text).truncate().selectable(false));
                                }
                            });
                            let cell = cell.on_hover_text(format!("{}\n{} · {}\n{}", field.path, field.kind,
                                crate::format::byte_range(field.range.start, field.range.end), display(field, 600)));
                            if cell.clicked() {
                                cell_clicked = true;
                                if let Some(collection) = first_collection(field) {
                                    *open = Some((collection.path.clone(), Some(collection.clone())));
                                } else {
                                    *chosen = Some((row.clone(), field.clone(), cell.ctx.input(|i| i.modifiers.shift)));
                                }
                            }
                        }
                        // Anywhere else on the row, including gaps between cells, selects the whole row.
                        let whole_row = table_row.response();
                        if whole_row.clicked() && !cell_clicked {
                            *chosen = Some((row.clone(), row.clone(), whole_row.ctx.input(|i| i.modifiers.shift)));
                        }
                    });
                })
            })
            .inner;
        let memory = self.table.memory.entry(path.clone()).or_default();
        if pending_select.is_some() && chosen.is_some() {
            memory.pending_select = None;
        }
        if let Some(index) = selected_now {
            memory.selected_row = Some(index);
        }
        memory.scroll = output_scroll.state.offset.y;
        if !ui.input(|input| input.pointer.primary_down()) {
            if let Some((start, offset)) = recenter_scroll_window(memory.window_start, count, memory.scroll, row_pitch(ui)) {
                memory.window_start = start;
                memory.pending_scroll = Some(offset);
                ui.ctx().request_repaint();
            }
        }
        if let (Some(array), Some(index)) = (&node.lazy, missing) {
            self.table.rows.request(array, index, source, ui.ctx());
        }
        if node.lazy.is_some() && self.table.rows.pending.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Scroll so `index` sits a third of the way down; `select` also selects it once loaded.
    fn scroll_table_to(&mut self, path: &str, index: usize, count: usize, (pitch, height): (f32, f32), select: bool) {
        let memory = self.table.memory.entry(path.to_owned()).or_default();
        memory.window_start = window_start(index, count);
        memory.pending_scroll = Some(((index - memory.window_start) as f32 * pitch - height * 0.35).max(0.));
        if select {
            memory.pending_select = Some(index);
        }
    }

    /// Select a row or cell and follow it in Structure: expand its ancestors and read
    /// the Structure page that contains it.
    fn select_table_row(&mut self, output: &ParseOutput, row: &Node, field: &Node, extend: bool) -> Range<usize> {
        let range = self.select_table_node(output, row, field, extend);
        expand_ancestors(&row.path, &mut self.expanded);
        if let Some((collection, index)) = row.path.strip_suffix(']').and_then(|path| path.rsplit_once('[')) {
            let node = self.collection_node(output, collection).cloned()
                .or_else(|| self.table.known.get(collection).cloned());
            if let (Some(node), Ok(index)) = (node, index.parse::<usize>()) {
                if node.lazy.is_some() {
                    self.queue_page(&node, index, output.revision);
                }
            }
        }
        self.reveal_tree = Some(field.path.clone());
        let memory = self.table.memory.entry(self.table_target.clone()).or_default();
        memory.pending_select = None;
        range
    }
}

/// `sections[2].payload.records` → `Sections › [2] › records`. Plain struct fields between
/// collections (`payload`) are skipped; each segment carries the path it opens.
fn breadcrumb_segments(path: &str) -> Vec<(String, String)> {
    let mut segments = Vec::new();
    let mut tokens = Vec::new();
    let mut start = 0;
    for (index, ch) in path.char_indices() {
        if ch == '.' || ch == '[' {
            if index > start {
                tokens.push((start, index));
            }
            start = if ch == '.' { index + 1 } else { index };
        }
    }
    if start < path.len() {
        tokens.push((start, path.len()));
    }
    for (position, &(from, to)) in tokens.iter().enumerate() {
        let text = &path[from..to];
        let is_index = text.starts_with('[');
        let next_is_index = tokens.get(position + 1).is_some_and(|&(next, _)| path[next..].starts_with('['));
        let last = position + 1 == tokens.len();
        if is_index || next_is_index || last {
            let label = if segments.is_empty() && !is_index { humanize(text) } else { text.to_owned() };
            segments.push((label, path[..to].to_owned()));
        }
    }
    segments
}

/// Fields of one item with their depth; collections become one openable row.
fn flatten_item_fields(node: &Node, depth: usize, parent_kind: Option<String>, lines: &mut Vec<(usize, Node, Option<String>)>) {
    lines.push((depth, node.clone(), parent_kind));
    if !is_collection(node) {
        for child in &node.children {
            flatten_item_fields(child, depth + 1, Some(node.kind.clone()), lines);
        }
    }
}

/// Shared leaf columns, plus one Payload column per field whose type differs between rows.
pub(super) fn table_layout(rows: &[Node]) -> Vec<TableColumn> {
    let mut names: Vec<String> = Vec::new();
    for row in rows {
        for child in &row.children {
            if !names.contains(&child.name) {
                names.push(child.name.clone());
            }
        }
    }
    if rows.iter().all(|row| row.children.is_empty()) {
        return vec![TableColumn::Field(String::new())];
    }
    let mut columns = Vec::new();
    for name in names {
        let fields: Vec<&Node> = rows.iter().filter_map(|row| row.children.iter().find(|child| child.name == name)).collect();
        let first_kind = &fields[0].kind;
        let variant = fields.len() < rows.len() || fields.iter().any(|field| &field.kind != first_kind);
        if variant {
            columns.push(TableColumn::Payload(name));
            continue;
        }
        let mut leaves = Vec::new();
        for field in &fields {
            leaf_columns(field, &name, &mut leaves);
        }
        columns.extend(leaves.into_iter().map(TableColumn::Field));
    }
    columns
}

fn first_collection(node: &Node) -> Option<&Node> {
    if is_collection(node) {
        return Some(node);
    }
    node.children.iter().find_map(first_collection)
}

/// Distance between row tops: TableBuilder adds the vertical item spacing to each row.
fn row_pitch(ui: &Ui) -> f32 {
    crate::theme::DARK.size.table_row + ui.spacing().item_spacing.y
}

fn numeric_cell(ui: &mut Ui, text: &str, font: &egui::FontId) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.add(egui::Label::new(egui::RichText::new(text).font(font.clone())).truncate().selectable(false));
    });
}

fn humanize(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::Original;
    use crate::byte_store::ByteStore;

    const RECORDS: usize = 200_000;

    /// A header, two sections whose payload type differs, and a large lazy collection.
    fn fixture() -> (ByteStore, ParseOutput) {
        let yaml = "structures:
  - name: Header
    fields: [{name: magic, type: string, length: 4}, {name: count, type: u16}]
  - name: Entry
    fields: [{name: code, type: u8}]
  - name: Entries
    fields: [{name: entries, type: {structure: Entry}, repeat: 3}]
  - name: Marker
    fields: [{name: marker, type: string, length: 4}]
  - name: Section
    fields:
      - {name: id, type: u8}
      - name: payload
        type: {choose: {ref: id, cases: {'1': {structure: Entries}, '2': {structure: Marker}}}}
actions:
  - read: {name: header, type: {structure: Header}}
  - read: {name: sections, type: {structure: Section}, repeat: 2}
  - read: {name: records, type: u16, repeat: until end}
";
        let mut bytes = b"PWAV".to_vec();
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&[1, 10, 11, 12]);
        bytes.extend_from_slice(b"\x02END!");
        for index in 0..RECORDS {
            bytes.extend_from_slice(&(index as u16).to_le_bytes());
        }
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        (store, output)
    }

    fn frame(ctx: &egui::Context, views: &mut StructureViews, output: &ParseOutput, store: &ByteStore,
        events: Vec<egui::Event>) -> (egui::FullOutput, Option<Range<usize>>) {
        let mut selection = None;
        let full = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000., 500.))),
            events,
            ..Default::default()
        }, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                selection = views.table(ui, output, None, store.view());
            });
        });
        (full, selection)
    }

    /// Render until the Table's own reader is idle; returns the last frame and any selection.
    fn settle(ctx: &egui::Context, views: &mut StructureViews, output: &ParseOutput, store: &ByteStore)
        -> (egui::FullOutput, Option<Range<usize>>) {
        let mut selection = None;
        for _ in 0..400 {
            let (full, picked) = frame(ctx, views, output, store, Vec::new());
            selection = selection.or(picked);
            if views.table.rows.pending.is_none() && views.discovery_rx.is_none() {
                let (full, picked) = frame(ctx, views, output, store, Vec::new());
                return (full, selection.or(picked));
            }
            let _ = full;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("table reader did not finish");
    }

    fn texts(full: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        full.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some((text.galley.text().to_owned(),
                text.galley.rect.translate(text.pos.to_vec2()))),
            _ => None,
        }).collect()
    }

    fn setup() -> (egui::Context, StructureViews, ByteStore, ParseOutput) {
        let (store, output) = fixture();
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut views = StructureViews::default();
        views.tick(&ctx, store.view());
        (ctx, views, store, output)
    }

    #[test]
    fn picker_lists_collections_in_decoder_order_with_counts() {
        let (_ctx, views, _store, output) = setup();
        let labels: Vec<String> = views.table_chips(&output).into_iter().map(|chip| chip.label).collect();
        assert_eq!(labels, ["File header", "Sections (2)", "Entries (3)", "Records (200,000)"]);
    }

    #[test]
    fn variant_payloads_become_one_summary_column() {
        let (_ctx, _views, _store, output) = setup();
        let sections = find_node(&output.nodes, "sections").unwrap();
        assert_eq!(table_layout(&sections.children),
            [TableColumn::Field("id".into()), TableColumn::Payload("payload".into())]);
        let summaries: Vec<_> = sections.children.iter()
            .map(|section| payload_summary(field_at(section, "payload").unwrap()).unwrap()).collect();
        assert_eq!(summaries, ["3 entries", "marker END!"]);
    }

    #[test]
    fn clicking_a_payload_opens_its_nested_collection() {
        let (ctx, mut views, store, output) = setup();
        views.open_table_target("sections".into(), None, false);
        let (full, _) = settle(&ctx, &mut views, &output, &store);
        let cell = texts(&full).into_iter().find(|(text, _)| text == "3 entries").expect("payload summary").1;
        for pressed in [None, Some(true), Some(false)] {
            let event = pressed.map_or(egui::Event::PointerMoved(cell.center()), |pressed| egui::Event::PointerButton {
                pos: cell.center(), button: egui::PointerButton::Primary, pressed, modifiers: Default::default() });
            frame(&ctx, &mut views, &output, &store, vec![event]);
        }
        assert_eq!(views.table_target, "sections[0].payload.entries");
        assert_eq!(breadcrumb_segments(&views.table_target).len(), 3);
    }

    #[test]
    fn singletons_show_a_vertical_field_list() {
        let (ctx, mut views, store, output) = setup();
        views.open_table_target(String::new(), None, false);
        let (full, _) = settle(&ctx, &mut views, &output, &store);
        let labels: Vec<String> = texts(&full).into_iter().map(|(text, _)| text).collect();
        for label in ["Field", "Value", "Offset", "Size", "header", "magic", "\"PWAV\"", "0x00000004"] {
            assert!(labels.iter().any(|text| text == label), "missing {label} in {labels:?}");
        }
    }

    #[test]
    fn go_to_row_scrolls_a_large_collection_and_selects_the_row_there() {
        let (ctx, mut views, store, output) = setup();
        views.open_table_target("records".into(), None, false);
        settle(&ctx, &mut views, &output, &store);
        views.scroll_table_to("records", 150_000, RECORDS, (26., 300.), true);
        let (full, selection) = settle(&ctx, &mut views, &output, &store);
        let labels: Vec<String> = texts(&full).into_iter().map(|(text, _)| text).collect();
        assert!(labels.iter().any(|text| text == "150,000"), "row 150,000 should be visible: {labels:?}");
        let start = 15 + 150_000 * 2;
        assert_eq!(selection, Some(start..start + 2));
        // Selecting a row follows it in Structure.
        assert_eq!(views.selected.as_deref(), Some("records[150000]"));
        assert!(views.expanded.contains("records"));
    }

    #[test]
    fn columns_appear_when_the_table_opens_far_from_the_first_row() {
        let (ctx, mut views, store, output) = setup();
        views.open_table_target("records".into(), None, false);
        views.scroll_table_to("records", 150_000, RECORDS, (26., 300.), false);
        let (full, _) = settle(&ctx, &mut views, &output, &store);
        let labels: Vec<String> = texts(&full).into_iter().map(|(text, _)| text).collect();
        assert!(labels.iter().any(|text| text == "Value"), "field columns missing: {labels:?}");
    }
    #[test]
    fn each_collection_remembers_its_scroll_and_selection() {
        let (ctx, mut views, store, output) = setup();
        views.open_table_target("records".into(), None, false);
        settle(&ctx, &mut views, &output, &store);
        views.scroll_table_to("records", 150_000, RECORDS, (26., 300.), true);
        settle(&ctx, &mut views, &output, &store);
        views.open_table_target("sections".into(), None, true);
        settle(&ctx, &mut views, &output, &store);
        views.open_table_target("records".into(), None, true);
        let (full, selection) = settle(&ctx, &mut views, &output, &store);
        assert!(texts(&full).iter().any(|(text, _)| text == "150,000"), "scroll position was not restored");
        let start = 15 + 150_000 * 2;
        assert_eq!(selection, Some(start..start + 2), "selection was not restored");
    }

    #[test]
    fn breadcrumb_skips_plain_fields_and_links_each_level() {
        assert_eq!(breadcrumb_segments("sections[2].payload.records"), [
            ("Sections".to_owned(), "sections".to_owned()),
            ("[2]".to_owned(), "sections[2]".to_owned()),
            ("records".to_owned(), "sections[2].payload.records".to_owned()),
        ]);
        assert_eq!(breadcrumb_segments("samples").len(), 1);
    }
}
