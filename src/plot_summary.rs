//! Signal's drawing for fixed-layout numeric collections: each pixel column
//! shows the lowest and highest sample it covers, drawn by the GPU column
//! renderer. Zoomed out, columns come from the collection's min/max pyramid;
//! closer in, until a pixel covers only a couple of samples, they are read
//! straight from the samples with the SIMD reduction. Panning and zooming never
//! wait on the general-purpose plot reader, which only draws single samples.
use crate::byte_store::ByteView;
use crate::plot_columns_gpu::{ColumnUpload, GpuColumns};
use crate::plot_presentation::Presentation;
use crate::plot_pyramid::{self, Columns, PyramidBuilder, BIN};
use crate::structure::NumericPlan;
use crate::structure_plot::PlotPath;
use egui::{Color32, Painter, Pos2};
use egui_plot::{PlotPoint, PlotTransform};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Fewest samples per pixel drawn as columns; closer in, the reader draws samples.
pub const MIN_SAMPLES_PER_PIXEL: usize = 2;
/// View widths fetched on each side of the view, so panning draws from the
/// upload already on the GPU.
const MARGIN_VIEWS: f64 = 1.;
/// Under columns read from the samples, pyramid columns this many view widths
/// on each side (at half the pixel resolution) fill the ends while a read runs.
const UNDER_MARGIN_VIEWS: f64 = 2.;
/// While the summary builds, how often shown columns are fetched again.
const REFRESH: Duration = Duration::from_millis(250);
/// While the summary builds, how often the whole-collection overview is.
const OVERVIEW_REFRESH: Duration = Duration::from_secs(1);
/// Whole-collection columns kept for the overview strip and series ranges.
const OVERVIEW_COLUMNS: usize = 2000;
/// Bytes a click reads at most to find the sample under the pointer.
const PICK_BYTES: usize = 64 << 20;

/// What a set of columns covers.
#[derive(Clone, Debug, PartialEq)]
struct Cover {
    window: Range<usize>,
    /// Samples per column.
    per_column: f64,
    /// Channels uploaded, in upload order.
    channels: Vec<usize>,
    /// Read from the samples rather than the pyramid.
    direct: bool,
    /// Share of the pyramid built when fetched; 1 for columns read from samples.
    built: f32,
}

impl Cover {
    /// Whether these columns still serve `view` at `per_column`: same channels
    /// and source, a similar zoom, and the view at least a quarter view inside
    /// the window's edges, so a steady pan fetches ahead of running out.
    fn serves(&self, view: &Range<usize>, per_column: f64, channels: &[usize], direct: bool, count: usize) -> bool {
        let slack = view.len() / 4;
        self.channels == channels && self.direct == direct
            && !(self.window.start > 0 && view.start < self.window.start + slack)
            && !(self.window.end < count && view.end + slack > self.window.end)
            && self.per_column <= per_column * 1.5 && self.per_column >= per_column / 2.
    }
}

/// Columns on the GPU and what they cover.
struct Shown {
    cover: Cover,
    columns: Arc<Columns>,
    at: Instant,
    /// Columns this fetch read from the file, rather than kept from the last one.
    #[cfg(test)]
    fresh: usize,
}

impl Shown {
    fn column_samples(&self, column: usize, count: usize) -> Range<usize> {
        let first = self.cover.window.start + (column as f64 * self.cover.per_column) as usize;
        let last = (self.cover.window.start + ((column + 1) as f64 * self.cover.per_column) as usize).min(count);
        first..last.max(first + 1)
    }

    /// Columns overlapping `view`.
    fn columns_in(&self, view: &Range<usize>) -> Range<usize> {
        let count = self.columns.values.first().map_or(0, Vec::len);
        let first = (view.start.saturating_sub(self.cover.window.start) as f64 / self.cover.per_column) as usize;
        let last = (view.end.saturating_sub(self.cover.window.start) as f64 / self.cover.per_column).ceil() as usize;
        first.min(count)..last.min(count)
    }
}

/// A fetch running off the UI thread.
struct Fetch {
    receiver: mpsc::Receiver<(Shown, ColumnUpload)>,
    cover: Cover,
    cancel: Arc<AtomicBool>,
}

