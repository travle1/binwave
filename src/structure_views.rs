//! Shared selection and bounded, asynchronous pages for decoded structure views.
use crate::plot_trace::{PlacedPoints, placed_points};
#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use crate::structure_selection::{covering, sample_indices_for_bytes, sample_bytes, matching_collection};
use crate::byte_store::ByteView;
use crate::hex_editor::HexHighlight;
use crate::plot_trace::{decoded_channel_borrowed, TraceMeshCache};
use crate::plot_raster::PlotRaster;
use crate::plot_gpu::GpuPlot;
use crate::structure::{LazyArray, Node, NumericPlan, ParseOutput, Program, Value};
use crate::structure_plot::{PlotData, PlotQuality, PlotReader};
use egui::{Color32, Ui};
use egui_plot::{AxisHints, Plot, PlotBounds, PlotPoint, PlotTransform, Points};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, OnceLock,
};
use std::time::{Duration, Instant};

#[path = "structure_table.rs"]
mod table;

const PAGE_SIZE: usize = 128;
// Large records contain nested lazy arrays. Keep one inspector request small
// instead of constructing 128 complete record trees regardless of record size.
const INSPECTOR_PAGE_BYTES: usize = 8 * 1024 * 1024;
fn inspector_page_size(array: &LazyArray) -> usize {
    let width = usize::try_from(array.stride).unwrap_or(usize::MAX).max(1);
    (INSPECTOR_PAGE_BYTES / width).clamp(1, PAGE_SIZE)
}
const INITIAL_PLOT_SAMPLES: usize = 1024;
const MAX_HIGHLIGHTS: usize = 256;
/// Plotted points shared by all visible series. Hundreds of channels still get
/// about one min/max bucket per pixel.
const DISPLAY_POINT_BUDGET: usize = 4_000_000;
fn plot_query_pixel_width(physical_width: usize, visible_series: usize) -> usize {
    // A min/max bucket can emit up to four points. Display buckets request
    // roughly two per pixel, so divide the shared output budget accordingly.
    physical_width.min(DISPLAY_POINT_BUDGET / visible_series.max(1).saturating_mul(8).max(1))
        .max(1)
}
fn plot_worker_count() -> usize {
    static WORKERS: OnceLock<usize> = OnceLock::new();
    *WORKERS.get_or_init(|| {
        let available = std::thread::available_parallelism().map_or(4, |count| count.get());
        let headroom = available.saturating_sub(2).max(1);
        // Each reader may fan out to the shared Rayon pool. Running one reader
        // per logical core oversubscribes scans and competes with the UI.
        let workers = std::env::var("BINWAVE_PLOT_WORKERS").ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map_or(headroom.min(8), |configured| configured.clamp(1, headroom));
        log::info!("Plot reader limit: {workers} concurrent readers ({available} logical cores)");
        workers
    })
}

fn log_slow_plot_phase(clock: &mut Instant, phase: &str) {
    let elapsed = clock.elapsed();
    if elapsed >= Duration::from_millis(200) {
        log::warn!("Slow plot UI phase: {phase} took {:.2}s", elapsed.as_secs_f64());
    }
    *clock = Instant::now();
}


