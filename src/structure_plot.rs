//! Viewport-driven plotting of generic decoded collections. Workers retain
//! bounded pages and extrema while scanning a requested range; an optional
//! sampled preview avoids a full scan when explicitly selected.
use crate::byte_store::{ByteView, READ_LIMIT};
use crate::structure::{Address, Node, NumericPlan};
use anyhow::{ensure, Result};
use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Condvar, Mutex};
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

const MIN_EXACT_LIMIT: Address = 2048;
const MAX_EXACT_LIMIT: Address = 8192;
const AUTO_SCAN_LIMIT: Address = 65_536;
const PREVIEW_LIMIT: Address = 65_536;
const PREVIEW_WINDOWS: Address = 128;
const PREVIEW_WINDOW_SIZE: Address = PREVIEW_LIMIT / PREVIEW_WINDOWS;
const MAX_BUCKETS: Address = 4096;
/// Buckets shared by every channel of a grouped scan: enough for hundreds of
/// channels at about one bucket per pixel.
const MAX_TOTAL_NUMERIC_POINTS: Address = 500_000;
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop only.
const CACHE_ENTRIES: Address = 3;
#[cfg(not(target_arch = "wasm32"))]
const GROUP_TILE_SAMPLES: Address = 4096;
#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_GROUP_BUFFER_BYTES: usize = 1024 * 1024 * 1024;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn group_buffer_bytes() -> usize {
    std::env::var("BINWAVE_PLOT_CACHE_MB").ok()
        .and_then(|value| value.parse::<usize>().ok())
        .and_then(|mib| mib.checked_mul(1024 * 1024))
        .unwrap_or(DEFAULT_GROUP_BUFFER_BYTES)
}
#[cfg(not(target_arch = "wasm32"))]
const GROUP_SUMMARY_SAMPLES: Address = 1024;