/// One set of columns: those on the GPU and the fetch that will replace them.
/// The last columns stay drawn until the next ones land.
#[derive(Default)]
struct Layer<const L: usize> {
    shown: Option<Shown>,
    fetching: Option<Fetch>,
    gpu: GpuColumns<L>,
}

impl<const L: usize> Layer<L> {
    /// Take a finished fetch; `wait` blocks for one in flight.
    fn take(&mut self, wait: bool) {
        let Some(fetch) = &self.fetching else { return };
        let result = if wait { fetch.receiver.recv().ok() } else { fetch.receiver.try_recv().ok() };
        if let Some((shown, upload)) = result {
            self.gpu.set(upload);
            self.shown = Some(shown);
            self.fetching = None;
        } else if wait || matches!(fetch.receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
            self.fetching = None;
        }
    }

    /// Whether `view` needs new columns. A fetch in flight that no longer
    /// serves the view is cancelled.
    fn needs(&mut self, view: &Range<usize>, per_column: f64, channels: &[usize], direct: bool, built: f32,
        count: usize) -> bool {
        if let Some(fetch) = &self.fetching {
            if fetch.cover.serves(view, per_column, channels, direct, count) { return false; }
            fetch.cancel.store(true, Ordering::Relaxed);
            self.fetching = None;
        }
        self.shown.as_ref().is_none_or(|shown| !shown.cover.serves(view, per_column, channels, direct, count)
            || shown.cover.built < built && (built >= 1. || shown.at.elapsed() >= REFRESH))
    }

    /// Run `job` off the UI thread for `cover`; `None` from it (cancelled or
    /// failed) keeps the current columns.
    fn fetch(&mut self, cover: Cover, colors: Vec<(usize, Color32)>, repaint: Option<egui::Context>,
        job: impl FnOnce(&AtomicBool) -> Option<(Columns, usize)> + Send + 'static) {
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(1);
        let (worker_cover, worker_cancel) = (cover.clone(), cancel.clone());
        std::thread::spawn(move || {
            let Some((columns, _fresh)) = job(&worker_cancel) else { return };
            let upload = ColumnUpload::new(&columns, &colors);
            let shown = Shown { cover: worker_cover, columns: Arc::new(columns), at: Instant::now(),
                #[cfg(test)] fresh: _fresh };
            let _ = sender.send((shown, upload));
            if let Some(repaint) = repaint { repaint.request_repaint(); }
        });
        self.fetching = Some(Fetch { receiver, cover, cancel });
    }

    fn window(&self) -> Option<Range<f64>> {
        self.shown.as_ref().map(|shown| shown.cover.window.start as f64 - 0.5..shown.cover.window.end as f64 - 0.5)
    }
}

/// One channel's column under the pointer.
pub struct Hit {
    pub name: String,
    channel: usize,
    pub samples: Range<usize>,
    pub range: [f64; 2],
}

#[derive(Default)]
pub struct SummaryPlot {
    key: Option<u64>,
    builder: Option<PyramidBuilder>,
    source: Option<ByteView>,
    plan: Option<NumericPlan>,
    /// Series name per channel.
    names: Vec<String>,
    count: usize,
    ctx: Option<egui::Context>,
    overview: Option<(f32, Instant, Columns)>,
    /// Each channel's range over the overview, when it has data.
    ranges: Vec<Option<[f64; 2]>>,
    /// The columns drawn for the view.
    detail: Layer<0>,
    /// Pyramid columns under columns read from the samples.
    under: Layer<2>,
    /// Whether the view's columns are read from the samples.
    direct: bool,
    /// The whole collection at overview resolution, drawn under everything so
    /// a fast pan or zoom-out never shows empty ends; keyed by the overview it
    /// came from and its channels.
    whole: GpuColumns<1>,
    whole_key: Option<(Instant, Vec<usize>)>,
}

impl SummaryPlot {
    /// Summarize the collection `start` describes (its plan, a series name per
    /// field, and its sample count) while `key` stays the same; `None` stops.
    pub fn sync(&mut self, ctx: &egui::Context, key: Option<u64>, focus: Range<usize>,
        start: impl FnOnce() -> Option<(ByteView, NumericPlan, Vec<String>, usize)>) {
        if self.key == key { return; }
        *self = Self { key, ..Self::default() };
        let Some((source, plan, names, count)) = key.and_then(|_| start()) else { return };
        self.ctx = Some(ctx.clone());
        let repaint = ctx.clone();
        self.builder = Some(PyramidBuilder::start(source.clone(), plan.clone(), count, focus,
            move || repaint.request_repaint()));
        (self.source, self.plan, self.names, self.count) = (Some(source), Some(plan), names, count);
    }

