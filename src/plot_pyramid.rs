//! A whole-collection min/max pyramid for numeric channels, like texture mipmaps.
//!
//! Level 0 holds each channel's min and max per `BIN` samples; each level above
//! merges pairs of the one below. A view is drawn from the coarsest level with at
//! least one bin per pixel column, so wide zooms and pans need no file reads.
//! A background builder fills level 0 in chunks, nearest the view first, and
//! every finished chunk can be drawn at once. A finished pyramid is cached on
//! disk for the file, decoder and layout.
use crate::byte_store::{ByteView, READ_LIMIT};
use crate::plot_reduce::{Matrix, MinMax, Sample};
use crate::structure::{NumericPlan, Primitive};
use anyhow::{ensure, Context, Result};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Samples per level-0 bin.
pub const BIN: usize = 256;
/// Bytes a build job aims to read at once.
const CHUNK_BYTES: usize = 4 << 20;

/// Neighbouring fields of one type and byte order, read as one channel matrix.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    primitive: Primitive,
    little: bool,
    /// Byte offset of the first channel within a row.
    first: usize,
    channels: usize,
}

fn width(primitive: Primitive) -> usize {
    match primitive {
        Primitive::U8 | Primitive::I8 => 1,
        Primitive::U16 | Primitive::I16 => 2,
        Primitive::U32 | Primitive::I32 | Primitive::F32 => 4,
        Primitive::U64 | Primitive::I64 | Primitive::F64 => 8,
    }
}

/// Split a plan's fields, in order, into runs of neighbouring same-type channels.
fn runs(plan: &NumericPlan) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for field in &plan.fields {
        let (primitive, little, offset) = (field.primitive(), field.little_endian(), field.offset);
        match runs.last_mut() {
            Some(run) if run.primitive == primitive && run.little == little
                && run.first + run.channels * width(primitive) == offset => run.channels += 1,
            _ => runs.push(Run { primitive, little, first: offset, channels: 1 }),
        }
    }
    runs
}

/// Per-level data for one run, in the run's own type: for each bin, the
/// channels' minimums then their maximums.
enum Store {
    U8(Vec<Vec<u8>>), I8(Vec<Vec<i8>>), U16(Vec<Vec<u16>>), I16(Vec<Vec<i16>>),
    U32(Vec<Vec<u32>>), I32(Vec<Vec<i32>>), U64(Vec<Vec<u64>>), I64(Vec<Vec<i64>>),
    F32(Vec<Vec<f32>>), F64(Vec<Vec<f64>>),
}
/// One chunk's level-0 bins for one run, laid out like a `Store` level.
enum Values {
    U8(Vec<u8>), I8(Vec<i8>), U16(Vec<u16>), I16(Vec<i16>), U32(Vec<u32>), I32(Vec<i32>),
    U64(Vec<u64>), I64(Vec<i64>), F32(Vec<f32>), F64(Vec<f64>),
}

macro_rules! each {
    ($value:expr, $enum:ident, $inner:ident => $body:expr) => {
        match $value {
            $enum::U8($inner) => $body, $enum::I8($inner) => $body, $enum::U16($inner) => $body,
            $enum::I16($inner) => $body, $enum::U32($inner) => $body, $enum::I32($inner) => $body,
            $enum::U64($inner) => $body, $enum::I64($inner) => $body, $enum::F32($inner) => $body,
            $enum::F64($inner) => $body,
        }
    };
}
macro_rules! by_primitive {
    ($primitive:expr, $ty:ident => $body:expr) => {
        match $primitive {
            Primitive::U8 => { type $ty = u8; $body } Primitive::I8 => { type $ty = i8; $body }
            Primitive::U16 => { type $ty = u16; $body } Primitive::I16 => { type $ty = i16; $body }
            Primitive::U32 => { type $ty = u32; $body } Primitive::I32 => { type $ty = i32; $body }
            Primitive::U64 => { type $ty = u64; $body } Primitive::I64 => { type $ty = i64; $body }
            Primitive::F32 => { type $ty = f32; $body } Primitive::F64 => { type $ty = f64; $body }
        }
    };
}