#[derive(Clone, Debug)]
pub struct PlotSample {
    pub index: Address,
    pub value: f64,
    pub bytes: Range<Address>,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop only.
    pub path: PlotPath,
}
/// Numeric points share the path's fixed parts across a field. The full path
/// is formed only when selection or inspection needs it.
#[derive(Clone, Debug)]
pub enum PlotPath {
    Concrete(String),
    Indexed { parts: Arc<(String, String)>, index: Address },
}
impl PartialEq for PlotPath {
    fn eq(&self, other: &Self) -> bool { self.to_string() == other.to_string() }
}
impl Eq for PlotPath {}
impl From<String> for PlotPath {
    fn from(path: String) -> Self { Self::Concrete(path) }
}
impl From<&str> for PlotPath {
    fn from(path: &str) -> Self { Self::Concrete(path.to_owned()) }
}
impl std::fmt::Display for PlotPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Concrete(path) => f.write_str(path),
            Self::Indexed { parts, index } => write!(f, "{}[{index}]{}", parts.0, parts.1),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlotQuality {
    Exact,
    MinMax,
    SampledPreview,
}
impl PlotQuality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "All samples",
            Self::MinMax => "Reduced · min/max retained",
            Self::SampledPreview => "Estimated range · peaks may be missed",
        }
    }
}
/// An amplitude band over one display bucket. Endpoints retain the actual
/// observed samples; SampledPreview does not claim to have seen every peak.
#[derive(Clone, Debug)]
pub struct PlotEnvelope {
    pub range: Range<Address>,
    pub min: PlotSample,
    pub max: PlotSample,
}
#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop only.
pub struct PlotData {
    pub range: Range<Address>,
    pub series: BTreeMap<String, Vec<PlotSample>>,
    pub envelopes: BTreeMap<String, Vec<PlotEnvelope>>,
    pub bounds: BTreeMap<String, (f64, f64)>,
    pub quality: PlotQuality,
    pub scanned: Address,
    pub total: Address,
    pub complete: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    revision: u64,
    definition: u64,
    path: String,
    range: Range<Address>,
    buckets: Address,
    exact_limit: Address,
    refine: bool,
    fields: Option<Vec<String>>,
}
#[derive(Clone)]
struct Request {
    key: Key,
    source: ByteView,
    node: Node,
    numeric_plan: Option<NumericPlan>,
    #[cfg(not(target_arch = "wasm32"))]
    group_tiles: Option<Arc<Mutex<GroupTileCache>>>,
    partial_updates: bool,
}
#[cfg(not(target_arch = "wasm32"))]
struct Cached {
    request: Request,
    data: Arc<PlotData>,
}
#[cfg(not(target_arch = "wasm32"))]
struct Outcome {
    key: Key,
    result: std::result::Result<PlotData, String>,
}
#[cfg(not(target_arch = "wasm32"))]
struct Running {
    cancel: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Outcome>>>,
    finished: Arc<AtomicBool>,
}
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Hash, PartialEq, Eq)]
struct GroupSignature {
    revision: u64,
    definition: u64,
    path: String,
    fields: Vec<String>,
}
#[cfg(not(target_arch = "wasm32"))]
struct GroupTile {
    start: Address,
    end: Address,
    values: Vec<Vec<f64>>,
    summary: Arc<GroupTileSummary>,
}
#[cfg(not(target_arch = "wasm32"))]
struct GroupTileSummary {
    start: Address,
    end: Address,
    bins: Vec<(Range<Address>, Vec<Option<NumericBucket>>)>,
    complete_fields: bool,
}
#[cfg(not(target_arch = "wasm32"))]
impl GroupTileSummary {
    fn bytes(&self) -> usize {
        self.bins.len().saturating_mul(self.bins.first().map_or(0, |(_, fields)| fields.len()))
            .saturating_mul(std::mem::size_of::<NumericBucket>())
    }
    fn merge(left: &Self, right: &Self) -> Option<Self> {
        if !left.complete_fields || !right.complete_fields
            || left.end != right.start || left.bins.first()?.1.len() != right.bins.first()?.1.len() {
            return None;
        }
        let field_count = left.bins.first()?.1.len();
        let mut fields: Vec<Option<NumericBucket>> = vec![None; field_count];
        for (_, bins) in left.bins.iter().chain(&right.bins) {
            for (target, bucket) in fields.iter_mut().zip(bins) {
                if let Some(bucket) = bucket {
                    match target {
                        Some(target) => target.merge(*bucket),
                        slot => *slot = Some(*bucket),
                    }
                }
            }
        }
        Some(Self {
            start: left.start, end: right.end,
            bins: vec![(left.start..right.end, fields)],
            complete_fields: true,
        })
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl GroupTile {
    fn bytes(&self) -> usize {
        (self.end - self.start).saturating_mul(self.values.len()).saturating_mul(std::mem::size_of::<f64>())
    }
    fn from_values(start: Address, values: Vec<Vec<f64>>) -> Self {
        let end = start + values.first().map_or(0, Vec::len);
        let mut bins = Vec::new();
        for bin_start in (start..end).step_by(GROUP_SUMMARY_SAMPLES) {
            let bin_end = bin_start.saturating_add(GROUP_SUMMARY_SAMPLES).min(end);
            let mut fields = Vec::with_capacity(values.len());
            for field_values in &values {
                let mut summary: Option<NumericBucket> = None;
                for index in bin_start..bin_end {
                    let value = field_values[index - start];
                    if !value.is_finite() { continue; }
                    let point = NumericPoint { index, value };
                    match &mut summary {
                        Some(bucket) => bucket.push(point),
                        slot => *slot = Some(NumericBucket { first: point, min: point, max: point, last: point }),
                    }
                }
                fields.push(summary);
            }
            bins.push((bin_start..bin_end, fields));
        }
        let complete_fields = bins.iter().all(|(_, fields)| fields.iter().all(Option::is_some));
        let summary = Arc::new(GroupTileSummary { start, end, bins, complete_fields });
        Self { start, end, values, summary }
    }
}
#[cfg(not(target_arch = "wasm32"))]
struct LruEntry<T> {
    value: T,
    older: Option<Address>,
    newer: Option<Address>,
}
#[cfg(not(target_arch = "wasm32"))]
struct LruMap<T> {
    entries: HashMap<Address, LruEntry<T>>,
    newest: Option<Address>,
    oldest: Option<Address>,
}
#[cfg(not(target_arch = "wasm32"))]
impl<T> Default for LruMap<T> {
    fn default() -> Self {
        Self { entries: HashMap::new(), newest: None, oldest: None }
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Clone> LruMap<T> {
    #[cfg(test)]
    fn len(&self) -> usize { self.entries.len() }
    fn clear(&mut self) {
        self.entries.clear();
        self.newest = None;
        self.oldest = None;
    }
    fn get(&mut self, key: Address) -> Option<T> {
        let value = self.entries.get(&key)?.value.clone();
        self.touch(key);
        Some(value)
    }
    fn insert(&mut self, key: Address, value: T) -> bool {
        if self.entries.contains_key(&key) { self.touch(key); return false; }
        if let Some(newest) = self.newest {
            self.entries.get_mut(&newest).unwrap().newer = Some(key);
        } else {
            self.oldest = Some(key);
        }
        self.entries.insert(key, LruEntry { value, older: self.newest, newer: None });
        self.newest = Some(key);
        true
    }
    fn pop_oldest(&mut self) -> Option<T> {
        let key = self.oldest?;
        let entry = self.entries.remove(&key).unwrap();
        self.oldest = entry.newer;
        if let Some(next) = self.oldest {
            self.entries.get_mut(&next).unwrap().older = None;
        } else {
            self.newest = None;
        }
        Some(entry.value)
    }
    fn touch(&mut self, key: Address) {
        if self.newest == Some(key) { return; }
        let (older, newer) = {
            let entry = self.entries.get(&key).unwrap();
            (entry.older, entry.newer)
        };
        if let Some(older) = older {
            self.entries.get_mut(&older).unwrap().newer = newer;
        } else {
            self.oldest = newer;
        }
        if let Some(newer) = newer {
            self.entries.get_mut(&newer).unwrap().older = older;
        }
        if let Some(previous) = self.newest {
            self.entries.get_mut(&previous).unwrap().newer = Some(key);
        }
        let entry = self.entries.get_mut(&key).unwrap();
        entry.older = self.newest;
        entry.newer = None;
        self.newest = Some(key);
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct TileFlight {
    result: Mutex<Option<std::result::Result<Arc<GroupTile>, String>>>,
    ready: Condvar,
}
#[cfg(not(target_arch = "wasm32"))]
struct GroupTileCache {
    signature: Option<GroupSignature>,
    tiles: LruMap<Arc<GroupTile>>,
    bytes: usize,
    summaries: LruMap<Arc<GroupTileSummary>>,
    coarse_summaries: Vec<LruMap<Arc<GroupTileSummary>>>,
    summary_bytes: usize,
    in_flight: HashMap<(GroupSignature, Address), Arc<TileFlight>>,
    budget_bytes: usize,
    #[cfg(test)]
    loads: usize,
    #[cfg(test)]
    coarse_hits: usize,
}
#[cfg(not(target_arch = "wasm32"))]
impl Default for GroupTileCache {
    fn default() -> Self {
        Self {
            signature: None, tiles: LruMap::default(), bytes: 0,
            summaries: LruMap::default(), summary_bytes: 0,
            coarse_summaries: Vec::new(),
            in_flight: HashMap::new(), budget_bytes: group_buffer_bytes(),
            #[cfg(test)] loads: 0,
            #[cfg(test)] coarse_hits: 0,
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl GroupTileCache {
    fn set_budget(&mut self, bytes: usize) {
        self.budget_bytes = bytes;
        self.trim();
    }
    fn trim(&mut self) {
        let raw = self.budget_bytes.saturating_mul(3) / 4;
        let summary = self.budget_bytes.saturating_sub(raw);
        while self.bytes > raw {
            let Some(old) = self.tiles.pop_oldest() else { break; };
            self.bytes = self.bytes.saturating_sub(old.bytes());
        }
        while self.summary_bytes > summary {
            let old = self.summaries.pop_oldest().or_else(|| self.coarse_summaries.iter_mut()
                .find_map(LruMap::pop_oldest));
            let Some(old) = old else { break; };
            self.summary_bytes = self.summary_bytes.saturating_sub(old.bytes());
        }
    }
    fn prepare(&mut self, signature: &GroupSignature) {
        if self.signature.as_ref() != Some(signature) {
            self.signature = Some(signature.clone());
            self.tiles.clear();
            self.bytes = 0;
            self.summaries.clear();
            self.coarse_summaries.clear();
            self.summary_bytes = 0;
            self.in_flight.clear();
        }
    }
    fn get(&mut self, start: Address) -> Option<Arc<GroupTile>> {
        self.tiles.get(start)
    }
    fn insert(&mut self, tile: Arc<GroupTile>, tile_size: Address) {
        if tile.bytes() <= self.budget_bytes.saturating_mul(3) / 4
            && self.tiles.insert(tile.start, tile.clone()) {
            self.bytes = self.bytes.saturating_add(tile.bytes());
            #[cfg(test)] { self.loads += 1; }
        }
        let summary = tile.summary.clone();
        if summary.bytes() <= self.budget_bytes / 4
            && self.summaries.insert(summary.start, summary.clone()) {
            self.summary_bytes += summary.bytes();
            self.promote_summary(summary, tile_size);
        }
        self.trim();
    }
    fn promote_summary(&mut self, mut current: Arc<GroupTileSummary>, tile_size: Address) {
        let mut span = tile_size;
        for level in 0..usize::BITS as usize {
            let Some(sibling_start) = (current.start / span ^ 1).checked_mul(span) else { break; };
            let sibling = if level == 0 { self.summaries.get(sibling_start) }
                else { self.coarse_summaries.get_mut(level - 1).and_then(|map| map.get(sibling_start)) };
            let Some(sibling) = sibling else { break; };
            let (left, right) = if current.start < sibling.start {
                (&current, &sibling)
            } else { (&sibling, &current) };
            let Some(parent) = GroupTileSummary::merge(left, right).map(Arc::new) else { break; };
            if parent.bytes() > self.budget_bytes / 4 { break; }
            if self.coarse_summaries.len() <= level {
                self.coarse_summaries.push(LruMap::default());
            }
            if !self.coarse_summaries[level].insert(parent.start, parent.clone()) { break; }
            self.summary_bytes = self.summary_bytes.saturating_add(parent.bytes());
            current = parent;
            let Some(next) = span.checked_mul(2) else { break; };
            span = next;
        }
    }
    fn covering_summary(&mut self, index: Address, range: &Range<Address>, buckets: Address)
        -> Option<Arc<GroupTileSummary>> {
        for level in (0..self.coarse_summaries.len()).rev() {
            let Some(summary) = self.coarse_summaries[level].get(index) else { continue; };
            if !summary.complete_fields || summary.end > range.end { continue; }
            let first_bucket = ((index - range.start) as u128 * buckets as u128
                / range.len().max(1) as u128) as Address;
            let last_bucket = ((summary.end - 1 - range.start) as u128 * buckets as u128
                / range.len().max(1) as u128) as Address;
            if first_bucket == last_bucket {
                #[cfg(test)] { self.coarse_hits += 1; }
                return Some(summary);
            }
        }
        None
    }
    fn summary(&mut self, start: Address) -> Option<Arc<GroupTileSummary>> {
        self.summaries.get(start)
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct PlotReader {
    desired: Option<Request>,
    running: Option<Running>,
    data: Option<Arc<PlotData>>,
    data_key: Option<Key>,
    /// The last complete result for these series, drawn beyond a new read's scan
    /// front so the plot never blanks while it fills in.
    backdrop: Option<(Key, Arc<PlotData>)>,
    start_after: Option<Instant>,
    error: Option<String>,
    cache: VecDeque<Cached>,
    group_tiles: Arc<Mutex<GroupTileCache>>,
    cache_budget: usize,
    data_generation: u64,
    /// Publish only complete reads: the last complete data stays drawn until
    /// the next read replaces it in one step. For when something else (the
    /// plot summary) fills whatever that data doesn't cover.
    complete_only: bool,
}
#[cfg(not(target_arch = "wasm32"))]
impl Drop for PlotReader {
    fn drop(&mut self) {
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl PlotReader {
    /// `pixel_width` is the plot canvas width in physical display pixels.
    #[cfg(test)]
    pub fn request(
        &mut self,
        ctx: &egui::Context,
        source: ByteView,
        node: Node,
        range: Range<Address>,
        pixel_width: Address,
        refine: bool,
    ) {
        self.request_filtered(ctx, source, node, range, pixel_width, refine, None);
    }

    pub fn request_filtered(
        &mut self,
        ctx: &egui::Context,
        source: ByteView,
        node: Node,
        range: Range<Address>,
        pixel_width: Address,
        refine: bool,
        fields: Option<Vec<String>>,
    ) {
        let plan = eligible_numeric_plan(&node, fields.as_deref());
        self.request_with_updates(ctx, source, node, range, pixel_width, refine, fields, plan, true);
    }

    pub fn request_background(
        &mut self,
        ctx: &egui::Context,
        source: ByteView,
        node: Node,
        range: Range<Address>,
        pixel_width: Address,
        refine: bool,
    ) {
        let plan = eligible_numeric_plan(&node, None);
        self.request_with_updates(ctx, source, node, range, pixel_width, refine, None, plan, false);
    }

    pub fn request_group(
        &mut self, ctx: &egui::Context, source: ByteView, node: Node,
        plan: NumericPlan, range: Range<Address>, pixel_width: Address, refine: bool,
    ) {
        let fields = Some(plan.fields.iter().map(|field| field.name.clone()).collect());
        let partial_updates = range.len() > AUTO_SCAN_LIMIT;
        self.request_with_updates(ctx, source, node, range, pixel_width, refine, fields, Some(plan), partial_updates);
    }

    pub fn matches_group_request(&self, revision: u64, node: &Node, plan: &NumericPlan,
        range: Range<Address>, pixel_width: Address, refine: bool) -> bool {
        let count = node.lazy.as_ref().map_or(node.children.len(), |array| array.count);
        let start = range.start.min(count);
        let range = start..range.end.max(start).min(count);
        self.desired.as_ref().is_some_and(|request| {
            let key = &request.key;
            key.revision == revision && key.definition == identity(node)
                && key.path == node.path
                && key.refine == refine
                && key.fields.as_ref().is_some_and(|fields| fields.len() == plan.fields.len()
                    && fields.iter().zip(&plan.fields).all(|(name, field)| name == &field.name))
                && covers_view(key, &range, pixel_width, Some(plan.fields.len()),
                    &fresh_query(&range, count, true, refine, Some(plan.fields.len())))
        })
    }

    pub fn matches_background_request(
        &self,
        revision: u64,
        node: &Node,
        range: Range<Address>,
        pixel_width: Address,
        refine: bool,
    ) -> bool {
        let count = node.lazy.as_ref().map_or(node.children.len(), |array| array.count);
        let start = range.start.min(count);
        let range = start..range.end.max(start).min(count);
        let plan = eligible_numeric_plan(node, None);
        self.desired.as_ref().is_some_and(|request| {
            let key = &request.key;
            key.revision == revision
                && key.definition == identity(node)
                && key.path == node.path
                && key.refine == refine
                && key.fields.is_none()
                && covers_view(key, &range, pixel_width,
                    plan.as_ref().map(|plan| plan.fields.len()),
                    &fresh_query(&range, count, plan.is_some(), refine, plan.as_ref().map(|plan| plan.fields.len())))
        })
    }

    fn request_with_updates(
        &mut self,
        ctx: &egui::Context,
        source: ByteView,
        node: Node,
        range: Range<Address>,
        pixel_width: Address,
        refine: bool,
        fields: Option<Vec<String>>,
        numeric_plan: Option<NumericPlan>,
        partial_updates: bool,
    ) {
        let partial_updates = partial_updates && !self.complete_only;
        let count = node
            .lazy
            .as_ref()
            .map_or(node.children.len(), |array| array.count);
        let start = range.start.min(count);
        let range = start..range.end.max(start).min(count);
        let field_count = numeric_plan.as_ref().map(|plan| plan.fields.len());
        if self.desired.as_ref().is_some_and(|request| {
            let key = &request.key;
            key.revision == source.revision && key.definition == identity(&node)
                && key.path == node.path && key.refine == refine
                && key.fields.as_deref() == fields.as_deref()
                && covers_view(key, &range, pixel_width, field_count,
                    &fresh_query(&range, count, numeric_plan.is_some(), refine, field_count))
        }) {
            if let Err(error) = source.validate() {
                self.invalidate();
                self.error = Some(format!("{error:#}"));
            }
            return;
        }
        let query_range = fresh_query(&range, count, numeric_plan.is_some(), refine, field_count);
        let (buckets, exact_limit) = query_resolution(&range, &query_range,
            pixel_width, field_count);
        let key = Key {
            revision: source.revision,
            definition: identity(&node),
            path: node.path.clone(),
            range: query_range,
            buckets,
            exact_limit,
            refine,
            fields,
        };
        if self
            .desired
            .as_ref()
            .is_some_and(|request| request.key == key)
        {
            return;
        }
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
        let compatible = self.data_key.as_ref().is_some_and(|old|
            old.revision == key.revision && old.definition == key.definition
                && old.path == key.path && old.fields == key.fields);
        if !compatible {
            self.data = None;
            self.data_key = None;
            self.backdrop = None;
            self.data_generation = self.data_generation.wrapping_add(1);
        }
        self.start_after = (compatible && self.data.is_some())
            .then(|| Instant::now() + Duration::from_millis(16));
        self.error = None;
        // Retaining the request also retains its decode plan, so pointer identity
        // cannot be recycled while a cached interpretation is still present.
        self.cache.retain(|entry| {
            entry.request.key.revision == key.revision
                && entry.request.key.definition == key.definition
                && entry.request.key.path == key.path
        });
        if let Some(index) = self.cache.iter().position(|entry| entry.request.key == key) {
            let cached = self.cache.remove(index).unwrap();
            match source.validate() {
                Ok(()) => {
                    self.data = Some(cached.data.clone());
                    self.data_key = Some(key.clone());
                    self.start_after = None;
                    self.data_generation = self.data_generation.wrapping_add(1);
                    self.cache.push_front(cached);
                }
                Err(error) => {
                    self.cache.clear();
                    self.error = Some(format!("{error:#}"));
                }
            }
        }
        let group_tiles = numeric_plan.as_ref().map(|_| self.group_tiles.clone());
        self.desired = Some(Request { key, source, node, numeric_plan, group_tiles, partial_updates });
        self.tick(ctx);
    }
    pub fn tick(&mut self, ctx: &egui::Context) {
        if let Some(running) = &self.running {
            let latest = running.latest.lock().unwrap().take();
            if let Some(outcome) = latest {
                if self
                    .desired
                    .as_ref()
                    .is_some_and(|request| request.key == outcome.key)
                {
                    match outcome.result {
                        Ok(data) => {
                            let data = match &self.backdrop {
                                Some((key, back)) if !data.complete
                                    && data.quality != PlotQuality::SampledPreview
                                    && same_series(key, &outcome.key) => Arc::new(fill_unscanned(data, back)),
                                _ => Arc::new(data),
                            };
                            if data.complete {
                                self.cache.push_front(Cached {
                                    request: self.desired.as_ref().unwrap().clone(),
                                    data: data.clone(),
                                });
                                self.cache.truncate(CACHE_ENTRIES);
                                self.backdrop = Some((outcome.key.clone(), data.clone()));
                            }
                            self.data = Some(data);
                            self.data_key = Some(outcome.key.clone());
                            self.data_generation = self.data_generation.wrapping_add(1);
                        }
                        Err(error) => {
                            self.data = None;
                            self.data_key = None;
                            self.data_generation = self.data_generation.wrapping_add(1);
                            self.error = Some(error);
                        }
                    }
                }
            }
            // The worker publishes its final outcome before setting this flag.
            // If it finished just after taking the mailbox, consume it next tick.
            if running.finished.load(Ordering::Acquire) && running.latest.lock().unwrap().is_none()
            {
                self.running = None;
            }
        }
        if self.running.is_none()
            && self.error.is_none()
            && !self.data.as_ref().is_some_and(|data| data.complete
                && self.data_key.as_ref() == self.desired.as_ref().map(|request| &request.key))
        {
            if let Some(start) = self.start_after {
                if let Some(wait) = start.checked_duration_since(Instant::now()) {
                    ctx.request_repaint_after(wait);
                    return;
                }
                self.start_after = None;
            }
            if let Some(request) = self.desired.clone() {
                let cancel = Arc::new(AtomicBool::new(false));
                let latest = Arc::new(Mutex::new(None));
                let finished = Arc::new(AtomicBool::new(false));
                self.running = Some(Running {
                    cancel: cancel.clone(),
                    latest: latest.clone(),
                    finished: finished.clone(),
                });
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let started = Instant::now();
                    let report = request.key.range.len() > AUTO_SCAN_LIMIT;
                    if report {
                        log::info!("Plot load started: {} samples {}..{}, {} fields, path {}",
                            request.key.range.len(), request.key.range.start, request.key.range.end,
                            request.numeric_plan.as_ref().map_or_else(
                                || request.key.fields.as_ref().map_or(0, Vec::len),
                                |plan| plan.fields.len()), request.key.path);
                    }
                    let publish = |data| {
                        *latest.lock().unwrap() = Some(Outcome {
                            key: request.key.clone(),
                            result: Ok(data),
                        });
                    };
                    let result = read_plot(&request, &cancel, publish);
                    if report {
                        match &result {
                            Ok(data) => log::info!("Plot load finished: {} in {:.1}s, {:?}, {} of {} samples read",
                                request.key.path, started.elapsed().as_secs_f32(), data.quality,
                                data.scanned, data.total),
                            Err(error) if cancel.load(Ordering::Relaxed) => log::info!(
                                "Plot load cancelled: {} after {:.1}s ({error})",
                                request.key.path, started.elapsed().as_secs_f32()),
                            Err(error) => log::error!("Plot load failed: {} after {:.1}s: {error}",
                                request.key.path, started.elapsed().as_secs_f32()),
                        }
                    }
                    let succeeded = result.is_ok();
                    if !cancel.load(Ordering::Relaxed) {
                        *latest.lock().unwrap() = Some(Outcome {
                            key: request.key.clone(),
                            result: result.map_err(|error| format!("{error:#}")),
                        });
                        // The visible result is ready even if neighbor prefetch
                        // continues in this worker after publication.
                        ctx.request_repaint();
                    }
                    if succeeded && !cancel.load(Ordering::Relaxed) {
                        if let (Some(plan), Some(tiles)) = (&request.numeric_plan, &request.group_tiles) {
                            prefetch_group_neighbors(&request, plan, tiles, &cancel);
                        }
                    }
                    finished.store(true, Ordering::Release);
                    ctx.request_repaint();
                });
            }
        }
        if self.running.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
    pub fn invalidate(&mut self) {
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
        self.desired = None;
        self.data = None;
        self.data_key = None;
        self.backdrop = None;
        self.start_after = None;
        self.data_generation = self.data_generation.wrapping_add(1);
        self.error = None;
        self.cache.clear();
        let mut tiles = self.group_tiles.lock().unwrap();
        let budget = tiles.budget_bytes;
        *tiles = GroupTileCache::default();
        tiles.set_budget(budget);
    }
    pub fn stop(&mut self) {
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
        self.desired = None;
    }
    pub fn data(&self) -> Option<&PlotData> {
        (self.data_key.as_ref() == self.desired.as_ref().map(|request| &request.key))
            .then(|| self.data.as_deref()).flatten()
    }
    /// Retain a prior result only when it can represent the new viewport at
    /// least as accurately. A coarse overview must not masquerade as samples
    /// after zooming back in while the exact request is still loading.
    pub fn display_data(&self) -> Option<&PlotData> {
        let data = self.data.as_deref()?;
        let current = self.data_key.as_ref()?;
        let Some(desired) = self.desired.as_ref().map(|request| &request.key) else {
            return Some(data);
        };
        // While a new view loads, keep drawing the last data for the same series
        // at its own sample positions instead of blanking the plot; the new read
        // replaces it when it lands.
        (current.revision == desired.revision
            && current.definition == desired.definition
            && current.path == desired.path
            && current.fields == desired.fields).then_some(data)
    }
    pub fn data_generation(&self) -> u64 {
        self.data_generation
    }
    pub fn set_complete_only(&mut self, complete_only: bool) {
        self.complete_only = complete_only;
    }
    pub(crate) fn set_cache_budget(&mut self, bytes: usize) {
        if self.cache_budget == bytes { return; }
        self.cache_budget = bytes;
        self.group_tiles.lock().unwrap().set_budget(bytes);
    }
    #[cfg(test)]
    pub(crate) fn group_buffer_stats(&self) -> (usize, usize) {
        let cache = self.group_tiles.lock().unwrap();
        (cache.tiles.len(), cache.loads)
    }
    #[cfg(test)]
    pub(crate) fn requested_fields(&self) -> Option<&[String]> {
        self.desired.as_ref()?.key.fields.as_deref()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn is_loading(&self) -> bool {
        self.desired.is_some()
            && self.error.is_none()
            && (self.running.is_some() || !self.data.as_ref().is_some_and(|data| data.complete
                && self.data_key.as_ref() == self.desired.as_ref().map(|request| &request.key)))
    }
}
pub(crate) fn identity(node: &Node) -> u64 {
    if let Some(array) = &node.lazy {
        return array.identity() as u64;
    }
    fn hash_node(node: &Node, hash: &mut impl Hasher) {
        node.path.hash(hash);
        node.kind.hash(hash);
        node.range.hash(hash);
        node.numeric().map(f64::to_bits).hash(hash);
        node.children.len().hash(hash);
        for child in &node.children {
            hash_node(child, hash);
        }
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    hash_node(node, &mut hash);
    hash.finish()
}
#[derive(Clone)]
struct Bucket {
    first: PlotSample,
    min: PlotSample,
    max: PlotSample,
    last: PlotSample,
}
impl Bucket {
    fn new(point: PlotSample) -> Self {
        Self {
            first: point.clone(),
            min: point.clone(),
            max: point.clone(),
            last: point,
        }
    }
    fn push(&mut self, point: PlotSample) {
        if point.value < self.min.value {
            self.min = point.clone();
        }
        if point.value > self.max.value {
            self.max = point.clone();
        }
        self.last = point;
    }
    fn points(&self) -> Vec<PlotSample> {
        let mut points = vec![
            self.first.clone(),
            self.min.clone(),
            self.max.clone(),
            self.last.clone(),
        ];
        points.sort_by_key(|point| point.index);
        points.dedup_by_key(|point| point.index);
        points
    }
}
struct Reduction {
    range: Range<Address>,
    quality: PlotQuality,
    buckets: Address,
    exact: BTreeMap<String, Vec<PlotSample>>,
    reduced: BTreeMap<String, Vec<Option<Bucket>>>,
}
impl Reduction {
    fn new(range: Range<Address>, quality: PlotQuality, buckets: Address) -> Self {
        Self {
            range,
            quality,
            buckets,
            exact: BTreeMap::new(),
            reduced: BTreeMap::new(),
        }
    }
    fn push(&mut self, name: String, point: PlotSample) {
        if self.quality == PlotQuality::Exact {
            self.exact.entry(name).or_default().push(point);
        } else {
            let bucket = ((point.index - self.range.start) as u128 * self.buckets as u128
                / (self.range.end - self.range.start).max(1) as u128)
                as Address;
            let buckets = self
                .reduced
                .entry(name)
                .or_insert_with(|| vec![None; self.buckets as usize]);
            match &mut buckets[bucket.min(self.buckets - 1) as usize] {
                Some(bucket) => bucket.push(point),
                slot => *slot = Some(Bucket::new(point)),
            }
        }
    }
    fn node(&mut self, node: &Node, prefix: &str, index: Address) {
        if let Some(value) = node.numeric().filter(|value| value.is_finite()) {
            self.push(
                if prefix.is_empty() {
                    "value".into()
                } else {
                    prefix.into()
                },
                PlotSample {
                    index,
                    value,
                    bytes: node.range.clone(),
                    path: node.path.clone().into(),
                },
            );
        }
        for child in &node.children {
            let prefix = if prefix.is_empty() {
                child.name.clone()
            } else {
                format!("{prefix}.{}", child.name)
            };
            self.node(child, &prefix, index);
        }
    }
    fn data(&self, scanned: Address, complete: bool) -> PlotData {
        let mut envelopes = BTreeMap::new();
        let series = if self.quality != PlotQuality::Exact {
            self.reduced
                .iter()
                .map(|(name, buckets)| {
                    envelopes.insert(
                        name.clone(),
                        buckets
                            .iter()
                            .enumerate()
                            .filter_map(|(index, bucket)| {
                                let bucket = bucket.as_ref()?;
                                Some(PlotEnvelope {
                                    range: bucket_range(
                                        &self.range,
                                        self.buckets,
                                        index as Address,
                                    ),
                                    min: bucket.min.clone(),
                                    max: bucket.max.clone(),
                                })
                            })
                            .collect(),
                    );
                    (
                        name.clone(),
                        buckets.iter().flatten().flat_map(Bucket::points).collect(),
                    )
                })
                .collect()
        } else {
            self.exact.clone()
        };
        let bounds = series.iter().filter_map(|(name, points)| {
            let mut values = points.iter().map(|point| point.value).filter(|value| value.is_finite());
            let first = values.next()?;
            Some((name.clone(), values.fold((first, first), |(lo, hi), value| (lo.min(value), hi.max(value)))))
        }).collect();
        PlotData {
            range: self.range.clone(),
            series,
            envelopes,
            bounds,
            quality: self.quality,
            scanned,
            total: (self.range.end - self.range.start),
            complete,
        }
    }
}
fn node_count(node: &Node) -> Address {
    1 + node.children.iter().map(node_count).sum::<Address>()
}

// The push reducers use floor(index * buckets / length). Ceil boundaries
// describe precisely the sample indices assigned to that display bucket.
fn bucket_range(range: &Range<Address>, buckets: Address, index: Address) -> Range<Address> {
    let length = (range.end - range.start) as u128;
    let start = (index as u128 * length).div_ceil(buckets as u128) as Address;
    let end = ((index + 1) as u128 * length).div_ceil(buckets as u128) as Address;
    range.start + start..range.start + end
}

// Preserve individual samples while there is useful screen resolution. The
// 64-pixel quantization keeps small pane resizes from restarting plot queries;
// the cap bounds exact point storage even on very large displays.
fn exact_limit(pixel_width: Address) -> Address {
    pixel_width
        .clamp(MIN_EXACT_LIMIT / 2, MAX_EXACT_LIMIT / 2)
        .div_ceil(64)
        * 64
        * 2
}

// Subpixel buckets preserve local turning points before the renderer chooses
// between individual traces and dense amplitude ranges. Quantization limits
// resize churn; saturation bounds storage on large or high-DPI displays.
fn display_buckets(pixel_width: Address) -> Address {
    pixel_width
        .saturating_mul(2)
        .clamp(128, MAX_BUCKETS)
        .div_ceil(64)
        * 64
}

fn request_resolution(
    range: &Range<Address>, pixel_width: Address, field_count: Option<usize>,
) -> (Address, Address) {
    // A grouped scan has a shared point budget. Wider canvases cannot add
    // buckets once that budget is reached, so they should reuse the same scan.
    let buckets = field_count.map_or(display_buckets(pixel_width), |count| {
        display_buckets(pixel_width)
            .min((MAX_TOTAL_NUMERIC_POINTS / count.max(1) as Address).max(1))
    });
    let exact = if range.end - range.start > MAX_EXACT_LIMIT {
        MAX_EXACT_LIMIT
    } else {
        exact_limit(pixel_width)
    };
    // A grouped scan reads every field exactly only within its shared budget;
    // beyond it the reader reduces, so it must not be recorded as exact.
    let exact = field_count.map_or(exact, |count| exact.min(field_budget(count)));
    (buckets, exact)
}

#[cfg(not(target_arch = "wasm32"))]
fn numeric_query_coverage(view: &Range<Address>, count: Address) -> Range<Address> {
    if view.is_empty() { return view.clone(); }
    // A modest aligned margin means a short pan changes only the plot transform.
    // The cap prevents broad scans from growing substantially beyond the view.
    let quantum = view.len().div_ceil(4).checked_next_power_of_two()
        .unwrap_or(Address::MAX).clamp(256, 4096);
    let start = view.start.saturating_sub(quantum) / quantum * quantum;
    let end = view.end.saturating_add(quantum).div_ceil(quantum)
        .saturating_mul(quantum).min(count);
    start..end.max(view.end)
}

#[cfg(not(target_arch = "wasm32"))]
fn query_resolution(view: &Range<Address>, query: &Range<Address>,
    pixel_width: Address, field_count: Option<usize>) -> (Address, Address) {
    let (base_buckets, base_exact) = request_resolution(view, pixel_width, field_count);
    let field_cap = field_count.map_or(MAX_BUCKETS, |count|
        (MAX_TOTAL_NUMERIC_POINTS / count.max(1) as Address).max(1).min(MAX_BUCKETS));
    let buckets = base_buckets.saturating_mul(query.len())
        .div_ceil(view.len().max(1)).clamp(1, field_cap);
    let exact = if view.len() <= base_exact { query.len() } else { base_exact };
    (buckets, exact)
}

/// Samples per field a grouped scan can read exactly.
fn field_budget(field_count: usize) -> Address {
    (MAX_TOTAL_NUMERIC_POINTS / field_count.max(1) as Address).max(1)
}

/// The range a new request for `view` would scan: numeric refinements add an
/// aligned margin so short pans reuse the scan. When the view itself fits a
/// grouped scan's exact budget, the margin is trimmed to fit it too, so the
/// view is read exactly rather than reduced.
#[cfg(not(target_arch = "wasm32"))]
fn fresh_query(view: &Range<Address>, count: Address, numeric: bool, refine: bool,
    field_count: Option<usize>) -> Range<Address> {
    if !(numeric && refine) {
        return view.clone();
    }
    let coverage = numeric_query_coverage(view, count);
    match field_count.map(field_budget) {
        Some(budget) if view.len() <= budget && coverage.len() > budget => {
            let start = view.start.saturating_sub((budget - view.len()) / 2);
            let end = (start + budget).min(count);
            end.saturating_sub(budget).min(view.start)..end
        }
        _ => coverage,
    }
}

/// Whether a cached scan is as good as a fresh one for `view`: it spans the view
/// and has at least as many buckets per sample. Comparing density rather than
/// bucket counts matters once a budget caps buckets: a wide scan zoomed into
/// must not pass just because a fresh scan hits the same cap over a narrower range.
/// Two requests for the same series of the same decode, whatever their range.
#[cfg(not(target_arch = "wasm32"))]
fn same_series(a: &Key, b: &Key) -> bool {
    a.revision == b.revision && a.definition == b.definition && a.path == b.path && a.fields == b.fields
}

/// A partial result with the unscanned rest of its range taken from `back`. The
/// scan runs forward from the range start, so everything past `start + scanned`
/// is not read yet; `back` fills it until the scan gets there.
#[cfg(not(target_arch = "wasm32"))]
fn fill_unscanned(partial: PlotData, back: &PlotData) -> PlotData {
    let front = partial.range.start + partial.scanned;
    let end = partial.range.end;
    let unscanned = |index: Address| index >= front && index < end;
    let mut series = partial.series;
    for (name, points) in &back.series {
        series.entry(name.clone()).or_default()
            .extend(points.iter().filter(|point| unscanned(point.index)).cloned());
    }
    let mut envelopes = partial.envelopes;
    for (name, bands) in &back.envelopes {
        envelopes.entry(name.clone()).or_default()
            .extend(bands.iter().filter(|band| unscanned(band.range.start)).cloned());
    }
    let mut bounds = partial.bounds;
    for (name, &(low, high)) in &back.bounds {
        let entry = bounds.entry(name.clone()).or_insert((low, high));
        *entry = (entry.0.min(low), entry.1.max(high));
    }
    PlotData { series, envelopes, bounds, ..partial }
}

#[cfg(not(target_arch = "wasm32"))]
fn covers_view(key: &Key, view: &Range<Address>, pixel_width: Address,
    field_count: Option<usize>, fresh: &Range<Address>) -> bool {
    if key.range.start > view.start || key.range.end < view.end { return false; }
    let (_, wanted_exact) = request_resolution(view, pixel_width, field_count);
    if view.len() <= wanted_exact && key.range.len() > key.exact_limit { return false; }
    if key.range.len() <= key.exact_limit { return true; }
    let (fresh_buckets, _) = query_resolution(view, fresh, pixel_width, field_count);
    // Up to a third coarser still counts, so pans (whose margins shift the range
    // a little) reuse the scan while zooming in twofold or more reads again.
    3 * key.buckets as u128 * fresh.len().max(1) as u128 >= 2 * fresh_buckets as u128 * key.range.len() as u128
}

fn reduction_buckets(quality: PlotQuality, requested: Address) -> Address {
    if quality == PlotQuality::SampledPreview {
        requested.clamp(2, PREVIEW_WINDOWS)
    } else {
        requested.clamp(1, MAX_BUCKETS)
    }
}

// Contiguous windows capture local waveform amplitude instead of isolated
// phases. Starts vary within evenly distributed strata; exact endpoints are
// included without claiming that any unread interval has been scanned.
fn preview_windows(range: Range<Address>, buckets: Address) -> Vec<Range<Address>> {
    preview_windows_with_budget(range, buckets, PREVIEW_LIMIT)
}
fn preview_windows_with_budget(range: Range<Address>, buckets: Address, sample_budget: Address) -> Vec<Range<Address>> {
    if range.is_empty() {
        return Vec::new();
    }
    let count =
        reduction_buckets(PlotQuality::SampledPreview, buckets).min(range.end - range.start);
    (0..count)
        .map(|bucket| {
            let stratum = bucket_range(&range, count, bucket);
            let width = (stratum.end - stratum.start)
                .min(PREVIEW_WINDOW_SIZE)
                .min((sample_budget / count).max(1));
            let start = if bucket == 0 {
                stratum.start
            } else if bucket + 1 == count {
                stratum.end - width
            } else {
                let mut hash = (bucket as u64)
                    .wrapping_add(range.start as u64)
                    .wrapping_add(0x9e3779b97f4a7c15);
                hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d049bb133111eb);
                hash ^= hash >> 31;
                stratum.start + hash as Address % ((stratum.end - stratum.start) - width + 1)
            };
            start..start + width
        })
        .collect()
}
fn read_plot(
    request: &Request,
    cancel: &AtomicBool,
    mut publish: impl FnMut(PlotData),
) -> Result<PlotData> {
    request.source.validate()?;
    ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
    let range = &request.key.range;
    let quality = if (range.end - range.start) <= request.key.exact_limit {
        PlotQuality::Exact
    } else if (range.end - range.start) <= AUTO_SCAN_LIMIT || request.key.refine {
        PlotQuality::MinMax
    } else {
        PlotQuality::SampledPreview
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let (Some(plan), Some(tiles)) = (&request.numeric_plan, &request.group_tiles) {
        // An overview is deliberately sampled. Building complete group tiles
        // here turns every selected collection into a full-file scan.
        if quality == PlotQuality::SampledPreview {
            return read_numeric(request, plan.clone(), quality, cancel, publish);
        }
        return read_group_buffered(request, plan.clone(), quality, tiles, cancel, publish);
    }
    if let Some(mut plan) = request.numeric_plan.clone().or_else(|| request
        .node
        .lazy
        .as_ref()
        .and_then(|array| array.numeric_plan()))
        .filter(|plan| plan.stride > 0 && plan.stride <= READ_LIMIT)
    {
        if let Some(selected) = &request.key.fields {
            let selected: std::collections::HashSet<_> = selected.iter().collect();
            plan.fields.retain(|field| selected.contains(&field.name));
        }
        return read_numeric(request, plan, quality, cancel, publish);
    }
    let mut reduction = Reduction::new(
        range.clone(),
        quality,
        reduction_buckets(quality, request.key.buckets),
    );
    let mut scanned = 0;
    let mut last_publish = Instant::now();
    let mut add = |node: &Node, index: Address, reduction: &mut Reduction| {
        reduction.node(node, "", index);
        scanned += 1;
        if request.partial_updates && last_publish.elapsed() >= Duration::from_millis(250) {
            publish(reduction.data(scanned, false));
            last_publish = Instant::now();
        }
    };
    let windows = if quality == PlotQuality::SampledPreview {
        preview_windows(range.clone(), request.key.buckets)
    } else {
        vec![range.clone()]
    };
    if let Some(array) = &request.node.lazy {
        let mut batch = None;
        for window in windows {
            let mut index = window.start;
            while index < window.end {
                ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
                let rows = array.page(
                    &request.source,
                    cancel,
                    index,
                    batch.unwrap_or(1).min(window.end - index),
                )?;
                ensure!(!rows.is_empty(), "collection ended during plot read");
                if batch.is_none() {
                    batch = Some((16_000 / node_count(&rows[0]).max(1)).clamp(1, 128));
                }
                for (offset, row) in rows.iter().enumerate() {
                    add(row, index + offset as Address, &mut reduction);
                }
                index += rows.len() as Address;
            }
        }
    } else {
        for window in windows {
            for index in window {
                ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
                if let Some(node) = request
                    .node
                    .children
                    .get(usize::try_from(index).unwrap_or(usize::MAX))
                {
                    add(node, index, &mut reduction);
                }
            }
        }
    }
    request.source.validate()?;
    ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
    Ok(reduction.data(scanned, true))
}

#[derive(Clone, Copy)]
struct NumericPoint {
    index: Address,
    value: f64,
}
#[derive(Clone, Copy)]
struct NumericBucket {
    first: NumericPoint,
    min: NumericPoint,
    max: NumericPoint,
    last: NumericPoint,
}
impl NumericBucket {
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop only.
    fn merge(&mut self, next: Self) {
        if next.min.value < self.min.value { self.min = next.min; }
        if next.max.value > self.max.value { self.max = next.max; }
        self.last = next.last;
    }
    fn push(&mut self, point: NumericPoint) {
        if point.value < self.min.value {
            self.min = point;
        }
        if point.value > self.max.value {
            self.max = point;
        }
        self.last = point;
    }
}
struct NumericReduction {
    plan: NumericPlan,
    range: Range<Address>,
    quality: PlotQuality,
    buckets: Address,
    exact: Vec<Vec<NumericPoint>>,
    reduced: Vec<Vec<Option<NumericBucket>>>,
}
impl NumericReduction {
    fn new(
        plan: NumericPlan,
        range: Range<Address>,
        quality: PlotQuality,
        buckets: Address,
    ) -> Self {
        let count = plan.fields.len();
        Self {
            plan,
            range,
            quality,
            buckets,
            exact: vec![Vec::new(); count],
            reduced: if quality != PlotQuality::Exact {
                vec![vec![None; buckets as usize]; count]
            } else {
                Vec::new()
            },
        }
    }
    fn record(&mut self, record: &[u8], index: Address) -> Result<()> {
        for field_index in 0..self.plan.fields.len() {
            let value = self.plan.fields[field_index].decode(record)?;
            self.record_value(field_index, value, index);
        }
        Ok(())
    }
    fn record_value(&mut self, field_index: usize, value: f64, index: Address) {
        if !value.is_finite() { return; }
        let bucket = ((index - self.range.start) as u128 * self.buckets as u128
            / (self.range.end - self.range.start).max(1) as u128) as Address;
        let point = NumericPoint { index, value };
        if self.quality != PlotQuality::Exact {
            match &mut self.reduced[field_index][bucket.min(self.buckets - 1) as usize] {
                Some(bucket) => bucket.push(point),
                slot => {
                    *slot = Some(NumericBucket {
                        first: point,
                        min: point,
                        max: point,
                        last: point,
                    })
                }
            }
        } else {
            self.exact[field_index].push(point);
        }
    }
    fn data(&self, scanned: Address, complete: bool) -> PlotData {
        let mut series: BTreeMap<String, Vec<PlotSample>> = BTreeMap::new();
        let mut envelopes = BTreeMap::new();
        for (field_index, field) in self.plan.fields.iter().enumerate() {
            let path_parts = Arc::new((field.source_path.as_deref().unwrap_or(&self.plan.path).to_owned(),
                field.suffix.clone()));
            let sample = |point: NumericPoint| {
                let start = self.plan.offset(point.index).unwrap_or(self.plan.start) + field.offset;
                PlotSample {
                    index: point.index,
                    value: point.value,
                    bytes: start..start + field.span,
                    path: PlotPath::Indexed { parts: path_parts.clone(), index: point.index },
                }
            };
            let points = if self.quality != PlotQuality::Exact {
                let mut points = Vec::new();
                let mut bands = Vec::new();
                for (index, bucket) in self.reduced[field_index].iter().enumerate() {
                    let Some(bucket) = bucket else {
                        continue;
                    };
                    bands.push(PlotEnvelope {
                        range: bucket_range(&self.range, self.buckets, index as Address),
                        min: sample(bucket.min),
                        max: sample(bucket.max),
                    });
                    let mut selected = [bucket.first, bucket.min, bucket.max, bucket.last];
                    selected.sort_unstable_by_key(|point| point.index);
                    let mut previous = None;
                    for point in selected {
                        if previous == Some(point.index) {
                            continue;
                        }
                        previous = Some(point.index);
                        points.push(point);
                    }
                }
                envelopes.insert(field.name.clone(), bands);
                points
            } else {
                self.exact[field_index].clone()
            };
            series.insert(field.name.clone(), points.into_iter().map(sample).collect());
        }
        let bounds = series.iter().filter_map(|(name, points)| {
            let mut values = points.iter().map(|point| point.value).filter(|value| value.is_finite());
            let first = values.next()?;
            Some((name.clone(), values.fold((first, first), |(lo, hi), value| (lo.min(value), hi.max(value)))))
        }).collect();
        PlotData {
            range: self.range.clone(),
            series,
            envelopes,
            bounds,
            quality: self.quality,
            scanned,
            total: (self.range.end - self.range.start),
            complete,
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
fn group_tile_size(plan: &NumericPlan) -> Address {
    let stride = plan.physical_stride().max(1);
    GROUP_TILE_SAMPLES.min(((READ_LIMIT.saturating_sub(plan.stride)) / stride).saturating_add(1).max(1))
}
#[cfg(not(target_arch = "wasm32"))]
fn eligible_numeric_plan(node: &Node, selected: Option<&[String]>) -> Option<NumericPlan> {
    let mut plan = node.lazy.as_ref()?.numeric_plan()?;
    if plan.stride == 0 || plan.stride > READ_LIMIT { return None; }
    if let Some(selected) = selected {
        let names: std::collections::HashSet<_> = selected.iter().collect();
        plan.fields.retain(|field| names.contains(&field.name));
    }
    (!plan.fields.is_empty()).then_some(plan)
}
#[cfg(not(target_arch = "wasm32"))]
fn group_signature(request: &Request) -> GroupSignature {
    GroupSignature {
        revision: request.key.revision,
        definition: request.key.definition,
        path: request.key.path.clone(),
        fields: request.key.fields.clone().unwrap_or_default(),
    }
}
#[cfg(not(target_arch = "wasm32"))]
fn load_group_tile(request: &Request, plan: &NumericPlan, start: Address, size: Address,
    cancel: &AtomicBool) -> Result<GroupTile> {
    let count = request.node.lazy.as_ref().map_or(0, |array| array.count);
    let end = start.saturating_add(size).min(count);
    let mut chunks = Vec::new();
    let mut index = start;
    while index < end {
        ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
        let next = index.saturating_add(plan.contiguous_count(index)).min(end);
        let offset = plan.offset(index).ok_or_else(|| anyhow::anyhow!("plot offset overflow"))?;
        let length = (next - index - 1).checked_mul(plan.physical_stride())
            .and_then(|bytes| bytes.checked_add(plan.stride))
            .ok_or_else(|| anyhow::anyhow!("plot length overflow"))?;
        let bytes = request.source.read_validated(offset, length)?;
        chunks.push((index, next, bytes));
        index = next;
    }
    let decode_field = |field: &crate::structure::NumericField| -> Result<Vec<f64>> {
        let mut values = Vec::with_capacity(end - start);
        for (first_index, last_index, bytes) in &chunks {
            for sample in *first_index..*last_index {
                if (sample - *first_index) % 256 == 0 {
                    ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
                }
                let offset = (sample - *first_index) * plan.physical_stride();
                values.push(field.decode(&bytes[offset..offset + plan.stride])?);
            }
        }
        Ok(values)
    };
    // A wide standalone tile can use field parallelism. During a parallel
    // tile scan, the outer batch already occupies the Rayon pool.
    let values: Vec<Vec<f64>> = if plan.fields.len() > 8 && end - start >= 1024
        && rayon::current_thread_index().is_none() {
        use rayon::prelude::*;
        plan.fields.par_iter().map(decode_field).collect::<Result<Vec<_>>>()?
    } else {
        plan.fields.iter().map(decode_field).collect::<Result<Vec<_>>>()?
    };
    Ok(GroupTile::from_values(start, values))
}
#[cfg(not(target_arch = "wasm32"))]
fn cached_group_tile(request: &Request, plan: &NumericPlan,
    cache: &Arc<Mutex<GroupTileCache>>, signature: &GroupSignature,
    start: Address, cancel: &AtomicBool) -> Result<Arc<GroupTile>> {
    let (flight, owner) = {
        let mut cache = cache.lock().unwrap();
        cache.prepare(signature);
        if let Some(tile) = cache.get(start) {
            return Ok(tile);
        }
        let key = (signature.clone(), start);
        if let Some(flight) = cache.in_flight.get(&key) {
            (flight.clone(), false)
        } else {
            let flight = Arc::new(TileFlight::default());
            cache.in_flight.insert(key, flight.clone());
            (flight, true)
        }
    };
    if !owner {
        let mut result = flight.result.lock().unwrap();
        loop {
            ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
            if let Some(result) = result.as_ref() {
                return result.clone().map_err(anyhow::Error::msg);
            }
            result = flight.ready.wait_timeout(result, Duration::from_millis(25)).unwrap().0;
        }
    }
    let size = group_tile_size(plan);
    let result = load_group_tile(request, plan, start, size, cancel).map(Arc::new);
    {
        let mut cache = cache.lock().unwrap();
        let key = (signature.clone(), start);
        if cache.in_flight.get(&key).is_some_and(|current| Arc::ptr_eq(current, &flight)) {
            cache.in_flight.remove(&key);
        }
        if !cancel.load(Ordering::Relaxed) && cache.signature.as_ref() == Some(signature) {
            if let Ok(tile) = &result { cache.insert(tile.clone(), size); }
        }
    }
    *flight.result.lock().unwrap() = Some(result.as_ref().map(Arc::clone)
        .map_err(|error| format!("{error:#}")));
    flight.ready.notify_all();
    result
}
#[cfg(not(target_arch = "wasm32"))]
fn cached_group_summary(request: &Request, plan: &NumericPlan,
    cache: &Arc<Mutex<GroupTileCache>>, signature: &GroupSignature,
    start: Address, cancel: &AtomicBool) -> Result<Arc<GroupTileSummary>> {
    {
        let mut cache = cache.lock().unwrap();
        cache.prepare(signature);
        if let Some(summary) = cache.summary(start) { return Ok(summary); }
    }
    Ok(cached_group_tile(request, plan, cache, signature, start, cancel)?.summary.clone())
}
#[cfg(not(target_arch = "wasm32"))]
fn read_group_buffered(request: &Request, plan: NumericPlan, quality: PlotQuality,
    cache: &Arc<Mutex<GroupTileCache>>, cancel: &AtomicBool,
    mut publish: impl FnMut(PlotData)) -> Result<PlotData> {
    use rayon::prelude::*;
    let field_count = plan.fields.len().max(1);
    let quality = if quality == PlotQuality::Exact
        && request.key.range.len().saturating_mul(field_count) > MAX_TOTAL_NUMERIC_POINTS {
        PlotQuality::MinMax
    } else { quality };
    let buckets = reduction_buckets(quality, request.key.buckets)
        .min((MAX_TOTAL_NUMERIC_POINTS / field_count).max(1));
    let mut reduction = NumericReduction::new(plan.clone(), request.key.range.clone(), quality, buckets);
    let tile_size = group_tile_size(&plan);
    let mut index = request.key.range.start;
    let mut scanned = 0;
    let mut last_publish = Instant::now();
    let mut last_progress = Instant::now();
    let mut last_reported = 0;
    let mut published = false;
    let signature = group_signature(request);
    let tile_bytes = tile_size.saturating_mul(field_count).saturating_mul(std::mem::size_of::<f64>());
    let headroom = std::thread::available_parallelism().map_or(1, |cores| cores.get().saturating_sub(2).max(1));
    let workers = rayon::current_num_threads().min(headroom).max(1);
    let budget = cache.lock().unwrap().budget_bytes;
    let batch_tiles = ((budget / 2) / tile_bytes.max(1)).clamp(1, workers.saturating_mul(2));
    if request.key.range.len() > AUTO_SCAN_LIMIT {
        log::info!("Grouped plot scan: {} Rayon threads, up to {} tiles per batch, {} samples per tile",
            workers, batch_tiles, tile_size);
    }
    while index < request.key.range.end {
        ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
        if quality != PlotQuality::Exact {
            let coarse = cache.lock().unwrap().covering_summary(index, &request.key.range, buckets);
            if let Some(coarse) = coarse {
                for (field_index, bucket) in coarse.bins[0].1.iter().enumerate() {
                    let bucket = bucket.expect("complete summary has all selected fields");
                    let mut points = [bucket.first, bucket.min, bucket.max, bucket.last];
                    points.sort_unstable_by_key(|point| point.index);
                    let mut previous = None;
                    for point in points {
                        if previous == Some(point.index) { continue; }
                        previous = Some(point.index);
                        reduction.record_value(field_index, point.value, point.index);
                    }
                }
                scanned += coarse.end - index;
                index = coarse.end;
                continue;
            }
        }
        // File reads and field decoding are independent for each tile. Prepare
        // a bounded batch in parallel, then fold summaries in sample order.
        // This keeps the reduction deterministic and partial updates useful.
        let first_tile = index / tile_size;
        let last_tile = (request.key.range.end - 1) / tile_size;
        let starts: Vec<_> = (first_tile..=last_tile).take(batch_tiles)
            .map(|tile| tile * tile_size).collect();
        let summaries: Vec<_> = starts.par_iter().map(|&start|
            cached_group_summary(request, &plan, cache, &signature, start, cancel)
        ).collect::<Result<_>>()?;
        for summary in summaries {
        ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
        let end = summary.end.min(request.key.range.end);
        let mut raw: Option<Arc<GroupTile>> = None;
        for (bin_range, fields) in &summary.bins {
            let first = bin_range.start.max(index);
            let last = bin_range.end.min(end);
            if first >= last { continue; }
            let first_bucket = ((first - request.key.range.start) as u128 * buckets as u128
                / request.key.range.len().max(1) as u128) as Address;
            let last_bucket = ((last - 1 - request.key.range.start) as u128 * buckets as u128
                / request.key.range.len().max(1) as u128) as Address;
            if quality != PlotQuality::Exact && first == bin_range.start
                && last == bin_range.end && first_bucket == last_bucket {
                for (field_index, bucket) in fields.iter().enumerate() {
                    if let Some(bucket) = bucket {
                        let mut points = [bucket.first, bucket.min, bucket.max, bucket.last];
                        points.sort_unstable_by_key(|point| point.index);
                        let mut previous = None;
                        for point in points {
                            if previous == Some(point.index) { continue; }
                            previous = Some(point.index);
                            reduction.record_value(field_index, point.value, point.index);
                        }
                    }
                }
            } else {
                if raw.is_none() { raw = Some(cached_group_tile(request, &plan, cache, &signature, summary.start, cancel)?); }
                let tile = raw.as_ref().unwrap();
                for sample in first..last {
                    let position = sample - tile.start;
                    for field_index in 0..tile.values.len() {
                        reduction.record_value(field_index, tile.values[field_index][position], sample);
                    }
                }
            }
            scanned += last - first;
        }
        index = end;
        if last_progress.elapsed() >= Duration::from_secs(5) {
            log::info!("Plot load progress: {} — {} of {} samples scanned ({:.0} samples/s over last interval)",
                request.key.path, scanned, request.key.range.len(),
                (scanned - last_reported) as f64 / last_progress.elapsed().as_secs_f64().max(0.001));
            last_reported = scanned;
            last_progress = Instant::now();
        }
        if request.partial_updates && (!published || last_publish.elapsed() >= Duration::from_secs(1)) {
            request.source.validate()?;
            publish(reduction.data(scanned, false));
            last_publish = Instant::now();
            published = true;
        }
        }
    }
    request.source.validate()?;
    ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
    Ok(reduction.data(scanned, true))
}
#[cfg(not(target_arch = "wasm32"))]
fn prefetch_group_neighbors(request: &Request, plan: &NumericPlan,
    cache: &Arc<Mutex<GroupTileCache>>, cancel: &AtomicBool) {
    if request.key.range.len() > AUTO_SCAN_LIMIT { return; }
    let size = group_tile_size(plan);
    let count = request.node.lazy.as_ref().map_or(0, |array| array.count);
    let tiles = (request.key.range.len().div_ceil(size)).clamp(1, 8);
    let signature = group_signature(request);
    let before = request.key.range.start / size;
    let after = request.key.range.end.div_ceil(size);
    for step in 0..tiles {
        if cancel.load(Ordering::Relaxed) { break; }
        if let Some(start) = before.checked_sub(step + 1).and_then(|tile| tile.checked_mul(size)) {
            let _ = cached_group_tile(request, plan, cache, &signature, start, cancel);
        }
        if let Some(start) = after.checked_add(step).and_then(|tile| tile.checked_mul(size)) {
            if start < count { let _ = cached_group_tile(request, plan, cache, &signature, start, cancel); }
        }
    }
}
fn read_numeric(
    request: &Request,
    plan: NumericPlan,
    quality: PlotQuality,
    cancel: &AtomicBool,
    mut publish: impl FnMut(PlotData),
) -> Result<PlotData> {
    ensure!(
        request.source.revision == plan.revision,
        "Bytes changed; run the decoder again before plotting this collection."
    );
    let field_count = plan.fields.len().max(1) as Address;
    let quality = if quality == PlotQuality::Exact
        && (request.key.range.end - request.key.range.start).saturating_mul(field_count) > MAX_TOTAL_NUMERIC_POINTS
    {
        PlotQuality::MinMax
    } else {
        quality
    };
    let buckets = reduction_buckets(quality, request.key.buckets)
        .min((MAX_TOTAL_NUMERIC_POINTS / field_count).max(1));
    let mut reduction = NumericReduction::new(
        plan,
        request.key.range.clone(),
        quality,
        buckets,
    );
    let mut scanned = 0;
    let mut last_publish = Instant::now();
    let mut last_progress = Instant::now();
    let range = request.key.range.clone();
    let stride = reduction.plan.stride;
    let physical_stride = reduction.plan.physical_stride();
    // Window size and physical read size are independent: even very wide
    // records are read in chunks no larger than the ByteView's 1 MiB limit.
    let windows = if quality == PlotQuality::SampledPreview {
        let budget = (1_000_000 / reduction.plan.fields.len().max(1) as Address)
            .clamp(2_048, PREVIEW_LIMIT);
        preview_windows_with_budget(range, request.key.buckets, budget)
    } else {
        vec![range]
    };
    let batch = (READ_LIMIT / physical_stride.max(1)).max(1);
    for window in windows {
        let mut index = window.start;
        while index < window.end {
            let span = index..index.saturating_add(batch)
                .min(index.saturating_add(reduction.plan.contiguous_count(index)))
                .min(window.end);
            ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
            let start = reduction.plan.offset(span.start)
                .ok_or_else(|| anyhow::anyhow!("plot offset overflow"))?;
            let length = (span.end - span.start - 1)
                .checked_mul(physical_stride)
                .and_then(|length| length.checked_add(stride))
                .ok_or_else(|| anyhow::anyhow!("plot length overflow"))?;
            let bytes = request.source.read_validated(start, length)?;
            ensure!(
                bytes.len() as Address == length,
                "source ended during plot read"
            );
            for offset in 0..(span.end - span.start) {
                if offset % 128 == 0 {
                    ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
                }
                let start = usize::try_from(offset * physical_stride)?;
                let end = start + usize::try_from(stride)?;
                let record = &bytes[start..end];
                reduction.record(record, span.start + offset as Address)?;
                scanned += 1;
            }
            if request.partial_updates && last_publish.elapsed() >= Duration::from_millis(250) {
                request.source.validate()?;
                publish(reduction.data(scanned, false));
                last_publish = Instant::now();
            }
            if last_progress.elapsed() >= Duration::from_secs(5) {
                log::info!("Plot load progress: {} — {} of {} samples scanned",
                    request.key.path, scanned, request.key.range.end - request.key.range.start);
                last_progress = Instant::now();
            }
            index = span.end;
        }
    }
    request.source.validate()?;
    ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
    Ok(reduction.data(scanned, true))
}

/// Shared viewport query used by native thread workers and browser Web Workers.
#[cfg(any(test, target_arch = "wasm32"))]
pub(crate) fn read_for_viewport(
    source: ByteView, node: Node, range: Range<Address>, pixel_width: Address,
) -> Result<PlotData> {
    let plan = node.lazy.as_ref().and_then(|array| array.numeric_plan());
    read_viewport_plan(source, node, plan, range, pixel_width)
}

/// Coalesce compatible projections before reading bytes, using the same numeric
/// plans and reduction policy as native grouped readers. Incompatible layouts
/// retain their own query rather than assuming a common record stride.
#[cfg(any(test, target_arch = "wasm32"))]
pub(crate) fn read_for_collections(
    source: ByteView, nodes: &[Node], range: Range<Address>, pixel_width: Address,
) -> Result<Vec<(Vec<String>, PlotData)>> {
    compatible_numeric_groups(nodes).into_iter().map(|(node, plan, paths)| {
        let data = read_viewport_plan(source.clone(), node, plan, range.clone(), pixel_width)?;
        Ok((paths, data))
    }).collect()
}

/// Shared grouping policy for native reader scheduling and browser workers.
pub(crate) fn compatible_numeric_groups(nodes: &[Node]) -> Vec<(Node, Option<NumericPlan>, Vec<String>)> {
    let mut groups: Vec<(Node, Vec<NumericPlan>, Vec<String>)> = Vec::new();
    for node in nodes {
        let plan = node.lazy.as_ref().and_then(|array| array.numeric_plan());
        if let Some(plan) = plan {
            let count = node.lazy.as_ref().map(|a| a.count);
            if let Some((_, plans, paths)) = groups.iter_mut().find(|(first, plans, _)| {
                if first.lazy.as_ref().map(|a| a.count) != count || plans.is_empty() { return false; }
                let mut candidates = plans.clone(); candidates.push(plan.clone());
                NumericPlan::combine(&candidates).is_some()
            }) {
                plans.push(plan); paths.push(node.path.clone()); continue;
            }
            groups.push((node.clone(), vec![plan], vec![node.path.clone()]));
        } else {
            groups.push((node.clone(), Vec::new(), vec![node.path.clone()]));
        }
    }
    groups.into_iter().map(|(node, plans, paths)| {
        let plan = if plans.len() > 1 { NumericPlan::combine(&plans) } else { plans.into_iter().next() };
        (node, plan, paths)
    }).collect()
}

#[cfg(any(test, target_arch = "wasm32"))]
fn read_viewport_plan(
    source: ByteView, node: Node, numeric_plan: Option<NumericPlan>,
    range: Range<Address>, pixel_width: Address,
) -> Result<PlotData> {
    let fields = numeric_plan.as_ref().map(|plan| plan.fields.len());
    let count = node.lazy.as_ref().map_or(node.children.len() as Address, |a| a.count);
    let start = range.start.min(count);
    let range = start..range.end.max(start).min(count);
    let (buckets, exact_limit) = request_resolution(&range, pixel_width, fields);
    let request = Request {
        key: Key { revision: source.revision, definition: identity(&node),
            path: node.path.clone(), range, buckets, exact_limit,
            refine: false, fields: None },
        source, node, numeric_plan,
        #[cfg(not(target_arch = "wasm32"))]
        group_tiles: None,
        partial_updates: false,
    };
    read_plot(&request, &AtomicBool::new(false), |_| {})
}
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    #[test]
    fn filtered_numeric_plot_reads_only_selected_fields() {
        let yaml = "structures:\n  - name: Record\n    fields:\n      - {name: a, type: u16}\n      - {name: b, type: u16}\n      - {name: c, type: u16}\nactions:\n  - read: {name: records, type: {structure: Record}, repeat: 512}\n";
        let bytes: Vec<u8> = (0..512_u16).flat_map(|i| [i, i + 1, i + 2].into_iter().flat_map(u16::to_le_bytes)).collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request_filtered(&ctx, store.view(), output.nodes[0].clone(), 0..512, 800, false, Some(vec!["b".into()]));
        settle(&mut reader, &ctx);
        let data = reader.data().unwrap();
        assert_eq!(data.series.keys().map(String::as_str).collect::<Vec<_>>(), ["b"]);
        assert_eq!(data.series["b"].len(), 512);
        assert_eq!(data.series["b"][0].bytes, 2..4);
    }
    #[test]
    fn reductions_keep_fields_beyond_thirty_two() {
        for quality in [PlotQuality::Exact, PlotQuality::MinMax, PlotQuality::SampledPreview] {
            let mut reduction = Reduction::new(0..2, quality, 1);
            for field in 0..65 {
                for index in 0..2 {
                    reduction.push(format!("field{field}"), PlotSample {
                        index, value: field as f64 + index as f64,
                        bytes: (index*130+field*2)..(index*130+field*2+2), path: format!("samples[{index}].field{field}").into(),
                    });
                }
            }
            let data = reduction.data(2,true);
            assert_eq!(data.series.len(),65);
            assert_eq!(data.series["field64"][1].value,65.);
            assert_eq!(data.series["field64"][1].bytes,258..260);
        }
    }

    use super::*;
    use crate::binary::Original;
    use crate::byte_store::ByteStore;
    use crate::structure::Program;

    #[test]
    fn interleaved_lazy_plot_keeps_each_stream_and_exact_byte_ranges() {
        let yaml = r#"
structures:
  - name: A
    fields:
      - {name: value_a, type: u16}
  - name: B
    fields:
      - {name: value_b, type: u32}
  - name: Record
    fields:
      - {name: tag, type: u8}
      - name: payload
        type: {choose: {ref: tag, cases: {'1': {structure: A}, '2': {structure: B}}}}
actions:
  - read: {name: records, type: {structure: Record}, repeat: until end}
"#;
        let mut bytes = Vec::new();
        for index in 0..2_000u32 {
            if index % 2 == 0 {
                bytes.push(1);
                bytes.extend_from_slice(&(index as u16).to_le_bytes());
            } else {
                bytes.push(2);
                bytes.extend_from_slice(&index.to_le_bytes());
            }
        }
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let node = parse(&store.view(), yaml);
        assert!(node.lazy.is_some());
        let data = read_for_viewport(store.view(), node, 0..2_000, 1_000).unwrap();
        assert_eq!(data.quality, PlotQuality::Exact);
        assert_eq!(data.series["payload.value_a"].len(), 1_000);
        assert_eq!(data.series["payload.value_b"].len(), 1_000);
        assert_eq!(data.series["payload.value_a"][0].bytes, 1..3);
        assert_eq!(data.series["payload.value_b"][0].bytes, 4..8);
    }

    fn fixture(count: usize, spike: usize) -> (ByteStore, Node) {
        let bytes: Vec<_> = (0..count)
            .flat_map(|index| {
                let value = if index == spike {
                    100_000i32
                } else {
                    index as i32 % 17 - 8
                };
                value.to_le_bytes()
            })
            .collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let node = parse(
            &store.view(),
            "actions:\n  - read: {name: samples, type: i32, repeat: until end}\n",
        );
        (store, node)
    }
    fn parse(source: &ByteView, yaml: &str) -> Node {
        Program::parse(yaml)
            .unwrap()
            .run(source, &AtomicBool::new(false))
            .unwrap()
            .nodes
            .remove(0)
    }
    fn request(source: ByteView, node: Node, range: Range<usize>, refine: bool) -> Request {
        Request {
            key: Key {
                revision: source.revision,
                definition: identity(&node),
                path: node.path.clone(),
                range,
                buckets: 128,
                exact_limit: exact_limit(512),
                refine,
                fields: None,
            },
            source,
            node,
            numeric_plan: None,
            group_tiles: None,
            partial_updates: true,
        }
    }
    fn settle(reader: &mut PlotReader, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while reader.is_loading() || reader.running.is_some() {
            reader.tick(ctx);
            assert!(Instant::now() < deadline, "plot worker did not settle");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    /// Run with `cargo test --release benchmark_numeric_plot_navigation -- --ignored --nocapture`.
    /// The allocated file and decoder layout are generic; no user data is needed.
    #[test]
    #[ignore = "manual performance measurement"]
    fn benchmark_numeric_plot_navigation() {
        let count = std::env::var("BINWAVE_BENCH_SAMPLES").ok()
            .and_then(|value| value.parse::<usize>().ok()).unwrap_or(1_000_000);
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut bytes = Vec::with_capacity(count * 4);
        for index in 0..count {
            bytes.extend_from_slice(&((index % 1024) as i32).to_le_bytes());
        }
        std::fs::write(file.path(), &bytes).unwrap();
        drop(bytes);
        let file_source = Arc::new(crate::file_source::FileSource::open(file.path()).unwrap());
        let store = ByteStore::from_file(file_source.clone());
        let node = parse(&store.view(),
            "actions:\n  - read: {name: samples, type: i32, repeat: until end}\n");
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        let mut query = request(store.view(), node, 0..count, true);
        query.key.buckets = display_buckets(1920);
        query.numeric_plan = Some(plan);
        let tiles = Arc::new(Mutex::new(GroupTileCache::default()));
        query.group_tiles = Some(tiles.clone());
        for (name, range) in [
            ("overview", 0..count),
            ("zoom", count / 2..(count / 2 + 8192).min(count)),
            ("adjacent", count / 2 + 4096..(count / 2 + 12_288).min(count)),
            ("return", count / 2..(count / 2 + 8192).min(count)),
            ("overview_return", 0..count),
        ] {
            query.key.range = range;
            let start = Instant::now();
            let before = file_source.io_stats();
            let data = read_plot(&query, &AtomicBool::new(false), |_| {}).unwrap();
            let after = file_source.io_stats();
            println!("plot_bench phase={name} elapsed_ms={:.3} scanned={} source_calls={} source_bytes={} tile_loads={}",
                start.elapsed().as_secs_f64() * 1000., data.scanned,
                after.0 - before.0, after.1 - before.1, tiles.lock().unwrap().loads);
        }
    }
    /// An allocated, format-neutral file exercises the UI's request/coverage
    /// path. The source remains OS-cache warm; this is not a cold-disk result.
    #[test]
    #[ignore = "manual native navigation measurement"]
    fn benchmark_plot_reader_navigation_trace() {
        let count = std::env::var("BINWAVE_BENCH_SAMPLES").ok()
            .and_then(|value| value.parse::<usize>().ok()).unwrap_or(1_000_000);
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut output = std::io::BufWriter::new(std::fs::File::create(file.path()).unwrap());
        use std::io::Write;
        for index in 0..count { output.write_all(&((index % 1024) as i32).to_le_bytes()).unwrap(); }
        output.flush().unwrap();
        drop(output);
        let source = Arc::new(crate::file_source::FileSource::open(file.path()).unwrap());
        let store = ByteStore::from_file(source.clone());
        let node = parse(&store.view(),
            "actions:\n  - read: {name: samples, type: i32, repeat: until end}\n");
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        let mut timings = Vec::new();
        let mut query_changes = 0;
        let mut measure = |label: &str, range: Range<Address>, width: Address,
            timings: &mut Vec<f64>| {
            let before = source.io_stats();
            let generation = reader.data_generation();
            let started = Instant::now();
            reader.request(&ctx, store.view(), node.clone(), range, width, true);
            settle(&mut reader, &ctx);
            let ms = started.elapsed().as_secs_f64() * 1000.;
            let after = source.io_stats();
            if reader.data_generation() != generation { query_changes += 1; }
            println!("plot_trace phase={label} latency_ms={ms:.3} source_calls={} source_bytes={} query_changed={} coverage={:?}",
                after.0 - before.0, after.1 - before.1,
                reader.data_generation() != generation, reader.data().unwrap().range);
            timings.push(ms);
        };
        measure("overview", 0..count, 1920, &mut timings);
        let overview_ms = timings.pop().unwrap();
        let center = count / 2;
        for index in 0..40 {
            let offset = match index % 8 { 0 => 0, 1 => 128, 2 => 256, 3 => 384,
                4 => 512, 5 => 384, 6 => 256, _ => 128 };
            let start = center.saturating_add(offset).min(count.saturating_sub(1024));
            measure("nearby", start..start + 1024, 1920, &mut timings);
        }
        timings.sort_by(f64::total_cmp);
        let p50 = timings[timings.len() / 2];
        let p95 = timings[(timings.len() * 95 / 100).min(timings.len() - 1)];
        println!("plot_trace summary overview_ms={overview_ms:.3} nearby_p50_ms={p50:.3} nearby_p95_ms={p95:.3} query_changes={query_changes} source_calls={} source_bytes={} tile_loads={}",
            source.io_stats().0, source.io_stats().1, reader.group_buffer_stats().1);
    }
    /// Compare independent overview readers under the same shared Rayon pool.
    /// This is a warm-OS-cache throughput probe, not a UI frame benchmark.
    #[test]
    #[ignore = "manual reader-concurrency measurement"]
    fn benchmark_parallel_plot_readers() {
        use std::io::Write;
        let count = std::env::var("BINWAVE_BENCH_SAMPLES").ok()
            .and_then(|value| value.parse::<usize>().ok()).unwrap_or(2_000_000);
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut output = std::io::BufWriter::new(std::fs::File::create(file.path()).unwrap());
        for index in 0..count {
            output.write_all(&((index % 1024) as i32).to_le_bytes()).unwrap();
        }
        output.flush().unwrap();
        drop(output);
        let source = Arc::new(crate::file_source::FileSource::open(file.path()).unwrap());
        let store = ByteStore::from_file(source.clone());
        let node = parse(&store.view(),
            "actions:\n  - read: {name: samples, type: i32, repeat: until end}\n");
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        for readers in [1, 4, 8, 16] {
            let started = Instant::now();
            let before = source.io_stats();
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..readers).map(|_| {
                    let mut query = request(store.view(), node.clone(), 0..count, true);
                    query.key.buckets = display_buckets(1920);
                    query.numeric_plan = Some(plan.clone());
                    query.group_tiles = Some(Arc::new(Mutex::new(GroupTileCache::default())));
                    scope.spawn(move || read_plot(&query, &AtomicBool::new(false), |_| {}))
                }).collect();
                for handle in handles {
                    assert!(handle.join().unwrap().unwrap().complete);
                }
            });
            let after = source.io_stats();
            println!("plot_concurrency readers={readers} samples_per_reader={count} elapsed_ms={:.3} source_calls={} source_bytes={}",
                started.elapsed().as_secs_f64() * 1000., after.0 - before.0, after.1 - before.1);
        }
    }
    #[test]
    fn tile_lookup_keeps_constant_time_eviction_order() {
        let mut cache = LruMap::default();
        for key in 0..10_000 { assert!(cache.insert(key, key)); }
        assert_eq!(cache.get(0), Some(0));
        assert_eq!(cache.pop_oldest(), Some(1));
        assert_eq!(cache.get(0), Some(0));
        assert!(!cache.insert(0, 999));
        assert_eq!(cache.get(0), Some(0));
        for expected in 2..10_000 { assert_eq!(cache.pop_oldest(), Some(expected)); }
        assert_eq!(cache.pop_oldest(), Some(0));
        assert_eq!(cache.pop_oldest(), None);
        assert_eq!(cache.len(), 0);
    }
    #[test]
    fn coarse_summaries_keep_spike_identity_and_stop_at_invalid_gaps() {
        let mut cache = GroupTileCache::default();
        cache.set_budget(16 * 1024 * 1024);
        for tile in 0..16 {
            let mut values = vec![tile as f64; 1024];
            if tile == 6 { values[5] = 100_000.0; }
            if tile == 10 { values.fill(f64::NAN); }
            cache.insert(Arc::new(GroupTile::from_values(tile * 1024, vec![values])), 1024);
        }
        let left = cache.covering_summary(0, &(0..16_384), 1).unwrap();
        assert_eq!(left.end, 8192);
        let peak = left.bins[0].1[0].unwrap().max;
        assert_eq!((peak.index, peak.value), (6 * 1024 + 5, 100_000.0));
        assert!(cache.covering_summary(10 * 1024, &(0..16_384), 1).is_none());
    }
    #[test]
    fn repeated_wide_query_uses_coarse_summaries_without_changing_points() {
        let (store, node) = fixture(65_536, 12_345);
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        let cache = Arc::new(Mutex::new(GroupTileCache::default()));
        let mut query = request(store.view(), node, 0..65_536, true);
        query.key.buckets = 8;
        query.numeric_plan = Some(plan);
        query.group_tiles = Some(cache.clone());
        let first = read_plot(&query, &AtomicBool::new(false), |_| {}).unwrap();
        let before = cache.lock().unwrap().coarse_hits;
        let second = read_plot(&query, &AtomicBool::new(false), |_| {}).unwrap();
        let after = cache.lock().unwrap().coarse_hits;
        assert!(after >= before + 8, "revisit should read eight coarse buckets");
        let points = |data: &PlotData| data.series["value"].iter()
            .map(|point| (point.index, point.value)).collect::<Vec<_>>();
        assert_eq!(points(&first), points(&second));
        assert!(points(&second).contains(&(12_345, 100_000.0)));
    }
    #[test]
    fn reader_budgets_trim_raw_tiles_and_summaries() {
        let mut caches = [GroupTileCache::default(), GroupTileCache::default()];
        for cache in &mut caches {
            cache.set_budget(64 * 1024);
            for tile in 0..8 {
                cache.insert(Arc::new(GroupTile::from_values(tile * 1024,
                    vec![vec![tile as f64; 1024]])), 1024);
            }
            assert!(cache.bytes + cache.summary_bytes <= 64 * 1024);
        }
        caches[0].set_budget(0);
        assert_eq!(caches[0].bytes + caches[0].summary_bytes, 0);
        assert!(caches[1].bytes + caches[1].summary_bytes <= 64 * 1024);
    }
    #[test]
    fn concurrent_group_tile_requests_share_one_decode() {
        let (store, node) = fixture(8192, usize::MAX);
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        let query = request(store.view(), node, 0..8192, true);
        let signature = group_signature(&query);
        let cache = Arc::new(Mutex::new(GroupTileCache::default()));
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8).map(|_| {
                let cache = cache.clone();
                let query = &query;
                let plan = &plan;
                let signature = &signature;
                let cancel = &cancel;
                scope.spawn(move || cached_group_tile(query, plan, &cache, signature, 0, cancel).unwrap())
            }).collect();
            for job in jobs { assert_eq!(job.join().unwrap().start, 0); }
        });
        assert_eq!(cache.lock().unwrap().loads, 1);
    }
    #[test]
    fn a_partial_read_is_filled_past_its_scan_front_with_the_last_complete_data() {
        let point = |index: Address, value: f64| PlotSample { index, value, bytes: index..index + 1, path: "v".into() };
        let data = |range: Range<Address>, points: Vec<PlotSample>, scanned: Address, complete: bool| PlotData {
            range, series: BTreeMap::from([("v".to_owned(), points)]), envelopes: BTreeMap::new(),
            bounds: BTreeMap::from([("v".to_owned(), (0., 9.))]), quality: PlotQuality::MinMax,
            scanned, total: 100, complete,
        };
        let back = data(0..100, (0..100).step_by(10).map(|i| point(i, 1.)).collect(), 100, true);
        // The new read covers 20..80 and has scanned 20..50 so far.
        let partial = data(20..80, (20..50).step_by(5).map(|i| point(i, 2.)).collect(), 30, false);
        let filled = fill_unscanned(partial, &back);
        let indices: Vec<_> = filled.series["v"].iter().map(|p| (p.index, p.value)).collect();
        assert_eq!(indices, [(20, 2.), (25, 2.), (30, 2.), (35, 2.), (40, 2.), (45, 2.), (50, 1.), (60, 1.), (70, 1.)],
            "new samples up to the scan front, the last complete data after it, nothing outside the view");
        assert!(!filled.complete);
        assert_eq!(filled.range, 20..80);
    }

    #[test]
    fn zooming_out_keeps_the_plot_filled_while_the_wider_view_loads() {
        let (store, node) = fixture(400_000, usize::MAX);
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request(&ctx, store.view(), node.clone(), 100_000..110_000, 800, true);
        settle(&mut reader, &ctx);
        reader.request(&ctx, store.view(), node, 0..400_000, 800, true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while reader.is_loading() {
            reader.tick(&ctx);
            if let Some(data) = reader.display_data().filter(|data| !data.complete && data.range.start == 0) {
                let front = data.range.start + data.scanned;
                if front < 100_000 {
                    assert!(data.series["value"].iter().any(|point| point.index >= 100_000 && point.index < 110_000),
                        "the previous view's data stays visible until the scan reaches it");
                }
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn complete_only_reads_keep_the_last_complete_data_until_the_next_is_done() {
        let (store, node) = fixture(400_000, usize::MAX);
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.set_complete_only(true);
        reader.request_group(&ctx, store.view(), node.clone(), plan.clone(), 100_000..200_000, 1280, true);
        settle(&mut reader, &ctx);
        let first = reader.data.as_ref().unwrap().clone();
        reader.request_group(&ctx, store.view(), node, plan, 150_000..350_000, 1280, true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while reader.is_loading() {
            reader.tick(&ctx);
            let shown = reader.display_data().unwrap();
            assert!(shown.complete, "no partial read replaces the complete one");
            if !Arc::ptr_eq(reader.data.as_ref().unwrap(), &first) {
                assert!(shown.range.start <= 150_000 && shown.range.end >= 350_000, "{:?}", shown.range);
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let range = reader.data().unwrap().range.clone();
        assert!(range.start <= 150_000 && range.end >= 350_000, "{range:?}");
        assert!(!Arc::ptr_eq(reader.data.as_ref().unwrap(), &first), "the new view replaced it");
    }

    #[test]
    fn zooming_in_reads_again_instead_of_stretching_a_capped_scan() {
        let (store, node) = fixture(100_000, usize::MAX);
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request_group(&ctx, store.view(), node.clone(), plan.clone(), 0..100_000, 1280, true);
        settle(&mut reader, &ctx);
        assert!(!reader.matches_group_request(store.view().revision, &node, &plan, 40_000..50_000, 1280, true),
            "a tenfold zoom needs finer buckets than the whole-range scan has");
        assert!(reader.matches_group_request(store.view().revision, &node, &plan, 0..95_000, 1280, true),
            "a small change still reuses it");
    }
    #[test]
    fn grouped_pan_reuses_prefetched_numeric_tiles() {
        let (store, node) = fixture(100_000, usize::MAX);
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request_group(&ctx, store.view(), node.clone(), plan.clone(),
            20_000..30_000, 1280, true);
        settle(&mut reader, &ctx);
        let loads = reader.group_buffer_stats().1;
        assert!(reader.matches_group_request(store.view().revision, &node, &plan,
            25_000..35_000, 1280, true));
        let first = reader.data.as_ref().unwrap().clone();
        reader.request_group(&ctx, store.view(), node, plan,
            25_000..35_000, 1280, true);
        settle(&mut reader, &ctx);
        assert!(reader.data().unwrap().range.start <= 25_000);
        assert!(reader.data().unwrap().range.end >= 35_000);
        assert!(Arc::ptr_eq(reader.data.as_ref().unwrap(), &first));
        assert_eq!(reader.group_buffer_stats().1, loads);
    }
    #[test]
    fn aligned_coverage_reuses_result_for_small_pan_without_source_reads() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let bytes: Vec<u8> = (0..20_000i32).flat_map(i32::to_le_bytes).collect();
        std::fs::write(file.path(), bytes).unwrap();
        let source = Arc::new(crate::file_source::FileSource::open(file.path()).unwrap());
        let store = ByteStore::from_file(source.clone());
        let node = parse(&store.view(),
            "actions:\n  - read: {name: samples, type: i32, repeat: until end}\n");
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request(&ctx, store.view(), node.clone(), 1000..2000, 1280, true);
        settle(&mut reader, &ctx);
        let first = reader.data.as_ref().unwrap().clone();
        assert!(first.range.start < 1000 && first.range.end > 2000);
        let before = source.io_stats();
        reader.request(&ctx, store.view(), node.clone(), 1100..2100, 1280, true);
        assert!(Arc::ptr_eq(reader.data.as_ref().unwrap(), &first));
        assert_eq!(source.io_stats(), before);
        reader.request(&ctx, store.view(), node, 2000..3000, 1280, true);
        assert!(reader.is_loading());
        assert!(Arc::ptr_eq(reader.data.as_ref().unwrap(), &first));
        settle(&mut reader, &ctx);
        assert!(reader.data().unwrap().range.start <= 2000);
        assert!(reader.data().unwrap().range.end >= 3000);
    }
    #[test]
    fn zooming_back_in_keeps_coarse_data_labelled_reduced_until_exact_samples_load() {
        let (store, node) = fixture(20_000, 5_001);
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request(&ctx, store.view(), node.clone(), 0..20_000, 800, true);
        settle(&mut reader, &ctx);
        assert_eq!(reader.data().unwrap().quality, PlotQuality::MinMax);
        reader.request(&ctx, store.view(), node, 4_900..5_100, 800, true);
        // The coarse data stays on screen instead of blanking while exact samples
        // load, and still reports itself as reduced so it isn't taken for samples.
        assert_eq!(reader.display_data().unwrap().quality, PlotQuality::MinMax,
            "coarse data stays visible, labelled as reduced, while exact samples load");
        settle(&mut reader, &ctx);
        assert_eq!(reader.display_data().unwrap().quality, PlotQuality::Exact);
        assert!(reader.data().unwrap().series["value"].iter().any(|point|
            point.index == 5_001 && point.value == 100_000.));
    }
    #[test]
    fn covered_pan_rejects_replaced_source() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), vec![7u8; 20_000]).unwrap();
        let source = Arc::new(crate::file_source::FileSource::open(file.path()).unwrap());
        let store = ByteStore::from_file(source);
        let node = parse(&store.view(),
            "actions:\n  - read: {name: samples, type: u8, repeat: until end}\n");
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request(&ctx, store.view(), node.clone(), 1000..2000, 1280, true);
        settle(&mut reader, &ctx);
        assert!(reader.display_data().is_some());

        // A changed length makes source replacement detectable even on a
        // filesystem with coarse modification timestamps.
        std::fs::write(file.path(), vec![9u8; 20_001]).unwrap();
        reader.request(&ctx, store.view(), node, 1100..2100, 1280, true);
        assert!(reader.display_data().is_none());
        assert!(reader.error().unwrap().contains("Source file changed"));
    }
    #[test]
    fn ordinary_numeric_collection_uses_the_tile_cache() {
        let (store, node) = fixture(100_000, usize::MAX);
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request_filtered(&ctx, store.view(), node.clone(),
            20_000..30_000, 1280, true, None);
        settle(&mut reader, &ctx);
        assert!(reader.group_buffer_stats().0 > 0);
        assert_eq!(reader.data().unwrap().quality, PlotQuality::MinMax);
        assert!(reader.data().unwrap().range.start <= 20_000);
        let loads = reader.group_buffer_stats().1;
        reader.request_filtered(&ctx, store.view(), node, 25_000..35_000, 1280, true, None);
        settle(&mut reader, &ctx);
        assert!(reader.group_buffer_stats().1 <= loads + 2,
            "nearby pan may extend prefetch but must reuse visible tiles");
    }
    #[test]
    fn numeric_points_share_path_parts_and_resolve_exact_indices() {
        let (store, node) = fixture(1024, usize::MAX);
        let data = read_plot(&request(store.view(), node, 0..1024, true),
            &AtomicBool::new(false), |_| {}).unwrap();
        let points = data.series.values().next().unwrap();
        let (PlotPath::Indexed { parts: first, index: first_index },
            PlotPath::Indexed { parts: second, index: second_index }) =
            (&points[0].path, &points[1].path) else { panic!("numeric paths must be indexed"); };
        assert!(Arc::ptr_eq(first, second));
        assert_ne!(first_index, second_index);
        assert_eq!(points[0].path.to_string(), format!("{}[{}]{}", first.0, first_index, first.1));
        assert_eq!(points[1].path.to_string(), format!("{}[{}]{}", second.0, second_index, second.1));
    }
    #[test]
    fn exact_plot_is_independent_of_128_row_pages_and_preserves_byte_provenance() {
        let (store, node) = fixture(4096, usize::MAX);
        let data = read_plot(
            &request(store.view(), node, 251..1275, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(data.quality, PlotQuality::Exact);
        assert!(data.envelopes.is_empty());
        assert_eq!(data.scanned, 1024);
        let points = &data.series["value"];
        assert_eq!(points.len(), 1024);
        assert_eq!(points[0].index, 251);
        assert_eq!(points[1023].index, 1274);
        assert_eq!(points[1023].bytes, 5096..5100);
        assert_eq!(points[1023].path.to_string(), "samples[1274]");
    }
    #[test]
    fn repeated_numeric_fields_use_segmented_fast_plan_across_groups() {
        let yaml = "structures:\n  - name: Item\n    fields:\n      - {name: marker, type: u8}\n      - {name: sensors, type: u16, repeat: 3}\n  - name: Group\n    fields:\n      - {name: header, type: bytes, length: 2}\n      - {name: items, type: {structure: Item}, repeat: 256}\nactions:\n  - read: {name: groups, type: {structure: Group}, repeat: 130}\n";
        let stride = 2 + 256 * 7;
        let mut bytes = vec![0; 130 * stride];
        let first = 2 + 255 * 7 + 5;
        let second = stride + 2 + 5;
        bytes[first..first + 2].copy_from_slice(&11_u16.to_le_bytes());
        bytes[second..second + 2].copy_from_slice(&22_u16.to_le_bytes());
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let output = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap();
        let groups = &output.nodes[0];
        let first_group = groups.lazy.as_ref().unwrap().page(&store.view(), &AtomicBool::new(false), 0, 1).unwrap();
        let flat = crate::structure::LazyArray::flattened_child(groups, &first_group[0].children[1]).unwrap();
        let plan = flat.lazy.as_ref().unwrap().numeric_plan().unwrap();
        assert_eq!(plan.fields.len(), 4);
        assert_eq!(plan.fields[3].name, "sensors.[2]");
        let data = read_for_viewport(store.view(), flat.clone(), 255..257, 800).unwrap();
        let shared = read_for_collections(store.view(), &[flat.clone()], 255..257, 800).unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].1.quality, data.quality);
        for (name, points) in &data.series {
            assert_eq!(shared[0].1.series[name].iter().map(|p| (p.index, p.value, p.bytes.clone())).collect::<Vec<_>>(),
                points.iter().map(|p| (p.index, p.value, p.bytes.clone())).collect::<Vec<_>>());
        }
        let points = &data.series["sensors.[2]"];
        assert_eq!(points.iter().map(|point| point.value).collect::<Vec<_>>(), [11., 22.]);
        assert_eq!(points[0].bytes, first..first + 2);
        assert_eq!(points[1].bytes, second..second + 2);
        let mut query = request(store.view(), flat, 0..130 * 256, true);
        query.numeric_plan = Some(plan);
        query.group_tiles = Some(Arc::new(Mutex::new(GroupTileCache::default())));
        let grouped = read_plot(&query, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(grouped.scanned, 130 * 256);
        let band = grouped.envelopes["sensors.[2]"].iter()
            .find(|band| band.range.contains(&256)).unwrap();
        assert_eq!(band.max.value, 22.);
        assert_eq!(band.max.bytes, second..second + 2);
    }
    #[test]
    fn exact_point_budget_scales_with_plot_width_and_remains_bounded() {
        for width in [0, 128, 512, 1024] {
            assert_eq!(exact_limit(width), MIN_EXACT_LIMIT);
        }
        assert_eq!(exact_limit(1025), 2176);
        assert_eq!(exact_limit(1088), 2176);
        assert_eq!(exact_limit(1089), 2304);
        assert_eq!(exact_limit(1200), 2432);
        assert_eq!(exact_limit(2048), 4096);
        for width in [4096, 8192, usize::MAX] {
            assert_eq!(exact_limit(width), MAX_EXACT_LIMIT);
        }
    }

    #[test]
    fn reduction_budget_tracks_physical_pixels_without_overflow() {
        assert_eq!(display_buckets(0), 128);
        assert_eq!(display_buckets(64), 128);
        assert_eq!(display_buckets(65), 192);
        assert_eq!(display_buckets(512), 1024);
        assert_eq!(display_buckets(1250), 2560);
        assert_eq!(display_buckets(1280), 2560);
        assert_eq!(display_buckets(1281), 2624);
        for width in [2048, 4096, usize::MAX] {
            assert_eq!(display_buckets(width), MAX_BUCKETS);
        }
        assert_eq!(
            reduction_buckets(PlotQuality::SampledPreview, display_buckets(4096)),
            PREVIEW_WINDOWS,
            "a larger display must not increase preview reads"
        );
    }

    #[test]
    fn display_resolution_preserves_short_cycles_and_exact_extrema_at_12496_samples() {
        let range = 28_358..40_854;
        let value = |name: &str, index: usize| match name {
            "counter" => 110. + (index % 16) as f64,
            "sine" => ((index % 64) as f64 * std::f64::consts::TAU / 64.).sin() * 4.,
            "spike" => {
                if index == 33_451 {
                    10_000.
                } else {
                    0.
                }
            }
            _ => unreachable!(),
        };
        let names = ["counter", "sine", "spike"];
        let bytes: Vec<_> = (0..range.end)
            .flat_map(|index| {
                names
                    .into_iter()
                    .flat_map(move |name| value(name, index).to_le_bytes())
            })
            .collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let node = parse(
            &store.view(),
            r#"
structures:
  - name: Sample
    fields: [{name: counter, type: f64}, {name: sine, type: f64}, {name: spike, type: f64}]
actions: [{read: {name: samples, type: {structure: Sample}, repeat: until end}}]
"#,
        );
        let mut request = request(store.view(), node, range.clone(), false);
        request.key.exact_limit = exact_limit(1280);
        let turns: Vec<_> = (range.start + 1..range.end - 1)
            .filter(|index| matches!(index % 16, 0 | 15))
            .collect();
        assert_eq!(turns.len(), 1562);
        request.key.buckets = 512;
        let coarse = read_plot(&request, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(turns.iter().any(|index| !coarse.series["counter"]
            .iter()
            .any(|point| point.index == *index)));
        request.key.buckets = display_buckets(1280);
        let data = read_plot(&request, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(data.quality, PlotQuality::MinMax);
        assert_eq!(data.scanned, range.len());
        assert!(data.complete);
        for index in turns {
            assert!(
                data.series["counter"]
                    .iter()
                    .any(|point| point.index == index),
                "lost the real turning point at sample {index}"
            );
        }
        for (field_index, name) in names.into_iter().enumerate() {
            let points = &data.series[name];
            assert!(points.len() <= 4 * request.key.buckets);
            assert!(points.windows(2).all(|pair| pair[0].index < pair[1].index));
            assert_eq!(data.envelopes[name].len(), request.key.buckets);
            for band in &data.envelopes[name] {
                let expected_min = band
                    .range
                    .clone()
                    .map(|index| value(name, index))
                    .fold(f64::INFINITY, f64::min);
                let expected_max = band
                    .range
                    .clone()
                    .map(|index| value(name, index))
                    .fold(f64::NEG_INFINITY, f64::max);
                assert_eq!(band.min.value, expected_min);
                assert_eq!(band.max.value, expected_max);
                for point in [&band.min, &band.max] {
                    assert!(band.range.contains(&point.index));
                    assert_eq!(
                        point.bytes,
                        point.index * 24 + field_index * 8..point.index * 24 + field_index * 8 + 8
                    );
                    assert_eq!(point.path.to_string(), format!("samples[{}].{name}", point.index));
                }
            }
        }
        let spike = data.series["spike"]
            .iter()
            .find(|point| point.index == 33_451)
            .unwrap();
        assert_eq!(spike.value, 10_000.);
        assert_eq!(spike.bytes, 33_451 * 24 + 16..33_451 * 24 + 24);
    }

    #[test]
    fn reduction_cache_distinguishes_physical_canvas_widths() {
        let (store, node) = fixture(50_000, 33_451);
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        for width in [640, 1280, 640] {
            reader.request(
                &ctx,
                store.view(),
                node.clone(),
                28_358..40_854,
                width,
                false,
            );
            settle(&mut reader, &ctx);
            assert!(reader.error().is_none());
            let data = reader.data().unwrap();
            assert_eq!(data.quality, PlotQuality::MinMax);
            assert_eq!(data.scanned, 12_496);
            assert!(data.envelopes["value"].len() >= display_buckets(width));
            assert!(data.series["value"]
                .iter()
                .any(|point| point.index == 33_451 && point.value == 100_000.));
        }
        assert_eq!(reader.cache.len(), 2);
    }

    #[test]
    fn grouped_scan_resolution_stays_stable_after_point_budget_is_reached() {
        let range = 0..100_000;
        let expected_buckets = MAX_TOTAL_NUMERIC_POINTS / 512;
        for width in [640, 1280, 1920, 4096] {
            // Exactness is limited by the same shared budget: the reader reduces
            // any wider grouped scan, so it must not be recorded as exact.
            assert_eq!(request_resolution(&range, width, Some(512)),
                (expected_buckets, expected_buckets));
        }
        assert_ne!(request_resolution(&range, 640, Some(1)),
            request_resolution(&range, 1280, Some(1)));
    }

    #[test]
    fn zooming_past_2048_samples_keeps_each_sample_when_the_plot_has_room() {
        let (store, node) = fixture(6000, 5500);
        for count in [2048, 2049, 2179, 2432] {
            let mut request = request(store.view(), node.clone(), 3342..3342 + count, false);
            request.key.exact_limit = exact_limit(1200);
            let data = read_plot(&request, &AtomicBool::new(false), |_| {}).unwrap();
            assert_eq!(data.quality, PlotQuality::Exact);
            assert_eq!(data.scanned, count);
            assert!(data.envelopes.is_empty());
            let samples = &data.series["value"];
            assert_eq!(samples.len(), count);
            for (offset, sample) in samples.iter().enumerate() {
                let index = 3342 + offset;
                assert_eq!(sample.index, index);
                assert_eq!(sample.bytes, index * 4..index * 4 + 4);
                assert_eq!(sample.path.to_string(), format!("samples[{index}]"));
            }
        }
    }

    #[test]
    fn resized_plot_requeries_exact_budget_and_reuses_matching_cached_data() {
        let (store, node) = fixture(9000, 2111);
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        for (width, quality) in [
            (512, PlotQuality::MinMax),
            (1200, PlotQuality::Exact),
            (512, PlotQuality::Exact),
        ] {
            reader.request(&ctx, store.view(), node.clone(), 0..2179, width, false);
            settle(&mut reader, &ctx);
            assert!(reader.error().is_none());
            let data = reader.data().unwrap();
            assert_eq!(data.quality, quality);
            assert_eq!(data.scanned, 2179);
            let points = &data.series["value"];
            if quality == PlotQuality::Exact {
                assert_eq!(points.len(), 2179);
            } else {
                assert!(points.len() <= 4 * MAX_BUCKETS);
            }
            let spike = points.iter().find(|point| point.index == 2111).unwrap();
            assert_eq!(spike.value, 100_000.);
            assert_eq!(spike.bytes, 8444..8448);
        }
        assert_eq!(reader.cache.len(), 2);
        reader.request(&ctx, store.view(), node, 0..8193, usize::MAX, false);
        settle(&mut reader, &ctx);
        let data = reader.data().unwrap();
        assert_eq!(data.quality, PlotQuality::MinMax);
        assert_eq!(data.scanned, 8193);
        assert!(data.series["value"].len() <= 4 * MAX_BUCKETS);
    }

    #[test]
    fn min_max_keeps_an_isolated_spike_and_temporal_order_with_bounded_points() {
        let (store, node) = fixture(65_536, 33_333);
        let data = read_plot(
            &request(store.view(), node, 0..65_536, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(data.quality, PlotQuality::MinMax);
        assert_eq!(data.scanned, 65_536);
        let points = &data.series["value"];
        assert!(points.len() <= 4 * 128);
        assert!(points.windows(2).all(|pair| pair[0].index < pair[1].index));
        let spike = points.iter().find(|point| point.index == 33_333).unwrap();
        assert_eq!(spike.value, 100_000.);
        assert_eq!(spike.bytes, 133_332..133_336);
        let envelope = data.envelopes["value"]
            .iter()
            .find(|envelope| envelope.range.contains(&33_333))
            .unwrap();
        assert_eq!(envelope.range, 33_280..33_792);
        assert_eq!(envelope.max.index, spike.index);
        assert_eq!(envelope.max.value, spike.value);
        assert_eq!(envelope.max.bytes, spike.bytes);
        assert_eq!(envelope.max.path.to_string(), "samples[33333]");
        assert_eq!(envelope.min.value, -8.);
        assert!(envelope.range.contains(&envelope.min.index));
    }
    #[test]
    fn preview_windows_bound_reads_and_cover_endpoints_without_fixed_phase_sampling() {
        for range in [123..624_999_935, usize::MAX - 1_000_000..usize::MAX] {
            let windows = preview_windows(range.clone(), 512);
            assert_eq!(windows.len(), PREVIEW_WINDOWS);
            assert_eq!(windows.iter().map(Range::len).sum::<usize>(), PREVIEW_LIMIT);
            assert_eq!(windows[0].start, range.start);
            assert_eq!(windows.last().unwrap().end, range.end);
            assert!(windows.windows(2).all(|pair| pair[0].end <= pair[1].start));
            for (index, window) in windows.iter().enumerate() {
                let bucket = bucket_range(&range, PREVIEW_WINDOWS, index);
                assert!(window.start >= bucket.start && window.end <= bucket.end);
                assert!(window.len() <= PREVIEW_WINDOW_SIZE);
            }
            let phases: std::collections::HashSet<_> = windows
                .iter()
                .skip(1)
                .take(PREVIEW_WINDOWS - 2)
                .map(|window| window.start % 384)
                .collect();
            assert!(
                phases.len() > 64,
                "preview window starts must vary across periodic phases"
            );
        }
        let windows = preview_windows(0..1_000_000, 64);
        assert_eq!(windows.len(), 64);
        assert_eq!(windows.iter().map(Range::len).sum::<usize>(), 32_768);
        let (store, node) = fixture(100_000, usize::MAX);
        let data = read_plot(
            &request(store.view(), node, 0..100_000, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(data.quality, PlotQuality::SampledPreview);
        assert_eq!(
            data.quality.label(),
            "Estimated range · peaks may be missed"
        );
        assert_eq!(data.scanned, PREVIEW_LIMIT);
        assert_eq!(data.total, 100_000);
        assert_eq!(data.series["value"].first().unwrap().index, 0);
        assert_eq!(data.series["value"].last().unwrap().index, 99_999);
        assert!(data.series["value"].len() <= 4 * PREVIEW_WINDOWS);
        let bands = &data.envelopes["value"];
        assert_eq!(bands.len(), PREVIEW_WINDOWS);
        assert_eq!(bands.first().unwrap().range.start, 0);
        assert_eq!(bands.last().unwrap().range.end, 100_000);
        assert!(bands
            .windows(2)
            .all(|pair| pair[0].range.end == pair[1].range.start));
        for band in bands {
            assert_eq!(band.min.value, -8.);
            assert_eq!(band.max.value, 8.);
            assert!(band.range.contains(&band.min.index));
            assert!(band.range.contains(&band.max.index));
        }
    }

    #[test]
    fn estimated_waveform_envelopes_match_generic_fallback_with_actual_byte_provenance() {
        let count = 100_000;
        let bytes: Vec<_> = (0..count)
            .flat_map(|index| {
                let mut record = ((index % 2) as u32).to_le_bytes().to_vec();
                record.extend_from_slice(
                    &((index % 64) as f64 * std::f64::consts::TAU / 64.)
                        .sin()
                        .to_le_bytes(),
                );
                record.extend_from_slice(&((index % 128) as u32).to_le_bytes());
                record
            })
            .collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let source = store.view();
        let generic = parse(
            &source,
            r#"
structures:
  - name: Row
    fields:
      - {name: tag, type: u32}
      - name: sine
        type: {choose: {ref: tag, cases: {"0": f64, "1": f64}}}
      - {name: ramp, type: u32}
actions:
  - read: {name: samples, type: {structure: Row}, length: 16, repeat: until end}
"#,
        );
        let numeric = parse(
            &source,
            r#"
structures:
  - name: Row
    fields:
      - {name: tag, type: u32}
      - {name: sine, type: f64}
      - {name: ramp, type: u32}
actions:
  - read: {name: samples, type: {structure: Row}, length: 16, repeat: until end}
"#,
        );
        assert!(generic.lazy.as_ref().unwrap().numeric_plan().is_none());
        assert!(numeric.lazy.as_ref().unwrap().numeric_plan().is_some());
        let numeric = read_plot(
            &request(source.clone(), numeric, 0..count, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let generic = read_plot(
            &request(source, generic, 0..count, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(numeric.scanned, PREVIEW_LIMIT);
        assert_eq!(generic.scanned, numeric.scanned);
        let windows = preview_windows(0..count, 128);
        for name in ["tag", "sine", "ramp"] {
            assert_eq!(numeric.envelopes[name].len(), PREVIEW_WINDOWS);
            assert_eq!(numeric.series[name].len(), generic.series[name].len());
            for (actual, expected) in numeric.series[name].iter().zip(&generic.series[name]) {
                assert_eq!(actual.index, expected.index);
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.bytes, expected.bytes);
                assert_eq!(actual.path, expected.path);
            }
            for (actual, expected) in numeric.envelopes[name].iter().zip(&generic.envelopes[name]) {
                assert_eq!(actual.range, expected.range);
                for (actual, expected) in
                    [(&actual.min, &expected.min), (&actual.max, &expected.max)]
                {
                    assert_eq!(actual.index, expected.index);
                    assert_eq!(actual.value, expected.value);
                    assert_eq!(actual.bytes, expected.bytes);
                    assert_eq!(actual.path, expected.path);
                    assert!(windows.iter().any(|window| window.contains(&actual.index)));
                }
            }
        }
        for band in &numeric.envelopes["sine"] {
            assert!((band.min.value + 1.).abs() < 1e-12);
            assert!((band.max.value - 1.).abs() < 1e-12);
            assert_eq!(
                band.min.bytes,
                band.min.index * 16 + 4..band.min.index * 16 + 12
            );
            assert_eq!(band.max.path.to_string(), format!("samples[{}].sine", band.max.index));
        }
        for band in &numeric.envelopes["ramp"] {
            assert_eq!(band.min.value, 0.);
            assert_eq!(band.max.value, 127.);
        }
    }

    #[test]
    fn numeric_preview_windows_split_wide_records_into_bounded_physical_reads() {
        let store = ByteStore::from_snapshot(Original::from_bytes(&vec![0; 4096 * 1024]).unwrap());
        let node = parse(
            &store.view(),
            r#"
structures:
  - name: Row
    fields:
      - {name: value, type: u32}
      - {name: padding, type: bytes, length: 4092}
actions:
  - read: {name: samples, type: {structure: Row}, repeat: until end}
"#,
        );
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        assert_eq!(plan.stride, 4096);
        let mut request = request(store.view(), node, 0..1024, false);
        request.key.buckets = 2;
        // Two 512-record windows are 2 MiB each. ByteView rejects reads above
        // 1 MiB, so this succeeds only if physical batching splits both windows.
        let data = read_numeric(
            &request,
            plan,
            PlotQuality::SampledPreview,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(data.scanned, 1024);
        assert_eq!(data.envelopes["value"].len(), 2);
        assert_eq!(data.series["value"].last().unwrap().index, 1023);
        assert_eq!(
            data.series["value"].last().unwrap().bytes,
            4_190_208..4_190_212
        );
    }
    #[test]
    fn grouped_overview_uses_bounded_preview_instead_of_scanning_every_record() {
        let (store, node) = fixture(100_000, 50_000);
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        let mut query = request(store.view(), node, 0..100_000, false);
        query.numeric_plan = Some(plan);
        query.group_tiles = Some(Arc::new(Mutex::new(GroupTileCache::default())));
        let data = read_plot(&query, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(data.quality, PlotQuality::SampledPreview);
        assert!(data.scanned <= PREVIEW_LIMIT);
        assert_eq!(data.total, 100_000);
    }
    #[test]
    fn explicit_refinement_scans_every_sample_and_finds_unsampled_spikes() {
        let count = 100_000;
        let windows = preview_windows(0..count, 128);
        let spike = (10_000..20_000)
            .find(|index| !windows.iter().any(|window| window.contains(index)))
            .unwrap();
        let (store, node) = fixture(count, spike);
        let preview = read_plot(
            &request(store.view(), node.clone(), 0..count, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert!(preview.envelopes["value"]
            .iter()
            .all(|band| band.max.value < 100_000.));
        let data = read_plot(
            &request(store.view(), node, 0..count, true),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(data.quality, PlotQuality::MinMax);
        assert_eq!(data.scanned, count);
        assert!(data.series["value"]
            .iter()
            .any(|point| point.index == spike && point.value == 100_000.));
        let band = data.envelopes["value"]
            .iter()
            .find(|band| band.range.contains(&spike))
            .unwrap();
        assert_eq!(band.max.index, spike);
        assert_eq!(band.max.value, 100_000.);
        assert_eq!(band.max.bytes, spike * 4..spike * 4 + 4);
    }
    #[test]
    fn edits_reject_stale_decode_plans_and_cancellation_returns_no_data() {
        let (mut store, node) = fixture(4096, usize::MAX);
        let old = request(store.view(), node.clone(), 0..2048, false);
        assert!(read_plot(&old, &AtomicBool::new(true), |_| {}).is_err());
        store.replace(0, &42i32.to_le_bytes()).unwrap();
        let error = read_plot(
            &request(store.view(), node, 0..2048, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("Bytes changed"));
    }
    #[test]
    fn rapid_navigation_only_publishes_latest_range_and_invalidation_clears_it() {
        let (store, node) = fixture(100_000, usize::MAX);
        let mut reader = PlotReader::default();
        let ctx = egui::Context::default();
        reader.request(&ctx, store.view(), node.clone(), 0..100_000, 512, true);
        for start in (1000..10_000).step_by(1000) {
            reader.request(
                &ctx,
                store.view(),
                node.clone(),
                start..start + 500,
                512,
                false,
            );
        }
        settle(&mut reader, &ctx);
        assert!(reader.error().is_none());
        assert_eq!(reader.data().unwrap().range, 9000..9500);
        assert_eq!(reader.data().unwrap().series["value"][0].index, 9000);
        reader.invalidate();
        assert!(reader.data().is_none());
        assert!(!reader.is_loading());
    }
    #[test]
    fn completed_and_revisited_plots_share_cached_sample_storage() {
        let (store, node) = fixture(65_536, 33_333);
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request(&ctx, store.view(), node.clone(), 0..65_536, 2048, false);
        settle(&mut reader, &ctx);
        let first = reader.data.as_ref().unwrap().clone();
        assert!(Arc::ptr_eq(&first, &reader.cache.front().unwrap().data));
        assert_eq!(first.envelopes["value"].len(), MAX_BUCKETS);
        // Zooming in twofold reads the narrower view at full resolution rather
        // than stretching the coarser whole-range scan over it.
        reader.request(&ctx, store.view(), node.clone(), 1000..33_768, 2048, false);
        settle(&mut reader, &ctx);
        assert_eq!(reader.cache.len(), 2);
        assert!(!Arc::ptr_eq(reader.data.as_ref().unwrap(), &first));
        assert_eq!(reader.data().unwrap().envelopes["value"].len(), MAX_BUCKETS);
        // Returning to the whole range reuses its cached scan.
        reader.request(&ctx, store.view(), node, 0..65_536, 2048, false);
        settle(&mut reader, &ctx);
        assert!(Arc::ptr_eq(reader.data.as_ref().unwrap(), &first));
        assert_eq!(reader.cache.len(), 2);
    }

    #[test]
    fn cached_ranges_do_not_survive_a_definition_change_or_byte_edit() {
        let (mut store, node) = fixture(4096, usize::MAX);
        let ctx = egui::Context::default();
        let mut reader = PlotReader::default();
        reader.request(&ctx, store.view(), node, 0..100, 512, false);
        settle(&mut reader, &ctx);
        assert_eq!(reader.data().unwrap().series["value"][0].value, -8.);
        let floats = parse(
            &store.view(),
            "actions:\n  - read: {name: samples, type: f32, repeat: until end}\n",
        );
        reader.request(&ctx, store.view(), floats, 0..100, 512, false);
        settle(&mut reader, &ctx);
        assert!(reader.data().unwrap().series["value"]
            .iter()
            .all(|point| point.value != -8.));
        store.replace(0, &42i32.to_le_bytes()).unwrap();
        let integers = parse(
            &store.view(),
            "actions:\n  - read: {name: samples, type: i32, repeat: until end}\n",
        );
        reader.request(&ctx, store.view(), integers, 0..100, 512, false);
        settle(&mut reader, &ctx);
        assert_eq!(reader.data().unwrap().series["value"][0].value, 42.);
        assert_eq!(reader.cache.len(), 1);
    }
    #[test]
    fn numeric_projection_matches_nested_endian_and_padding_in_real_parser() {
        let record = [
            0x01, 0x02, 0xaa, 0xbb, 0x00, 0x00, 0x80, 0x3f, 0x07, 0x08, 0xee, 0xee,
        ];
        let store = ByteStore::from_snapshot(Original::from_bytes(&record.repeat(300)).unwrap());
        let node = parse(
            &store.view(),
            r#"
endian: little
structures:
  - name: Inner
    endian: big
    fields:
      - {name: count, type: u16, length: 4}
      - {name: amplitude, type: f32, endian: little}
  - name: Row
    fields:
      - {name: sensor, type: {structure: Inner}}
      - {name: tail, type: u16, endian: big}
      - {name: pad, type: bytes, length: 2}
actions:
  - read: {name: rows, type: {structure: Row}, repeat: until end}
"#,
        );
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        assert_eq!(plan.fields.len(), 3);
        let data = read_plot(
            &request(store.view(), node.clone(), 200..220, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let rows = node
            .lazy
            .as_ref()
            .unwrap()
            .page(&store.view(), &AtomicBool::new(false), 200, 20)
            .unwrap();
        let mut expected = Reduction::new(200..220, PlotQuality::Exact, 128);
        for (offset, row) in rows.iter().enumerate() {
            expected.node(row, "", offset + 200);
        }
        let expected = expected.data(20, true);
        for (name, points) in &data.series {
            for (actual, expected) in points.iter().zip(&expected.series[name]) {
                assert_eq!(actual.index, expected.index);
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.bytes, expected.bytes);
                assert_eq!(actual.path, expected.path);
            }
        }
        assert_eq!(data.series["sensor.count"][0].value, 258.);
        assert_eq!(data.series["sensor.count"][0].bytes, 2400..2404);
    }
    #[test]
    fn dynamic_choices_and_validation_fall_back_to_the_full_parser() {
        let bytes: Vec<_> = (0..300u32)
            .flat_map(|index| {
                let mut record = (index % 2).to_le_bytes().to_vec();
                if index % 2 == 0 {
                    record.extend_from_slice(&(index as f32).to_le_bytes());
                } else {
                    record.extend_from_slice(&index.to_le_bytes());
                }
                record
            })
            .collect();
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let node = parse(
            &store.view(),
            r#"
structures:
  - name: Row
    fields:
      - {name: tag, type: u32}
      - name: value
        type: {choose: {ref: tag, cases: {"0": f32, "1": u32}}}
actions:
  - read: {name: rows, type: {structure: Row}, length: 8, repeat: until end}
"#,
        );
        assert!(node.lazy.as_ref().unwrap().numeric_plan().is_none());
        let data = read_plot(
            &request(store.view(), node, 200..220, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert!(data.series["value"]
            .iter()
            .all(|point| point.value == point.index as f64
                && point.bytes == (point.index * 8 + 4..point.index * 8 + 8)));

        let (store, _) = fixture(300, usize::MAX);
        let expected = parse(
            &store.view(),
            r#"
structures:
  - name: Row
    fields:
      - {name: value, type: i32, expect: -8}
actions:
  - read: {name: rows, type: {structure: Row}, length: 4, repeat: until end}
"#,
        );
        assert!(expected.lazy.as_ref().unwrap().numeric_plan().is_none());
        let error = read_plot(
            &request(store.view(), expected, 0..20, false),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("expected"));
    }
}