    /// Whether `view` is drawn as columns: each pixel covers a couple of samples or more.
    pub fn active(&self, view: &Range<usize>, pixel_width: usize) -> bool {
        self.builder.is_some() && view.len() >= MIN_SAMPLES_PER_PIXEL * pixel_width.max(1)
    }

    /// Whether the whole collection is summarized, so the summary can stand in
    /// anywhere the reader hasn't drawn yet.
    pub fn ready(&self) -> bool {
        self.builder.as_ref().is_some_and(|builder| builder.built() >= 1.)
    }

    /// Share of the collection summarized, while building.
    pub fn building(&self) -> Option<f32> {
        self.builder.as_ref().map(PyramidBuilder::built).filter(|built| *built < 1.)
    }

    pub fn error(&self) -> Option<String> {
        self.builder.as_ref()?.error()
    }

    /// Take finished fetches; `wait` blocks for those in flight.
    fn take_fetches(&mut self, wait: bool) {
        self.detail.take(wait);
        self.under.take(wait);
    }

    fn colors(&self, channels: &[usize], slots: bool) -> Vec<(usize, Color32)> {
        channels.iter().enumerate().map(|(slot, &channel)|
            (if slots { slot } else { channel }, crate::plot_trace::series_color(&self.names[channel]))).collect()
    }

    /// Fetch columns for `view` when it leaves the fetched window, the zoom
    /// changes enough to matter, the shown channels change, or more of the
    /// pyramid has been built. Fetches run off the UI thread.
    pub fn prepare(&mut self, view: Range<usize>, pixel_width: usize, visible: impl Fn(&str) -> bool) {
        self.take_fetches(false);
        if self.builder.is_none() { return; }
        // Stack channels in name order, as the line renderers do, so the colours
        // on top don't change when a zoom moves between them.
        let mut channels: Vec<usize> = (0..self.names.len()).filter(|&channel| visible(&self.names[channel])).collect();
        channels.sort_by(|&a, &b| self.names[a].cmp(&self.names[b]));
        self.update_whole(&channels);
        let Some(builder) = &self.builder else { return };
        builder.focus(view.clone());
        let (query, built, count) = (builder.query(), builder.built(), self.count);
        let per_column = view.len().max(1) as f64 / pixel_width.max(1) as f64;
        self.direct = per_column < BIN as f64;
        if self.direct {
            // Pyramid columns under the read ones, wider and coarser.
            let under = (per_column * 2.).max(BIN as f64);
            if self.under.needs(&view, under, &channels, false, built, count) {
                let margin = (view.len() as f64 * UNDER_MARGIN_VIEWS) as usize;
                let window = view.start.saturating_sub(margin)..view.end.saturating_add(margin).min(count);
                let columns = ((window.len() as f64 / under).round() as usize).max(1);
                let cover = Cover { per_column: window.len() as f64 / columns as f64, window: window.clone(),
                    channels: channels.clone(), direct: false, built };
                let query = query.clone();
                let job_channels = channels.clone();
                self.under.fetch(cover, self.colors(&channels, true), self.ctx.clone(),
                    move |_| Some((query.columns_of(window, columns, &job_channels), columns)));
            }
        }
        if !self.detail.needs(&view, per_column, &channels, self.direct, if self.direct { 1. } else { built }, count) {
            return;
        }
        let margin = (view.len() as f64 * MARGIN_VIEWS) as usize;
        let colors = self.colors(&channels, true);
        if self.direct {
            // Columns of a whole number of samples on a grid from sample 0, so a
            // pan at the same zoom keeps every column it already has.
            let step = (per_column.round() as usize).max(1);
            let first = view.start.saturating_sub(margin) / step * step;
            let end = view.end.saturating_add(margin).min(count);
            let columns = (end - first).div_ceil(step).max(1);
            let cover = Cover { window: first..end, per_column: step as f64, channels: channels.clone(),
                direct: true, built: 1. };
            let previous = self.detail.shown.as_ref()
                .filter(|shown| shown.cover.direct && shown.cover.per_column == step as f64
                    && shown.cover.channels == channels)
                .map(|shown| (shown.cover.window.start, shown.columns.clone()));
            let (source, plan) = (self.source.clone().unwrap(), self.plan.clone().unwrap());
            self.detail.fetch(cover, colors, self.ctx.clone(), move |cancel| {
                read_window(&source, &plan, count, first, step, columns, &channels, previous, cancel)
            });
        } else {
            let window = view.start.saturating_sub(margin)..view.end.saturating_add(margin).min(count);
            let columns = ((window.len() as f64 / per_column).round() as usize).max(1);
            let cover = Cover { per_column: window.len() as f64 / columns as f64, window: window.clone(),
                channels: channels.clone(), direct: false, built };
            self.detail.fetch(cover, colors, self.ctx.clone(),
                move |_| Some((query.columns_of(window, columns, &channels), columns)));
        }
    }