/// Typed access used by `Store` and `Values`.
trait Typed: Sample {
    fn store(levels: Vec<Vec<Self>>) -> Store;
    fn values(values: Vec<Self>) -> Values;
}
macro_rules! typed {
    ($ty:ty, $variant:ident) => {
        impl Typed for $ty {
            fn store(levels: Vec<Vec<Self>>) -> Store { Store::$variant(levels) }
            fn values(values: Vec<Self>) -> Values { Values::$variant(values) }
        }
    };
}
typed!(u8, U8); typed!(i8, I8); typed!(u16, U16); typed!(i16, I16); typed!(u32, U32);
typed!(i32, I32); typed!(u64, U64); typed!(i64, I64); typed!(f32, F32); typed!(f64, F64);

fn empty_store<T: Typed>(level_bins: &[usize], channels: usize) -> Store {
    T::store(level_bins.iter().map(|&bins| {
        let mut level = Vec::with_capacity(bins * channels * 2);
        for _ in 0..bins {
            level.extend(std::iter::repeat_n(T::HIGHEST, channels));
            level.extend(std::iter::repeat_n(T::LOWEST, channels));
        }
        level
    }).collect())
}

/// Fold `children` bins of `below` into bin `bin` of `above`.
fn merge_bin<T: Sample>(above: &mut [T], below: &[T], bin: usize, children: Range<usize>, channels: usize) {
    let slot = &mut above[bin * channels * 2..(bin + 1) * channels * 2];
    let (min, max) = slot.split_at_mut(channels);
    min.fill(T::HIGHEST);
    max.fill(T::LOWEST);
    for child in children {
        let source = &below[child * channels * 2..(child + 1) * channels * 2];
        for (lo, &value) in min.iter_mut().zip(&source[..channels]) {
            if value < *lo { *lo = value; }
        }
        for (hi, &value) in max.iter_mut().zip(&source[channels..]) {
            if value > *hi { *hi = value; }
        }
    }
}

/// Reduce one chunk's rows into level-0 bins for one run.
fn reduce_chunk<T: Typed>(reads: &[(usize, Vec<u8>)], run: &Run, row_stride: usize,
    samples: Range<usize>) -> Values {
    let bins = samples.len().div_ceil(BIN);
    let mut out = Vec::with_capacity(bins * run.channels * 2);
    let matrix = Matrix { first: run.first, channels: run.channels, row_stride, little_endian: run.little };
    for bin in 0..bins {
        let bin_samples = samples.start + bin * BIN..(samples.start + (bin + 1) * BIN).min(samples.end);
        let mut result = MinMax::<T>::new(run.channels);
        for (first, bytes) in reads {
            let rows = bytes.len().saturating_sub(run.first + run.channels * width_of::<T>()) / row_stride + 1;
            let (lo, hi) = (bin_samples.start.max(*first), bin_samples.end.min(first + rows));
            if lo < hi {
                result.reduce(bytes, &matrix, lo - first..hi - first);
            }
        }
        out.extend(result.min);
        out.extend(result.max);
    }
    T::values(out)
}

fn width_of<T: Sample>() -> usize { T::WIDTH }

/// The pyramid for one numeric collection.
pub struct Pyramid {
    count: usize,
    /// Bins per level; level k bins cover `BIN << k` samples.
    level_bins: Vec<usize>,
    /// Which bins hold complete data, per level.
    present: Vec<Vec<bool>>,
    /// Level-0 bins present, so progress is cheap to ask for every frame.
    done: usize,
    runs: Vec<(Run, Store)>,
    channels: usize,
}

/// A view's per-pixel min/max for every channel, `None` where not built yet.
#[derive(Clone, Debug, PartialEq)]
pub struct Columns {
    pub range: Range<usize>,
    /// Samples each level bin covers at the level used.
    pub bin_samples: usize,
    /// Channel-major: `values[channel][column]`.
    pub values: Vec<Vec<Option<[f64; 2]>>>,
}