#[derive(Clone, Debug, Hash, PartialEq, Eq)]
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
    tree_jump_definition: Option<String>,
    tree_reveal_hex: bool,
    table_target: String,
    table: table::TableState,
    table_anchor: Option<TableAnchor>,
    table_jump: usize,
    navigation_selection: Option<Range<usize>>,
    target_initialized: bool,
    definition_index: usize,
    desired: Option<Request>,
    page: Option<Page>,
    cached_pages: Vec<Page>,
    flattened_collections: Vec<Node>,
    discovered_pages: HashSet<PageKey>,
    #[cfg(test)]
    discovery_page_scans: usize,
    discovery_rx: Option<mpsc::Receiver<(u64, Result<Vec<Node>, String>)>>,
    discovery_attempted: HashSet<String>,
    discovery_error: Option<String>,
    rx: Option<mpsc::Receiver<Page>>,
    running: Option<PageKey>,
    cancel: Arc<AtomicBool>,
    source_revision: Option<u64>,
    repaint_context: Option<egui::Context>,
    plot_reader: PlotReader,
    /// The whole-file overview strip under the plot.
    overview: crate::overview_strip::OverviewStrip,
    ram_cache_bytes: Option<usize>,
    presentation: crate::plot_presentation::Presentation,
    plot_hidden: HashSet<String>,
    plot_field_filter: String,
    plot_series_expanded: HashSet<String>,
    plot_series_collapsed: HashSet<String>,
    plot_catalog_target: String,
    plot_catalog: Vec<String>,
    plot_target: String,
    plot_sources: std::collections::BTreeSet<String>,
    plot_sources_initialized: bool,
    plot_collections_open: bool,
    plot_sidebar_series: bool,
    plot_axis_series: Option<String>,
    plot_axis_group: Option<String>,
    plot_collection_filter: String,
    extra_plot_readers: std::collections::BTreeMap<String, PlotReader>,
    group_plot_readers: BTreeMap<String, PlotReader>,
    group_plan_cache: Option<(u64, Arc<BTreeMap<String, (Node, NumericPlan, Vec<String>)>>)>,
    plot_overlay: Option<(u64, PlotData)>,
    plot_overlay_generations: BTreeMap<String, u64>,
    plot_overlay_points: BTreeMap<String, Arc<Vec<PlotPoint>>>,
    /// Display-space copies of overlay points for non-GPU drawing, per trace.
    plot_placed: HashMap<String, PlacedPoints>,
    plot_overlay_revision: u64,
    plot_mesh_cache: Arc<TraceMeshCache>,
    plot_raster: PlotRaster,
    plot_gpu: GpuPlot,
    #[cfg(test)]
    plot_overlay_builds: usize,
    plot_view: Option<Range<usize>>,
    plot_axis_width: f32,
    /// Widest Y axis seen for the current axis series; the axis never shrinks
    /// while panning, so tick label width changes don't move the plot.
    plot_y_axis_thickness: f32,
    plot_y_axis_key: Option<(Option<String>, Option<String>, bool)>,
    plot_force_view: Option<Range<usize>>,
    plot_fit_y: bool,
    plot_fit_all_y: bool,
    plot_free_y: bool,
    plot_dot_radius: Option<f32>,
    plot_range_fill_percent: Option<u32>,
    plot_range_zoom: Option<RangeZoom>,
    plot_middle_panning: bool,
    plot_axis_panning: Option<usize>,
    plot_zoom_y: Option<(f64, f64)>,
    plot_follow_selection: bool,
    plot_sampled_preview: bool,
    plot_cache_error: Option<String>,
    plot_paused: bool,
    plot_anchor: Option<usize>,
    plot_jump: usize,
    plot_jump_text: String,
    plot_go_open: bool,
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
        let budget = self.ram_cache_bytes;
        *self = Self::default();
        self.ram_cache_bytes = budget;
    }
    pub fn set_ram_cache_budget(&mut self, bytes: usize) {
        if self.ram_cache_bytes == Some(bytes) { return; }
        self.ram_cache_bytes = Some(bytes);
        let count = 1 + self.group_plot_readers.len() + self.extra_plot_readers.len();
        let per_reader = bytes / count;
        self.plot_reader.set_cache_budget(per_reader);
        for reader in self.group_plot_readers.values_mut() { reader.set_cache_budget(per_reader); }
        for reader in self.extra_plot_readers.values_mut() { reader.set_cache_budget(per_reader); }
    }
    pub fn take_plot_action(&mut self) -> Option<PlotAction> {
        self.plot_action.take()
    }
    pub fn invalidate(&mut self) {
        self.plot_reader.invalidate();
        self.plot_overlay = None;
        self.plot_overlay_generations.clear();
        self.plot_overlay_points.clear();
        self.plot_placed.clear();
        self.plot_mesh_cache.clear();
        self.plot_raster.invalidate();
        self.plot_gpu.invalidate();
        self.plot_catalog_target.clear();
        self.plot_catalog.clear();
        for reader in self.extra_plot_readers.values_mut() { reader.invalidate(); }
        for reader in self.group_plot_readers.values_mut() { reader.invalidate(); }
        self.group_plan_cache = None;
        self.plot_action = None;
        self.plot_range_zoom = None;
        self.plot_zoom_y = None;
        self.navigation_selection = None;
        // Byte edits replace the decode output, but preserve the user's viewport.
        // reset() still starts fresh for a different file or definition.
        self.plot_force_view = self.plot_view.clone();
        self.plot_fit_y = true;
        self.plot_fit_all_y = false;
        self.plot_paused = false;
        self.plot_anchor = None;
        self.table_anchor = None;
        self.cancel.store(true, Ordering::Relaxed);
        self.desired = None;
        self.page = None;
        self.cached_pages.clear();
        self.flattened_collections.clear();
        self.discovered_pages.clear();
        self.discovery_rx = None;
        self.discovery_attempted.clear();
        self.discovery_error = None;
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
                if let Some(previous) = self.page.replace(page) {
                    self.remember_page(previous);
                }
                self.resolve_pending_selection();
            }
        }
        let next = self
            .desired
            .as_ref()
            .filter(|request| {
                request.key.revision == source.revision
                    && self.page_for_key(&request.key).is_none()
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
                                .page(&source, &cancel, request.key.start, inspector_page_size(&request.array))?;
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
        let page_size = inspector_page_size(array);
        let start = start.min(array.count.saturating_sub(1)) / page_size * page_size;
        let key = PageKey {
            path: node.path.clone(),
            start,
            revision,
        };
        // Page controls read their position from `desired`, so a page served from
        // memory must also become the desired one; tick() won't re-read it.
        if self.page.as_ref().is_some_and(|page| page.key == key) {
            self.desired = Some(Request { key, array: array.clone() });
            return;
        }
        if let Some(index) = self.cached_pages.iter().position(|page| page.key == key) {
            let cached = self.cached_pages.remove(index);
            if let Some(previous) = self.page.replace(cached) {
                self.remember_page(previous);
            }
            self.desired = Some(Request { key, array: array.clone() });
            return;
        }
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
                    let collection = self.collection_node(output, &self.table_target)?;
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
        self.plot_anchor = self.collection_node(output, &self.table_target)
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
    #[cfg(test)]
    fn page_nodes(&self, path: &str, revision: u64) -> Option<&[Node]> {
        self.page_for(path, revision)
            .and_then(|page| page.result.as_ref().ok())
            .map(Vec::as_slice)
    }
    fn page_for_key(&self, key: &PageKey) -> Option<&Page> {
        self.page.as_ref().filter(|page| &page.key == key)
            .or_else(|| self.cached_pages.iter().rev().find(|page| &page.key == key))
    }
    fn remember_page(&mut self, page: Page) {
        // Only one remembered page per collection is useful to the tree.
        // Keeping every old sample page can evict the ancestor that contains
        // this collection, making its expanded rows and controls disappear.
        self.cached_pages.retain(|cached| cached.key.path != page.key.path);
        self.cached_pages.push(page);
        if self.cached_pages.len() > 8 {
            let index = self.cached_pages.iter()
                .position(|cached| cached.key.path != self.table_target && !self.expanded.contains(&cached.key.path))
                .unwrap_or(0);
            self.cached_pages.remove(index);
        }
    }
    fn page_for(&self, path: &str, revision: u64) -> Option<&Page> {
        self.page.as_ref().filter(|page| page.key.path == path && page.key.revision == revision)
            .or_else(|| self.cached_pages.iter().rev().find(|page| page.key.path == path && page.key.revision == revision))
    }
    fn tree_page_control_node(&self, output: &ParseOutput, path: &str) -> Option<Node> {
        find_node(&output.nodes, path)
            .or_else(|| self.page.as_ref().into_iter().chain(self.cached_pages.iter())
                .filter(|page| page.key.revision == output.revision)
                .filter_map(|page| page.result.as_ref().ok())
                .find_map(|nodes| find_node(nodes, path)))
            .cloned()
    }
    fn collection_node<'a>(&'a self, output: &'a ParseOutput, path: &str) -> Option<&'a Node> {
        find_node(&output.nodes, path)
            .or_else(|| self.flattened_collections.iter().find(|node| node.path == path))
            .or_else(|| self.table.known.get(path).filter(|node| is_collection(node)))
    }
    fn discover_flattened_collections(&mut self, output: &ParseOutput) {
        let mut found = Vec::new();
        if let Some(rx) = &self.discovery_rx {
            match rx.try_recv() {
                Ok((revision, result)) => {
                    self.discovery_rx = None;
                    if revision == output.revision {
                        match result {
                            Ok(nodes) => found.extend(nodes),
                            Err(error) => self.discovery_error = Some(error),
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.discovery_rx = None;
                    self.discovery_error = Some("Nested collection discovery stopped unexpectedly".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let resident: HashSet<_> = self.page.as_ref().into_iter().chain(self.cached_pages.iter())
            .map(|page| page.key.clone()).collect();
        self.discovered_pages.retain(|key| resident.contains(key));
        for page in self.page.as_ref().into_iter().chain(self.cached_pages.iter()) {
            if page.key.revision != output.revision { continue; }
            let Some(parent) = find_node(&output.nodes, &page.key.path) else { continue; };
            let Some(first) = page.result.as_ref().ok().and_then(|rows| rows.first()) else { continue; };
            if !self.discovered_pages.insert(page.key.clone()) { continue; }
            #[cfg(test)] { self.discovery_page_scans += 1; }
            collect_flattened_descendants(parent, first, None, &mut found);
        }
        if !found.is_empty() {
            let mut names: HashSet<_> = self.flattened_collections.iter()
                .map(|node| node.path.clone()).collect();
            found.retain(|node| names.insert(node.path.clone()));
        }
        if !found.is_empty() {
            self.discovery_error = None;
            let preferred = found.iter().filter(|node| sensor_group_key(node).is_some())
                .reduce(|best, node| {
                if node.lazy.as_ref().map_or(0, |array| array.count)
                    > best.lazy.as_ref().map_or(0, |array| array.count) { node } else { best }
            }).or_else(|| found.iter().reduce(|best, node| {
                if node.lazy.as_ref().map_or(0, |array| array.count)
                    > best.lazy.as_ref().map_or(0, |array| array.count) { node } else { best }
            })).unwrap().path.clone();
            self.flattened_collections.extend(found);
            if self.table_target.is_empty() || self.collection_node(output, &self.table_target).is_some_and(|node| node.lazy.as_ref().is_some_and(|array| array.count < self.collection_node(output, &preferred).and_then(|node| node.lazy.as_ref()).map_or(0, |array| array.count))) {
                self.table_target = preferred.clone();
                self.target_initialized = true;
            }
            if self.plot_target.is_empty() && self.plot_sources.is_empty() {
                self.plot_target = preferred;
            }
        }
    }
    fn start_nested_discovery(&mut self, output: &ParseOutput, source: ByteView, ctx: &egui::Context) {
        if self.discovery_rx.is_some() { return; }
        let mut candidates = Vec::new();
        collect_collections(&output.nodes, &mut candidates);
        candidates.retain(|node| node.lazy.is_some() && !numeric_kind(&node.kind)
            && !self.discovery_attempted.contains(&node.path));
        candidates.sort_by_key(|node| std::cmp::Reverse(collection_count(node)));
        let Some(parent) = candidates.first().copied().cloned() else { return; };
        self.discovery_attempted.insert(parent.path.clone());
        let (tx, rx) = mpsc::channel();
        self.discovery_rx = Some(rx);
        let revision = output.revision;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            log::info!("Nested collection discovery started: {}", parent.path);
            let result = (|| -> anyhow::Result<Vec<Node>> {
                let array = parent.lazy.as_ref().unwrap();
                let first = array.page(&source, &AtomicBool::new(false), 0, 1)?;
                let mut found = Vec::new();
                if let Some(row) = first.first() {
                    collect_flattened_descendants(&parent, row, Some(&source), &mut found);
                }
                Ok(found)
            })().map_err(|error| format!("{}: {error:#}", parent.path));
            match &result {
                Ok(found) => log::info!("Nested collection discovery finished: {} yielded {} collections in {:.2}s",
                    parent.path, found.len(), started.elapsed().as_secs_f64()),
                Err(error) => log::warn!("Nested collection discovery failed: {} after {:.2}s: {error}",
                    parent.path, started.elapsed().as_secs_f64()),
            }
            let _ = tx.send((revision, result));
            ctx.request_repaint();
        });
    }
    pub fn selected_node(&self, output: &ParseOutput) -> Option<Node> {
        self.node_at(output, self.selected.as_ref()?)
    }

    /// A decoded node by path, from the output or an already-loaded page.
    pub fn node_at(&self, output: &ParseOutput, path: &str) -> Option<Node> {
        if self.stale(output) {
            return None;
        }
        find_node(&output.nodes, path)
            .or_else(|| {
                self.page.as_ref().into_iter().chain(self.cached_pages.iter())
                    .filter(|page| page.key.revision == output.revision)
                    .filter_map(|page| page.result.as_ref().ok())
                    .find_map(|nodes| find_node(nodes, path))
            })
            .cloned()
    }

    /// Select a node by path, as a click on its Structure row does.
    pub fn select_node(&mut self, node: &Node) -> Range<usize> {
        expand_ancestors(&node.path, &mut self.expanded);
        self.reveal_selected_row = true;
        self.select(node)
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
        program: Option<&Program>,
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
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(egui_phosphor::regular::INFO)
                    .font(egui::FontId::new(14., egui::FontFamily::Name("phosphor".into())))
                    .color(crate::theme::DARK.color.text.muted))
                    .on_hover_text("Expand an item to inspect its fields. Click a row to highlight its bytes in Hex. Hover a number for decimal, hex, and binary.");
            });
        });
        let mut clicked = None;
        egui::Panel::bottom("structure_types")
            .show(ui, |ui| self.definition_details(ui, output, &mut clicked));
        if let Some(request) = self.desired.clone().filter(|request| self.expanded.contains(&request.key.path)) {
            let node = self.tree_page_control_node(output, &request.key.path);
            if let Some(node) = node {
                egui::Panel::bottom("structure_page_controls").show(ui, |ui| {
                    if let Some(array) = &node.lazy {
                        let displayed = self.page_for(&node.path, output.revision)
                            .and_then(|page| page.result.as_ref().ok()).map_or(0, Vec::len);
                        let remaining = array.count.saturating_sub(request.key.start.saturating_add(displayed));
                        if remaining > 0 {
                            ui.weak(format!("… {} more", crate::format::plural(remaining as u64, "item", "items")));
                        }
                    }
                    self.page_controls(ui, &node, output.revision, true);
                });
            }
        }
        let mut rows = Vec::new();
        flatten_visible(
            &output.nodes,
            0,
            &self.expanded,
            self.page.as_ref(),
            &self.cached_pages,
            output.revision,
            &mut rows,
        );
        if let Some(error) = self.page.as_ref().into_iter().chain(self.cached_pages.iter())
            .filter(|page| page.key.revision == output.revision && self.expanded.contains(&page.key.path))
            .filter_map(|page| page.result.as_ref().err())
            .next()
        {
            ui.colored_label(ui.visuals().error_fg_color, format!("Could not load items: {error}"));
        }
        let row_height = 24.;
        let name_width = rows.iter().map(|(_, depth)| 160. + (*depth).min(32) as f32 * 16.)
            .max_by(f32::total_cmp).unwrap_or(300.).max(300.);
        let sizes = &crate::theme::DARK.size;
        let show_type = ui.available_width() >= sizes.structure_type_min_width;
        let has_issues = tree_has_issues(&output.nodes)
            || self.page.as_ref().into_iter().chain(self.cached_pages.iter())
                .any(|page| page.key.revision == output.revision && page.result.is_err());
        let type_width = if show_type { 150. } else { 0. };
        let status_width = if has_issues { sizes.structure_status_width } else { 0. };
        let minimum_width = name_width + type_width + 450. + status_width;
        let scroll = &ui.spacing().scroll;
        let table_width = (ui.available_width() - scroll.bar_width - scroll.bar_outer_margin).max(minimum_width);
        let widths = [
            name_width + (table_width - minimum_width) * 0.55,
            type_width,
            220. + (table_width - minimum_width) * 0.45,
            130.,
            100.,
            status_width,
        ];
        let mut toggle = None;
        let mut node_click = None;
        let mut reveal_definition = None;
        let mut reveal_hex = false;
        let old_row_spacing = ui.spacing().item_spacing.y;
        ui.spacing_mut().item_spacing.y = 0.;
        let mut scroll = egui::ScrollArea::both();
        if let Some(path) = self.reveal_tree.take() {
            if let Some(index) = rows.iter().position(|(node, _)| node.path == path) { scroll = scroll.vertical_scroll_offset((index + 1) as f32 * row_height); }
        }
        scroll
            .id_salt("decoded_structure_tree")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, rows.len() + 1, |ui, range| {
                for index in range {
                    if index == 0 {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(table_width, row_height), egui::Sense::hover());
                        ui.painter().rect_filled(rect, egui::CornerRadius::same(crate::theme::DARK.radius.row), crate::theme::DARK.color.surface.raised);
                        paint_tree_columns(ui, rect, widths, ["Name", "Type", "Value", "Offset", "Size", ""], true);
                        continue;
                    }
                    let index = index - 1;
                    let (node, depth) = rows[index];
                    let parent_kind = rows[..index].iter().rev()
                        .find(|(_, parent_depth)| *parent_depth + 1 == depth)
                        .map(|(parent, _)| parent.kind.as_str());
                    ui.push_id(&node.path, |ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(table_width, row_height), egui::Sense::hover());
                        let selected = self.selected.as_deref() == Some(&node.path);
                        if selected {
                            ui.painter().rect_filled(rect.shrink2(egui::vec2(0., 1.)),
                                egui::CornerRadius::same(crate::theme::DARK.radius.selection),
                                crate::theme::DARK.color.accent.base.gamma_multiply(crate::theme::DARK.color.accent.selection_alpha));
                        }
                        let expandable = node.lazy.is_some() || !node.children.is_empty();
                        let open = self.expanded.contains(&node.path);
                        let name_left = rect.left() + 8. + depth.min(32) as f32 * 16.;
                        let chevron_rect = egui::Rect::from_min_size(
                            egui::pos2(name_left, rect.top() + 3.), egui::vec2(16., 18.));
                        if expandable {
                            ui.painter().with_clip_rect(chevron_rect).text(
                                chevron_rect.center(), egui::Align2::CENTER_CENTER,
                                if open { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_RIGHT },
                                egui::FontId::new(11., egui::FontFamily::Name("phosphor".into())),
                                crate::theme::DARK.color.text.muted);
                        }
                        let name_rect = egui::Rect::from_min_max(
                            egui::pos2(name_left + 16., rect.top()),
                            egui::pos2(rect.left() + widths[0] - 4., rect.bottom()));
                        let mut name_clicked = false;
                        if node.path == node.name {
                            let mut name_ui = ui.new_child(egui::UiBuilder::new().max_rect(name_rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)));
                            name_clicked = names(&mut name_ui, node).clicked();
                        } else {
                            paint_tree_text(ui, name_rect, &node.tree_label(),
                                crate::theme::DARK.color.text.primary, false, false, crate::theme::DARK.font.body);
                        }
                        let size = node.range.end.saturating_sub(node.range.start);
                        let value = tree_column_value(node, parent_kind, program);
                        let kind = tree_column_type(node);
                        let offset = format!("0x{:08X}", node.range.start);
                        let size_label = crate::format::size(size as u64);
                        paint_tree_columns(ui, rect, widths,
                            ["", &kind, &value, &offset, &size_label, ""], false);
                        let issue = self.page_for(&node.path, output.revision)
                            .and_then(|page| page.result.as_ref().err());
                        let warning = issue.is_some() || node.kind.starts_with("partial");
                        let status_rect = egui::Rect::from_min_max(
                            egui::pos2(rect.right() - widths[5], rect.top()), rect.right_bottom());
                        let status_tooltip = (has_issues && (warning || depth == 0)).then(|| {
                            let (icon, color, text) = if warning {
                                (egui_phosphor::regular::WARNING, crate::theme::DARK.color.status.warning,
                                    issue.map_or_else(|| "Incomplete: decoding stopped inside this item".to_owned(),
                                        |error| format!("Could not load items: {error}")))
                            } else {
                                (egui_phosphor::regular::CHECK, crate::theme::DARK.color.status.success,
                                    "Decoded without issues".to_owned())
                            };
                            if ui.is_rect_visible(status_rect) {
                                ui.painter().with_clip_rect(status_rect).text(status_rect.center(),
                                    egui::Align2::CENTER_CENTER, icon,
                                    egui::FontId::new(crate::theme::DARK.font.body,
                                        egui::FontFamily::Name("phosphor".into())), color);
                            }
                            text
                        });
                        let tooltip = format!("{}\n{}\n{}\n{}{}",
                            node.path, kind,
                            crate::format::byte_range(node.range.start as u64, node.range.end as u64),
                            crate::format::exact_bytes(size as u64),
                            if node.references.is_empty() { String::new() } else { format!("\n{}", node.references.join("\n")) });
                        let tooltip = issue.map_or(tooltip.clone(), |error| format!("{tooltip}\n{error}"));
                        let tooltip = node.numeric_tooltip().map_or(tooltip.clone(), |number| format!("{number}\n\n{tooltip}"));
                        let over_status = ui.input(|i| i.pointer.hover_pos())
                            .is_some_and(|position| status_rect.contains(position));
                        let tooltip = match status_tooltip {
                            Some(status) if over_status => status,
                            _ => tooltip,
                        };
                        let row_response = ui.interact(rect, ui.id().with("tree_row"), egui::Sense::click())
                            .on_hover_text(tooltip);
                        if expandable && row_response.clicked() && chevron_rect.contains(ui.input(|i| i.pointer.interact_pos().unwrap_or_default())) {
                            toggle = Some(node.clone());
                        } else if row_response.clicked() || name_clicked {
                            node_click = Some(node.clone());
                        }
                        row_response.context_menu(|ui| {
                            if ui.button("Copy value").clicked() {
                                ui.ctx().copy_text(node.value.as_ref().map_or_else(|| value.clone(), ToString::to_string));
                                ui.close();
                            }
                            if ui.button("Copy path").clicked() { ui.ctx().copy_text(node.path.clone()); ui.close(); }
                            if ui.button("Reveal in Hex").clicked() {
                                node_click = Some(node.clone());
                                reveal_hex = true;
                                ui.close();
                            }
                            if ui.button("Jump to definition").clicked() {
                                let kind = node.kind.trim_end_matches("[]");
                                reveal_definition = Some(if program.is_some_and(|program| program.structures.contains_key(kind)) {
                                    kind.to_owned()
                                } else {
                                    parent_kind.unwrap_or(&node.name).to_owned()
                                });
                                ui.close();
                            }
                        });
                    });
                }
            });
        ui.spacing_mut().item_spacing.y = old_row_spacing;
        if reveal_definition.is_some() { self.tree_jump_definition = reveal_definition; }
        if reveal_hex { self.tree_reveal_hex = true; }
        if let Some(node) = toggle {
            if is_collection(&node) {
                self.open_table_target(node.path.clone(), Some(node.clone()), false);
            }
            if !self.expanded.remove(&node.path) {
                self.expanded.insert(node.path.clone());
                self.request(&node, 0, output.revision, ui.ctx());
            }
            clicked = Some(self.select(&node));
        }
        if let Some(node) = node_click {
            if is_collection(&node) {
                self.open_table_target(node.path.clone(), Some(node.clone()), false);
            }
            self.request(&node, 0, output.revision, ui.ctx());
            clicked = Some(self.select(&node));
        }
        clicked.or_else(|| self.navigation_selection.take())
    }

    pub fn take_tree_definition_jump(&mut self) -> Option<String> {
        self.tree_jump_definition.take()
    }
    pub fn take_tree_reveal_hex(&mut self) -> bool {
        std::mem::take(&mut self.tree_reveal_hex)
    }

    fn definition_details(
        &mut self,
        ui: &mut Ui,
        output: &ParseOutput,
        clicked: &mut Option<Range<usize>>,
    ) {
        let consumed = output.consumed as u64;
        let header = egui::CollapsingHeader::new(egui::RichText::new(format!("Decoder types ({}) · covers {}",
            output.definitions.len(), crate::format::size(consumed)))
            .color(crate::theme::DARK.color.text.muted))
            .default_open(false)
            .icon(|ui, openness, response| {
                ui.painter().text(response.rect.left_center(), egui::Align2::LEFT_CENTER,
                    if openness > 0.5 { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_RIGHT },
                    egui::FontId::new(11., egui::FontFamily::Name("phosphor".into())),
                    crate::theme::DARK.color.text.muted);
            })
            .show(ui, |ui| {
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
                            "Defined by {}",
                            crate::format::byte_range(origin.start as u64, origin.end as u64)
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
            });
        header.header_response.on_hover_text(format!("The decoder reads {} of this file.", crate::format::exact_bytes(consumed)));
    }

    fn initialize_target(&mut self, output: &ParseOutput) {
        if !self.table_target.is_empty() && self.collection_node(output, &self.table_target).is_none() {
            self.target_initialized = false;
        }
        if self.target_initialized {
            return;
        }
        self.target_initialized = true;
        let mut collections = Vec::new();
        collect_collections(&output.nodes, &mut collections);
        collections.extend(self.flattened_collections.iter());
        self.table_target = collections
            .into_iter()
            .max_by_key(|node| collection_score(node, output))
            .map_or_else(String::new, |node| node.path.clone());
    }
    fn samples_path(&self, output: &ParseOutput) -> Option<String> {
        let mut collections = Vec::new();
        collect_collections(&output.nodes, &mut collections);
        collections.extend(self.flattened_collections.iter());
        samples_collection(collections, output).map(|node| node.path.clone())
    }
    fn page_controls(&mut self, ui: &mut Ui, node: &Node, revision: u64, item_navigation: bool) {
        let Some(array) = &node.lazy else {
            return;
        };
        let page_size = inspector_page_size(array);
        let mut start = self
            .desired
            .as_ref()
            .filter(|request| request.key.path == node.path && request.key.revision == revision)
            .map(|request| request.key.start)
            .or_else(|| self.page_for(&node.path, revision).map(|page| page.key.start))
            .unwrap_or(0);
        let before = start;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(start > 0, egui::Button::new("Previous"))
                .clicked()
            {
                start = start.saturating_sub(page_size);
                self.table_jump = start;
            }
            ui.label(format!(
                "{} {}–{} of {}",
                if item_navigation { "Items" } else { "Samples" },
                crate::format::count(start),
                crate::format::count(start
                    .saturating_add(page_size)
                    .min(array.count)
                    .saturating_sub(1)),
                crate::format::count(array.count)
            ));
            if ui
                .add_enabled(
                    start.saturating_add(page_size) < array.count,
                    egui::Button::new("Next"),
                )
                .clicked()
            {
                start = start.saturating_add(page_size);
                self.table_jump = start;
            }
            ui.label(if item_navigation { "Go to index" } else { "Go to sample" });
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
        let aligned = start.min(array.count.saturating_sub(1)) / page_size * page_size;
        if aligned != before {
            self.carry_expansion(&node.path, before, aligned, page_size);
        }
        self.request(node, start, revision, ui.ctx());
        if let Some(page) = self.page_for(&node.path, revision) {
            if let Err(error) = &page.result {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
        } else {
            ui.weak("Reading this page…");
        }
    }
    /// Paging keeps the same fields open: expansion under `collection[i]` on the
    /// old page follows to the item at the same position on the new page.
    fn carry_expansion(&mut self, collection: &str, from: usize, to: usize, page_size: usize) {
        let prefix = format!("{collection}[");
        let moved: Vec<String> = self.expanded.iter().filter_map(|path| {
            let (index, tail) = path.strip_prefix(&prefix)?.split_once(']')?;
            let index: usize = index.parse().ok()?;
            if !(from..from.saturating_add(page_size)).contains(&index) {
                return None;
            }
            let target = (index + to).checked_sub(from)?;
            Some(format!("{prefix}{target}]{tail}"))
        }).collect();
        self.expanded.extend(moved);
    }
    #[cfg(test)]
    fn target_rows<'a>(&'a self, output: &'a ParseOutput) -> &'a [Node] {
        match self.collection_node(output, &self.table_target) {
            Some(node) if node.lazy.is_some() => {
                self.page_nodes(&node.path, output.revision).unwrap_or(&[])
            }
            Some(node) => &node.children,
            None => &output.nodes,
        }
    }

    fn initialize_plot(&mut self, output: &ParseOutput) {
        if self.plot_target.is_empty() || self.collection_node(output, &self.plot_target).is_none() {
            self.plot_view = None;
            self.plot_force_view = None;
            self.initialize_target(output);
            self.plot_target = self.table_target.clone();
            if self.plot_target.is_empty() {
                let mut collections = Vec::new();
                collect_collections(&output.nodes, &mut collections);
                collections.extend(self.flattened_collections.iter());
                self.plot_target = collections
                    .into_iter()
                    .max_by_key(|node| collection_score(node, output))
                    .map_or_else(String::new, |node| node.path.clone());
            }
        }
        if self.plot_follow_selection {
            self.plot_follow_selection = false;
            if let Some(range) = &self.last_hex_selection {
                let selected_virtual = self.flattened_collections.iter().find(|node|
                    node.path == self.table_target && sample_indices_for_bytes(node, range).is_some());
                if let Some(collection) = matching_collection(
                    self.flattened_collections.iter(), &self.table_target, range)
                    .or_else(|| containing_collection(&output.nodes, range)) {
                    if !self.plot_sources.contains(&collection.path) && self.plot_sources_initialized {
                        if selected_virtual.is_none() { return; }
                        self.plot_sources.insert(collection.path.clone());
                        self.plot_target = collection.path.clone();
                    }
                    if let Some(samples) = sample_indices_for_bytes(collection, range) {
                        self.plot_jump = samples.start;
                        self.plot_jump_text = samples.start.to_string();
                        self.plot_range_first = samples.start;
                        self.plot_range_last = samples.end.saturating_sub(1);
                        let view = selection_view(
                            self.plot_view.as_ref(),
                            &samples,
                            collection_count(collection),
                        );
                        if self.plot_view.as_ref() != Some(&view) {
                            self.plot_force_view = Some(view.clone());
                            self.plot_view = Some(view);
                            self.plot_fit_y = true;
                        }
                    }
                }
            }
        }
        if let Some(node) = self.collection_node(output, &self.plot_target) {
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
            if let Some(node) = self.collection_node(output, &self.plot_target) {
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
        self.select_bytes(output, Some(bytes.clone()));
        let virtual_collection = path.as_ref()
            .and_then(|path| self.flattened_collections.iter()
                .filter(|node| contains_path(&node.path, path))
                .max_by_key(|node| node.path.len()))
            .or_else(|| self.flattened_collections.iter()
                .find(|node| node.path == self.plot_target && sample_indices_for_bytes(node, &bytes).is_some()))
            .cloned();
        if let Some(collection) = virtual_collection {
            if let Some(samples) = sample_indices_for_bytes(&collection, &bytes) {
                self.table_target = collection.path.clone();
                self.table_jump = samples.start;
                self.plot_jump = samples.start;
                self.plot_jump_text = samples.start.to_string();
                self.plot_range_first = samples.start;
                self.plot_range_last = samples.end.saturating_sub(1);
                self.target_initialized = true;
                self.queue_page(&collection, samples.start, output.revision);
                self.pending_hex_selection = true;
            }
        }
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
        let mut phase_clock = Instant::now();
        // A covered pan can reuse cached geometry without entering a reader.
        // Check the file identity once here before any retained data is shown.
        if let Err(error) = source.validate() {
            self.plot_reader.stop();
            for reader in self.group_plot_readers.values_mut() { reader.stop(); }
            for reader in self.extra_plot_readers.values_mut() { reader.stop(); }
            ui.colored_label(Color32::LIGHT_RED, format!("{error:#}"));
            return None;
        }
        self.tick(ui.ctx(), source.clone());
        self.discover_flattened_collections(output);
        if self.stale(output) {
            ui.label("Bytes changed. Press ▶ Run (F5) to refresh the plot.");
            return None;
        }
        self.start_nested_discovery(output, source.clone(), ui.ctx());
        if let Some(error) = &self.discovery_error { ui.colored_label(Color32::LIGHT_RED, error); }
        if self.discovery_rx.is_some() && self.flattened_collections.is_empty() {
            ui.horizontal(|ui| { ui.spinner(); ui.label("Finding nested sample collections…"); });
            return None;
        }
        self.initialize_plot(output);
        let mut collections = Vec::new();
        collect_collections(&output.nodes, &mut collections);
        collections.extend(self.flattened_collections.iter());
        let collection_by_path: HashMap<&str, &Node> = collections.iter()
            .map(|node| (node.path.as_str(), *node)).collect();
        if !self.plot_sources_initialized {
            self.plot_sources_initialized = true;
        }
        let mut changed = false;
        let mut sidebar_names: Vec<String> = self
            .plot_catalog
            .iter()
            .map(|field| format!("{} / {field}", self.plot_target))
            .collect();
        if let Some((_, overlay)) = &self.plot_overlay {
            sidebar_names.extend(overlay.series.keys().cloned());
        }
        sidebar_names.extend(self.plot_hidden.iter().cloned());
        sidebar_names.sort();
        sidebar_names.dedup();
        let mut close_collections = false;
        egui::Panel::right("plot_collections_sidebar")
            .default_size(320.0)
            .min_size(260.0)
            .max_size(380.0)
            .resizable(true)
            .show_collapsible(ui, &mut self.plot_collections_open, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_label(!self.plot_sidebar_series, "Collections").clicked() {
                        self.plot_sidebar_series = false;
                    }
                    if ui.selectable_label(self.plot_sidebar_series, "Series").clicked() {
                        self.plot_sidebar_series = true;
                    }
                    if ui.button("×").on_hover_text("Close collections").clicked() {
                        close_collections = true;
                    }
                });
                ui.separator();
                if self.plot_sidebar_series {
                    let body = egui::TextStyle::Body.resolve(ui.style());
                    if let Some(group) = &self.plot_axis_group {
                        let text = format!("Y axis: {} group", plot_series_group_label(group));
                        ui.label(elide_start(ui, &text, &body, ui.available_width()))
                            .on_hover_text(group);
                    } else if let Some(axis) = &self.plot_axis_series {
                        let text = format!("Y axis: {}", short_series_label(axis));
                        ui.label(elide_start(ui, &text, &body, ui.available_width()))
                            .on_hover_text(axis);
                    } else {
                        ui.weak("Select a series for the Y axis");
                    }
                    ui.horizontal(|ui| {
                        if ui.small_button("Fit selected Y")
                            .on_hover_text("Fit the selected series or group; keep the sample range").clicked() {
                            self.plot_fit_y = true;
                            self.plot_fit_all_y = false;
                        }
                        if ui.small_button("Fit all Y")
                            .on_hover_text("Fit every visible series; keep the sample range").clicked() {
                            self.plot_fit_all_y = true;
                            self.plot_fit_y = false;
                        }
                    });
                    ui.add(egui::TextEdit::singleline(&mut self.plot_field_filter)
                        .hint_text("Find series…"));
                    let filter = self.plot_field_filter.trim().to_lowercase();
                    let matching: Vec<_> = sidebar_names.iter()
                        .filter(|name| filter.is_empty() || name.to_lowercase().contains(&filter))
                        .collect();
                    ui.horizontal(|ui| {
                        if ui.small_button("Show all").clicked() {
                            for name in &matching { self.plot_hidden.remove(*name); }
                            self.plot_fit_y = true;
                        }
                        if ui.small_button("Hide all").clicked() {
                            for name in &matching { self.plot_hidden.insert((*name).clone()); }
                            self.plot_fit_y = true;
                        }
                    });
                    egui::ScrollArea::vertical().id_salt("plot-series-sidebar")
                        .show(ui, |ui| {
                            let groups = grouped_plot_series(matching.into_iter().map(String::as_str));
                            for (group, entries) in groups {
                                let open = !filter.is_empty() || (entries.len() <= 32
                                    && !self.plot_series_collapsed.contains(&group))
                                    || self.plot_series_expanded.contains(&group);
                                let visible = entries.iter().filter(|(_, name)| !self.plot_hidden.contains(*name)).count();
                                ui.push_id(&group, |ui| {
                                    ui.horizontal(|ui| {
                                        let arrow = ui.add_sized([18., 18.], egui::Button::new(""));
                                        let c = arrow.rect.center();
                                        let (a, b, d) = if open {
                                            (egui::pos2(c.x - 4., c.y - 2.), egui::pos2(c.x, c.y + 2.), egui::pos2(c.x + 4., c.y - 2.))
                                        } else {
                                            (egui::pos2(c.x - 2., c.y - 4.), egui::pos2(c.x + 2., c.y), egui::pos2(c.x - 2., c.y + 4.))
                                        };
                                        let stroke = egui::Stroke::new(1.5, ui.visuals().weak_text_color());
                                        ui.painter().line_segment([a, b], stroke);
                                        ui.painter().line_segment([b, d], stroke);
                                        if arrow.clicked() {
                                            if open { self.plot_series_expanded.remove(&group); self.plot_series_collapsed.insert(group.clone()); }
                                            else { self.plot_series_collapsed.remove(&group); self.plot_series_expanded.insert(group.clone()); }
                                        }
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            let group_shown = visible == entries.len();
                                            if ui.small_button(if group_shown { "Hide" } else { "Show" })
                                                .on_hover_text("Show or hide every trace in this group").clicked() {
                                                for (_, name) in &entries {
                                                    if group_shown { self.plot_hidden.insert((*name).to_owned()); }
                                                    else { self.plot_hidden.remove(*name); }
                                                }
                                                if group_shown && self.plot_axis_group.as_deref() == Some(group.as_str()) {
                                                    self.plot_axis_group = None;
                                                }
                                                self.plot_fit_y = true;
                                            }
                                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                                let title = format!("{}  {visible}/{}", plot_series_group_label(&group), entries.len());
                                                let font = egui::TextStyle::Button.resolve(ui.style());
                                                let title = elide_start(ui, &title, &font, button_text_width(ui));
                                                if ui.selectable_label(self.plot_axis_group.as_deref() == Some(group.as_str()), title)
                                                    .on_hover_text(format!("{group}\nClick to use this group for the Y axis")).clicked() {
                                                    self.plot_axis_group = Some(group.clone());
                                                }
                                            });
                                        });
                                    });
                                    if open {
                                        for (label, name) in entries {
                                            ui.push_id(name, |ui| {
                                                ui.horizontal(|ui| {
                                                    ui.add_space(16.);
                                                    let mut shown = !self.plot_hidden.contains(name);
                                                    if ui.checkbox(&mut shown, "").changed() {
                                                        if shown { self.plot_hidden.remove(name); }
                                                        else { self.plot_hidden.insert(name.to_owned()); }
                                                        self.plot_fit_y = true;
                                                    }
                                                    let axis = self.plot_axis_series.as_deref() == Some(name);
                                                    let font = egui::TextStyle::Button.resolve(ui.style());
                                                    let label = elide_start(ui, &label, &font, button_text_width(ui));
                                                    let label_response = ui.selectable_label(axis, &label)
                                                        .on_hover_text(format!("{name}\nClick to use this series for the Y axis"));
                                                    if label_response.clicked() {
                                                        self.plot_axis_series = Some(name.to_owned());
                                                        self.plot_axis_group = None;
                                                    }
                                                });
                                            });
                                        }
                                    }
                                });
                            }
                        });
                } else {
                    let samples = samples_collection(collections.iter().copied(), output)
                        .map(|node| node.path.clone());
                    changed |= plot_collection_choices(ui, &collections, samples.as_deref(),
                        &mut self.plot_sources, &mut self.plot_collection_filter);
                }
            });
        if close_collections { self.plot_collections_open = false; }
        if changed {
            self.plot_sources.retain(|path| collection_by_path.contains_key(path.as_str()));
            self.plot_fit_y = true;
        }
        if !self.plot_sources.iter().any(|path| collection_by_path.contains_key(path.as_str())) {
            if collections.is_empty() {
                crate::decoder_ui::empty_state(
                    ui,
                    "No sample series yet",
                    "Define a repeated structure or numeric field, then press ▶ Run (F5).",
                );
                return None;
            }
            self.extra_plot_readers.clear();
            self.group_plot_readers.clear();
            self.group_plan_cache = None;
            if ui.button("Collections…").clicked() { self.plot_collections_open = !self.plot_collections_open; }
            ui.label(if self.discovery_rx.is_some() {
                "Finding collections… Choose one to plot when it appears."
            } else {
                "Choose a collection to plot."
            });
            return None;
        }
        let primary = if self.plot_sources.contains(&self.plot_target)
            && collection_by_path.contains_key(self.plot_target.as_str()) {
            self.plot_target.clone()
        } else {
            collections.iter().filter(|node| self.plot_sources.contains(&node.path))
                .max_by_key(|node| collection_count(node)).unwrap().path.clone()
        };
        if primary != self.plot_target {
            self.plot_target = primary;
            self.plot_reader.invalidate();
            changed = true;
        }
        if changed { self.plot_fit_y = true; }
        let Some(node) = self.collection_node(output, &self.plot_target).cloned() else {
            ui.label("No sample collection. Repeat a numeric field or structure in the decoder to plot it.");
            return None;
        };
        let count = collection_count(&node);
        if count == 0 {
            ui.label("This collection contains no samples.");
            return None;
        }
        let mut group_hash = std::collections::hash_map::DefaultHasher::new();
        output.revision.hash(&mut group_hash);
        self.plot_target.hash(&mut group_hash);
        for path in &self.plot_sources { path.hash(&mut group_hash); }
        let group_key = group_hash.finish();
        if self.group_plan_cache.as_ref().map(|(key, _)| *key) != Some(group_key) {
            self.group_plan_cache = Some((group_key, Arc::new(grouped_numeric_plans(
                &self.plot_sources, &collection_by_path))));
        }
        let grouped_plans = self.group_plan_cache.as_ref().unwrap().1.clone();
        let primary_group = grouped_plans.iter().find(|(_, (_, _, paths))|
            paths.iter().any(|path| path == &self.plot_target));
        log_slow_plot_phase(&mut phase_clock, "prepare collections and group plans");
        let grouped_paths: HashSet<_> = grouped_plans.values().flat_map(|(_, _, paths)| paths.iter().cloned()).collect();
        self.group_plot_readers.retain(|key, _| grouped_plans.contains_key(key));
        self.extra_plot_readers.retain(|path, _| self.plot_sources.contains(path)
            && path != &self.plot_target && !grouped_paths.contains(path));
        let reader_count = 1 + grouped_plans.len().saturating_sub(usize::from(primary_group.is_some()))
            + self.plot_sources.iter().filter(|path| *path != &self.plot_target
                && !grouped_paths.contains(*path)).count();
        let reader_budget = self.ram_cache_bytes.unwrap_or_else(crate::structure_plot::group_buffer_bytes) / reader_count.max(1);
        self.plot_reader.set_cache_budget(reader_budget);
        for reader in self.group_plot_readers.values_mut() { reader.set_cache_budget(reader_budget); }
        for reader in self.extra_plot_readers.values_mut() { reader.set_cache_budget(reader_budget); }
        if self.plot_catalog_target != self.plot_target {
            self.plot_catalog_target = self.plot_target.clone();
            self.plot_catalog = node.lazy.as_ref().and_then(|array| array.numeric_plan())
                .map_or_else(Vec::new, |plan| plan.fields.into_iter().map(|field| field.name).collect());
            // Hidden names can outlive a decode or collection change. Never
            // reopen a numeric collection with every field silently hidden.
            if !self.plot_catalog.is_empty() && self.plot_catalog.iter().all(|name|
                self.plot_hidden.contains(&format!("{} / {name}", self.plot_target))) {
                for name in self.plot_catalog.iter().take(16) {
                    self.plot_hidden.remove(&format!("{} / {name}", self.plot_target));
                }
            }
            if self.plot_catalog.len() > 32 {
                let prefix = format!("{} / ", self.plot_target);
                if !self.plot_hidden.iter().any(|name| name.starts_with(&prefix)) {
                    for name in self.plot_catalog.iter().skip(16) {
                        self.plot_hidden.insert(format!("{prefix}{name}"));
                    }
                }
            }
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
        let mut ranges: Vec<_> = self.plot_reader.data().into_iter().flat_map(|data|
            data.bounds.iter().map(|(name, (min, max))| {
                (if primary_group.is_some() { name.clone() }
                 else { format!("{} / {name}", self.plot_target) }, *min, *max)
            })).collect();
        ranges.extend(self.extra_plot_readers.iter().flat_map(|(path, reader)|
            reader.data().into_iter().flat_map(move |data| data.bounds.iter().map(move |(name, (min, max))|
                (format!("{path} / {name}"), *min, *max)))));
        for reader in self.group_plot_readers.values() {
            if let Some(data) = reader.data() {
                ranges.extend(data.bounds.iter().map(|(name, (min, max))| (name.clone(), *min, *max)));
            }
        }
        let mut names: Vec<String> = self
            .plot_catalog
            .iter()
            .map(|name| format!("{} / {name}", self.plot_target))
            .collect();
        let mut known: HashSet<String> = names.iter().cloned().collect();
        for (name, _, _) in &ranges {
            if known.insert(name.clone()) {
                names.push(name.clone());
            }
        }
        if self.plot_axis_group.as_ref().is_some_and(|group| {
            !names
                .iter()
                .any(|name| plot_series_group_key(name) == *group)
        }) {
            self.plot_axis_group = None;
        }
        let axis_names = plot_axis_target_names(
            &names,
            self.plot_axis_group.as_deref(),
            self.plot_axis_series.as_deref(),
        );
        let visible_series = names.iter().filter(|name| !self.plot_hidden.contains(*name)).count();
        if self.plot_axis_series.as_ref().is_none_or(|axis|
            !names.contains(axis) || self.plot_hidden.contains(axis)) {
            self.plot_axis_series = names.iter().find(|name| !self.plot_hidden.contains(*name)).cloned();
            self.plot_fit_y = true;
        }
        let series_label = if names.is_empty() {
            "Series".to_owned()
        } else {
            format!("Series {visible_series}/{}", names.len())
        };
        let mut focus_plot_go = false;
        ui.horizontal(|ui| {
            if ui.button(series_label).clicked() {
                self.plot_collections_open = true;
                self.plot_sidebar_series = true;
            }
            if ui.button("Collections…").clicked() {
                self.plot_collections_open = true;
                self.plot_sidebar_series = false;
            }
            if ui.button("Fit").on_hover_text("Show the full sample range and fit all visible series").clicked() { force_view=Some(0..count); self.plot_fit_all_y=true; self.plot_fit_y=false; }
            if ui.button("Go to sample…").clicked() {
                self.plot_go_open = !self.plot_go_open;
                if self.plot_go_open && self.plot_jump_text.is_empty() {
                    self.plot_jump_text = self.plot_jump.min(count - 1).to_string();
                }
                focus_plot_go = self.plot_go_open;
            }
            ui.menu_button("Plot options", |ui| {
            if self.presentation.menu(ui, &ranges) { self.plot_fit_y = true; }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let mut keep = crate::structure_plot::keep_decoded_data(&source);
                if ui.checkbox(&mut keep, "Keep decoded data for this file")
                    .on_hover_text("Retain decoded plot values as you explore. Storage grows with covered ranges; this does not decode the whole file.")
                    .changed() {
                    match crate::structure_plot::set_keep_decoded_data(&source, keep) {
                        Ok(()) => {
                            self.plot_cache_error = None;
                            if keep {
                                self.plot_reader.persist_available_tiles();
                                for reader in self.extra_plot_readers.values() { reader.persist_available_tiles(); }
                                for reader in self.group_plot_readers.values() { reader.persist_available_tiles(); }
                            }
                        }
                        Err(error) => self.plot_cache_error = Some(format!("{error:#}")),
                    }
                }
                let (bytes, pending, error) = crate::structure_plot::decoded_data_status(&source);
                ui.weak(format!("Decoded data on disk: {} · {} pending",
                    crate::format::size(bytes), crate::format::count(pending)))
                    .on_hover_text(crate::format::exact_bytes(bytes));
                if let Some(error) = error.or_else(|| self.plot_cache_error.clone()) {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("New decoded data was not retained: {error}"));
                }
                if ui.button("Remove decoded data for this file").clicked() {
                    match crate::structure_plot::remove_decoded_data(&source) {
                        Ok(()) => self.plot_cache_error = None,
                        Err(error) => self.plot_cache_error = Some(format!("{error:#}")),
                    }
                }
                ui.separator();
            }
            if ui.checkbox(&mut self.plot_sampled_preview, "Fast sampled preview")
                .on_hover_text("May miss peaks. Off by default; full scans run in the background and can be stopped.")
                .changed() {
                self.plot_paused = false;
            }
            let mut lock_y=!self.plot_free_y;
            if ui.checkbox(&mut lock_y,"Lock vertical zoom").changed() {self.plot_free_y=!lock_y;}
            let mut show_dots = self.plot_dot_radius.unwrap_or(2.5) > 0.0;
            if ui.checkbox(&mut show_dots, "Show sample dots").changed() {
                self.plot_dot_radius = Some(if show_dots { 2.5 } else { 0.0 });
            }
            if show_dots {
                let mut radius = self.plot_dot_radius.unwrap_or(2.5);
                if ui.add(egui::Slider::new(&mut radius, 0.5..=5.0).text("Dot radius")).changed() {
                    self.plot_dot_radius = Some(radius);
                }
            }
            let mut range_fill = self.plot_range_fill_percent.unwrap_or(20);
            if ui.add(egui::Slider::new(&mut range_fill, 0..=100)
                .text("Range highlight fill").suffix("%"))
                .on_hover_text("0% hides the fill; the range edges remain visible")
                .changed()
            {
                self.plot_range_fill_percent = Some(range_fill);
            }
            if ui.button("Fit selected Y").clicked() {self.plot_fit_y=true;self.plot_fit_all_y=false;ui.close();}
            if ui.button("Fit all visible Y").clicked() {self.plot_fit_all_y=true;self.plot_fit_y=false;ui.close();}
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
                    if let Some(bytes) = sample_bytes(&node, first..last + 1) {
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
                    format!("Sample {}", crate::format::count(samples.start))
                } else {
                    format!("Samples {}–{}", crate::format::count(samples.start),
                        crate::format::count(samples.end.saturating_sub(1)))
                };
                ui.weak(format!("{label} · {} selected", crate::format::plural(bytes.len() as u64, "byte", "bytes")))
                    .on_hover_text(format!("{}\nWhole-sample export includes all interleaved fields.",
                        crate::format::byte_range(bytes.start as u64, bytes.end as u64)));
            }
        });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("Samples {}–{} of {}", crate::format::count(view.start),
                    crate::format::count(view.end.saturating_sub(1)), crate::format::count(count)));
                let axis = self.plot_axis_group.as_ref()
                    .map(|group| (format!("Y: {} group", plot_series_group_label(group)), group))
                    .or_else(|| self.plot_axis_series.as_ref()
                        .map(|axis| (format!("Y: {}", short_series_label(axis)), axis)));
                if let Some((text, full)) = axis {
                    // Only the space left beside the toolbar; never overlap the sidebar.
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let text = elide_start(ui, &text, &font, ui.available_width());
                    if text != "…" { ui.weak(text).on_hover_text(full); }
                }
            });
        });
        if self.plot_go_open {
            ui.horizontal(|ui| {
                ui.label("Sample index");
                let entry = ui.add(egui::TextEdit::singleline(&mut self.plot_jump_text)
                    .desired_width(140.0).hint_text("0-based index"));
                if focus_plot_go { entry.request_focus(); }
                let sample = self.plot_jump_text.trim().parse::<usize>().ok()
                    .filter(|index| *index < count);
                let enter = (entry.has_focus() || entry.lost_focus())
                    && ui.input(|input| input.key_pressed(egui::Key::Enter));
                if ui.add_enabled(sample.is_some(), egui::Button::new("Go")).clicked()
                    || enter && sample.is_some() {
                    self.plot_jump = sample.unwrap();
                    force_view = Some(centered_view(self.plot_jump, view.len().min(256).max(64), count));
                    if let Some(bytes) = sample_bytes(&node, self.plot_jump..self.plot_jump + 1) {
                        picked = Some((bytes, None));
                    }
                    self.plot_go_open = false;
                }
                if ui.button("×").on_hover_text("Close Go to sample").clicked()
                    || entry.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                    self.plot_go_open = false;
                }
                if sample.is_none() { ui.weak(format!("Enter 0–{}", crate::format::count(count - 1))); }
            });
        }
        if changed { self.plot_fit_y=true; }
        if let Some(forced) = &force_view {
            self.plot_paused = false;
            self.plot_view = Some(forced.clone());
        }
        let requested = self.plot_view.clone().unwrap_or(view);
        // Keep the last measured axis gutter, so resizing uses the new canvas
        // width immediately. Reduction follows physical pixels, including DPI.
        let canvas_width = (ui.available_width() - self.plot_axis_width).max(1.);
        let pixel_width = (canvas_width * ui.ctx().pixels_per_point()).ceil() as usize;
        let query_pixel_width = plot_query_pixel_width(pixel_width,
            visible_series.max(self.plot_sources.len()));
        if !names.is_empty() && visible_series == 0 {
            ui.horizontal(|ui| {
                ui.label("All plot series are hidden.");
                if ui.button("Show all series").clicked() {
                    for name in &names { self.plot_hidden.remove(name); }
                    self.plot_fit_y = true;
                    self.plot_paused = false;
                }
            });
        }
        let selected_fields = (!self.plot_catalog.is_empty()).then(|| self.plot_catalog.iter()
            .filter(|name| !self.plot_hidden.contains(&format!("{} / {name}", self.plot_target)))
            .cloned().collect::<Vec<_>>());
        if primary_group.is_none() && selected_fields.as_ref().is_some_and(Vec::is_empty) {
            self.plot_reader.stop();
        } else if !self.plot_paused {
            let buffered = primary_group.map(|(_, (group_node, plan, _))|
                (group_node.clone(), plan.clone())).or_else(|| {
                let plan = sensor_group_key(&node)
                    .and_then(|_| node.lazy.as_ref()?.numeric_plan())
                    .filter(|plan| selected_fields.as_ref().is_none_or(|selected|
                        selected.len() == plan.fields.len()))?;
                Some((node.clone(), plan))
            });
            if let Some((group_node, plan)) = buffered {
                if !self.plot_reader.matches_group_request(source.revision, &group_node, &plan,
                    requested.clone(), query_pixel_width, !self.plot_sampled_preview) {
                    self.plot_reader.request_group(ui.ctx(), source.clone(), group_node, plan,
                        requested.clone(), query_pixel_width, !self.plot_sampled_preview);
                }
            } else {
                self.plot_reader.request_filtered(
                    ui.ctx(), source.clone(), node.clone(), requested.clone(),
                    query_pixel_width, !self.plot_sampled_preview, selected_fields.clone(),
                );
            }
        }
        self.plot_reader.tick(ui.ctx());
        let mut extra_loading = 0;
        let mut queued_collections = 0;
        let mut active_readers = usize::from(self.plot_reader.is_loading())
            + self.extra_plot_readers.values().filter(|reader| reader.is_loading()).count()
            + self.group_plot_readers.values().filter(|reader| reader.is_loading()).count();
        for (key, (group_node, plan, paths)) in grouped_plans.iter() {
            if paths.iter().any(|path| path == &self.plot_target) { continue; }
            let reader = self.group_plot_readers.entry(key.clone()).or_default();
            reader.set_cache_budget(reader_budget);
            let was_loading = reader.is_loading();
            if !self.plot_paused && (was_loading || active_readers < plot_worker_count()) {
                let count = collection_count(group_node);
                let range = requested.start.min(count)..requested.end.min(count);
                if !reader.matches_group_request(source.revision, group_node, plan,
                    range.clone(), query_pixel_width, !self.plot_sampled_preview) {
                    reader.request_group(ui.ctx(), source.clone(), group_node.clone(), plan.clone(),
                        range, query_pixel_width, !self.plot_sampled_preview);
                }
            } else if !was_loading && !self.plot_paused {
                queued_collections += paths.len();
            }
            reader.tick(ui.ctx());
            if !was_loading && reader.is_loading() { active_readers += 1; }
            if let Some(error) = reader.error() { ui.colored_label(Color32::LIGHT_RED, format!("{key}: {error}")); }
            if reader.is_loading() { extra_loading += 1; }
        }
        for path in &self.plot_sources {
            if path == &self.plot_target { continue; }
            if grouped_paths.contains(path) { continue; }
            if let Some(&extra) = collection_by_path.get(path.as_str()) {
                let count = collection_count(&extra);
                let reader = self.extra_plot_readers.entry(path.clone()).or_default();
                reader.set_cache_budget(reader_budget);
                let was_loading = reader.is_loading();
                if !self.plot_paused && (was_loading || active_readers < plot_worker_count()) {
                    let range = requested.start.min(count)..requested.end.min(count);
                    if !reader.matches_background_request(source.revision, extra, range.clone(), query_pixel_width, !self.plot_sampled_preview) {
                        reader.request_background(ui.ctx(), source.clone(), extra.clone(), range, query_pixel_width, !self.plot_sampled_preview);
                    }
                } else if !was_loading && !self.plot_paused {
                    queued_collections += 1;
                }
                reader.tick(ui.ctx());
                if !was_loading && reader.is_loading() { active_readers += 1; }
                if let Some(error) = reader.error() { ui.colored_label(Color32::LIGHT_RED, format!("{path}: {error}")); }
                if reader.is_loading() { extra_loading += 1; }
            }
        }
        log_slow_plot_phase(&mut phase_clock, "schedule plot readers");
        ui.horizontal_wrapped(|ui| {
            if let Some(data) = self.plot_reader.display_data() {
                if data.quality != PlotQuality::Exact {
                    ui.small(data.quality.label()).on_hover_text(format!(
                        "Data covers sample indices {}–{}. Zoom in to see individual samples.",
                        data.range.start,
                        data.range.end.saturating_sub(1)
                    ));
                }
                if matches!(data.quality, PlotQuality::SampledPreview) {
                    ui.weak("Bands show observed ranges from sampled windows.");
                    if ui.button("Scan for all peaks").clicked() {
                        self.plot_sampled_preview = false;
                        self.plot_paused = false;
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
            }
            if extra_loading > 0 {
                ui.spinner();
                ui.weak(format!("{} loading", crate::format::plural(extra_loading as u64, "other collection", "other collections")));
            }
            if queued_collections > 0 {
                ui.weak(format!("{} queued", crate::format::plural(queued_collections as u64, "collection", "collections")));
            }
            if !self.plot_paused && (self.plot_reader.is_loading() || extra_loading > 0 || queued_collections > 0)
                && ui.button("Stop loading").clicked() {
                self.plot_reader.stop();
                for reader in self.extra_plot_readers.values_mut() { reader.stop(); }
                for reader in self.group_plot_readers.values_mut() { reader.stop(); }
                self.plot_paused = true;
            }
            if self.plot_catalog.len() > 32 {
                ui.weak(format!("{} available; select fields in Series",
                    crate::format::plural(self.plot_catalog.len() as u64, "field", "fields")));
            }
            if self.plot_paused { ui.weak("Stopped · zoom or Fit to read again"); }
        });
        if let Some(error) = self.plot_reader.error() {
            ui.colored_label(Color32::LIGHT_RED, error);
        }
        let mut overlay_hash = std::collections::hash_map::DefaultHasher::new();
        output.revision.hash(&mut overlay_hash);
        self.plot_target.hash(&mut overlay_hash);
        for path in &self.plot_sources { path.hash(&mut overlay_hash); }
        let mut hidden_fields: Vec<_> = self.plot_hidden.iter().collect();
        hidden_fields.sort_unstable();
        for field in hidden_fields { field.hash(&mut overlay_hash); }
        // Series scale and offset are applied when drawing, so moving a trace
        // vertically never rebuilds the overlay or the GPU and raster geometry.
        let overlay_key = overlay_hash.finish();
        if self.plot_overlay.as_ref().map(|(key, _)| *key) != Some(overlay_key) {
            self.plot_overlay = None;
            self.plot_overlay_generations.clear();
            self.plot_overlay_points.clear();
            self.plot_placed.clear();
            self.plot_overlay_revision = self.plot_overlay_revision.wrapping_add(1);
            self.plot_mesh_cache.clear();
            self.plot_raster.invalidate();
            self.plot_gpu.invalidate();
        }
        if let Some(raw) = self.plot_reader.display_data() {
            if self.plot_overlay.is_none() {
                self.plot_overlay = Some((overlay_key, PlotData {
                    range: raw.range.clone(),
                    series: BTreeMap::new(),
                    envelopes: BTreeMap::new(),
                    bounds: BTreeMap::new(),
                    quality: raw.quality,
                    scanned: raw.scanned,
                    total: raw.total,
                    complete: raw.complete,
                }));
            }
            let (_, combined) = self.plot_overlay.as_mut().unwrap();
            let primary_generation = self.plot_reader.data_generation();
            if self.plot_overlay_generations.get(&self.plot_target) != Some(&primary_generation) {
                if let Some((_, (_, _, paths))) = primary_group {
                    update_group_overlay(combined, &mut self.plot_overlay_points, paths, Some(raw),
                        &self.plot_hidden);
                } else {
                    update_overlay_source(combined, &mut self.plot_overlay_points, &self.plot_target,
                        Some(raw), &self.plot_hidden);
                }
                self.plot_mesh_cache.clear();
                self.plot_overlay_revision = self.plot_overlay_revision.wrapping_add(1);
                self.plot_overlay_generations.insert(self.plot_target.clone(), primary_generation);
                #[cfg(test)] { self.plot_overlay_builds += 1; }
            }
            combined.range = raw.range.clone();
            combined.scanned = raw.scanned;
            combined.total = raw.total;
            combined.quality = raw.quality;
            combined.complete = raw.complete;
            for (path, reader) in &self.extra_plot_readers {
                let extra = reader.display_data().filter(|_| collection_by_path.contains_key(path.as_str()));
                let generation = reader.data_generation();
                if self.plot_overlay_generations.get(path) != Some(&generation) {
                    update_overlay_source(combined, &mut self.plot_overlay_points, path, extra, &self.plot_hidden);
                    self.plot_mesh_cache.clear();
                    self.plot_overlay_revision = self.plot_overlay_revision.wrapping_add(1);
                    self.plot_overlay_generations.insert(path.clone(), generation);
                    #[cfg(test)] { self.plot_overlay_builds += 1; }
                }
                if let Some(extra) = extra {
                    if extra.quality == PlotQuality::SampledPreview { combined.quality = PlotQuality::SampledPreview; }
                    combined.complete &= extra.complete;
                }
            }
            for (key, (_group_node, _, paths)) in grouped_plans.iter() {
                if paths.iter().any(|path| path == &self.plot_target) { continue; }
                let Some(reader) = self.group_plot_readers.get(key) else { continue; };
                let extra = reader.display_data();
                let generation_key = format!("@group:{key}");
                let generation = reader.data_generation();
                if self.plot_overlay_generations.get(&generation_key) != Some(&generation) {
                    update_group_overlay(combined, &mut self.plot_overlay_points, paths, extra,
                        &self.plot_hidden);
                    self.plot_mesh_cache.clear();
                    self.plot_overlay_revision = self.plot_overlay_revision.wrapping_add(1);
                    self.plot_overlay_generations.insert(generation_key, generation);
                    #[cfg(test)] { self.plot_overlay_builds += 1; }
                }
                if let Some(extra) = extra {
                    if extra.quality == PlotQuality::SampledPreview { combined.quality = PlotQuality::SampledPreview; }
                    combined.complete &= extra.complete;
                }
            }
        } else {
            self.plot_overlay = None;
            self.plot_overlay_generations.clear();
            self.plot_overlay_points.clear();
            self.plot_placed.clear();
            self.plot_mesh_cache.clear();
            self.plot_raster.invalidate();
            self.plot_gpu.invalidate();
        }
        log_slow_plot_phase(&mut phase_clock, "assemble plot overlay");
        let placements = self.presentation.series.clone();
        let axis_placement = axis_names
            .first()
            .map(String::as_str)
            .map(|name| self.presentation.placement(name))
            .unwrap_or_default();
        let axis_uniform = axis_names.iter().all(|name| {
            let placement = self.presentation.placement(name);
            placement.scale == axis_placement.scale && placement.offset == axis_placement.offset
        });
        let data = self.plot_overlay.as_ref().map(|(_, data)| data);
        let plot_id = egui::Id::new(("decoded_structure_plot", "overlay", output.revision));
        let hidden = self.plot_hidden.clone();
        let y_fit = if (self.plot_fit_y || self.plot_fit_all_y) && self.plot_range_zoom.is_none() {
            data.and_then(|data| {
                numeric_bounds(
                    data.series
                        .iter()
                        .filter(|(name, _)| {
                            !hidden.contains(*name)
                                && (self.plot_fit_all_y || axis_names.is_empty() || axis_names.contains(name))
                        })
                        .flat_map(|(name, values)| {
                            let placement = self.presentation.placement(name);
                            values.iter().map(move |point| placement.display(point.value))
                        })
                        .chain(data.envelopes.iter()
                            .filter(|(name, _)| {
                                !hidden.contains(*name)
                                    && (self.plot_fit_all_y || axis_names.is_empty() || axis_names.contains(name))
                            })
                            .flat_map(|(name, bands)| {
                                let placement = self.presentation.placement(name);
                                bands.iter().flat_map(move |band|
                                    [placement.display(band.min.value), placement.display(band.max.value)])
                            })),
                )
            })
        } else {
            None
        };
        if y_fit.is_some() && data.is_some_and(|data| data.complete) {
            self.plot_fit_y = false;
            self.plot_fit_all_y = false;
        }
        let mut wheel_zoomed = false;
        let plot_ctx = ui.ctx().clone();
        let plot_outer = ui.available_rect_before_wrap();
        let plot_clip = ui.clip_rect();
        let plot_layer = ui.layer_id();
        let dot_radius = self.plot_dot_radius.unwrap_or(2.5);
        let mut axis_hover = None;
        let visible_trace_count = self.plot_overlay_points.keys()
            .filter(|name| !hidden.contains(*name)).count();
        let gpu_mode = data.is_some_and(|data| data.quality != PlotQuality::SampledPreview)
            && visible_trace_count >= 64
            && self.plot_gpu.update(&plot_ctx, &self.plot_overlay_points, &hidden,
                overlay_key ^ self.plot_overlay_revision.rotate_left(17));
        // The GPU applies placements itself; other paths draw placed copies that
        // are recomputed only for traces whose scale or offset changed.
        let placed = if gpu_mode { BTreeMap::new() } else {
            placed_points(&mut self.plot_placed, &self.plot_mesh_cache, &self.plot_overlay_points,
                &hidden, &self.presentation)
        };
        let axis_key = (self.plot_axis_series.clone(), self.plot_axis_group.clone(), axis_uniform);
        if self.plot_y_axis_key.as_ref() != Some(&axis_key) {
            self.plot_y_axis_key = Some(axis_key);
            self.plot_y_axis_thickness = 0.;
        }
        let [label_min, label_max] = crate::theme::DARK.size.plot_signal_label_spacing;
        // Leave room under the plot for the whole-file overview strip.
        let strip_height = crate::theme::DARK.size.overview_height + crate::theme::DARK.size.overview_gap;
        let plot_height = (ui.available_height() - strip_height).max(crate::theme::DARK.size.overview_height);
        let response = crate::theme::labeled_grid_spaced(Plot::new(plot_id), label_min)
            .id(plot_id)
            .height(plot_height)
            .auto_bounds(egui::Vec2b::new(false, false))
            .allow_double_click_reset(false)
            .allow_boxed_zoom(false)
            .allow_drag(egui::Vec2b::new(true, false))
            .allow_axis_zoom_drag(false)
            .pan_pointer_button(egui::PointerButton::Primary)
            .allow_zoom(egui::Vec2b::new(true, false))
            .allow_scroll(false)
            .label_formatter(move |position| {
                if let egui_plot::HoverPosition::NearDataPoint {
                    plot_name,
                    position,
                    ..
                } = position
                {
                    let p = placements.get(*plot_name).copied().unwrap_or_default();
                    Some(format!(
                        "{plot_name}\nSample: {:.0}\nValue: {}",
                        position.x,
                        p.original(position.y)
                    ))
                } else {
                    None
                }
            })
            .custom_y_axes(vec![AxisHints::new_y()
                .min_thickness(self.plot_y_axis_thickness)
                .label(if axis_uniform { "Selected series value" } else { "Display position" })
                .formatter(move |mark, _| {
                    format!("{:.2}", if axis_uniform { axis_placement.original(mark.value) }
                        else { mark.value })
                })])
            .custom_x_axes(vec![AxisHints::new_x()
                .label("Sample index")
                .label_spacing(egui::Rangef::new(label_min, label_max))
                .formatter(|mark, _| format!("{:.0}", mark.value))
                .tick_label_font(egui::FontId::new(
                    crate::theme::DARK.font.axis,
                    egui::FontFamily::Proportional,
                ))])
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
                    let raster_candidate = !gpu_mode && data.quality != PlotQuality::SampledPreview
                        && visible_trace_count >= 16
                        && (data.quality != PlotQuality::Exact
                            || requested.len() > pixel_width);
                    let raster_mode = raster_candidate && self.plot_raster.show(&plot_ctx, plot_ui,
                        &self.plot_overlay_points, &hidden,
                        overlay_key ^ self.plot_overlay_revision.rotate_left(17), &self.presentation);
                    for (name, values) in data.series.iter() {
                        if raster_mode || gpu_mode { break; }
                        if hidden.contains(name) {
                            continue;
                        }
                        let Some(points) = placed.get(name).map(|points| points.as_slice()) else { continue; };
                        let color = plot_series_color(name);
                        let placement = self.presentation.placement(name);
                        let bands = (data.quality == PlotQuality::SampledPreview).then(|| {
                            data.envelopes.get(name).into_iter().flatten().map(|band| [
                                band.range.start as f64 - 0.5, band.range.end as f64 - 0.5,
                                placement.display(band.min.value), placement.display(band.max.value),
                            ]).collect::<Vec<_>>()
                        });
                        if data.quality == PlotQuality::Exact && values.len() <= canvas_width as usize / 5 {
                            decoded_channel_borrowed(plot_ui, name, color, points, bands.as_deref(), visible_trace_count, self.plot_mesh_cache.clone());
                            if dot_radius > 0.0 {
                                plot_ui.points(Points::new(name, points).color(color).radius(dot_radius));
                            }
                        } else {
                            decoded_channel_borrowed(plot_ui, name, color, points, bands.as_deref(), visible_trace_count, self.plot_mesh_cache.clone());
                        }
                    }
                }
                let inner = plot_ui.response().rect;
                // Middle-button panning is handled directly. Switching the plot's
                // drag button while a pointer gesture is in progress is unreliable
                // with native input, since egui has already assigned the drag to
                // the response from the press frame.
                let (middle_pressed, middle_down, middle_delta, middle_origin) =
                    plot_ctx.input(|input| (
                        input.pointer.button_pressed(egui::PointerButton::Middle),
                        input.pointer.button_down(egui::PointerButton::Middle),
                        input.pointer.delta(),
                        input.pointer.press_origin(),
                    ));
                if middle_pressed {
                    self.plot_middle_panning = middle_origin.is_some_and(|origin|
                        inner.contains(origin) && plot_clip.contains(origin));
                }
                if !middle_down {
                    self.plot_middle_panning = false;
                } else if !middle_pressed
                    && self.plot_middle_panning
                    && middle_delta != egui::Vec2::ZERO
                {
                    let old = plot_ui.plot_bounds();
                    let x = -(middle_delta.x as f64) * old.width() / inner.width().max(1.) as f64;
                    if self.plot_free_y {
                        let amount =
                            -(middle_delta.y as f64) * old.height() / inner.height().max(1.) as f64;
                        for name in &axis_names {
                            self.presentation.pan_series(name, amount);
                        }
                    }
                    let mut min = old.min();
                    let mut max = old.max();
                    min[0] += x;
                    max[0] += x;
                    plot_ui.set_plot_bounds(PlotBounds::from_min_max(min, max));
                    self.plot_fit_y = false;
                    self.plot_fit_all_y = false;
                    plot_ctx.request_repaint();
                }
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
                let (primary_pressed, primary_down, primary_delta, primary_origin) = plot_ctx
                    .input(|input| {
                        (
                            input.pointer.button_pressed(egui::PointerButton::Primary),
                            input.pointer.button_down(egui::PointerButton::Primary),
                            input.pointer.delta(),
                            input.pointer.press_origin(),
                        )
                    });
                if primary_pressed {
                    self.plot_axis_panning = primary_origin.and_then(|origin| {
                        axis_rects.iter().enumerate().find_map(|(axis, rect)| {
                            rect.intersect(plot_clip).contains(origin).then_some(axis)
                        })
                    });
                }
                if !primary_down {
                    self.plot_axis_panning = None;
                } else if let Some(axis) = self.plot_axis_panning.filter(|_| !primary_pressed) {
                    if primary_delta[axis] != 0. {
                        let old = plot_ui.plot_bounds();
                        let mut min = old.min();
                        let mut max = old.max();
                        let pixels = if axis == 0 {
                            inner.width()
                        } else {
                            inner.height()
                        }
                        .max(1.);
                        let distance = primary_delta[axis] as f64
                            * if axis == 0 {
                                -old.width()
                            } else {
                                old.height()
                            }
                            / pixels as f64;
                        if axis == 1 {
                            for name in &axis_names {
                                self.presentation.pan_series(name, -distance);
                            }
                        } else {
                            min[axis] += distance;
                            max[axis] += distance;
                            plot_ui.set_plot_bounds(PlotBounds::from_min_max(min, max));
                        }
                        self.plot_fit_y = false;
                        self.plot_fit_all_y = false;
                        plot_ctx.request_repaint();
                    }
                }
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
                    let (scroll, pan_scroll, shift, pinch, pointer) = plot_ctx.input(|input| {
                        let scroll = wheel_pan_points(&input.raw.events);
                        let pan_scroll = scroll;
                        let shift = input.modifiers.shift;
                        (
                            scroll,
                            pan_scroll,
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
                            if axis == 1 {
                                for name in &axis_names {
                                    self.presentation.zoom_series(name, factor as f64, center.y);
                                }
                            } else {
                                plot_ui.zoom_bounds(egui::vec2(factor, 1.), center);
                            }
                            wheel_zoomed = factor != 1.;
                        } else if pinch == egui::Vec2::splat(1.) {
                            // egui-winit remaps Shift+wheel into horizontal scroll.
                            let delta = if shift {
                                pan_scroll.x + pan_scroll.y
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
                                    let fraction = (f64::from(delta)
                                        / if axis == 1 { 120. } else { 240. }).clamp(-1., 1.);
                                    let distance = if axis == 0 {
                                        -fraction * old.width()
                                    } else {
                                        fraction * old.height()
                                    };
                                    if axis == 1 {
                                        for name in &axis_names {
                                            self.presentation.pan_series(name, -distance);
                                        }
                                    } else {
                                        min[axis] += distance;
                                        max[axis] += distance;
                                        plot_ui.set_plot_bounds(PlotBounds::from_min_max(min, max));
                                    }
                                } else {
                                    let factor = (delta * if axis_hover.is_some_and(|(axis, _)| axis == 1)
                                        { 0.012 } else { 0.005 }).clamp(-1., 1.).exp();
                                    let axes = axis_hover.map_or(
                                        egui::Vec2b::new(true, self.plot_free_y),
                                        |(axis, _)| egui::Vec2b::new(axis == 0, axis == 1),
                                    );
                                    if axes.y {
                                        for name in &axis_names {
                                            self.presentation.zoom_series(
                                                name,
                                                factor as f64,
                                                center.y,
                                            );
                                        }
                                    }
                                    if axes.x {
                                        plot_ui.zoom_bounds(egui::vec2(factor, 1.), center);
                                    }
                                }
                                wheel_zoomed = true;
                            }
                        }
                    }
                }
            });
        response.response.clone().on_hover_text(
            "Drag or middle-drag to pan · Shift+wheel to pan horizontally · wheel or pinch to zoom · click a point to link Hex and Inspector"
        );
        if gpu_mode { self.plot_gpu.paint(ui.painter(), &response.transform, &self.presentation); }
        log_slow_plot_phase(&mut phase_clock, "draw plot");
        self.plot_axis_width = (plot_outer.width() - response.transform.frame().width()).max(0.);
        self.plot_y_axis_thickness = self.plot_y_axis_thickness
            .max(response.transform.frame().left() - plot_outer.left());
        if let Some((axis, rect)) = axis_hover {
            ui.interact(
                rect.intersect(plot_clip),
                plot_id.with(("axis_wheel", axis)),
                egui::Sense::hover(),
            )
            .on_hover_text(if axis == 0 {
                "X axis: drag or Shift+wheel to pan · wheel to zoom"
            } else {
                "Y axis: drag or Shift+wheel to pan · wheel to zoom"
            });
        }
        if response.response.dragged()
            || wheel_zoomed
            || response.response.hovered() && ui.input(|input| input.zoom_delta() != 1.0)
        {
            self.plot_fit_y = false;
            self.plot_fit_all_y = false;
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
            self.plot_paused = false;
            ui.ctx().request_repaint();
        }
        let frame = *response.transform.frame();
        {
            let catalog_hidden = |field: &str| hidden.contains(&format!("{} / {field}", self.plot_target));
            let fields = crate::overview_strip::scanned_fields(&self.plot_catalog, catalog_hidden);
            // Scan grouped channels the way the plot does, not as one combined series.
            let group = primary_group.map(|(_, (group_node, plan, _))| (group_node.clone(), plan.clone()))
                .or_else(|| sensor_group_key(&node).and_then(|_| node.lazy.as_ref()?.numeric_plan())
                    .map(|plan| (node.clone(), plan)));
            let loading = self.plot_reader.is_loading()
                || self.group_plot_readers.values().any(PlotReader::is_loading)
                || self.extra_plot_readers.values().any(PlotReader::is_loading);
            self.overview.update(ui.ctx(), &source, &node, count, fields, group, loading);
            ui.add_space(crate::theme::DARK.size.overview_gap);
            let issues = overview_issues(&node, count);
            match self.overview.show(ui, frame.x_range(), &node, count, actual.clone(), &issues) {
                Some(crate::overview_strip::Action::View(range)) => {
                    self.plot_force_view = Some(range);
                    ui.ctx().request_repaint();
                }
                Some(crate::overview_strip::Action::Fit) => {
                    self.plot_force_view = Some(0..count);
                    self.plot_fit_all_y = true;
                    self.plot_fit_y = false;
                    ui.ctx().request_repaint();
                }
                None => {}
            }
        }
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
                self.plot_fit_all_y = false;
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
                        self.plot_fit_all_y = false;
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
            let selection_color = ui.visuals().selection.stroke.color;
            let [red, green, blue, _] = selection_color.to_array();
            let right = right.max(left + 2.);
            let fill_alpha = (self.plot_range_fill_percent.unwrap_or(20) as f32 * 2.55).round() as u8;
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(left, frame.top()),
                    egui::pos2(right, frame.bottom()),
                ),
                0.,
                Color32::from_rgba_unmultiplied(red, green, blue, fill_alpha),
            );
            for x in [left, right] {
                painter.line_segment(
                    [egui::pos2(x, frame.top()), egui::pos2(x, frame.bottom())],
                    egui::Stroke::new(1.5, selection_color),
                );
            }
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
                if visible_series == 0 {
                    "All plot series are hidden; use Show all series above"
                } else if self.plot_reader.is_loading() {
                    "Reading samples for this view…"
                } else {
                    "No numeric samples in this range"
                },
                egui::TextStyle::Body.resolve(ui.style()),
                ui.visuals().text_color(),
            );
        }
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
                        sample_bytes(&node, anchor.min(index)..anchor.max(index) + 1)
                    {
                        picked = Some((bytes, None));
                    }
                } else {
                    let nearest =
                        data.into_iter()
                            .flat_map(|data| data.series.iter())
                            .filter(|(name, _)| !hidden.contains(*name))
                            .flat_map(|(name, values)| values.iter().map(move |point| (name, point)))
                            .filter_map(|(name, point)| {
                                let value = self.presentation.placement(name).display(point.value);
                                let screen = response.transform.position_from_point(
                                    &PlotPoint::new(point.index as f64, value),
                                );
                                let distance = screen.distance_sq(pointer);
                                (distance < 144.).then_some((distance, name, point))
                            })
                            .min_by(|a, b| a.0.total_cmp(&b.0));
                    if let Some((_, name, point)) = nearest {
                        self.plot_axis_series = Some(name.clone());
                        self.plot_axis_group = None;
                        self.plot_anchor = Some(point.index);
                        picked = Some((point.bytes.clone(), Some(point.path.to_string())));
                    } else if let Some(bytes) = sample_bytes(&node, index..index + 1) {
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
        if !self.plot_paused && (primary_group.is_some()
            || !selected_fields.as_ref().is_some_and(Vec::is_empty)) {
            let width = (response.transform.frame().width() * ui.ctx().pixels_per_point()).ceil() as usize;
            if let Some((_, (group_node, plan, _))) = primary_group {
                self.plot_reader.request_group(ui.ctx(), source, group_node.clone(), plan.clone(),
                    actual, width, !self.plot_sampled_preview);
            } else {
                self.plot_reader.request_filtered(ui.ctx(), source, node.clone(), actual,
                    width, !self.plot_sampled_preview, selected_fields);
            }
        }
        log_slow_plot_phase(&mut phase_clock, "handle plot interaction");
        if let Some((bytes, path)) = picked {
            if let Some(samples) = sample_indices_for_bytes(&node, &bytes) {
                self.plot_range_first = samples.start;
                self.plot_range_last = samples.end.saturating_sub(1);
                self.plot_jump = samples.start;
                self.plot_jump_text = samples.start.to_string();
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

fn plot_collection_choices(ui: &mut Ui, collections: &[&Node], samples: Option<&str>, selected: &mut std::collections::BTreeSet<String>, filter: &mut String) -> bool {
    let mut changed=false;
    let (single, groups) = grouped_collection_choices(collections);
    let grouped_containers: HashSet<_> = groups.keys().filter_map(|key|
        key.rsplit_once('.').map(|(path, _)| path.to_owned())).collect();
    ui.horizontal(|ui| {
        ui.label(format!("{} selected", crate::format::count(selected.len())));
        if ui.add_enabled(!selected.is_empty(), egui::Button::new("Clear selection")).clicked() {
            selected.clear();
            changed = true;
        }
    });
    ui.add(egui::TextEdit::singleline(filter).hint_text("Find collections or items…"));
    let query = filter.trim().to_lowercase();
    egui::ScrollArea::vertical()
        .id_salt("plot_collection_choices")
        .auto_shrink([false, false])
        .max_height(ui.available_height().max(100.0))
        .show(ui, |ui| {
                for node in single {
                    if grouped_containers.contains(&node.path) { continue; }
                    if !query.is_empty() && !node.name.to_lowercase().contains(&query)
                        && !node.path.to_lowercase().contains(&query) { continue; }
                    ui.push_id(&node.path, |ui| {
                        let mut enabled=selected.contains(&node.path);
                        if ui.checkbox(&mut enabled,collection_label(node, samples == Some(node.path.as_str())))
                            .on_hover_text(&node.path).changed() {
                            if enabled {selected.insert(node.path.clone());} else {selected.remove(&node.path);}
                            changed=true;
                        }
                    });
                }
                for (key, sensors) in groups {
                    let label = key.rsplit('.').nth(1).unwrap_or(&key);
                    let matches_group = query.is_empty() || label.to_lowercase().contains(&query)
                        || key.to_lowercase().contains(&query);
                    let matching: Vec<_> = sensors.into_iter().filter(|(index, node)| matches_group
                        || index.to_string().contains(&query)
                        || node.name.to_lowercase().contains(&query)
                        || node.path.to_lowercase().contains(&query)).collect();
                    if matching.is_empty() { continue; }
                    if matching.len() == 1 {
                        let (index, node) = matching[0];
                        ui.push_id(&node.path, |ui| {
                            let mut shown = selected.contains(&node.path);
                            if ui.checkbox(&mut shown, format!("{label} · Item {index}"))
                                .on_hover_text(&node.path).changed() {
                                if shown { selected.insert(node.path.clone()); }
                                else { selected.remove(&node.path); }
                                changed = true;
                            }
                        });
                        continue;
                    }
                    let enabled = matching.iter().filter(|(_, node)| selected.contains(&node.path)).count();
                    egui::CollapsingHeader::new(format!("{label} · {}/{} items", crate::format::count(enabled),
                        crate::format::count(matching.len())))
                        .id_salt(&key)
                        .default_open(!query.is_empty())
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui.small_button("Show group").clicked() {
                                    for (_, node) in &matching { changed |= selected.insert(node.path.clone()); }
                                }
                                if ui.small_button("Only group").clicked() {
                                    selected.clear();
                                    selected.extend(matching.iter().map(|(_, node)| node.path.clone()));
                                    changed = true;
                                }
                                if ui.small_button("Hide group").clicked() {
                                    for (_, node) in &matching { changed |= selected.remove(&node.path); }
                                }
                            });
                            egui::ScrollArea::vertical().id_salt(("plot_sensor_group", &key))
                                .max_height(260.)
                                .show_rows(ui, ui.spacing().interact_size.y, matching.len(), |ui, range| {
                                    for row in range {
                                        let (index, node) = matching[row];
                                        ui.push_id(&node.path, |ui| {
                                            ui.horizontal(|ui| {
                                                let mut shown = selected.contains(&node.path);
                                                if ui.checkbox(&mut shown, format!("Item {index}"))
                                                    .on_hover_text(&node.path).changed() {
                                                    if shown { selected.insert(node.path.clone()); }
                                                    else { selected.remove(&node.path); }
                                                    changed = true;
                                                }
                                                if ui.small_button("Only").on_hover_text("Plot only this sensor").clicked() {
                                                    selected.clear();
                                                    selected.insert(node.path.clone());
                                                    changed = true;
                                                }
                                            });
                                        });
                                    }
                                });
                        });
                }
            });
    changed
}

