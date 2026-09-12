//! Viewport-driven plotting of generic decoded collections. Workers retain only
//! bounded pages and extrema; opening a large range never starts a full scan.
use crate::byte_store::{ByteView, READ_LIMIT};
use crate::structure::{Node, NumericPlan};
use anyhow::{ensure, Result};
use std::collections::{BTreeMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const EXACT_LIMIT: usize = 2048;
const AUTO_SCAN_LIMIT: usize = 65_536;
const PREVIEW_LIMIT: usize = 65_536;
const PREVIEW_WINDOWS: usize = 128;
const PREVIEW_WINDOW_SIZE: usize = PREVIEW_LIMIT / PREVIEW_WINDOWS;
const MAX_BUCKETS: usize = 512;
const MAX_SERIES: usize = 32;
const CACHE_ENTRIES: usize = 3;

#[derive(Clone, Debug)]
pub struct PlotSample {
    pub index: usize,
    pub value: f64,
    pub bytes: Range<usize>,
    pub path: String,
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
            Self::MinMax => "Min/max overview",
            Self::SampledPreview => "Estimated range · peaks may be missed",
        }
    }
}
/// An amplitude band over one display bucket. Endpoints retain the actual
/// observed samples; SampledPreview does not claim to have seen every peak.
#[derive(Clone, Debug)]
pub struct PlotEnvelope {
    pub range: Range<usize>,
    pub min: PlotSample,
    pub max: PlotSample,
}
#[derive(Clone, Debug)]
pub struct PlotData {
    pub range: Range<usize>,
    pub series: BTreeMap<String, Vec<PlotSample>>,
    pub envelopes: BTreeMap<String, Vec<PlotEnvelope>>,
    pub quality: PlotQuality,
    pub scanned: usize,
    pub total: usize,
    pub complete: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    revision: u64,
    definition: u64,
    path: String,
    range: Range<usize>,
    buckets: usize,
    refine: bool,
}
#[derive(Clone)]
struct Request {
    key: Key,
    source: ByteView,
    node: Node,
}
struct Cached {
    request: Request,
    data: PlotData,
}
struct Outcome {
    key: Key,
    result: std::result::Result<PlotData, String>,
}
struct Running {
    cancel: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Outcome>>>,
    finished: Arc<AtomicBool>,
}
#[derive(Default)]
pub struct PlotReader {
    desired: Option<Request>,
    running: Option<Running>,
    data: Option<PlotData>,
    error: Option<String>,
    cache: VecDeque<Cached>,
}
impl Drop for PlotReader {
    fn drop(&mut self) {
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
    }
}
impl PlotReader {
    pub fn request(
        &mut self,
        ctx: &egui::Context,
        source: ByteView,
        node: Node,
        range: Range<usize>,
        pixel_width: usize,
        refine: bool,
    ) {
        let count = node
            .lazy
            .as_ref()
            .map_or(node.children.len(), |array| array.count);
        let start = range.start.min(count);
        let range = start..range.end.max(start).min(count);
        // Quantized widths avoid restarting a query for single-pixel pane changes.
        let buckets = pixel_width.clamp(64, MAX_BUCKETS).div_ceil(64) * 64;
        let key = Key {
            revision: source.revision,
            definition: identity(&node),
            path: node.path.clone(),
            range,
            buckets,
            refine,
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
        self.data = None;
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
                    self.cache.push_front(cached);
                }
                Err(error) => {
                    self.cache.clear();
                    self.error = Some(format!("{error:#}"));
                }
            }
        }
        self.desired = Some(Request { key, source, node });
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
                            if data.complete {
                                self.cache.push_front(Cached {
                                    request: self.desired.as_ref().unwrap().clone(),
                                    data: data.clone(),
                                });
                                self.cache.truncate(CACHE_ENTRIES);
                            }
                            self.data = Some(data);
                        }
                        Err(error) => {
                            self.data = None;
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
            && !self.data.as_ref().is_some_and(|data| data.complete)
        {
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
                    let publish = |data| {
                        *latest.lock().unwrap() = Some(Outcome {
                            key: request.key.clone(),
                            result: Ok(data),
                        });
                        ctx.request_repaint();
                    };
                    let result = read_plot(&request, &cancel, publish);
                    if !cancel.load(Ordering::Relaxed) {
                        *latest.lock().unwrap() = Some(Outcome {
                            key: request.key,
                            result: result.map_err(|error| format!("{error:#}")),
                        });
                    }
                    finished.store(true, Ordering::Release);
                    ctx.request_repaint();
                });
            }
        }
        if self.running.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
    pub fn invalidate(&mut self) {
        if let Some(running) = &self.running {
            running.cancel.store(true, Ordering::Relaxed);
        }
        self.desired = None;
        self.data = None;
        self.error = None;
        self.cache.clear();
    }
    pub fn data(&self) -> Option<&PlotData> {
        self.data.as_ref()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn is_loading(&self) -> bool {
        self.desired.is_some()
            && self.error.is_none()
            && !self.data.as_ref().is_some_and(|data| data.complete)
    }
}
fn identity(node: &Node) -> u64 {
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
    range: Range<usize>,
    quality: PlotQuality,
    buckets: usize,
    exact: BTreeMap<String, Vec<PlotSample>>,
    reduced: BTreeMap<String, Vec<Option<Bucket>>>,
}
impl Reduction {
    fn new(range: Range<usize>, quality: PlotQuality, buckets: usize) -> Self {
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
            if self.exact.len() < MAX_SERIES || self.exact.contains_key(&name) {
                self.exact.entry(name).or_default().push(point);
            }
        } else {
            if self.reduced.len() >= MAX_SERIES && !self.reduced.contains_key(&name) {
                return;
            }
            let bucket = ((point.index - self.range.start) as u128 * self.buckets as u128
                / self.range.len().max(1) as u128) as usize;
            let buckets = self
                .reduced
                .entry(name)
                .or_insert_with(|| vec![None; self.buckets]);
            match &mut buckets[bucket.min(self.buckets - 1)] {
                Some(bucket) => bucket.push(point),
                slot => *slot = Some(Bucket::new(point)),
            }
        }
    }
    fn node(&mut self, node: &Node, prefix: &str, index: usize) {
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
                    path: node.path.clone(),
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
    fn data(&self, scanned: usize, complete: bool) -> PlotData {
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
                                    range: bucket_range(&self.range, self.buckets, index),
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
        PlotData {
            range: self.range.clone(),
            series,
            envelopes,
            quality: self.quality,
            scanned,
            total: self.range.len(),
            complete,
        }
    }
}
fn node_count(node: &Node) -> usize {
    1 + node.children.iter().map(node_count).sum::<usize>()
}