impl Pyramid {
    fn new(plan: &NumericPlan, count: usize) -> Self {
        let mut level_bins = vec![count.div_ceil(BIN).max(1)];
        while *level_bins.last().unwrap() > 1 {
            level_bins.push(level_bins.last().unwrap().div_ceil(2));
        }
        let runs: Vec<_> = runs(plan).into_iter().map(|run| {
            let store = by_primitive!(run.primitive, T => empty_store::<T>(&level_bins, run.channels));
            (run, store)
        }).collect();
        let channels = runs.iter().map(|(run, _)| run.channels).sum();
        let present = level_bins.iter().map(|&bins| vec![false; bins]).collect();
        Self { count, level_bins, present, done: 0, runs, channels }
    }

    /// Share of level 0 built, 0 to 1.
    pub fn built(&self) -> f32 {
        self.done as f32 / self.present[0].len().max(1) as f32
    }

    /// Store a chunk's level-0 bins and fill every level above them that is now complete.
    fn store_chunk(&mut self, first_bin: usize, values: Vec<Values>) {
        let mut bins = 0;
        for ((run, store), values) in self.runs.iter_mut().zip(values) {
            let channels = run.channels;
            macro_rules! put {
                ($variant:ident) => {
                    if let (Store::$variant(levels), Values::$variant(values)) = (&mut *store, values) {
                        let start = first_bin * channels * 2;
                        levels[0][start..start + values.len()].copy_from_slice(&values);
                        bins = values.len() / (channels * 2);
                    }
                };
            }
            match run.primitive {
                Primitive::U8 => put!(U8), Primitive::I8 => put!(I8), Primitive::U16 => put!(U16),
                Primitive::I16 => put!(I16), Primitive::U32 => put!(U32), Primitive::I32 => put!(I32),
                Primitive::U64 => put!(U64), Primitive::I64 => put!(I64), Primitive::F32 => put!(F32),
                Primitive::F64 => put!(F64),
            }
        }
        for bin in first_bin..first_bin + bins {
            self.done += usize::from(!self.present[0][bin]);
            self.present[0][bin] = true;
        }
        let mut changed = first_bin..first_bin + bins;
        for level in 1..self.level_bins.len() {
            let parents = changed.start / 2..changed.end.div_ceil(2);
            let below = self.level_bins[level - 1];
            for parent in parents.clone() {
                let children = parent * 2..(parent * 2 + 2).min(below);
                if self.present[level][parent] || !children.clone().all(|child| self.present[level - 1][child]) {
                    continue;
                }
                for (run, store) in &mut self.runs {
                    let channels = run.channels;
                    each!(store, Store, levels => {
                        let (lower, upper) = levels.split_at_mut(level);
                        merge_bin(&mut upper[0], &lower[level - 1], parent, children.clone(), channels);
                    });
                }
                self.present[level][parent] = true;
            }
            changed = parents;
        }
    }

    /// Per-pixel min/max for `view` across `columns` columns.
    pub fn columns(&self, view: Range<usize>, columns: usize) -> Columns {
        self.columns_of(view, columns, &(0..self.channels).collect::<Vec<_>>())
    }

    /// Like `columns`, for only the listed channels: `values[i]` is `channels[i]`.
    // `column` picks each column's sample range and its slot in every channel.
    #[allow(clippy::needless_range_loop)]
    pub fn columns_of(&self, view: Range<usize>, columns: usize, channels: &[usize]) -> Columns {
        let view = view.start.min(self.count)..view.end.min(self.count);
        let columns = columns.max(1);
        let per_column = view.len() as f64 / columns as f64;
        let level = (0..self.level_bins.len()).rev()
            .find(|&level| (BIN << level) as f64 <= per_column).unwrap_or(0);
        let bin_samples = BIN << level;
        let mut slots = vec![None; self.channels];
        for (slot, &channel) in channels.iter().enumerate() {
            if let Some(entry) = slots.get_mut(channel) { *entry = Some(slot); }
        }
        let mut values = vec![vec![None; columns]; channels.len()];
        for column in 0..columns {
            let first = view.start + (column as f64 * per_column) as usize;
            let last = (view.start + ((column + 1) as f64 * per_column) as usize).min(view.end).max(first + 1);
            let bins = first / bin_samples..last.div_ceil(bin_samples).min(self.level_bins[level]);
            if bins.is_empty() || !bins.clone().all(|bin| self.present[level][bin]) {
                continue;
            }
            let mut channel = 0;
            for (run, store) in &self.runs {
                let channels = run.channels;
                each!(store, Store, levels => {
                    let data = &levels[level];
                    for offset in 0..channels {
                        let Some(slot) = slots[channel + offset] else { continue };
                        let (mut low, mut high) = (f64::INFINITY, f64::NEG_INFINITY);
                        for bin in bins.clone() {
                            let base = bin * channels * 2;
                            let (lo, hi) = (data[base + offset], data[base + channels + offset]);
                            if lo <= hi {
                                low = low.min(lo.to_f64());
                                high = high.max(hi.to_f64());
                            }
                        }
                        if low <= high {
                            values[slot][column] = Some([low, high]);
                        }
                    }
                });
                channel += channels;
            }
        }
        Columns { range: view, bin_samples, values }
    }
}