fn sensor_group_key(node: &Node) -> Option<(String, usize)> {
    crate::plot_series::collection_group(&node.name, &node.path)
}

fn grouped_numeric_plans(
    selected: &std::collections::BTreeSet<String>,
    collections: &HashMap<&str, &Node>,
) -> BTreeMap<String, (Node, NumericPlan, Vec<String>)> {
    let mut candidates: BTreeMap<String, Vec<(&Node, NumericPlan)>> = BTreeMap::new();
    for path in selected {
        let Some(&node) = collections.get(path.as_str()) else { continue; };
        let (Some((key, _)), Some(plan)) = (sensor_group_key(node), node.lazy.as_ref().and_then(|array| array.numeric_plan())) else { continue; };
        candidates.entry(key).or_default().push((node, plan));
    }
    let mut grouped = BTreeMap::new();
    for (key, mut members) in candidates {
        members.sort_by_key(|(_, plan)| plan.offset(0).unwrap_or(usize::MAX));
        let nodes: Vec<Node> = members.into_iter().map(|(node, _)| node.clone()).collect();
        let runs = crate::structure_plot::compatible_numeric_groups(&nodes);
        let multiple_runs = runs.len() > 1;
        for (index, (node, plan, paths)) in runs.into_iter().enumerate() {
            if paths.len() < 2 { continue; }
            let Some(plan) = plan else { continue; };
            let run_key = if multiple_runs { format!("{key}#{index}") } else { key.clone() };
            grouped.insert(run_key, (node, plan, paths));
        }
    }
    grouped
}
fn grouped_collection_choices<'a>(collections: &[&'a Node]) -> (Vec<&'a Node>, BTreeMap<String, Vec<(usize, &'a Node)>>) {
    let mut single = Vec::new();
    let mut groups: BTreeMap<String, Vec<(usize, &Node)>> = BTreeMap::new();
    for node in collections {
        if let Some((key, index)) = sensor_group_key(node) {
            groups.entry(key).or_default().push((index, node));
        } else {
            single.push(*node);
        }
    }
    for sensors in groups.values_mut() { sensors.sort_by_key(|(index, _)| *index); }
    (single, groups)
}