// The push reducers use floor(index * buckets / length). Ceil boundaries
// describe precisely the sample indices assigned to that display bucket.
fn bucket_range(range: &Range<usize>, buckets: usize, index: usize) -> Range<usize> {
    let length = range.len() as u128;
    let start = (index as u128 * length).div_ceil(buckets as u128) as usize;
    let end = ((index + 1) as u128 * length).div_ceil(buckets as u128) as usize;
    range.start + start..range.start + end
}

fn reduction_buckets(quality: PlotQuality, requested: usize) -> usize {
    if quality == PlotQuality::SampledPreview {
        requested.clamp(2, PREVIEW_WINDOWS)
    } else {
        requested.clamp(1, MAX_BUCKETS)
    }
}

// Contiguous windows capture local waveform amplitude instead of isolated
// phases. Starts vary within evenly distributed strata; exact endpoints are
// included without claiming that any unread interval has been scanned.
fn preview_windows(range: Range<usize>, buckets: usize) -> Vec<Range<usize>> {
    if range.is_empty() {
        return Vec::new();
    }
    let count = reduction_buckets(PlotQuality::SampledPreview, buckets).min(range.len());
    (0..count)
        .map(|bucket| {
            let stratum = bucket_range(&range, count, bucket);
            let width = stratum.len().min(PREVIEW_WINDOW_SIZE);
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
                stratum.start + hash as usize % (stratum.len() - width + 1)
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
    let quality = if range.len() <= EXACT_LIMIT {
        PlotQuality::Exact
    } else if range.len() <= AUTO_SCAN_LIMIT || request.key.refine {
        PlotQuality::MinMax
    } else {
        PlotQuality::SampledPreview
    };
    if let Some(plan) = request
        .node
        .lazy
        .as_ref()
        .and_then(|array| array.numeric_plan())
        .filter(|plan| plan.stride > 0 && plan.stride <= READ_LIMIT)
    {
        return read_numeric(request, plan, quality, cancel, publish);
    }
    let mut reduction = Reduction::new(
        range.clone(),
        quality,
        reduction_buckets(quality, request.key.buckets),
    );
    let mut scanned = 0;
    let mut last_publish = Instant::now();
    let mut add = |node: &Node, index: usize, reduction: &mut Reduction| {
        reduction.node(node, "", index);
        scanned += 1;
        if last_publish.elapsed() >= Duration::from_millis(250) {
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
                    add(row, index + offset, &mut reduction);
                }
                index += rows.len();
            }
        }
    } else {
        for window in windows {
            for index in window {
                ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
                if let Some(node) = request.node.children.get(index) {
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
    index: usize,
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
    range: Range<usize>,
    quality: PlotQuality,
    buckets: usize,
    exact: Vec<Vec<NumericPoint>>,
    reduced: Vec<Vec<Option<NumericBucket>>>,
}
impl NumericReduction {
    fn new(plan: NumericPlan, range: Range<usize>, quality: PlotQuality, buckets: usize) -> Self {
        let count = plan.fields.len();
        Self {
            plan,
            range,
            quality,
            buckets,
            exact: vec![Vec::new(); count],
            reduced: if quality != PlotQuality::Exact {
                vec![vec![None; buckets]; count]
            } else {
                Vec::new()
            },
        }
    }
    fn record(&mut self, record: &[u8], index: usize) -> Result<()> {
        let bucket = ((index - self.range.start) as u128 * self.buckets as u128
            / self.range.len().max(1) as u128) as usize;
        for (field_index, field) in self.plan.fields.iter().enumerate() {
            let value = field.decode(record)?;
            if !value.is_finite() {
                continue;
            }
            let point = NumericPoint { index, value };
            if self.quality != PlotQuality::Exact {
                match &mut self.reduced[field_index][bucket.min(self.buckets - 1)] {
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
        Ok(())
    }
    fn data(&self, scanned: usize, complete: bool) -> PlotData {
        let mut series = BTreeMap::new();
        let mut envelopes = BTreeMap::new();
        for (field_index, field) in self.plan.fields.iter().enumerate() {
            let sample = |point: NumericPoint| {
                let start = self.plan.start + point.index * self.plan.stride + field.offset;
                PlotSample {
                    index: point.index,
                    value: point.value,
                    bytes: start..start + field.span,
                    path: format!("{}[{}]{}", self.plan.path, point.index, field.suffix),
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
                        range: bucket_range(&self.range, self.buckets, index),
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
        PlotData {
            range: self.range.clone(),
            series,
            envelopes,
            quality: self.quality,
            scanned,
            total: self.range.len(),
            complete,
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
        "Bytes changed; apply the definition again before plotting this collection."
    );
    let mut reduction = NumericReduction::new(
        plan,
        request.key.range.clone(),
        quality,
        reduction_buckets(quality, request.key.buckets),
    );
    let mut scanned = 0;
    let mut last_publish = Instant::now();
    let range = request.key.range.clone();
    let stride = reduction.plan.stride;
    // Window size and physical read size are independent: even very wide
    // records are read in chunks no larger than the ByteView's 1 MiB limit.
    let windows = if quality == PlotQuality::SampledPreview {
        preview_windows(range, request.key.buckets)
    } else {
        vec![range]
    };
    let batch = (READ_LIMIT / stride.max(1)).max(1);
    for window in windows {
        for index in window.clone().step_by(batch) {
            let span = index..index.saturating_add(batch).min(window.end);
            ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
            let start = reduction
                .plan
                .start
                .checked_add(
                    span.start
                        .checked_mul(stride)
                        .ok_or_else(|| anyhow::anyhow!("plot offset overflow"))?,
                )
                .ok_or_else(|| anyhow::anyhow!("plot offset overflow"))?;
            let length = span
                .len()
                .checked_mul(stride)
                .ok_or_else(|| anyhow::anyhow!("plot length overflow"))?;
            let bytes = request.source.read(start, length)?;
            ensure!(bytes.len() == length, "source ended during plot read");
            for (offset, record) in bytes.chunks_exact(stride).enumerate() {
                if offset % 128 == 0 {
                    ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
                }
                reduction.record(record, span.start + offset)?;
                scanned += 1;
            }
            if last_publish.elapsed() >= Duration::from_millis(250) {
                request.source.validate()?;
                publish(reduction.data(scanned, false));
                last_publish = Instant::now();
            }
        }
    }
    request.source.validate()?;
    ensure!(!cancel.load(Ordering::Relaxed), "plot request cancelled");
    Ok(reduction.data(scanned, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::Original;
    use crate::byte_store::ByteStore;
    use crate::structure::Program;

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
                refine,
            },
            source,
            node,
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
        assert_eq!(points[1023].path, "samples[1274]");
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
        assert_eq!(envelope.max.path, "samples[33333]");
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
  Row:
    fields:
      - {name: tag, type: u32}
      - name: sine
        type: {choose: {ref: tag, cases: {"0": f64, "1": f64}}}
      - {name: ramp, type: u32}
actions:
  - read: {name: samples, type: Row, length: 16, repeat: until end}
"#,
        );
        let numeric = parse(
            &source,
            r#"
structures:
  Row:
    fields:
      - {name: tag, type: u32}
      - {name: sine, type: f64}
      - {name: ramp, type: u32}
actions:
  - read: {name: samples, type: Row, length: 16, repeat: until end}
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
            assert_eq!(band.max.path, format!("samples[{}].sine", band.max.index));
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
  Row:
    fields:
      - {name: value, type: u32}
      - {name: padding, type: bytes, length: 4092}
actions:
  - read: {name: samples, type: Row, repeat: until end}
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
  Inner:
    endian: big
    fields:
      - {name: count, type: u16, length: 4}
      - {name: amplitude, type: f32, endian: little}
  Row:
    fields:
      - {name: sensor, type: Inner}
      - {name: tail, type: u16, endian: big}
      - {name: pad, type: bytes, length: 2}
actions:
  - read: {name: rows, type: Row, repeat: until end}
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
  Row:
    fields:
      - {name: tag, type: u32}
      - name: value
        type: {choose: {ref: tag, cases: {"0": f32, "1": u32}}}
actions:
  - read: {name: rows, type: Row, length: 8, repeat: until end}
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
  Row:
    fields:
      - {name: value, type: i32, expect: -8}
actions:
  - read: {name: rows, type: Row, length: 4, repeat: until end}
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