/// Read a chunk's rows: contiguous runs of samples, at most `READ_LIMIT` bytes each.
fn read_rows(source: &ByteView, plan: &NumericPlan, samples: Range<usize>) -> Result<Vec<(usize, Vec<u8>)>> {
    let row_stride = plan.physical_stride().max(1);
    let per_read = (READ_LIMIT.saturating_sub(plan.stride) / row_stride + 1).max(1);
    let mut reads = Vec::new();
    let mut index = samples.start;
    while index < samples.end {
        let rows = plan.contiguous_count(index).min(samples.end - index).min(per_read);
        let offset = plan.offset(index).context("plot offset overflow")?;
        let length = (rows - 1) * row_stride + plan.stride;
        let bytes = source.read(offset, length)?;
        ensure!(bytes.len() == length, "source ended while building the plot summary");
        reads.push((index, bytes));
        index += rows;
    }
    Ok(reads)
}

/// The sample in `samples` whose `channel` value is nearest `target`, with that
/// value: what a click on a column picks.
pub fn nearest_sample(source: &ByteView, plan: &NumericPlan, channel: usize, samples: Range<usize>,
    target: f64) -> Result<Option<(usize, f64)>> {
    let field = plan.fields.get(channel).context("no such channel")?;
    let row_stride = plan.physical_stride().max(1);
    let mut best: Option<(usize, f64)> = None;
    for (first, bytes) in read_rows(source, plan, samples)? {
        let rows = (bytes.len() - plan.stride) / row_stride + 1;
        for row in 0..rows {
            let start = row * row_stride;
            let value = field.decode(&bytes[start..start + plan.stride])?;
            if !value.is_nan() && best.is_none_or(|(_, seen)| (value - target).abs() < (seen - target).abs()) {
                best = Some((first + row, value));
            }
        }
    }
    Ok(best)
}

struct Shared {
    pyramid: RwLock<Pyramid>,
    focus: Mutex<Range<usize>>,
    cancel: AtomicBool,
    error: Mutex<Option<String>>,
}

/// Builds a pyramid in the background, nearest the focused view first. Dropping
/// it stops the build.
pub struct PyramidBuilder {
    shared: Arc<Shared>,
}