    /// Upload the overview's columns for `channels` when either changed.
    fn update_whole(&mut self, channels: &[usize]) {
        self.overview();
        let Some((_, at, _)) = &self.overview else { return };
        if self.whole_key.as_ref().is_some_and(|(seen, shown)| seen == at && shown == channels) { return; }
        let colors = self.colors(channels, false);
        let Some((_, at, columns)) = &self.overview else { return };
        self.whole.set(ColumnUpload::new(columns, &colors));
        self.whole_key = Some((*at, channels.to_vec()));
    }

    fn placements(&self, channels: &[usize], presentation: &Presentation) -> Vec<[f32; 2]> {
        channels.iter().map(|&channel| {
            let placement = presentation.placement(&self.names[channel]);
            [placement.scale as f32, placement.offset as f32]
        }).collect()
    }

    /// Paint the view's columns, except over `skip` (sample coordinates), where
    /// another renderer already draws samples. Without `skip` the columns are
    /// the plot, and the coarser layers fill whatever the view's columns don't
    /// reach yet.
    pub fn paint(&self, painter: &Painter, transform: &PlotTransform, presentation: &Presentation,
        skip: Option<Range<f64>>) {
        if skip.is_none() {
            let under = self.under.shown.as_ref().filter(|_| self.direct);
            if let Some((_, channels)) = &self.whole_key {
                let above = under.and_then(|_| self.under.window()).or_else(|| self.detail.window());
                self.whole.paint(painter, transform, &self.placements(channels, presentation), above);
            }
            if let Some(under) = under {
                self.under.gpu.paint(painter, transform, &self.placements(&under.cover.channels, presentation),
                    self.detail.window());
            }
        }
        let Some(shown) = &self.detail.shown else { return };
        self.detail.gpu.paint(painter, transform, &self.placements(&shown.cover.channels, presentation), skip);
    }

    /// Displayed Y extent of the included channels across `view`.
    pub fn y_bounds(&self, view: &Range<usize>, include: impl Fn(&str) -> bool,
        presentation: &Presentation) -> Option<(f64, f64)> {
        let shown = self.detail.shown.as_ref()?;
        let columns = shown.columns_in(view);
        let mut bounds: Option<(f64, f64)> = None;
        for (slot, &channel) in shown.cover.channels.iter().enumerate() {
            let name = &self.names[channel];
            if !include(name) { continue; }
            let placement = presentation.placement(name);
            for [low, high] in shown.columns.values[slot][columns.clone()].iter().flatten() {
                let (a, b) = (placement.display(*low), placement.display(*high));
                let (low, high) = (a.min(b), a.max(b));
                if !low.is_finite() || !high.is_finite() { continue; }
                bounds = Some(bounds.map_or((low, high), |(min, max)| (min.min(low), max.max(high))));
            }
        }
        bounds
    }

    /// Whether the shown columns cover `view` and every one of them has data.
    pub fn complete(&self, view: &Range<usize>) -> bool {
        let Some(shown) = &self.detail.shown else { return false };
        if view.start < shown.cover.window.start || view.end > shown.cover.window.end { return false; }
        if shown.cover.built >= 1. { return true; }
        let columns = shown.columns_in(view);
        shown.columns.values.iter().all(|values| values[columns.clone()].iter().all(Option::is_some))
    }