fn field_color(path: &str) -> Color32 {
    // A field keeps its color across sample indices and row/cell selection.
    let relative = path
        .rsplit_once(']')
        .map_or(path, |(_, suffix)| suffix)
        .trim_start_matches('.');
    crate::plot_trace::series_color(relative)
}
fn update_overlay_source(
    overlay: &mut PlotData,
    prepared: &mut BTreeMap<String, Arc<Vec<PlotPoint>>>,
    path: &str,
    source: Option<&PlotData>,
    hidden: &HashSet<String>,
) {
    let prefix = format!("{path} / ");
    overlay.series.retain(|name, _| !name.starts_with(&prefix));
    overlay.envelopes.retain(|name, _| !name.starts_with(&prefix));
    prepared.retain(|name, _| !name.starts_with(&prefix));
    let Some(source) = source else { return; };
    for (name, points) in &source.series {
        let full = format!("{prefix}{name}");
        if hidden.contains(&full) { continue; }
        let points = points.clone();
        prepared.insert(full.clone(), Arc::new(points.iter().map(|point| PlotPoint::new(point.index as f64, point.value)).collect()));
        overlay.series.insert(full, points);
    }
    for (name, bands) in &source.envelopes {
        let full = format!("{prefix}{name}");
        if hidden.contains(&full) { continue; }
        overlay.envelopes.insert(full, bands.clone());
    }
}
fn update_group_overlay(
    overlay: &mut PlotData,
    prepared: &mut BTreeMap<String, Arc<Vec<PlotPoint>>>,
    paths: &[String],
    source: Option<&PlotData>,
    hidden: &HashSet<String>,
) {
    let belongs = |name: &str| paths.iter().any(|path| name.starts_with(&format!("{path} / ")));
    overlay.series.retain(|name, _| !belongs(name));
    overlay.envelopes.retain(|name, _| !belongs(name));
    prepared.retain(|name, _| !belongs(name));
    let Some(source) = source else { return; };
    for (name, points) in &source.series {
        if hidden.contains(name) { continue; }
        let points = points.clone();
        prepared.insert(name.clone(), Arc::new(points.iter().map(|point| PlotPoint::new(point.index as f64, point.value)).collect()));
        overlay.series.insert(name.clone(), points);
    }
    for (name, bands) in &source.envelopes {
        if hidden.contains(name) { continue; }
        overlay.envelopes.insert(name.clone(), bands.clone());
    }
}
fn plot_series_color(name: &str) -> Color32 {
    crate::plot_trace::series_color(name)
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
/// Where the overview strip marks trouble: samples decoded only in part, and
/// the end of a collection whose decoding stopped early.
fn overview_issues(node: &Node, count: usize) -> Vec<(usize, String)> {
    let mut issues: Vec<(usize, String)> = node.children.iter().enumerate()
        .filter(|(_, child)| child.kind.starts_with("partial"))
        .map(|(index, _)| (index, format!("Sample {} is incomplete", crate::format::count(index))))
        .collect();
    if node.kind == "partial collection" && count > 0 {
        issues.push((count - 1, format!("Decoding stopped after sample {}", crate::format::count(count - 1))));
    }
    issues
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
fn collect_flattened_descendants(parent: &Node, node: &Node, source: Option<&ByteView>, output: &mut Vec<Node>) {
    for child in &node.children {
        if let Some(flat) = LazyArray::flattened_child(parent, child) {
            output.push(flat);
        }
        if let (Some(source), Some(groups)) = (source, child.lazy.as_ref()) {
            if !numeric_kind(&child.kind) && groups.count > 0 {
                if let Ok(first) = groups.page(source, &AtomicBool::new(false), 0, 1) {
                    if let Some(group_item) = first.first() {
                        for inner in &group_item.children {
                            let count = inner.lazy.as_ref().map_or(inner.children.len(), |array| array.count);
                            if count > 0 && (inner.lazy.is_some() || inner.children.iter().all(|value| value.numeric().is_some())) {
                                for index in 0..count {
                                    if let Some(flat) = LazyArray::flattened_group_sensor(parent, child, inner, index) {
                                        output.push(flat);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        collect_flattened_descendants(parent, child, source, output);
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
/// The samples collection is the numeric collection with the most elements.
/// Rows of every other collection are items.
fn samples_collection<'a>(
    collections: impl IntoIterator<Item = &'a Node>,
    output: &ParseOutput,
) -> Option<&'a Node> {
    collections
        .into_iter()
        .map(|node| (collection_score(node, output), node))
        .filter(|(score, _)| score.0)
        .max_by_key(|(score, _)| *score)
        .map(|(_, node)| node)
}
pub(crate) fn samples_collection_path(output: &ParseOutput) -> Option<String> {
    let mut collections = Vec::new();
    collect_collections(&output.nodes, &mut collections);
    samples_collection(collections, output).map(|node| node.path.clone())
}
fn row_noun(samples: bool, count: usize) -> &'static str {
    match (samples, count == 1) {
        (true, true) => "sample",
        (true, false) => "samples",
        (false, true) => "item",
        (false, false) => "items",
    }
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
fn flatten_visible<'a>(
    nodes: &'a [Node],
    depth: usize,
    expanded: &HashSet<String>,
    page: Option<&'a Page>,
    cached_pages: &'a [Page],
    revision: u64,
    rows: &mut Vec<(&'a Node, usize)>,
) {
    for node in nodes {
        rows.push((node, depth));
        if expanded.contains(&node.path) {
            flatten_visible(&node.children, depth + 1, expanded, page, cached_pages, revision, rows);
            if let Some(nodes) = page.into_iter().chain(cached_pages.iter().rev())
                .find(|page| page.key.path == node.path && page.key.revision == revision)
                .and_then(|page| page.result.as_ref().ok())
            {
                for child in nodes {
                    if !node.children.iter().any(|preview| preview.path == child.path) {
                        flatten_visible(std::slice::from_ref(child), depth + 1, expanded, page, cached_pages, revision, rows);
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



/// Shortens `text` from the start ("…tail") so it fits `max_width`. The end of a
/// series name is what tells neighbouring series apart; hover shows the full name.
fn elide_start(ui: &Ui, text: &str, font: &egui::FontId, max_width: f32) -> String {
    let width = |text: String| ui.fonts_mut(|fonts|
        fonts.layout_no_wrap(text, font.clone(), Color32::WHITE).size().x);
    if width(text.to_owned()) <= max_width {
        return text.to_owned();
    }
    let starts: Vec<usize> = text.char_indices().map(|(index, _)| index).skip(1).collect();
    // The shortest suffix that fits: later cuts are always narrower.
    let cut = starts.partition_point(|&start| width(format!("…{}", &text[start..])) > max_width);
    starts.get(cut).map_or_else(|| "…".to_owned(), |&start| format!("…{}", &text[start..]))
}
fn button_text_width(ui: &Ui) -> f32 {
    (ui.available_width() - 2. * ui.spacing().button_padding.x).max(0.)
}
fn short_series_label(name: &str) -> String {
    name.rsplit_once(" / ").map_or_else(
        || name.to_owned(),
        |(path, field)| format!("{} / {field}", path.rsplit('.').next().unwrap_or(path)),
    )
}

fn grouped_plot_series<'a>(
    names: impl Iterator<Item = &'a str>,
) -> BTreeMap<String, Vec<(String, &'a str)>> {
    let mut groups: BTreeMap<String, Vec<(String, &'a str)>> = BTreeMap::new();
    for name in names {
        let (path, field) = name.rsplit_once(" / ").unwrap_or((name, "value"));
        let indexed = plot_series_index(path);
        let (group, label) = if let Some((start, end)) = indexed {
            (
                path[..start].to_owned(),
                format!("{}{} / {field}", &path[start..=end], &path[end + 1..]),
            )
        } else {
            (path.to_owned(), field.to_owned())
        };
        groups.entry(group).or_default().push((label, name));
    }
    for entries in groups.values_mut() {
        entries.sort_by(|a, b| {
            let index = |label: &str| {
                label
                    .strip_prefix('[')
                    .and_then(|rest| rest.split_once(']'))
                    .and_then(|(digits, _)| digits.parse::<usize>().ok())
            };
            index(&a.0).cmp(&index(&b.0)).then_with(|| a.0.cmp(&b.0))
        });
    }
    groups
}

fn plot_series_index(path: &str) -> Option<(usize, usize)> {
    path.rfind('[').and_then(|start| {
        let end = path[start..].find(']')? + start;
        path[start + 1..end]
            .chars()
            .all(|c| c.is_ascii_digit())
            .then_some((start, end))
    })
}

fn plot_series_group_key(name: &str) -> String {
    let path = name.rsplit_once(" / ").map_or(name, |(path, _)| path);
    plot_series_index(path).map_or_else(|| path.to_owned(), |(start, _)| path[..start].to_owned())
}

fn plot_axis_target_names(
    names: &[String],
    group: Option<&str>,
    series: Option<&str>,
) -> Vec<String> {
    if let Some(group) = group {
        names
            .iter()
            .filter(|name| plot_series_group_key(name) == group)
            .cloned()
            .collect()
    } else {
        series.into_iter().map(str::to_owned).collect()
    }
}

fn plot_series_group_label(group: &str) -> &str {
    group.rsplit('.').next().unwrap_or(group)
}

fn centered_view(center: usize, width: usize, count: usize) -> Range<usize> {
    let width = width.max(1).min(count);
    let start = center
        .saturating_sub(width / 2)
        .min(count.saturating_sub(width));
    start..start + width
}

fn wheel_pan_points(events: &[egui::Event]) -> egui::Vec2 {
    events.iter().filter_map(|event| {
        if let egui::Event::MouseWheel { unit, delta, .. } = event {
            let scale = match unit {
                egui::MouseWheelUnit::Point => 1.,
                egui::MouseWheelUnit::Line => 48.,
                egui::MouseWheelUnit::Page => 240.,
            };
            Some(*delta * scale)
        } else {
            None
        }
    }).fold(egui::Vec2::ZERO, |sum, delta| sum + delta)
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
        current.filter(|current| current.len() <= PAGE_SIZE
            && current.start <= samples.start && current.end >= samples.end)
    {
        return current.clone();
    }
    if samples.len() > INITIAL_PLOT_SAMPLES {
        return padded_selection(samples, count);
    }
    let width = current
        .map_or(PAGE_SIZE, |current| current.len())
        .clamp(
            samples.len().saturating_add(16),
            PAGE_SIZE.max(samples.len().saturating_add(16)),
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

fn collection_label(node: &Node, samples: bool) -> String {
    let count = collection_count(node);
    format!("{} · {} {}", node.name, crate::format::count(count), row_noun(samples, count))
}

fn display(node: &Node, limit: usize) -> String {
    match &node.value {
        Some(Value::Float(value)) if limit <= 180 => crate::format::compact_float(*value),
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

fn tree_column_type(node: &Node) -> String {
    if let Some(array) = &node.lazy {
        format!("{}[{}]", node.kind.trim_end_matches("[]"), crate::format::count(array.count))
    } else if node.kind.ends_with("[]") {
        format!("{}[{}]", node.kind.trim_end_matches("[]"), crate::format::count(node.children.len()))
    } else if node.kind == "string" {
        format!("string[{}]", crate::format::count(node.range.end.saturating_sub(node.range.start)))
    } else {
        node.kind.clone()
    }
}

fn tree_enum_meaning<'a>(node: &Node, parent_kind: Option<&str>, program: Option<&'a Program>) -> Option<&'a str> {
    program?.enum_meaning(node, parent_kind)
}

fn tree_column_value(node: &Node, parent_kind: Option<&str>, program: Option<&Program>) -> String {
    if let Some(array) = &node.lazy { return crate::format::plural(array.count as u64, "item", "items"); }
    if let Some(value) = &node.value {
        let display = match value {
            Value::Signed(number) => crate::format::count(number),
            Value::Unsigned(number) => crate::format::count(number),
            Value::Float(number) => crate::format::compact_float(*number),
            Value::Text(text) => format!("{text:?}"),
            Value::Bytes(_) => display(node, 90),
        };
        return tree_enum_meaning(node, parent_kind, program)
            .map_or(display.clone(), |meaning| format!("{display} · {meaning}"));
    }
    if node.kind.ends_with("[]") || node.kind == "partial collection" {
        return crate::format::plural(node.children.len() as u64, "item", "items");
    }
    // An item's struct payload stands in for it: "12 entries", "marker PWAV-END".
    let item_payload = node.path.strip_suffix(&format!(".{}", node.name)).is_some_and(|parent| parent.ends_with(']'));
    if item_payload && is_struct(node) {
        if let Some(summary) = payload_summary(node) { return summary; }
    }
    if !node.name.starts_with('[') {
        return node.preview.as_ref().and_then(|preview| preview.split_once('=').map(|(_, value)| value.to_owned()))
            .unwrap_or_default();
    }
    let named = |names: &[&str]| node.children.iter()
        .find(|child| names.contains(&child.name.as_str()) && child.value.is_some());
    if named(&["name", "label"]).is_none() {
        if let Some(summary) = node.children.iter().filter(|child| is_struct(child)).find_map(payload_summary) {
            return summary;
        }
    }
    let identifying = named(&["name", "id", "label"])
        .or_else(|| node.children.iter().find(|child| child.value.is_some()));
    identifying.map(|child| {
        let value = match child.value.as_ref().unwrap() {
        Value::Text(text) => text.clone(),
        Value::Signed(number) => crate::format::count(number),
        Value::Unsigned(number) => crate::format::count(number),
        value => value.to_string(),
        };
        if child.name == "id" { format!("id {value}") } else { value }
    }).or_else(|| node.preview.as_ref().and_then(|preview| preview.split_once('=').map(|(_, value)| value.to_owned())))
        .unwrap_or_default()
}

fn is_struct(node: &Node) -> bool {
    node.value.is_none() && !is_collection(node) && !node.children.is_empty()
}

/// Summarizes a struct that wraps one nested collection ("31,249,935 records")
/// or carries a text marker ("marker PWAV-END").
fn payload_summary(node: &Node) -> Option<String> {
    if let [child] = node.children.as_slice() {
        if is_collection(child) {
            let singular = child.name.strip_suffix("ies").map(|stem| format!("{stem}y"))
                .or_else(|| child.name.strip_suffix('s').map(str::to_owned))
                .unwrap_or_else(|| child.name.clone());
            return Some(crate::format::plural(collection_count(child) as u64, &singular, &child.name));
        }
    }
    node.children.iter().find_map(|child| match &child.value {
        Some(Value::Text(text)) => Some(format!("{} {text}", child.name)),
        _ => None,
    })
}

fn tree_has_issues(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| node.kind.starts_with("partial") || tree_has_issues(&node.children))
}

fn paint_tree_text(ui: &Ui, rect: egui::Rect, text: &str, color: Color32, mono: bool, right: bool, size: f32) {
    if rect.width() <= 0. || !ui.is_rect_visible(rect) { return; }
    let font = egui::FontId::new(size, if mono { egui::FontFamily::Monospace } else { egui::FontFamily::Proportional });
    let position = if right { rect.right_center() - egui::vec2(6., 0.) } else { rect.left_center() + egui::vec2(4., 0.) };
    ui.painter().with_clip_rect(rect).text(position,
        if right { egui::Align2::RIGHT_CENTER } else { egui::Align2::LEFT_CENTER }, text, font, color);
}

fn paint_tree_columns(ui: &Ui, rect: egui::Rect, widths: [f32; 6], texts: [&str; 6], header: bool) {
    let mut left = rect.left();
    for (index, width) in widths.into_iter().enumerate() {
        let cell = egui::Rect::from_min_max(egui::pos2(left, rect.top()), egui::pos2(left + width, rect.bottom()));
        let color = if header || index == 1 || index == 3 || index == 4 {
            crate::theme::DARK.color.text.muted
        } else {
            crate::theme::DARK.color.text.primary
        };
        paint_tree_text(ui, cell, texts[index], color, !header && index >= 2, index == 4 && !header,
            if header { crate::theme::DARK.font.small } else { crate::theme::DARK.font.body });
        left += width;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ram_cache_budget_survives_reset_and_trims_every_reader() {
        let mut views = StructureViews::default();
        views.group_plot_readers.insert("group".into(), PlotReader::default());
        views.extra_plot_readers.insert("synthetic".into(), PlotReader::default());
        views.set_ram_cache_budget(0);
        assert_eq!(views.plot_reader.group_buffer_stats().0, 0);
        for reader in views.group_plot_readers.values().chain(views.extra_plot_readers.values()) {
            assert_eq!(reader.group_buffer_stats().0, 0);
        }
        views.reset();
        assert_eq!(views.ram_cache_bytes, Some(0));
        views.set_ram_cache_budget(2 * 1024 * 1024 * 1024);
        assert_eq!(views.ram_cache_bytes, Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn plot_series_groups_keep_sensor_identity() {
        let names = [
            "sections[3].payload.channels.sine[110].values / value",
            "sections[3].payload.channels.sine[111].values / value",
            "sections[3].payload.channels.triangle[0].values / value",
        ];
        let groups = super::grouped_plot_series(names.into_iter());
        assert_eq!(groups.len(), 2);
        let sine = &groups["sections[3].payload.channels.sine"];
        assert_eq!(sine[0].0, "[110].values / value");
        assert_eq!(sine[1].0, "[111].values / value");
        let names = names.map(str::to_owned);
        assert_eq!(
            super::plot_axis_target_names(
                &names,
                Some("sections[3].payload.channels.sine"),
                Some(&names[2])
            ),
            names[..2]
        );
    }
    const PAYLOAD_YAML: &str = "structures:
  - {name: Entry, fields: [{name: name, type: string, length: 2}]}
  - {name: List, fields: [{name: entries, type: {structure: Entry}, repeat: 2}]}
  - {name: Tail, fields: [{name: marker, type: string, length: 3}]}
  - name: Item
    fields:
      - {name: id, type: u8}
      - {name: size, type: u8}
      - name: payload
        type: {choose: {ref: id, cases: {'1': {structure: List}, '2': {structure: Tail}}, otherwise: bytes}}
        length: {ref: size}
actions:
  - read: {name: items, type: {structure: Item}, repeat: 3}
";
    const PAYLOAD_BYTES: &[u8] = b"\x01\x04abcd\x02\x03END\x07\x01z";

    fn tree_frame(views: &mut StructureViews, output: &ParseOutput, program: &Program, width: f32) -> Vec<String> {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.))),
            ..Default::default()
        }, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                views.tree(ui, output, Some(program), None, &mut |ui, node| ui.label(node.tree_label()));
            });
        }).shapes.into_iter().filter_map(|shape| match shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        }).collect()
    }

    #[test]
    fn structure_items_summarize_nested_payloads() {
        let program = Program::parse(PAYLOAD_YAML).unwrap();
        let store = ByteStore::from_snapshot(Original::from_bytes(PAYLOAD_BYTES).unwrap());
        let output = program.run(&store.view(), &AtomicBool::new(false)).unwrap();
        let items = &output.nodes[0].children;
        let values: Vec<_> = items.iter()
            .map(|item| tree_column_value(item, Some("Item[]"), Some(&program))).collect();
        assert_eq!(values, ["2 entries", "marker END", "id 7"]);
        assert_eq!(tree_column_value(&items[0].children[2], Some("Item"), Some(&program)), "2 entries");
        let entry = &items[0].children[2].children[0].children[0];
        assert_eq!(tree_column_value(entry, Some("Entry[]"), Some(&program)), "ab");
    }

    #[test]
    fn structure_status_and_type_columns_follow_issues_and_width() {
        let program = Program::parse(PAYLOAD_YAML).unwrap();
        let store = ByteStore::from_snapshot(Original::from_bytes(PAYLOAD_BYTES).unwrap());
        let output = program.run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        views.expanded.insert("items".into());
        let wide = tree_frame(&mut views, &output, &program, 1100.);
        assert!(wide.iter().any(|text| text == "Type"), "{wide:?}");
        assert!(!wide.iter().any(|text| text == egui_phosphor::regular::CHECK
            || text == egui_phosphor::regular::WARNING), "no status column without issues");
        let narrow = tree_frame(&mut views, &output, &program, 480.);
        assert!(!narrow.iter().any(|text| text == "Type" || text == "Item[3]"), "{narrow:?}");
        assert!(narrow.iter().any(|text| text == "Value"));

        let truncated = ByteStore::from_snapshot(Original::from_bytes(&PAYLOAD_BYTES[..9]).unwrap());
        let failure = program.run_with_partial(&truncated.view(), &AtomicBool::new(false)).unwrap_err();
        assert!(tree_has_issues(&failure.output.nodes));
        let partial = tree_frame(&mut views, &failure.output, &program, 1100.);
        assert!(partial.iter().any(|text| text == egui_phosphor::regular::WARNING), "{partial:?}");
        assert!(!partial.iter().any(|text| text == "!" || text == "✓"));
    }

    #[test]
    fn structure_row_columns_and_full_width_selection_render() {
        let yaml = "structures:\n  - name: Header\n    fields:\n      - {name: signature, type: string, length: 4}\nactions:\n  - read: {name: header, type: {structure: Header}}\n";
        let program = Program::parse(yaml).unwrap();
        let store = ByteStore::from_snapshot(Original::from_bytes(b"PWAV").unwrap());
        let output = program.run(&store.view(), &AtomicBool::new(false)).unwrap();
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut views = StructureViews::default();
        views.expanded.insert("header".into());
        let mut frame = |events| ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100., 700.))),
            events,
            ..Default::default()
        }, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let selection = views.last_hex_selection.clone();
                views.tree(ui, &output, Some(&program), selection, &mut |ui, node| ui.label(node.tree_label()));
            });
        });
        let first = frame(vec![]);
        let labels: Vec<_> = first.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        }).collect();
        for label in ["Name", "Type", "Value", "Offset", "Size", "0x00000000", "4 B"] {
            assert!(labels.iter().any(|item| item == label), "missing {label}: {labels:?}");
        }
        assert!(first.shapes.iter().any(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == egui_phosphor::regular::CARET_DOWN =>
                text.galley.job.sections.iter().any(|section|
                    section.format.font_id.family == egui::FontFamily::Name("phosphor".into())),
            _ => false,
        }), "tree chevrons must use the Phosphor font");
        let mut offset_rows: Vec<_> = first.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "0x00000000" => Some(text.pos.y),
            _ => None,
        }).collect();
        offset_rows.sort_by(f32::total_cmp);
        assert_eq!(offset_rows.len(), 2);
        assert!(((offset_rows[1] - offset_rows[0]) - 24.).abs() < 1.,
            "tree rows should be 24 px high: {offset_rows:?}");
        let offset = first.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "0x00000000" =>
                Some(text.pos + text.galley.rect.center().to_vec2()),
            _ => None,
        }).unwrap();
        frame(vec![egui::Event::PointerMoved(offset), egui::Event::PointerButton {
            pos: offset, button: egui::PointerButton::Primary, pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        frame(vec![egui::Event::PointerButton {
            pos: offset, button: egui::PointerButton::Primary, pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        let selected = frame(vec![]);
        drop(frame);
        assert_eq!(views.selected.as_deref(), Some("header"));
        assert!(selected.shapes.iter().any(|shape| match &shape.shape {
            egui::Shape::Rect(rect) => rect.rect.width() >= 920.
                && rect.fill == crate::theme::DARK.color.accent.base.gamma_multiply(crate::theme::DARK.color.accent.selection_alpha),
            _ => false,
        }), "selected row must be highlighted across all columns");
    }
    #[test]
    fn enum_meanings_come_from_a_define_that_reads_the_fields_collection() {
        let yaml = "structures:\n  - name: Field\n    fields:\n      - {name: name, type: string, length: 4}\n      - {name: format, type: u32}\n  - name: Header\n    fields:\n      - {name: entries, type: {structure: Field}, repeat: 2}\n    actions:\n      - define:\n          name: Sample\n          from: entries\n          fields:\n            name: {ref: name}\n            type: {ref: format, cases: {'3': f64, '5': u32}}\nactions:\n  - read: {name: header, type: {structure: Header}}\n";
        let program = Program::parse(yaml).unwrap();
        let mut bytes = Vec::new();
        for (name, format) in [(b"sine", 3_u32), (b"ramp", 5)] {
            bytes.extend_from_slice(name);
            bytes.extend_from_slice(&format.to_le_bytes());
        }
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = program.run(&store.view(), &AtomicBool::new(false)).unwrap();
        let format = |item: usize| find_node(&output.nodes, &format!("header.entries[{item}].format")).unwrap().clone();
        assert_eq!(tree_column_value(&format(0), Some("Field"), Some(&program)), "3 · f64");
        assert_eq!(tree_column_value(&format(1), Some("Field"), Some(&program)), "5 · u32");
    }
    #[test]
    fn structure_columns_show_item_names_enum_meanings_and_readable_numbers() {
        let yaml = "structures:\n  - name: Field\n    fields:\n      - {name: name, type: string, length: 4}\n      - {name: format, type: u32}\n      - {name: data, type: {choose: {ref: format, cases: {'3': f64}}}}\nactions:\n  - read: {name: entries, type: {structure: Field}, repeat: 2}\n";
        let program = Program::parse(yaml).unwrap();
        let mut bytes = Vec::new();
        for name in [b"sine", b"ramp"] {
            bytes.extend_from_slice(name);
            bytes.extend_from_slice(&3_u32.to_le_bytes());
            bytes.extend_from_slice(&1_f64.to_le_bytes());
        }
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = program.run(&store.view(), &AtomicBool::new(false)).unwrap();
        let entries = &output.nodes[0];
        let items = entries.lazy.as_ref().map_or_else(|| Ok(entries.children.clone()), |array|
            array.page(&store.view(), &AtomicBool::new(false), 0, 2)).unwrap();
        assert_eq!(tree_column_type(entries), "Field[2]");
        assert_eq!(tree_column_value(&items[0], None, Some(&program)), "sine");
        assert_eq!(tree_column_value(&items[1], None, Some(&program)), "ramp");
        assert_eq!(tree_column_value(&items[0].children[1], Some("Field"), Some(&program)), "3 · f64");
        assert_eq!(tree_column_value(&items[0].children[0], Some("Field"), Some(&program)), "\"sine\"");
        assert_eq!(crate::format::count(2_000_000_000), "2,000,000,000");
        assert_eq!(crate::format::size(2_000_000_000u64), "1.86 GiB");
    }
    #[test]
    fn many_visible_traces_share_a_bounded_query_resolution() {
        use super::plot_query_pixel_width;
        assert_eq!(plot_query_pixel_width(1920, 1), 1920);
        assert_eq!(plot_query_pixel_width(1920, 36), 1920);
        // 528 channels still get about one min/max bucket per two pixels.
        assert!((900..1920).contains(&plot_query_pixel_width(1920, 528)), "{}", plot_query_pixel_width(1920, 528));
        assert!(plot_query_pixel_width(1920, 528) > 0);
    }
    #[test]
    fn a_new_decode_waits_for_an_explicit_plot_collection() {
        let yaml = "actions: [{read: {name: samples, type: u8, repeat: 128}}]";
        let store = ByteStore::from_snapshot(Original::from_bytes(&[7; 128]).unwrap());
        let output = Program::parse(yaml).unwrap()
            .run(&store.view(), &AtomicBool::new(false)).unwrap();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        views.plot_collections_open = true;
        ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200., 700.))),
            ..Default::default()
        }, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                views.plot(ui, &output, store.view());
            });
        });
        assert!(views.plot_sources.is_empty());
        assert!(views.plot_reader.data().is_none());
        let sidebar = egui::containers::panel::PanelState::load(
            &ctx, egui::Id::new("plot_collections_sidebar"),
        ).unwrap();
        assert!(sidebar.size().x <= 381., "sidebar has unused horizontal space: {sidebar:?}");
    }

    #[test]
    fn go_to_sample_keeps_its_field_open_and_zooms_to_the_entered_index() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_go_open = true;
        views.plot_jump_text.clear();
        let frame = |views: &mut StructureViews, events| {
            ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO,
                    egui::vec2(1200., 700.))),
                events,
                ..Default::default()
            }, |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    views.plot(ui, &output, store.view());
                });
            })
        };
        let output_frame = frame(&mut views, vec![]);
        let label = output_frame.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "Sample index" => Some(text),
            _ => None,
        }).expect("Go to sample field should be visible");
        let field = egui::pos2(label.pos.x + label.galley.rect.width() + 24.,
            label.pos.y + label.galley.rect.height() / 2.);
        frame(&mut views, vec![egui::Event::PointerMoved(field)]);
        frame(&mut views, vec![egui::Event::PointerButton { pos: field,
            button: egui::PointerButton::Primary, pressed: true,
            modifiers: egui::Modifiers::NONE }]);
        frame(&mut views, vec![egui::Event::PointerButton { pos: field,
            button: egui::PointerButton::Primary, pressed: false,
            modifiers: egui::Modifiers::NONE }]);
        assert!(views.plot_go_open, "clicking the field must not close Go to sample");
        frame(&mut views, vec![egui::Event::Text("400".into())]);
        assert_eq!(views.plot_jump_text, "400");
        frame(&mut views, vec![egui::Event::Key { key: egui::Key::Enter,
            physical_key: None, pressed: true, repeat: false,
            modifiers: egui::Modifiers::NONE }]);
        assert_eq!(views.plot_jump, 400);
        assert!(!views.plot_go_open);
        let range = views.plot_view.as_ref().unwrap();
        assert!(range.contains(&400) && range.len() <= 256, "jump must zoom near sample 400: {range:?}");
    }

    #[test]
    fn series_controls_live_in_the_signal_sidebar() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_collections_open = true;
        views.plot_sidebar_series = true;
        let frame = ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO,
                egui::vec2(1200., 700.))),
            ..Default::default()
        }, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                views.plot(ui, &output, store.view());
            });
        });
        let text: Vec<_> = frame.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        }).collect();
        assert!(text.iter().any(|label| label.starts_with("Y axis: ")));
        assert!(!text.iter().any(|label| label == "Y"),
            "each series row should use its name, not a separate Y button");
        assert!(egui::containers::panel::PanelState::load(&ctx,
            egui::Id::new("plot_collections_sidebar")).is_some());
    }

    #[test]
    fn wide_numeric_collection_starts_with_bounded_visible_fields() {
        let mut yaml = String::from("structures:\n  - name: Record\n    fields:\n");
        for index in 0..128 {
            yaml.push_str(&format!("      - {{name: sensor_{index}, type: u16}}\n"));
        }
        yaml.push_str("actions:\n  - read: {name: records, type: {structure: Record}, repeat: 512}\n");
        let store = ByteStore::from_snapshot(Original::from_bytes(&vec![0; 512 * 128 * 2]).unwrap());
        let output = Program::parse(&yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while views.plot_catalog.is_empty() {
            render_plot(&ctx, &mut views, &output, &store, vec![], Default::default());
            assert!(std::time::Instant::now() < deadline, "plot catalog timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(views.plot_catalog.len(), 128);
        assert_eq!(views.plot_hidden.len(), 112);
        assert_eq!(views.plot_reader.requested_fields().unwrap().len(), 16);
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(views.plot_reader.data().unwrap().series.len(), 16);
        for name in &views.plot_catalog {
            views.plot_hidden.insert(format!("{} / {name}", views.plot_target));
        }
        views.plot_catalog_target.clear();
        render_plot(&ctx, &mut views, &output, &store, vec![], Default::default());
        assert_eq!(views.plot_reader.requested_fields().unwrap().len(), 16);
    }
    #[test]
    fn positioned_series_clicks_keep_original_values_and_bytes() {
        let ctx=egui::Context::default();
        let store=ByteStore::from_snapshot(Original::from_bytes(crate::demos::Demo::Advanced.bytes()).unwrap());
        let output=Program::parse(crate::demos::Demo::Advanced.yaml()).unwrap().run(&store.view(),&AtomicBool::new(false)).unwrap();
        let mut views=StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        let raw=views.extra_plot_readers["measurements"].data().unwrap().series["temperature"][10].value;
        views.presentation.series.insert("measurements / temperature".into(),crate::plot_presentation::Placement {scale:0.5,offset:200.});
        views.plot_axis_series = Some("measurements / temperature".into());
        views.plot_fit_y=true;
        render_plot(&ctx,&mut views,&output,&store,vec![],Default::default());
        let selected=click_plot(&ctx,&mut views,&output,&store,10.,raw*0.5+200.,false);
        assert_eq!(selected,Some(6240..6244));
        assert_eq!(views.plot_axis_series.as_deref(), Some("measurements / temperature"));
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

    #[test]
    fn updating_one_plot_collection_keeps_other_prepared_series() {
        fn source(value: f64) -> PlotData {
            PlotData {
                range: 0..1,
                series: BTreeMap::from([("value".into(), vec![crate::structure_plot::PlotSample {
                    index: 0, value, bytes: 0..2, path: "value[0]".into(),
                }])]),
                envelopes: BTreeMap::new(),
                bounds: BTreeMap::new(),
                quality: PlotQuality::Exact,
                scanned: 1,
                total: 1,
                complete: true,
            }
        }
        let mut overlay = source(0.0);
        overlay.series.clear();
        let mut prepared = BTreeMap::new();
        let hidden = HashSet::new();
        update_overlay_source(&mut overlay, &mut prepared, "a", Some(&source(1.0)), &hidden);
        let point_ptr = overlay.series["a / value"].as_ptr();
        let geometry_ptr = prepared["a / value"].as_ptr();
        update_overlay_source(&mut overlay, &mut prepared, "b", Some(&source(2.0)), &hidden);
        assert_eq!(overlay.series["a / value"].as_ptr(), point_ptr);
        assert_eq!(prepared["a / value"].as_ptr(), geometry_ptr);
        assert_eq!(overlay.series["b / value"][0].value, 2.0);
        update_overlay_source(&mut overlay, &mut prepared, "b", None, &hidden);
        assert!(!overlay.series.contains_key("b / value"));
        assert!(!prepared.contains_key("b / value"));
    }

    fn array() -> (ByteStore, ParseOutput) {
        let bytes: Vec<_> = (0_u16..1024).flat_map(u16::to_le_bytes).collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let program =
            Program::parse("actions:\n  - read: {name: items, type: u16, repeat: 1024}\n").unwrap();
        let output = program.run(&store.view(), &AtomicBool::new(false)).unwrap();
        (store, output)
    }
    #[test]
    fn nested_collection_discovery_scans_a_resident_page_once() {
        let (store, output) = array();
        let parent = &output.nodes[0];
        let mut views = StructureViews::default();
        for start in [0, 128] {
            let rows = parent.lazy.as_ref().unwrap().page(
                &store.view(), &AtomicBool::new(false), start, 1).unwrap();
            views.page = Some(Page {
                key: PageKey { path: parent.path.clone(), start, revision: output.revision },
                result: Ok(rows),
            });
            views.discover_flattened_collections(&output);
            views.discover_flattened_collections(&output);
            assert_eq!(views.discovery_page_scans, 1 + usize::from(start != 0));
            assert!(views.discovered_pages.len() <= 1);
        }
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
    fn nested_samples_are_discovered_without_expanding_the_tree() {
        let yaml = "structures:\n  - name: Item\n    fields:\n      - {name: value, type: u8}\n  - name: Group\n    fields:\n      - {name: prefix, type: bytes, length: 2}\n      - {name: items, type: {structure: Item}, repeat: 256}\nactions:\n  - read: {name: groups, type: {structure: Group}, repeat: 130}\n";
        let store = ByteStore::from_snapshot(Original::from_bytes(&vec![7; 130 * 258]).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        assert!(views.expanded.is_empty());
        let ctx = egui::Context::default();
        views.start_nested_discovery(&output, store.view(), &ctx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while views.flattened_collections.is_empty() {
            assert!(std::time::Instant::now() < deadline, "discovery timed out: {:?}", views.discovery_error);
            views.discover_flattened_collections(&output);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(views.plot_sources.is_empty());
        assert_eq!(views.plot_target, "groups.items");
        assert_eq!(views.table_target, "groups.items");
        views.plot_sources.insert("groups.items".into());
        views.plot_target = "groups.items".into();
        views.invalidate();
        views.start_nested_discovery(&output, store.view(), &ctx);
        while views.flattened_collections.is_empty() {
            assert!(std::time::Instant::now() < deadline, "rediscovery timed out: {:?}", views.discovery_error);
            views.discover_flattened_collections(&output);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(views.plot_sources.contains("groups.items"));
    }
    #[test]
    fn nested_channel_arrays_form_virtual_series_across_records() {
        let yaml = "structures:\n  - name: Payload\n    fields:\n      - {name: left, type: i16, repeat: 256}\n      - {name: right, type: i16, repeat: 256}\n  - name: Block\n    fields:\n      - {name: header, type: bytes, length: 2}\n      - {name: payload, type: {structure: Payload}}\nactions:\n  - read: {name: blocks, type: {structure: Block}, repeat: 130}\n";
        let mut bytes = Vec::new();
        for block in 0..130_i16 {
            bytes.extend([0xAA, 0xBB]);
            for index in 0..512_i16 { bytes.extend((block * 100 + index).to_le_bytes()); }
        }
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        let ctx = egui::Context::default();
        views.start_nested_discovery(&output, store.view(), &ctx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while views.flattened_collections.is_empty() {
            assert!(std::time::Instant::now() < deadline, "discovery timed out: {:?}", views.discovery_error);
            views.discover_flattened_collections(&output);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let left = views.flattened_collections.iter().find(|node| node.path == "blocks.payload.left").unwrap();
        let right = views.flattened_collections.iter().find(|node| node.path == "blocks.payload.right").unwrap();
        assert_eq!(left.lazy.as_ref().unwrap().count, 130 * 256);
        assert_eq!(right.lazy.as_ref().unwrap().count, 130 * 256);
        let boundary = left.lazy.as_ref().unwrap().page(&store.view(), &AtomicBool::new(false), 255, 2).unwrap();
        assert_eq!(boundary.iter().map(Node::numeric).collect::<Vec<_>>(), [Some(255.), Some(100.)]);
        assert_eq!(left.lazy.as_ref().unwrap().bounds(256), Some(1028..1030));
    }
    #[test]
    fn contiguous_sensor_collections_group_by_channel_and_sort_numerically() {
        let yaml = "structures:\n  - name: Sensors\n    fields:\n      - {name: sensor_10, type: u16, repeat: 4096}\n      - {name: sensor_2, type: u16, repeat: 4096}\n  - name: Block\n    fields:\n      - {name: Primary, type: {structure: Sensors}}\n      - {name: AUX, type: {structure: Sensors}}\nactions:\n  - read: {name: blocks, type: {structure: Block}, repeat: 130}\n";
        let bytes = vec![0; 130 * 4 * 4096 * 2];
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let source = store.view();
        let output = Program::parse(yaml).unwrap().run(&source, &AtomicBool::new(false)).unwrap();
        let parent = &output.nodes[0];
        let first = parent.lazy.as_ref().unwrap().page(&source, &AtomicBool::new(false), 0, 1).unwrap();
        let mut flat = Vec::new();
        collect_flattened_descendants(parent, &first[0], Some(&source), &mut flat);
        let refs: Vec<_> = flat.iter().collect();
        let (single, groups) = grouped_collection_choices(&refs);
        assert!(single.is_empty());
        assert_eq!(groups.len(), 2);
        for channel in ["Primary", "AUX"] {
            let sensors = &groups[&format!("blocks.{channel}.sensors")];
            assert_eq!(sensors.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [2, 10]);
            assert_eq!(collection_count(sensors[0].1), 130 * 4096);
        }
        let mut views = StructureViews::default();
        views.flattened_collections = flat;
        let chips = views.table_chips(&output);
        for channel in ["Primary", "AUX"] {
            let channel = chips.iter().flat_map(|chip| &chip.channels)
                .find(|candidate| candidate.label == channel).unwrap();
            assert_eq!(channel.members.len(), 2, "one channel per sensor group, not per sensor");
        }
        // All channels share one chip; the real collection keeps its own.
        let with_channels: Vec<_> = chips.iter().filter(|chip| !chip.channels.is_empty()).collect();
        assert_eq!(with_channels.len(), 1, "channels must not each become a chip");
        assert_eq!(with_channels[0].channels.len(), 2);
        assert!(chips.iter().any(|chip| chip.channels.is_empty() && chip.label.starts_with("Blocks (130")),
            "{:?}", chips.iter().map(|chip| &chip.label).collect::<Vec<_>>());
    }
    #[test]
    fn overview_strip_scans_grouped_channels_one_series_each() {
        let yaml = r#"
structures:
  - name: Descriptor
    fields:
      - {name: width, type: u8}
      - {name: groups, type: u8}
  - name: Metadata
    fields:
      - {name: items_per_group, type: u8}
      - {name: descriptors, type: {structure: Descriptor}, repeat: 1}
    actions:
      - define:
          name: Generated
          from: descriptors
          fields:
            type: {ref: width, cases: {'2': i16}}
            repeat: {ref: groups}
            items_per_group: {ref: items_per_group}
            layout: interleaved
  - name: Block
    fields:
      - {name: header, type: bytes, length: 2}
      - {name: payload, type: {structure: Generated}}
actions:
  - read: {name: metadata, type: {structure: Metadata}}
  - read: {name: blocks, type: {structure: Block}, repeat: 130}
"#;
        let mut bytes = vec![48, 2, 130];
        for block in 0..130_i16 {
            bytes.extend([0xAA, 0xBB]);
            for index in 0..6_240_i16 { bytes.extend((block * 100 + index).to_le_bytes()); }
        }
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        let ctx = egui::Context::default();
        views.request(&output.nodes[1], 0, output.revision, &ctx);
        settle(&mut views, store.view());
        views.discover_flattened_collections(&output);
        assert!(views.flattened_collections.iter().any(|node| node.path == "blocks.payload.field_0"));
        views.start_nested_discovery(&output, store.view(), &ctx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !views.flattened_collections.iter().any(|node| node.path == "blocks.payload.field_0[2].values") {
            assert!(std::time::Instant::now() < deadline, "discovery timed out: {:?}", views.discovery_error);
            views.discover_flattened_collections(&output);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let references: Vec<_> = views.flattened_collections.iter().collect();
        let (_, groups) = grouped_collection_choices(&references);
        let sensors = &groups["blocks.payload.field_0.values"];
        let selected: std::collections::BTreeSet<String> = sensors.iter().take(3)
            .map(|(_, node)| node.path.clone()).collect();
        let by_path: HashMap<&str, &Node> = references.iter().map(|node| (node.path.as_str(), *node)).collect();
        let grouped = grouped_numeric_plans(&selected, &by_path);
        let (group_node, group_plan, _) = &grouped["blocks.payload.field_0.values"];
        let count = collection_count(group_node);
        let mut strip = crate::overview_strip::OverviewStrip::default();
        let mut bands = None;
        while bands.is_none() {
            assert!(std::time::Instant::now() < deadline, "overview scan timed out");
            strip.update(&ctx, &store.view(), group_node, count, Vec::new(),
                Some((group_node.clone(), group_plan.clone())), false);
            bands = strip.envelope().filter(|envelope| envelope.complete).map(|envelope| envelope.bands.clone());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let bands = bands.unwrap();
        assert_eq!(bands.len(), 3, "one band per channel, not one combined series: {:?}", bands.keys());
        assert!(bands.keys().all(|name| name.contains("field_0[")));
    }
    #[test]
    fn configured_groups_expose_each_position_across_records() {
        let yaml = r#"
structures:
  - name: Descriptor
    fields:
      - {name: width, type: u8}
      - {name: groups, type: u8}
  - name: Metadata
    fields:
      - {name: items_per_group, type: u8}
      - {name: descriptors, type: {structure: Descriptor}, repeat: 1}
    actions:
      - define:
          name: Generated
          from: descriptors
          fields:
            type: {ref: width, cases: {'2': i16}}
            repeat: {ref: groups}
            items_per_group: {ref: items_per_group}
            layout: interleaved
  - name: Block
    fields:
      - {name: header, type: bytes, length: 2}
      - {name: payload, type: {structure: Generated}}
actions:
  - read: {name: metadata, type: {structure: Metadata}}
  - read: {name: blocks, type: {structure: Block}, repeat: 130}
"#;
        let mut bytes = vec![48, 2, 130];
        for block in 0..130_i16 {
            bytes.extend([0xAA, 0xBB]);
            for index in 0..6_240_i16 { bytes.extend((block * 100 + index).to_le_bytes()); }
        }
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        let ctx = egui::Context::default();
        views.request(&output.nodes[1], 0, output.revision, &ctx);
        settle(&mut views, store.view());
        views.discover_flattened_collections(&output);
        assert!(views.flattened_collections.iter().any(|node| node.path == "blocks.payload.field_0"));
        views.start_nested_discovery(&output, store.view(), &ctx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !views.flattened_collections.iter().any(|node| node.path == "blocks.payload.field_0[2].values") {
            assert!(std::time::Instant::now() < deadline, "discovery timed out: {:?}", views.discovery_error);
            views.discover_flattened_collections(&output);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let references: Vec<_> = views.flattened_collections.iter().collect();
        let (_, groups) = grouped_collection_choices(&references);
        let sensors = &groups["blocks.payload.field_0.values"];
        assert_eq!(sensors.len(), 48);
        assert_eq!(sensors[2].0, 2);
        let selected: std::collections::BTreeSet<String> = sensors.iter().take(3)
            .map(|(_, node)| node.path.clone()).collect();
        let by_path: HashMap<&str, &Node> = references.iter()
            .map(|node| (node.path.as_str(), *node)).collect();
        let grouped = grouped_numeric_plans(&selected, &by_path);
        let (group_node, group_plan, group_paths) = &grouped["blocks.payload.field_0.values"];
        assert_eq!(group_paths.len(), 3);
        let mut group_reader = crate::structure_plot::PlotReader::default();
        group_reader.request_group(&ctx, store.view(), group_node.clone(), group_plan.clone(),
            129..131, 800, false);
        while !group_reader.data().is_some_and(|data| data.complete) {
            assert!(std::time::Instant::now() < deadline, "grouped plot timed out: {:?}", group_reader.error());
            group_reader.tick(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for sensor_index in 0..3 {
            let name = format!("blocks.payload.field_0[{sensor_index}].values / value");
            let values = &group_reader.data().unwrap().series[&name];
            assert_eq!(values.iter().map(|point| point.value).collect::<Vec<_>>(),
                [((129 * 48) + sensor_index) as f64, (100 + sensor_index) as f64]);
        }
        while group_reader.group_buffer_stats().0 < 2 {
            assert!(std::time::Instant::now() < deadline, "neighbor tile was not prefetched");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let loads = group_reader.group_buffer_stats().1;
        group_reader.request_group(&ctx, store.view(), group_node.clone(), group_plan.clone(),
            130..132, 800, false);
        while !group_reader.data().is_some_and(|data| data.complete && data.range == (130..132)) {
            assert!(std::time::Instant::now() < deadline, "buffered pan timed out: {:?}", group_reader.error());
            group_reader.tick(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(group_reader.group_buffer_stats().1, loads, "nearby pan must reuse RAM tiles");
        let mut ordinary = sensors[0].1.clone();
        ordinary.name = "entries across records".into();
        ordinary.path = "records[4].payload.entries".into();
        assert!(sensor_group_key(&ordinary).is_none());
        for sensor in 0..3 {
            let path = format!("blocks.payload.field_0[{sensor}].values");
            let node = views.flattened_collections.iter().find(|node| node.path == path).unwrap();
            let array = node.lazy.as_ref().unwrap();
            assert_eq!(array.count, 130 * 130);
            let boundary = array.page(&store.view(), &AtomicBool::new(false), 129, 2).unwrap();
            assert_eq!(boundary.iter().map(Node::numeric).collect::<Vec<_>>(),
                [Some((129 * 48 + sensor) as f64), Some((100 + sensor) as f64)]);
        }
        let sensor = views.flattened_collections.iter()
            .find(|node| node.path == "blocks.payload.field_0[2].values").unwrap().clone();
        let mut reader = crate::structure_plot::PlotReader::default();
        reader.request(&ctx, store.view(), sensor, 129..131, 800, false);
        while !reader.data().is_some_and(|data| data.complete) {
            assert!(std::time::Instant::now() < deadline, "plot worker timed out: {:?}", reader.error());
            reader.tick(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let points = &reader.data().unwrap().series["value"];
        assert_eq!(points.iter().map(|point| point.value).collect::<Vec<_>>(), [6194., 102.]);
        let plotted_point = points[1].clone();
        let sensor_path = "blocks.payload.field_0[2].values";
        let sensor = views.flattened_collections.iter().find(|node| node.path == sensor_path).unwrap().clone();
        let bytes = sensor.lazy.as_ref().unwrap().bounds(130).unwrap();
        assert_eq!(plotted_point.bytes, bytes);
        views.plot_sources.clear();
        views.plot_sources.insert("blocks.payload.field_0[0].values".into());
        views.plot_sources_initialized = true;
        views.plot_target = "blocks.payload.field_0[0].values".into();
        views.table_target = sensor_path.into();
        views.last_hex_selection = Some(bytes.clone());
        views.plot_follow_selection = true;
        views.initialize_plot(&output);
        assert!(views.plot_sources.contains(sensor_path));
        assert_eq!(views.plot_target, sensor_path);
        assert_eq!(views.plot_jump, 130);
        views.choose_plot_bytes(&output, bytes, Some(plotted_point.path.to_string()));
        assert_eq!(views.table_target, sensor_path);
        assert_eq!(views.desired.as_ref().unwrap().key.path, sensor_path);
        assert_eq!(views.table_jump, 130);
        assert_eq!(views.selected.as_deref(), Some("blocks.payload.field_0[2].values[130]"));
        views.plot_sources = selected;
        views.plot_sources_initialized = true;
        views.plot_target = "blocks.payload.field_0[0].values".into();
        views.plot_view = Some(129..131);
        views.plot_force_view = None;
        views.plot_follow_selection = false;
        while !views.plot_reader.data().is_some_and(|data| data.complete && data.series.len() == 3) {
            render_plot(&ctx, &mut views, &output, &store, vec![], Default::default());
            assert!(std::time::Instant::now() < deadline, "grouped plot UI timed out: {:?}",
                views.plot_reader.error());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        render_plot(&ctx, &mut views, &output, &store, vec![], Default::default());
        assert!(views.plot_overlay.as_ref().is_some_and(|(_, data)|
            data.series.contains_key("blocks.payload.field_0[2].values / value")));
        assert_eq!(views.plot_reader.data().unwrap().series.len(), 3);
        assert!(views.group_plot_readers.is_empty(), "one group needs only the primary reader");
        assert!(!views.extra_plot_readers.contains_key("blocks.payload.field_0[2].values"));
        views.plot_hidden.insert("blocks.payload.field_0[0].values / value".into());
        render_plot(&ctx, &mut views, &output, &store, vec![], Default::default());
        assert!(views.plot_overlay.as_ref().is_some_and(|(_, data)|
            data.series.contains_key("blocks.payload.field_0[2].values / value")));
        // The Table picker shows one chip for the discovered channels, not one per position.
        let chips = views.table_chips(&output);
        let grouped: Vec<_> = chips.iter().filter(|chip| !chip.channels.is_empty()).collect();
        assert_eq!(grouped.len(), 1, "{:?}", chips.iter().map(|chip| &chip.label).collect::<Vec<_>>());
        assert!(grouped[0].label.starts_with("Payload ("), "{}", grouped[0].label);
        assert_eq!(grouped[0].channels[0].label, "field_0");
        assert!(grouped[0].channels[0].members.len() > 1);
    }
    #[test]
    fn indexed_virtual_series_supports_lazy_group_values() {
        let yaml = "structures:\n  - name: Group\n    fields:\n      - {name: values, type: u16, repeat: 129}\n  - name: Record\n    fields:\n      - {name: groups, type: {structure: Group}, repeat: 130}\nactions:\n  - read: {name: records, type: {structure: Record}, repeat: 130}\n";
        let bytes = vec![0; 130 * 130 * 129 * 2];
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        let ctx = egui::Context::default();
        views.start_nested_discovery(&output, store.view(), &ctx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !views.flattened_collections.iter().any(|node| node.path == "records.groups[128].values") {
            assert!(std::time::Instant::now() < deadline, "discovery timed out: {:?}", views.discovery_error);
            views.discover_flattened_collections(&output);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let node = views.flattened_collections.iter().find(|node| node.path == "records.groups[128].values").unwrap();
        let array = node.lazy.as_ref().unwrap();
        assert_eq!(array.count, 130 * 130);
        assert_eq!(array.bounds(0), Some(256..258));
        assert_eq!(array.bounds(1), Some(514..516));
        assert_eq!(array.bounds(130), Some((130 * 129 * 2 + 256)..(130 * 129 * 2 + 258)));
        let mut reader = crate::structure_plot::PlotReader::default();
        reader.request(&ctx, store.view(), node.clone(), 129..131, 800, false);
        while !reader.data().is_some_and(|data| data.complete) {
            assert!(std::time::Instant::now() < deadline, "plot worker timed out: {:?}", reader.error());
            reader.tick(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let points = &reader.data().unwrap().series["value"];
        assert_eq!(points.iter().map(|point| point.bytes.clone()).collect::<Vec<_>>(),
            [array.bounds(129).unwrap(), array.bounds(130).unwrap()]);
    }
    #[test]
    fn expanded_nested_lazy_page_shows_record_fields() {
        let mut yaml = String::from("structures:\n  - name: Item\n    fields:\n");
        for index in 0..200 {
            yaml.push_str(&format!("      - {{name: value_{index}, type: u8}}\n"));
        }
        yaml.push_str("  - name: Group\n    fields:\n      - {name: prefix, type: bytes, length: 2}\n      - {name: items, type: {structure: Item}, repeat: 256}\nactions:\n  - read: {name: groups, type: {structure: Group}, repeat: 130}\n");
        let store = ByteStore::from_snapshot(Original::from_bytes(&vec![7; 130 * (2 + 256 * 200)]).unwrap());
        let output = Program::parse(&yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let groups = &output.nodes[0];
        assert!(groups.lazy.is_some());
        let mut views = StructureViews::default();
        views.request(groups, 0, output.revision, &egui::Context::default());
        settle(&mut views, store.view());
        let items = views.page_nodes("groups", output.revision).unwrap()[0].children[1].clone();
        assert_eq!(items.lazy.as_ref().unwrap().count, 256);
        views.expanded.extend(["groups", "groups[0]", "groups[0].items", "groups[0].items[0]"].map(str::to_owned));
        views.request(&items, 0, output.revision, &egui::Context::default());
        settle(&mut views, store.view());
        assert!(views.page.as_ref().unwrap().result.is_ok());
        assert_eq!(views.tree_page_control_node(&output, "groups[0].items")
            .and_then(|node| node.lazy.map(|array| array.count)), Some(256));
        assert!(views.page_nodes("groups", output.revision).is_some());
        assert!(views.page_nodes("groups[0].items", output.revision).is_some());
        views.request(groups, 0, output.revision, &egui::Context::default());
        settle(&mut views, store.view());
        assert!(views.page_nodes("groups[0].items", output.revision).is_some());
        views.discover_flattened_collections(&output);
        let flat = views.flattened_collections.iter().find(|node| node.path == "groups.items").unwrap().clone();
        assert_eq!(flat.lazy.as_ref().unwrap().count, 130 * 256);
        assert!(flat.lazy.as_ref().unwrap().numeric_plan().is_some());
        assert_eq!(flat.lazy.as_ref().unwrap().bounds(255), Some((2 + 255 * 200)..(2 + 256 * 200)));
        assert_eq!(flat.lazy.as_ref().unwrap().bounds(256), Some((2 + 256 * 200 + 2)..(2 + 257 * 200 + 2)));
        let boundary = flat.lazy.as_ref().unwrap().page(&store.view(), &AtomicBool::new(false), 255, 2).unwrap();
        assert_eq!(boundary[0].children[199].numeric(), Some(7.));
        assert_eq!(boundary[1].children[199].numeric(), Some(7.));
        assert_eq!(views.table_target, "groups.items");
        assert_eq!(views.plot_target, "groups.items");
        views.request(&flat, 255, output.revision, &egui::Context::default());
        settle(&mut views, store.view());
        let table_rows = views.target_rows(&output);
        assert_eq!(table_rows.first().unwrap().path, "groups.items[128]");
        assert_eq!(table_rows[127].path, "groups.items[255]");
        views.request(&flat, 256, output.revision, &egui::Context::default());
        settle(&mut views, store.view());
        assert_eq!(views.target_rows(&output)[0].path, "groups.items[256]");
        let mut reader = crate::structure_plot::PlotReader::default();
        let ctx = egui::Context::default();
        reader.request(&ctx, store.view(), flat, 255..257, 800, false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !reader.data().is_some_and(|data| data.complete) {
            assert!(std::time::Instant::now() < deadline, "plot worker timed out");
            reader.tick(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let points = &reader.data().unwrap().series["value_0"];
        assert_eq!(points.iter().map(|point| point.index).collect::<Vec<_>>(), [255, 256]);
        assert_eq!(points[0].bytes, (2 + 255 * 200)..(3 + 255 * 200));
        assert_eq!(points[1].bytes, (2 + 256 * 200 + 2)..(3 + 256 * 200 + 2));
        let mut rows = Vec::new();
        flatten_visible(&output.nodes, 0, &views.expanded, views.page.as_ref(), &views.cached_pages, output.revision, &mut rows);
        assert!(rows.iter().any(|(node, _)| node.path == "groups[0].items[0]"));
        assert!(rows.iter().any(|(node, _)| node.path == "groups[0].items[0].value_199"));
    }
    #[test]
    fn large_block_inspector_pages_load_one_record_and_can_advance() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let block_bytes = 5_824_522;
        crate::cache::set_fixture_len(file.as_file(), 190 * block_bytes).unwrap();
        let source = crate::file_source::FileSource::open(file.path()).unwrap();
        let store = ByteStore::from_file(Arc::new(source));
        let yaml = format!("structures:\n  - name: Block\n    fields:\n      - {{name: samples, type: u16, repeat: {}}}\nactions:\n  - read: {{name: blocks, type: {{structure: Block}}, repeat: 190}}\n", block_bytes / 2);
        let output = Program::parse(&yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let blocks = &output.nodes[0];
        assert_eq!(inspector_page_size(blocks.lazy.as_ref().unwrap()), 1);
        let mut views = StructureViews::default();
        let ctx = egui::Context::default();
        for index in [0, 1, 189] {
            views.request(blocks, index, output.revision, &ctx);
            settle(&mut views, store.view());
            let page = views.page_nodes("blocks", output.revision).unwrap();
            assert_eq!(page.len(), 1);
            assert_eq!(page[0].path, format!("blocks[{index}]"));
            assert!(page[0].children[0].lazy.is_some());
        }
    }
    #[test]
    fn paging_nested_samples_past_1023_keeps_the_parent_visible() {
        let yaml = "structures:\n  - name: Block\n    fields:\n      - {name: sensor_0, type: u32, repeat: 4096}\nactions:\n  - read: {name: blocks, type: {structure: Block}, repeat: 190}\n";
        let bytes = vec![0; 190 * 4096 * 4];
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let mut views = StructureViews::default();
        let ctx = egui::Context::default();
        views.expand("blocks");
        views.request(&output.nodes[0], 0, output.revision, &ctx);
        settle(&mut views, store.view());
        let sensor = views.page_nodes("blocks", output.revision).unwrap()[0].children[0].clone();
        views.expand("blocks[0]");
        views.expand(&sensor.path);
        for start in (0..4096).step_by(PAGE_SIZE).chain([896, 1024, 0]) {
            views.request(&sensor, start, output.revision, &ctx);
            settle(&mut views, store.view());
            assert!(views.tree_page_control_node(&output, &sensor.path).is_some(), "lost controls at {start}");
            let mut rows = Vec::new();
            flatten_visible(&output.nodes, 0, &views.expanded, views.page.as_ref(), &views.cached_pages, output.revision, &mut rows);
            let expected = format!("{}[{start}]", sensor.path);
            assert!(rows.iter().any(|(node, _)| node.path == expected), "lost nested rows at {start}");
            assert!(views.cached_pages.len() <= 8);
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
        crate::theme::install(&ctx);
        let mut views = StructureViews::default();
        views.tick(&ctx, store.view());
        let _ = ctx.run(Default::default(), |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| { views.table(ui, &output, None, store.view()); });
        });
        assert_eq!(views.table_target, "items");
        views.table_target.clear();
        let _ = ctx.run(Default::default(), |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| { views.table(ui, &output, None, store.view()); });
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
    fn returning_to_a_cached_page_stays_there() {
        let (store, output) = records();
        let node = &output.nodes[2];
        let page = inspector_page_size(node.lazy.as_ref().unwrap());
        assert!(collection_count(node) > page);
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        for start in [0, page, 0] {
            views.request(node, start, output.revision, &ctx);
            settle(&mut views, store.view());
        }
        // Page controls read their position from the desired request each frame.
        assert_eq!(views.desired.as_ref().unwrap().key.start, 0);
        views.request(node, views.desired.as_ref().unwrap().key.start, output.revision, &ctx);
        assert_eq!(views.page_for(&node.path, output.revision).unwrap().key.start, 0);
    }
    #[test]
    fn paging_keeps_the_same_fields_expanded() {
        let mut views = StructureViews::default();
        for path in ["samples", "samples[5]", "samples[5].pair", "samples[300]"] {
            views.expanded.insert(path.into());
        }
        views.carry_expansion("samples", 0, 128, 128);
        assert!(views.expanded.contains("samples[133]"));
        assert!(views.expanded.contains("samples[133].pair"));
        assert!(views.expanded.contains("samples[5]"), "going back keeps the old item open");
        assert!(!views.expanded.contains("samples[428]"), "items outside the old page don't move");
        views.carry_expansion("samples", 133, 132, 1);
        assert!(views.expanded.contains("samples[132].pair"));
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
    /// Column names of the Table layout for these rows.
    fn table_columns(rows: &[Node]) -> Vec<String> {
        table::table_layout(rows).into_iter().map(|column| match column {
            table::TableColumn::Field(name) | table::TableColumn::Payload(name) => name,
        }).collect()
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
                    selection = views.table(ui, output, None, store.view());
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
                    if text.galley.text().trim() == label && text.pos.x < 250. {
                        // Row numbers are right-aligned; use the placed text rect.
                        return Some(text.galley.rect.translate(text.pos.to_vec2()).center());
                    }
                }
                None
            })
            .expect("sample number should be visible");
        // Field clicks land in the pair.a column, wherever the layout puts it.
        let pos = if field {
            let x = frame.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "pair.a" =>
                    Some(text.galley.rect.translate(text.pos.to_vec2()).center().x),
                _ => None,
            }).expect("pair.a column should be visible");
            egui::pos2(x, pos.y)
        } else {
            pos
        };
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
        crate::theme::install(&ctx);
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
    fn plot_colors_distinguish_indexed_series_with_the_same_leaf_name() {
        assert_ne!(plot_series_color("records[0].values"), plot_series_color("records[1].values"));
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
        crate::theme::install(&ctx);
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
                    views.table(ui, &output, None, store.view());
                });
            })
        };
        let first = render();
        let labels = texts(&first.shapes);
        for label in [
            "#",
            "Offset",
            "Size",
            "timestamp",
            "pair.z",
            "pair.a",
            "0x00000008",
        ] {
            assert!(
                labels.iter().any(|text| text == label),
                "missing {label} in {labels:?}"
            );
        }
        views.select_bytes(&output, Some(8 + 100 * 8..8 + 101 * 8));
        let scrolled = ctx.run(input, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                views.table(ui, &output, None, store.view());
            });
        });
        let labels = texts(&scrolled.shapes);
        for label in ["#", "timestamp", "100"] {
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
        // Plot interaction tests exercise an explicitly chosen set. Production
        // decoding leaves the selection empty until the user chooses one.
        if !views.plot_sources_initialized && views.plot_sources.is_empty() {
            let mut collections = Vec::new();
            collect_collections(&output.nodes, &mut collections);
            collections.extend(views.flattened_collections.iter());
            let eligible: Vec<_> = collections.into_iter().filter(|node|
                !views.flattened_collections.iter().any(|flat| flat.path == node.path)
                    && collection_score(node, output).0).collect();
            if eligible.len() > 8 {
                if let Some(best) = eligible.into_iter().max_by_key(|node| collection_score(node, output)) {
                    views.plot_sources.insert(best.path.clone());
                }
            } else {
                views.plot_sources.extend(eligible.into_iter().map(|node| node.path.clone()));
            }
            views.plot_sources_initialized = true;
        }
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
                // The plot draws nothing until nested discovery finishes, and extra
                // collections load on their own readers; slow runners reach this
                // loop before either is done.
                let busy = views.plot_reader.is_loading()
                    || views.discovery_rx.is_some()
                    || views.extra_plot_readers.values().any(|reader| reader.is_loading())
                    || views.group_plot_readers.values().any(|reader| reader.is_loading());
                if !busy {
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
        let builds = views.plot_overlay_builds;
        render_plot(&ctx, &mut views, &output, &store, Vec::new(), Default::default());
        assert_eq!(views.plot_overlay_builds, builds, "steady frames should reuse prepared plot data");
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
        assert!(views.plot_reader.data().unwrap().range.start <= 512);
        assert!(views.plot_reader.data().unwrap().range.end >= 768);
        assert_eq!(
            views.desired.as_ref().unwrap().key.start,
            128,
            "panning must not page the table"
        );
        views.request(&output.nodes[0], 896, output.revision, &ctx);
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(views.plot_view, Some(512..768),
            "paging the table must not change the plot viewport");
        assert!(views.plot_reader.data().unwrap().range.start <= 512);
        assert!(views.plot_reader.data().unwrap().range.end >= 768);
    }

    #[test]
    fn long_series_names_are_shortened_from_the_start() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let _ = ctx.run(egui::RawInput::default(), |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let font = egui::TextStyle::Button.resolve(ui.style());
                let name = "sections[3].payload.channels.sine[110] / values.[0].amplitude";
                let short = elide_start(ui, name, &font, 120.);
                assert!(short.starts_with('…') && name.ends_with(&short[short.char_indices().nth(1).unwrap().0..]));
                assert!(short.ends_with("amplitude"), "the distinctive tail stays visible: {short}");
                let width = ui.fonts_mut(|fonts| fonts.layout_no_wrap(short.clone(), font.clone(),
                    Color32::WHITE).size().x);
                assert!(width <= 120., "{short} is {width} px");
                assert_eq!(elide_start(ui, "sine", &font, 120.), "sine");
            });
        });
    }
    #[test]
    fn long_series_names_stay_inside_the_series_sidebar() {
        let field = |i: usize| format!("{{name: amplitude_measured_at_the_primary_input_channel_{i}, type: u16}}");
        let yaml = format!("structures: [{{name: S, fields: [{}, {}]}}]\nactions: [{{read: {{name: \
            samples_recorded_by_a_long_named_acquisition_channel, type: {{structure: S}}, repeat: until end}}}}]",
            field(0), field(1));
        let bytes = vec![0u8; 4096];
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(&yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_collections_open = true;
        views.plot_sidebar_series = true;
        views.plot_axis_series = views.plot_overlay_points.keys().next().cloned();
        let width = 1200.;
        let mut time = 0.;
        let mut frame = || {
            // Advance the clock so the sidebar's open animation completes.
            time += 1.;
            ctx.run(egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.))),
                time: Some(time),
                ..Default::default()
            }, |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| { views.plot(ui, &output, store.view()); });
            })
        };
        for _ in 0..3 { let _ = frame(); }
        let rendered = frame();
        let overflowing: Vec<_> = rendered.shapes.iter().filter_map(|shape| match &shape.shape {
            // Right-aligned galleys extend left of `pos`, so use the placed rect.
            egui::Shape::Text(text) if text.galley.rect.translate(text.pos.to_vec2()).right() > width + 1. =>
                Some(text.galley.text().to_owned()),
            _ => None,
        }).collect();
        assert!(overflowing.is_empty(), "text runs past the window: {overflowing:?}");
    }
    #[test]
    fn moving_a_trace_vertically_reuses_the_overlay() {
        let (store, output) = array();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        let name = views.plot_overlay_points.keys().next().unwrap().clone();
        let stored = Arc::clone(&views.plot_overlay_points[&name]);
        let (builds, revision) = (views.plot_overlay_builds, views.plot_overlay_revision);
        views.presentation.pan_series(&name, 100.);
        views.presentation.zoom_series(&name, 2., 0.);
        render_plot(&ctx, &mut views, &output, &store, Vec::new(), Default::default());
        assert_eq!((views.plot_overlay_builds, views.plot_overlay_revision), (builds, revision),
            "a Y pan or zoom must not rebuild the plot overlay");
        assert!(Arc::ptr_eq(&views.plot_overlay_points[&name], &stored));
        let placement = views.presentation.placement(&name);
        let placed = &views.plot_placed[&name].points;
        assert_eq!(placed[3].y, placement.display(stored[3].y));
    }
    #[test]
    fn y_axis_width_holds_when_tick_labels_get_shorter() {
        let bytes: Vec<u8> = (0..200_000)
            .flat_map(|i| if i < 100_000 { 1e9 + i as f64 } else { 0.5 }.to_le_bytes())
            .collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse("actions: [{read: {name: samples, type: f64, repeat: until end}}]")
            .unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        let mut widths = Vec::new();
        for view in [0..512, 150_000..150_512] {
            views.plot_view = Some(view.clone());
            views.plot_force_view = Some(view);
            views.plot_fit_y = true;
            settle_plot(&ctx, &mut views, &output, &store);
            settle_plot(&ctx, &mut views, &output, &store);
            widths.push(views.plot_axis_width);
        }
        assert!(widths[0] > 0.);
        assert_eq!(widths[1], widths[0], "short tick labels must not narrow the Y axis");
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
        let focused = selection_view(Some(&(0..100_000)), &(50_000..50_001), 100_000);
        assert_eq!(focused.len(), PAGE_SIZE);
        assert!(focused.contains(&50_000));
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
            let data = views.plot_reader.data().unwrap();
            assert!(data.range.start == 0 && data.range.end >= count);
            assert_eq!(data.scanned, data.range.len());
            assert!(data.complete);
            if count > 8192 {
                assert_eq!(data.quality, PlotQuality::MinMax);
            }
            for name in ["sine", "ramp", "square"] {
                views.plot_axis_series = Some(format!("{} / {name}", views.plot_target));
                views.plot_fit_y = true;
                settle_plot(&ctx, &mut views, &output, &store);
                let rendered = waveform_plot_frame(&ctx, &mut views, &output, &store);
                let transform = navigation_memory(&ctx, &views, &output).transform().clone();
                let data = views.plot_reader.data().unwrap();
                let values = &data.series[name];
                assert!(values.windows(2).all(|pair| pair[0].index < pair[1].index));
                let mut traces = Vec::new();
                for shape in &rendered.shapes {
                    paths(&shape.shape, plot_series_color(&format!("{} / {name}", views.plot_target)), &mut traces);
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
                            && mesh.vertices.iter().any(|v| v.color == plot_series_color(&format!("{} / {name}", views.plot_target)).gamma_multiply(0.45))
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
                let data = views.plot_reader.data().unwrap();
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
    fn selected_series_controls_the_plot_y_axis() {
        let (store, output) = waveform_plot_fixture();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_axis_series = Some("samples / sine".into());
        views.plot_fit_y = true;
        settle_plot(&ctx, &mut views, &output, &store);
        let sine_bounds = *navigation_memory(&ctx, &views, &output).bounds();
        views.plot_axis_series = Some("samples / ramp".into());
        views.plot_fit_y = true;
        settle_plot(&ctx, &mut views, &output, &store);
        let ramp_bounds = *navigation_memory(&ctx, &views, &output).bounds();
        assert!(sine_bounds.min()[1] < -3. && sine_bounds.max()[1] > 3.);
        assert!(ramp_bounds.min()[1] > 9. && ramp_bounds.max()[1] < 15.,
            "Y axis must follow its selected series: sine={sine_bounds:?}, ramp={ramp_bounds:?}");
    }

    #[test]
    fn fit_all_visible_series_keeps_the_sample_range() {
        let (store, output) = waveform_plot_fixture();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_force_view = Some(900..1100);
        views.plot_axis_series = Some("samples / sine".into());
        views.plot_fit_y = true;
        settle_plot(&ctx, &mut views, &output, &store);
        let selected = *navigation_memory(&ctx, &views, &output).bounds();

        views.plot_fit_all_y = true;
        views.plot_fit_y = false;
        settle_plot(&ctx, &mut views, &output, &store);
        let all = *navigation_memory(&ctx, &views, &output).bounds();
        assert_eq!(all.min()[0], selected.min()[0]);
        assert_eq!(all.max()[0], selected.max()[0]);
        assert!(all.min()[1] <= selected.min()[1]);
        assert!(all.max()[1] > 12., "all visible series should include ramp: {all:?}");
    }

    #[test]
    fn changing_y_axis_series_preserves_the_horizontal_zoom() {
        let (store, output) = waveform_plot_fixture();
        let ctx = egui::Context::default();
        let mut views = StructureViews::default();
        settle_plot(&ctx, &mut views, &output, &store);
        views.plot_collections_open = true;
        views.plot_sidebar_series = true;
        views.plot_force_view = Some(900..1100);
        settle_plot(&ctx, &mut views, &output, &store);
        let frame = |views: &mut StructureViews, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200., 700.),
                    )),
                    events,
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        views.plot(ui, &output, store.view());
                    });
                },
            )
        };
        for _ in 0..12 {
            frame(&mut views, vec![]);
        }
        let rendered = frame(&mut views, vec![]);
        let row = rendered
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "ramp" && text.pos.x > 800. => {
                    Some(text.pos + text.galley.rect.center().to_vec2())
                }
                _ => None,
            })
            .expect("ramp series row must be visible");
        let before = *navigation_memory(&ctx, &views, &output).bounds();
        let view = views.plot_view.clone();
        frame(&mut views, vec![egui::Event::PointerMoved(row)]);
        frame(
            &mut views,
            vec![egui::Event::PointerButton {
                pos: row,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        frame(
            &mut views,
            vec![egui::Event::PointerButton {
                pos: row,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        settle_plot(&ctx, &mut views, &output, &store);
        let after = *navigation_memory(&ctx, &views, &output).bounds();
        assert_eq!(views.plot_axis_series.as_deref(), Some("samples / ramp"));
        assert_eq!(views.plot_view, view);
        assert_eq!(after.min()[0], before.min()[0]);
        assert_eq!(after.max()[0], before.max()[0]);
        assert_eq!(after.min()[1], before.min()[1]);
        assert_eq!(after.max()[1], before.max()[1]);
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
        for (scale, quality) in [(1., PlotQuality::MinMax), (2., PlotQuality::Exact), (1., PlotQuality::Exact)] {
            ctx.set_pixels_per_point(scale);
            settle_plot(&ctx, &mut views, &output, &store);
            let data = views.plot_reader.data().unwrap();
            assert_eq!(data.quality, quality, "display scale {scale}");
            assert!(data.range.start == 0 && data.range.end >= 4096);
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
        assert_eq!(data.quality, PlotQuality::MinMax);
        assert!(data.envelopes["value"]
            .iter()
            .all(|band| band.min.value == 7. && band.max.value == 7.));
        views.plot_sampled_preview = true;
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(views.plot_reader.data().unwrap().quality, PlotQuality::SampledPreview);
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
        assert_eq!(data.quality, PlotQuality::MinMax);
        assert_eq!(data.scanned, count);
        let envelopes = &data.envelopes["value"];
        assert!(!envelopes.is_empty());
        assert!(envelopes.iter().any(|band| band.min.value < -0.99));
        assert!(envelopes.iter().any(|band| band.max.value > 0.99));
        views.plot_sampled_preview = true;
        settle_plot(&ctx, &mut views, &output, &store);
        assert_eq!(views.plot_reader.data().unwrap().quality, PlotQuality::SampledPreview);
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
                egui::Shape::Mesh(mesh) => *bands += mesh.indices.len() / 6,
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
                egui::Shape::Mesh(mesh) => mesh.vertices.iter().any(|vertex| {
                    vertex.color == color || vertex.color == color.gamma_multiply(0.23)
                }),
                _ => false,
            }
        }
        assert!(
            rendered
                .shapes
                .iter()
                .any(|shape| series_color(&shape.shape, plot_series_color("samples / value"))),
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
            assert!(views.plot_reader.data().unwrap().range.start <= 256);
            assert!(views.plot_reader.data().unwrap().range.end >= 768);
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
        assert!(views.plot_reader.data().unwrap().range.start <= 512);
        assert!(views.plot_reader.data().unwrap().range.end >= 1024);
        assert_eq!(after.max()[0], 1023.5);
        assert!(views.last_hex_selection.is_none());
    }

    #[test]
    fn ordinary_wheel_zooms_x_with_y_locked_and_both_axes_when_unlocked() {
        for (unit, delta, free_y) in [
            (egui::MouseWheelUnit::Point, 80., false),
            (egui::MouseWheelUnit::Point, 80., true),
            (egui::MouseWheelUnit::Line, 1., false),
            (egui::MouseWheelUnit::Line, 1., true),
        ] {
            let (store, output) = array();
            let ctx = egui::Context::default();
            let mut views = StructureViews::default();
            views.plot_free_y = free_y;
            settle_plot(&ctx, &mut views, &output, &store);
            let memory = navigation_memory(&ctx, &views, &output);
            let before = *memory.bounds();
            let axis = views.plot_axis_series.clone().unwrap();
            let scale_before = views.presentation.placement(&axis).scale;
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
                vec![egui::Event::MouseWheel {
                    unit,
                    delta: egui::vec2(0., delta),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                }],
                egui::Modifiers::NONE,
            );
            settle_plot(&ctx, &mut views, &output, &store);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            assert!(
                after.width() < before.width() * 0.9,
                "ordinary wheel must zoom horizontally: {before:?} -> {after:?}"
            );
            if free_y {
                navigation_assert_y(before, after);
                assert!(
                    views.presentation.placement(&axis).scale > scale_before,
                    "unlocked wheel should zoom only the selected trace vertically"
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
            let axis = views.plot_axis_series.clone().unwrap();
            let scale_before = views.presentation.placement(&axis).scale;
            let pointer = axis_navigation_position(&memory, y_axis);
            axis_navigation_send(
                &ctx,
                &mut views,
                &output,
                &store,
                pointer,
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0., 48.),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Modifiers::NONE,
            );
            settle_plot(&ctx, &mut views, &output, &store);
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            if y_axis {
                axis_navigation_assert_x(before, after);
                navigation_assert_y(before, after);
                assert!(
                    views.presentation.placement(&axis).scale > scale_before,
                    "explicit Y-axis zoom must scale the selected series"
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
            let axis = views.plot_axis_series.clone().unwrap();
            let offset_before = views.presentation.placement(&axis).offset;
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
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(48., 0.),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                },
                shift,
            );
            let immediately_after = *navigation_memory(&ctx, &views, &output).bounds();
            assert!((immediately_after.width() - before.width()).abs() < 1e-8);
            assert!((immediately_after.height() - before.height()).abs() < 1e-8);
            if y_axis {
                axis_navigation_assert_x(before, immediately_after);
                navigation_assert_y(before, immediately_after);
                assert!(
                    views.presentation.placement(&axis).offset < offset_before,
                    "positive Y scroll moves only the selected series"
                );
            } else {
                navigation_assert_y(before, immediately_after);
                assert!(
                    before.min()[0] - immediately_after.min()[0] > before.width() * 0.15,
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
    fn middle_drag_pans_signal_without_changing_zoom_or_selection() {
        let (ctx, store, output, mut views) = axis_navigation_fixture();
        let selected = views.last_hex_selection.clone();
        let before = *navigation_memory(&ctx, &views, &output).bounds();
        let from = navigation_memory(&ctx, &views, &output).transform().frame().center();
        let to = from + egui::vec2(80., 0.);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Middle,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render_plot(&ctx, &mut views, &output, &store,
            vec![egui::Event::PointerMoved(from), button(from, true)], egui::Modifiers::NONE);
        render_plot(&ctx, &mut views, &output, &store,
            vec![egui::Event::PointerMoved(to)], egui::Modifiers::NONE);
        render_plot(&ctx, &mut views, &output, &store,
            vec![button(to, false)], egui::Modifiers::NONE);
        let after = *navigation_memory(&ctx, &views, &output).bounds();
        assert!(after.min()[0] < before.min()[0], "middle drag should pan X: {before:?} -> {after:?}");
        assert!((after.width() - before.width()).abs() < 1e-8);
        navigation_assert_y(before, after);
        assert_eq!(views.last_hex_selection, selected);
    }

    #[test]
    fn left_drag_on_axis_pans_that_axis_without_zooming() {
        for y_axis in [false, true] {
            let (ctx, store, output, mut views) = axis_navigation_fixture();
            let selected = views.last_hex_selection.clone();
            let before = *navigation_memory(&ctx, &views, &output).bounds();
            let axis = views.plot_axis_series.clone().unwrap();
            let offset_before = views.presentation.placement(&axis).offset;
            let from = axis_navigation_position(&navigation_memory(&ctx, &views, &output), y_axis);
            let to = from
                + if y_axis {
                    egui::vec2(0., -40.)
                } else {
                    egui::vec2(80., 0.)
                };
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            render_plot(
                &ctx,
                &mut views,
                &output,
                &store,
                vec![egui::Event::PointerMoved(from), button(from, true)],
                egui::Modifiers::NONE,
            );
            render_plot(
                &ctx,
                &mut views,
                &output,
                &store,
                vec![egui::Event::PointerMoved(to)],
                egui::Modifiers::NONE,
            );
            render_plot(
                &ctx,
                &mut views,
                &output,
                &store,
                vec![button(to, false)],
                egui::Modifiers::NONE,
            );
            let after = *navigation_memory(&ctx, &views, &output).bounds();
            if y_axis {
                axis_navigation_assert_x(before, after);
                navigation_assert_y(before, after);
                assert!(
                    views.presentation.placement(&axis).offset > offset_before,
                    "Y drag should move the selected series"
                );
            } else {
                navigation_assert_y(before, after);
                assert!(
                    after.min()[0] < before.min()[0],
                    "X drag should pan: {before:?} -> {after:?}"
                );
                assert!((after.width() - before.width()).abs() < 1e-8);
            }
            assert_eq!(views.last_hex_selection, selected);
        }
    }

    #[test]
    fn shift_wheel_over_canvas_pans_x_without_zooming() {
        let (ctx, store, output, mut views) = axis_navigation_fixture();
        let before = *navigation_memory(&ctx, &views, &output).bounds();
        let pointer = navigation_memory(&ctx, &views, &output).transform().frame().center();
        let shift = egui::Modifiers { shift: true, ..Default::default() };
        axis_navigation_send(&ctx, &mut views, &output, &store, pointer,
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(48., 0.),
                phase: egui::TouchPhase::Move,
                modifiers: shift,
            }, shift);
        let after = *navigation_memory(&ctx, &views, &output).bounds();
        assert!(after.min()[0] < before.min()[0]);
        assert!((after.width() - before.width()).abs() < 1e-8);
        navigation_assert_y(before, after);
    }

    #[test]
    fn line_wheel_pan_has_a_useful_step() {
        let points = wheel_pan_points(&[egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(1., 0.),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert_eq!(points.x, 48.);
    }

    #[test]
    fn ctrl_wheel_or_pinch_over_axis_zoom_only_that_axis() {
        for y_axis in [false, true] {
            let (ctx, store, output, mut views) = axis_navigation_fixture();
            let selected = views.last_hex_selection.clone();
            let page = views.desired.as_ref().unwrap().key.clone();
            let memory = navigation_memory(&ctx, &views, &output);
            let before = *memory.bounds();
            let axis = views.plot_axis_series.clone().unwrap();
            let scale_before = views.presentation.placement(&axis).scale;
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
                navigation_assert_y(before, after);
                assert!(
                    (views.presentation.placement(&axis).scale - scale_before * 1.4).abs() < 1e-4,
                    "selected series zoom factor must be applied exactly once"
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