impl Drop for PyramidBuilder {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

impl PyramidBuilder {
    pub fn start(source: ByteView, plan: NumericPlan, count: usize, focus: Range<usize>,
        repaint: impl Fn() + Send + 'static) -> Self {
        let shared = Arc::new(Shared {
            pyramid: RwLock::new(Pyramid::new(&plan, count)),
            focus: Mutex::new(focus),
            cancel: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        let worker = shared.clone();
        // A saved summary can be hundreds of megabytes; load it off the UI thread too.
        std::thread::spawn(move || {
            if let Some(cached) = cache::load(&source, &plan, count) {
                *worker.pyramid.write().unwrap() = cached;
            } else if let Err(error) = build(&worker, &source, &plan, count, &repaint) {
                if !worker.cancel.load(Ordering::Relaxed) {
                    log::warn!("Plot summary build failed: {error:#}");
                    *worker.error.lock().unwrap() = Some(format!("{error:#}"));
                }
            }
            repaint();
        });
        Self { shared }
    }

    /// Build nearest this view next.
    pub fn focus(&self, view: Range<usize>) {
        *self.shared.focus.lock().unwrap() = view;
    }

    pub fn columns(&self, view: Range<usize>, columns: usize) -> Columns {
        self.shared.pyramid.read().unwrap().columns(view, columns)
    }

    pub fn columns_of(&self, view: Range<usize>, columns: usize, channels: &[usize]) -> Columns {
        self.shared.pyramid.read().unwrap().columns_of(view, columns, channels)
    }

    pub fn built(&self) -> f32 {
        self.shared.pyramid.read().unwrap().built()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }
}

/// Unbuilt chunks, those overlapping the focused view first, then by distance
/// from it, so the view and then its neighbours fill in.
fn build_order(built: &[bool], chunk_samples: usize, count: usize, focus: &Range<usize>) -> Vec<usize> {
    let centre = (focus.start + focus.end) / 2;
    let mut order: Vec<usize> = (0..built.len()).filter(|&chunk| !built[chunk]).collect();
    order.sort_by_key(|&chunk| {
        let span = chunk * chunk_samples..((chunk + 1) * chunk_samples).min(count);
        if span.start < focus.end && focus.start < span.end { 0 }
        else { span.start.abs_diff(centre).min(span.end.abs_diff(centre)) }
    });
    order
}

fn build(shared: &Shared, source: &ByteView, plan: &NumericPlan, count: usize, repaint: &dyn Fn()) -> Result<()> {
    use rayon::prelude::*;
    let row_bytes = plan.stride.max(1);
    let chunk_bins = (CHUNK_BYTES / (BIN * row_bytes)).clamp(1, 64);
    let chunks = count.div_ceil(BIN * chunk_bins);
    let mut built = vec![false; chunks];
    let workers = std::thread::available_parallelism().map_or(1, |cores| cores.get().saturating_sub(2).max(1));
    let runs: Vec<Run> = shared.pyramid.read().unwrap().runs.iter().map(|(run, _)| run.clone()).collect();
    let started = std::time::Instant::now();
    loop {
        ensure!(!shared.cancel.load(Ordering::Relaxed), "plot summary build cancelled");
        let focus = shared.focus.lock().unwrap().clone();
        let chunk_samples = BIN * chunk_bins;
        let mut batch = build_order(&built, chunk_samples, count, &focus);
        if batch.is_empty() { break; }
        batch.truncate(workers * 2);
        let results: Vec<(usize, Result<Vec<Values>>)> = batch.par_iter().map(|&chunk| {
            let samples = chunk * chunk_samples..((chunk + 1) * chunk_samples).min(count);
            let result = read_rows(source, plan, samples.clone()).map(|reads| runs.iter().map(|run|
                by_primitive!(run.primitive, T => reduce_chunk::<T>(&reads, run, plan.physical_stride().max(1), samples.clone()))
            ).collect());
            (chunk, result)
        }).collect();
        let mut pyramid = shared.pyramid.write().unwrap();
        for (chunk, result) in results {
            pyramid.store_chunk(chunk * chunk_bins, result?);
            built[chunk] = true;
        }
        drop(pyramid);
        repaint();
    }
    log::info!("Plot summary built: {} samples, {} channels in {:.1}s", count,
        shared.pyramid.read().unwrap().channels, started.elapsed().as_secs_f32());
    cache::store(source, plan, count, &shared.pyramid.read().unwrap());
    Ok(())
}

/// A finished pyramid's level 0 on disk, keyed by file, decoder layout and count.
mod cache {
    use super::*;
    use std::io::{Read, Write};

    pub(super) fn path(source: &ByteView, plan: &NumericPlan, count: usize) -> Option<std::path::PathBuf> {
        use std::hash::{Hash, Hasher};
        let key = source.plot_cache_key(&("pyramid-v1", plan.cache_layout_key(), count, BIN))?;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hash);
        Some(crate::cache::root().join("pyramid-v1").join(format!("{:016x}.bin", hash.finish())))
    }

    pub(super) fn store(source: &ByteView, plan: &NumericPlan, count: usize, pyramid: &Pyramid) {
        let Some(path) = path(source, plan, count) else { return };
        let mut bytes = Vec::new();
        for (_, store) in &pyramid.runs {
            each!(store, Store, levels => {
                for value in &levels[0] {
                    bytes.extend_from_slice(&value.to_le_bytes_vec());
                }
            });
        }
        let _ = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
            file.write_all(&bytes)?;
            file.persist(&path).map_err(|error| error.error)?;
            Ok(())
        })();
    }