    /// The shown column nearest `pointer`, within `radius` points of its bar.
    pub fn hit(&self, pointer: Pos2, transform: &PlotTransform, presentation: &Presentation,
        radius: f32) -> Option<Hit> {
        let shown = self.detail.shown.as_ref()?;
        let x = transform.value_from_position(pointer).x - shown.cover.window.start as f64;
        let columns = shown.columns.values.first().map_or(0, Vec::len);
        if x < 0. || columns == 0 { return None; }
        let column = (x / shown.cover.per_column) as usize;
        if column >= columns { return None; }
        let x = transform.value_from_position(pointer).x;
        shown.cover.channels.iter().enumerate().filter_map(|(slot, &channel)| {
            let [low, high] = shown.columns.values[slot][column]?;
            let placement = presentation.placement(&self.names[channel]);
            let [a, b] = [low, high].map(|value|
                transform.position_from_point(&PlotPoint::new(x, placement.display(value))).y);
            let (top, bottom) = (a.min(b), a.max(b));
            let distance = if pointer.y < top { top - pointer.y } else { (pointer.y - bottom).max(0.) };
            (distance <= radius).then_some((distance, slot, channel, [low, high]))
        }).min_by(|a, b| a.0.total_cmp(&b.0)).map(|(_, _, channel, range)| Hit {
            name: self.names[channel].clone(),
            channel,
            samples: shown.column_samples(column, self.count),
            range,
        })
    }

    /// The sample in a hit column nearest `value` (the pointer's displayed Y),
    /// with its bytes and path, for linking Hex and Inspector.
    pub fn pick(&self, hit: &Hit, near: usize, value: f64, presentation: &Presentation)
        -> Option<(usize, Range<usize>, String)> {
        let (source, plan) = (self.source.as_ref()?, self.plan.as_ref()?);
        let field = plan.fields.get(hit.channel)?;
        // A very wide column reads only the part around the pointer.
        let most = (PICK_BYTES / plan.physical_stride().max(1)).max(1);
        let near = near.clamp(hit.samples.start, hit.samples.end - 1);
        let first = near.saturating_sub(most / 2).max(hit.samples.start);
        let samples = first..(first + most).min(hit.samples.end);
        let target = presentation.placement(&hit.name).original(value);
        let (index, _) = plot_pyramid::nearest_sample(source, plan, hit.channel, samples, target)
            .map_err(|error| log::warn!("Reading the sample under the pointer failed: {error:#}")).ok()??;
        let start = plan.offset(index)? + field.offset;
        let path = PlotPath::Indexed {
            parts: Arc::new((field.source_path.clone().unwrap_or_else(|| plan.path.clone()), field.suffix.clone())),
            index,
        };
        Some((index, start..start + field.span, path.to_string()))
    }

    /// Whole-collection min/max per channel for the overview strip and series
    /// ranges, refetched as the summary grows.
    pub fn overview(&mut self) -> Option<(f32, &[String], &Columns)> {
        let builder = self.builder.as_ref()?;
        let built = builder.built();
        let stale = self.overview.as_ref().is_none_or(|(seen, at, _)|
            *seen < built && (built >= 1. || at.elapsed() >= OVERVIEW_REFRESH));
        if stale {
            let columns = builder.columns(0..self.count, OVERVIEW_COLUMNS);
            self.ranges = columns.values.iter().map(|bands| bands.iter().flatten().copied()
                .reduce(|[low, high], [min, max]| [low.min(min), high.max(max)])).collect();
            self.overview = Some((built, Instant::now(), columns));
        }
        self.overview.as_ref().map(|(built, _, columns)| (*built, self.names.as_slice(), columns))
    }