    pub(super) fn load(source: &ByteView, plan: &NumericPlan, count: usize) -> Option<Pyramid> {
        let path = path(source, plan, count)?;
        let mut bytes = Vec::new();
        std::fs::File::open(path).ok()?.read_to_end(&mut bytes).ok()?;
        let mut pyramid = Pyramid::new(plan, count);
        let mut at = 0;
        for (run, store) in &mut pyramid.runs {
            let size = width(run.primitive);
            each!(store, Store, levels => {
                for value in levels[0].iter_mut() {
                    let raw = bytes.get(at..at + size)?;
                    *value = Sample::read_le(raw);
                    at += size;
                }
            });
        }
        if at != bytes.len() { return None; }
        // Mark level 0 complete and rebuild the levels above it.
        let level0: Vec<Values> = pyramid.runs.iter()
            .map(|(_, store)| each!(store, Store, levels => to_values(levels[0].clone()))).collect();
        pyramid.store_chunk(0, level0);
        Some(pyramid)
    }

    fn to_values<T: Typed>(values: Vec<T>) -> Values { T::values(values) }

    /// Little-endian bytes of any sample type, for the cache file.
    pub(super) trait LeBytes { fn to_le_bytes_vec(&self) -> Vec<u8>; }
    macro_rules! le { ($($ty:ty),*) => { $(impl LeBytes for $ty { fn to_le_bytes_vec(&self) -> Vec<u8> { self.to_le_bytes().to_vec() } })* }; }
    le!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::byte_store::ByteStore;
    use crate::structure::Program;

    /// Records of 12 u16 channels, then an i8 and an f32: three runs.
    fn fixture(records: usize) -> (ByteStore, NumericPlan, Vec<Vec<f64>>) {
        let mut bytes = Vec::new();
        let mut values = vec![Vec::new(); 14];
        for record in 0..records {
            for channel in 0..12 {
                let value = ((record * 7 + channel * 131) % 5000) as u16 + if record == 70_001 && channel == 3 { 20_000 } else { 0 };
                bytes.extend(value.to_le_bytes());
                values[channel].push(value as f64);
            }
            let small = ((record % 200) as i32 - 100) as i8;
            bytes.push(small as u8);
            values[12].push(small as f64);
            let float = if record % 97 == 0 { f32::NAN } else { (record as f32 * 0.01).sin() };
            bytes.extend(float.to_le_bytes());
            values[13].push(float as f64);
        }
        let fields: String = (0..12).map(|c| format!("{{name: c{c}, type: u16}}, ")).collect();
        let yaml = format!("structures:\n  - name: R\n    fields: [{fields}{{name: small, type: i8}}, {{name: level, type: f32}}]\n\
            actions:\n  - read: {{name: records, type: {{structure: R}}, repeat: until end}}\n");
        let store = ByteStore::from_snapshot(crate::binary::Original::from_bytes(&bytes).unwrap());
        let node = Program::parse(&yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap().nodes.remove(0);
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        (store, plan, values)
    }

    fn brute(values: &[f64], range: Range<usize>) -> Option<[f64; 2]> {
        let seen: Vec<f64> = values[range].iter().copied().filter(|v| !v.is_nan()).collect();
        (!seen.is_empty()).then(|| [seen.iter().copied().fold(f64::INFINITY, f64::min), seen.iter().copied().fold(f64::NEG_INFINITY, f64::max)])
    }

    #[test]
    fn fields_split_into_runs_of_neighbouring_same_type_channels() {
        let (_, plan, _) = fixture(200);
        let runs = runs(&plan);
        assert_eq!(runs.iter().map(|run| (run.primitive, run.channels)).collect::<Vec<_>>(),
            [(Primitive::U16, 12), (Primitive::I8, 1), (Primitive::F32, 1)]);
    }

    #[test]
    fn built_pyramid_columns_match_a_brute_force_scan_at_every_zoom() {
        let count = 100_000;
        let (store, plan, values) = fixture(count);
        let builder = PyramidBuilder::start(store.view(), plan, count, 40_000..60_000, || {});
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while builder.built() < 1. {
            assert!(builder.error().is_none(), "{:?}", builder.error());
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for (view, columns) in [(0..count, 200), (1000..91_000, 97), (70_000..71_000, 3), (0..count, 1)] {
            let result = builder.columns(view.clone(), columns);
            for channel in 0..14 {
                for column in 0..columns {
                    // Columns cover whole bins, so compare with the bins they span.
                    let bin = result.bin_samples;
                    let per = view.len() as f64 / columns as f64;
                    let first = view.start + (column as f64 * per) as usize;
                    let last = (view.start + ((column + 1) as f64 * per) as usize).min(view.end).max(first + 1);
                    let span = first / bin * bin..(last.div_ceil(bin) * bin).min(count);
                    assert_eq!(result.values[channel][column], brute(&values[channel], span.clone()),
                        "view {view:?} column {column} channel {channel} span {span:?}");
                }
            }
        }
        // The spike at 70,001 shows at every zoom.
        let whole = builder.columns(0..count, 50);
        assert!(whole.values[3].iter().flatten().any(|[_, high]| *high >= 20_000.));
    }

    #[test]
    fn the_focused_view_is_built_first_and_usable_before_the_rest() {
        // 10 chunks of 1,000 samples; the view covers chunks 6 and 7.
        let order = build_order(&[false; 10], 1000, 10_000, &(6_500..7_500));
        assert_eq!(&order[..2], [6, 7], "the view first");
        assert_eq!(&order[2..4], [5, 8], "then its nearest neighbours");
        assert!(!build_order(&[true, false, true], 1000, 3000, &(0..1000)).contains(&0), "built chunks are skipped");
        // A pyramid with one chunk stored answers for that chunk and leaves the rest empty.
        let (store, plan, values) = fixture(4 * BIN);
        let mut pyramid = Pyramid::new(&plan, 4 * BIN);
        let reads = read_rows(&store.view(), &plan, BIN..2 * BIN).unwrap();
        let chunk = pyramid.runs.iter().map(|(run, _)| by_primitive!(run.primitive, T =>
            reduce_chunk::<T>(&reads, run, plan.physical_stride(), BIN..2 * BIN))).collect();
        pyramid.store_chunk(1, chunk);
        let columns = pyramid.columns(0..4 * BIN, 4);
        assert_eq!(columns.values[0][1], brute(&values[0], BIN..2 * BIN));
        assert_eq!((columns.values[0][0], columns.values[0][2]), (None, None), "not built yet");
        assert!(pyramid.built() > 0. && pyramid.built() < 1.);
    }

    #[test]
    fn a_finished_pyramid_reloads_from_the_disk_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.bin");
        let (snapshot, plan, _) = fixture(20_000);
        std::fs::write(&path, snapshot.view().read(0, snapshot.view().len()).unwrap()).unwrap();
        let store = ByteStore::from_file(Arc::new(crate::file_source::FileSource::open(&path).unwrap()));
        let node = crate::structure::Program::parse(&format!("structures:\n  - name: R\n    fields: [{}{{name: small, type: i8}}, {{name: level, type: f32}}]\nactions:\n  - read: {{name: records, type: {{structure: R}}, repeat: until end}}\n",
            (0..12).map(|c| format!("{{name: c{c}, type: u16}}, ")).collect::<String>()))
            .unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap().nodes.remove(0);
        let plan_file = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        assert_eq!(plan_file.fields.len(), plan.fields.len());
        let builder = PyramidBuilder::start(store.view(), plan_file.clone(), 20_000, 0..20_000, || {});
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while builder.built() < 1. { assert!(std::time::Instant::now() < deadline); std::thread::sleep(std::time::Duration::from_millis(2)); }
        let expected = builder.columns(0..20_000, 64);
        let cached = loop {
            if let Some(pyramid) = cache::load(&store.view(), &plan_file, 20_000) { break pyramid; }
            assert!(std::time::Instant::now() < deadline, "the finished pyramid is saved");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(cached.built(), 1.);
        assert_eq!(cached.columns(0..20_000, 64), expected);
        let _ = std::fs::remove_file(cache::path(&store.view(), &plan_file, 20_000).unwrap());
    }

    /// Build and query throughput on synthetic u16 channel files.
    /// Run: `BENCH_DIR=<scratch dir> cargo test --release --bin binwave pyramid_throughput -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; writes ~400 MB to BENCH_DIR"]
    fn pyramid_throughput() {
        use std::io::Write;
        let dir = std::env::var("BENCH_DIR").expect("BENCH_DIR names a scratch directory");
        for channels in [108usize, 432] {
            let records = 2_000_000 * 108 / channels;
            let path = std::path::Path::new(&dir).join(format!("pyramid{channels}.bin"));
            let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
            for record in 0..records { for channel in 0..channels { out.write_all(&(((record * 7 + channel * 13) % 4000) as u16).to_le_bytes()).unwrap(); } }
            drop(out);
            let fields: String = (0..channels).map(|c| format!("{{name: c{c}, type: u16}},")).collect();
            let yaml = format!("structures:\n  - name: R\n    fields: [{}]\nactions:\n  - read: {{name: records, type: {{structure: R}}, repeat: until end}}\n", fields.trim_end_matches(','));
            let store = ByteStore::from_file(Arc::new(crate::file_source::FileSource::open(&path).unwrap()));
            let node = Program::parse(&yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap().nodes.remove(0);
            let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
            let _ = std::fs::remove_file(cache::path(&store.view(), &plan, records).unwrap());
            let started = std::time::Instant::now();
            let builder = PyramidBuilder::start(store.view(), plan.clone(), records, 0..records / 10, || {});
            let mut first_view = None;
            while builder.built() < 1. {
                if first_view.is_none() && builder.columns(0..records / 10, 1157).values.iter().all(|c| c.iter().all(Option::is_some)) {
                    first_view = Some(started.elapsed().as_secs_f64());
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let build = started.elapsed().as_secs_f64();
            let t = std::time::Instant::now();
            for _ in 0..10 { let _ = builder.columns(0..records, 1157); }
            let whole = t.elapsed().as_secs_f64() / 10.;
            let t = std::time::Instant::now();
            for step in 0..10 { let _ = builder.columns(step * 1000..step * 1000 + records / 20, 1157); }
            let pan = t.elapsed().as_secs_f64() / 10.;
            eprintln!("{channels} ch, {records} samples: first tenth visible {:.3}s, whole build {build:.2}s ({:.0} M samples/s), whole-file query {:.1} ms, pan query {:.1} ms",
                first_view.unwrap_or(build), records as f64 / build / 1e6, whole * 1e3, pan * 1e3);
            // What Signal does when a pan leaves the fetched window or the zoom
            // changes: fetch three view widths for every channel and pack them.
            let all: Vec<usize> = (0..channels).collect();
            let colors: Vec<_> = (0..channels).map(|slot| (slot, egui::Color32::WHITE)).collect();
            let t = std::time::Instant::now();
            let window = builder.columns_of(records / 3..records / 3 + records / 2, 3 * 1157, &all);
            let fetch = t.elapsed().as_secs_f64();
            let t = std::time::Instant::now();
            let _ = crate::plot_columns_gpu::ColumnUpload::new(&window, &colors);
            let pack = t.elapsed().as_secs_f64();
            let pyramid = builder.shared.pyramid.read().unwrap();
            let bytes: usize = pyramid.level_bins.iter().sum::<usize>() * channels * 2 * 2;
            eprintln!("  refetch {:.1} ms + pack {:.1} ms ({:.1} MB to the GPU); summary in memory {:.1} MB ({:.1}% of the data)",
                fetch * 1e3, pack * 1e3, (channels * 3 * 1157 * 8) as f64 / 1e6, bytes as f64 / 1e6,
                bytes as f64 * 100. / (records * channels * 2) as f64);
            drop(pyramid);
            let _ = std::fs::remove_file(cache::path(&store.view(), &plan, records).unwrap());
            std::fs::remove_file(&path).unwrap();
        }
    }
}