    /// Each channel's name and range over the part summarized, as of the last
    /// `overview`.
    pub fn ranges(&self) -> impl Iterator<Item = (&String, [f64; 2])> {
        self.names.iter().zip(&self.ranges).filter_map(|(name, range)| Some((name, (*range)?)))
    }
}

/// Columns for a window of `columns` columns of `step` samples from `first`,
/// keeping those `previous` (its first sample and columns, on the same grid)
/// already has and reading the rest from the samples.
#[allow(clippy::too_many_arguments)] // The window's grid, the channels and what to keep.
fn read_window(source: &ByteView, plan: &NumericPlan, count: usize, first: usize, step: usize, columns: usize,
    channels: &[usize], previous: Option<(usize, Arc<Columns>)>, cancel: &AtomicBool) -> Option<(Columns, usize)> {
    let mut values = vec![vec![None; columns]; channels.len()];
    let mut have = vec![false; columns];
    if let Some((start, previous)) = &previous {
        let kept = previous.values.first().map_or(0, Vec::len);
        for (column, have) in have.iter_mut().enumerate() {
            let sample = first + column * step;
            let Some(offset) = sample.checked_sub(*start).filter(|offset| offset % step == 0) else { continue };
            let Some(index) = Some(offset / step).filter(|index| *index < kept) else { continue };
            for (slot, values) in values.iter_mut().enumerate() { values[column] = previous.values[slot][index]; }
            *have = true;
        }
    }
    let mut fresh = 0;
    let mut column = 0;
    while column < columns {
        if have[column] { column += 1; continue; }
        let start = column;
        while column < columns && !have[column] { column += 1; }
        match plot_pyramid::read_columns(source, plan, count, first + start * step, step, column - start, channels, cancel) {
            Ok(read) => {
                for (values, read) in values.iter_mut().zip(read) { values[start..column].copy_from_slice(&read); }
                fresh += column - start;
            }
            Err(_) if cancel.load(Ordering::Relaxed) => return None,
            Err(error) => log::warn!("Reading plot columns failed: {error:#}"),
        }
    }
    Some((Columns { range: first..first + columns * step, bin_samples: 1, values }, fresh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byte_store::ByteStore;
    use crate::structure::Program;
    use std::sync::atomic::AtomicBool;

    /// Four u16 channels: a slow ramp, its negation, a constant and a spike.
    fn fixture(count: usize) -> (ByteStore, NumericPlan, Vec<String>) {
        let mut bytes = Vec::new();
        for index in 0..count {
            let ramp = (index / 64) as u16;
            let spike = if index == count / 2 + 3 { 60_000 } else { 100 };
            for value in [ramp, u16::MAX - ramp, 7, spike] { bytes.extend(value.to_le_bytes()); }
        }
        let yaml = "structures:\n  - name: R\n    fields: [{name: a, type: u16}, {name: b, type: u16}, {name: c, type: u16}, {name: d, type: u16}]\n\
            actions:\n  - read: {name: records, type: {structure: R}, repeat: until end}\n";
        let store = ByteStore::from_snapshot(crate::binary::Original::from_bytes(&bytes).unwrap());
        let node = Program::parse(yaml).unwrap().run(&store.view(), &AtomicBool::new(false)).unwrap().nodes.remove(0);
        let plan = node.lazy.as_ref().unwrap().numeric_plan().unwrap();
        let names = plan.fields.iter().map(|field| format!("records / {}", field.name)).collect();
        (store, plan, names)
    }

    /// `prepare`, then wait for its fetch.
    fn prepare(summary: &mut SummaryPlot, view: Range<usize>, pixel_width: usize, visible: impl Fn(&str) -> bool) {
        summary.prepare(view, pixel_width, visible);
        summary.take_fetches(true);
    }

    fn built(count: usize) -> (SummaryPlot, ByteStore) {
        let (store, plan, names) = fixture(count);
        let mut summary = SummaryPlot::default();
        let view = store.view();
        summary.sync(&egui::Context::default(), Some(1), 0..count, || Some((view, plan, names, count)));
        let deadline = Instant::now() + Duration::from_secs(30);
        while summary.building().is_some() || summary.builder.as_ref().unwrap().built() < 1. {
            assert!(Instant::now() < deadline, "summary builds");
            std::thread::sleep(Duration::from_millis(2));
        }
        (summary, store)
    }

    #[test]
    fn views_with_a_couple_of_samples_per_pixel_are_drawn_as_columns() {
        let (summary, _store) = built(BIN * 1000);
        assert!(summary.active(&(0..BIN * 1000), 1000));
        assert!(summary.active(&(0..200), 100));
        assert!(!summary.active(&(0..199), 100), "closer in, the reader draws single samples");
        assert!(!SummaryPlot::default().active(&(0..BIN * 1000), 10), "nothing to summarize");
    }

    #[test]
    fn shown_columns_follow_hidden_series_and_fit_the_visible_ones() {
        let count = BIN * 1000;
        let (mut summary, _store) = built(count);
        let presentation = Presentation::default();
        prepare(&mut summary, 0..count, 500, |name| name != "records / b");
        let shown = summary.detail.shown.as_ref().unwrap();
        assert_eq!(shown.cover.channels, [0, 2, 3], "hidden series are not uploaded");
        assert!(summary.complete(&(0..count)));
        // The spike and the ramp set the fit; the hidden channel near u16::MAX does not.
        let (low, high) = summary.y_bounds(&(0..count), |_| true, &presentation).unwrap();
        assert_eq!((low, high), (0., 60_000.));
        let (low, high) = summary.y_bounds(&(0..count), |name| name == "records / c", &presentation).unwrap();
        assert_eq!((low, high), (7., 7.));
        // Panning within the fetched window keeps the upload; leaving it refetches.
        let at = shown.at;
        prepare(&mut summary, count / 10..count / 10 + count / 4, 500, |name| name != "records / b");
        let at_zoomed = summary.detail.shown.as_ref().unwrap().at;
        assert_ne!(at, at_zoomed, "zooming in refetches");
        prepare(&mut summary, count / 10 + 10..count / 10 + count / 4 + 10, 500, |name| name != "records / b");
        assert_eq!(summary.detail.shown.as_ref().unwrap().at, at_zoomed, "a small pan reuses the upload");
        // Nearing the fetched window's edge fetches ahead, before the view runs out.
        let window = summary.detail.shown.as_ref().unwrap().cover.window.clone();
        let near_end = window.end - count / 4 - count / 32..window.end - count / 32;
        prepare(&mut summary, near_end, 500, |name| name != "records / b");
        assert_ne!(summary.detail.shown.as_ref().unwrap().at, at_zoomed, "fetched ahead of the edge");
    }

    #[test]
    fn fetches_run_off_the_ui_thread_and_the_last_columns_stay_drawn() {
        let count = BIN * 1000;
        let (mut summary, _store) = built(count);
        prepare(&mut summary, 0..count, 500, |_| true);
        let first = summary.detail.shown.as_ref().unwrap().at;
        // Zooming in starts a fetch; until it lands the previous columns remain.
        summary.prepare(0..count / 2, 500, |_| true);
        assert!(summary.detail.fetching.is_some());
        assert_eq!(summary.detail.shown.as_ref().unwrap().at, first);
        summary.take_fetches(true);
        assert_ne!(summary.detail.shown.as_ref().unwrap().at, first);
        assert_eq!(summary.detail.shown.as_ref().unwrap().cover.window, 0..count, "the view and a view width after it");
    }

    #[test]
    fn the_whole_collection_layer_follows_the_shown_channels() {
        let count = BIN * 1000;
        let (mut summary, _store) = built(count);
        prepare(&mut summary, 0..count / 2, 500, |name| name != "records / b");
        assert_eq!(summary.whole_key.as_ref().unwrap().1, [0, 2, 3], "ready before any detailed fetch lands");
        let generation = summary.whole.generation();
        prepare(&mut summary, count / 16..count / 16 + count / 2, 500, |name| name != "records / b");
        assert_eq!(summary.whole.generation(), generation, "panning keeps the whole-collection upload");
        prepare(&mut summary, 0..count / 2, 500, |_| true);
        assert_eq!(summary.whole_key.as_ref().unwrap().1, [0, 1, 2, 3]);
        assert_ne!(summary.whole.generation(), generation, "showing a series uploads it");
    }

    #[test]
    fn closer_views_read_their_columns_from_the_samples_and_pans_keep_them() {
        let count = BIN * 1000;
        let (mut summary, store) = built(count);
        let presentation = Presentation::default();
        // 10 samples a pixel: too close for the pyramid's bins.
        let view = count / 2 - 2_500..count / 2 + 2_500;
        prepare(&mut summary, view.clone(), 500, |_| true);
        let shown = summary.detail.shown.as_ref().unwrap();
        assert!(shown.cover.direct && shown.cover.per_column == 10.);
        assert_eq!(shown.cover.window.start % 10, 0, "columns sit on a grid from sample 0");
        let values: Vec<Vec<u16>> = (0..4).map(|channel| (0..count).map(|index| {
            let at = index * 8 + channel * 2;
            u16::from_le_bytes(store.view().read(at, 2).unwrap().try_into().unwrap())
        }).collect()).collect();
        for column in [0, 7, 250, 499] {
            let samples = shown.column_samples(column, count);
            for (slot, &channel) in shown.cover.channels.iter().enumerate() {
                let seen = &values[channel][samples.clone()];
                let expected = [*seen.iter().min().unwrap() as f64, *seen.iter().max().unwrap() as f64];
                assert_eq!(shown.columns.values[slot][column], Some(expected), "column {column} channel {channel}");
            }
        }
        // The spike at count / 2 + 3 is exact, not smeared over a 256-sample bin.
        let (low, high) = summary.y_bounds(&view, |name| name == "records / d", &presentation).unwrap();
        assert_eq!((low, high), (100., 60_000.));
        let spike_column = shown.columns_in(&(count / 2 + 3..count / 2 + 4)).start;
        assert_eq!(shown.column_samples(spike_column, count), count / 2..count / 2 + 10);
        assert!(summary.under.shown.as_ref().is_some_and(|under| !under.cover.direct
            && under.cover.window.len() > shown.cover.window.len()), "pyramid columns under them, wider");
        // Panning past the prefetch point at the same zoom reads only the new columns.
        let before = shown.columns.values[0].len();
        let pan = view.start + 4_000..view.end + 4_000;
        prepare(&mut summary, pan, 500, |_| true);
        let shown = summary.detail.shown.as_ref().unwrap();
        assert!(shown.fresh > 0 && shown.fresh < before / 2, "read {} of {before} columns", shown.fresh);
        assert!(summary.complete(&(view.start + 4_000..view.end + 4_000)));
    }

    #[test]
    fn a_click_on_a_column_picks_the_sample_nearest_the_pointer() {
        let count = BIN * 1000;
        let (mut summary, store) = built(count);
        let presentation = Presentation::default();
        prepare(&mut summary, 0..count, 1000, |_| true);
        let frame = egui::Rect::from_min_size(egui::pos2(0., 0.), egui::vec2(1000., 600.));
        let transform = PlotTransform::new(frame, egui_plot::PlotBounds::from_min_max(
            [-0.5, 0.], [count as f64 - 0.5, 65_535.]), egui::Vec2b::new(false, false));
        // Point at the spike's column, near its top.
        let spike = count / 2 + 3;
        let pointer = transform.position_from_point(&PlotPoint::new(spike as f64, 59_000.));
        let hit = summary.hit(pointer, &transform, &presentation, 12.).expect("the spike's column");
        assert_eq!(hit.name, "records / d");
        assert!(hit.samples.contains(&spike));
        assert_eq!(hit.range, [100., 60_000.]);
        let (index, bytes, path) = summary.pick(&hit, spike - 40, 59_000., &presentation).unwrap();
        assert_eq!(index, spike, "the sample whose value is nearest the pointer");
        assert_eq!(bytes, spike * 8 + 6..spike * 8 + 8);
        assert_eq!(store.view().read(bytes.start, 2).unwrap(), 60_000u16.to_le_bytes());
        assert_eq!(path, format!("records[{spike}].d"));
        // Far from every bar there is no hit.
        let empty = transform.position_from_point(&PlotPoint::new(10., 30_000.));
        assert!(summary.hit(empty, &transform, &presentation, 12.).is_none());
    }

    #[test]
    fn the_overview_covers_the_whole_collection_per_channel() {
        let count = BIN * 1000;
        let (mut summary, _store) = built(count);
        let (built, names, columns) = summary.overview().unwrap();
        assert_eq!(built, 1.);
        assert_eq!(names.len(), 4);
        assert_eq!(columns.values.len(), 4);
        let spike = columns.values[3].iter().flatten().map(|[_, high]| *high).fold(0., f64::max);
        assert_eq!(spike, 60_000.);
    }
}
