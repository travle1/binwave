//! Signal's zoomed-out path. While each pixel spans at least a pyramid bin, the
//! primary collection is drawn from its min/max pyramid by the GPU column
//! renderer, so panning and zooming never wait on a file scan. Closer in, the
//! plot reader draws the samples themselves.
use crate::byte_store::ByteView;
use crate::plot_columns_gpu::{ColumnUpload, GpuColumns};
use crate::plot_presentation::Presentation;
use crate::plot_pyramid::{self, Columns, PyramidBuilder, BIN};
use crate::structure::NumericPlan;
use crate::structure_plot::PlotPath;
use egui::{Painter, Pos2};
use egui_plot::{PlotPoint, PlotTransform};
use std::ops::Range;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// View widths fetched on each side of the view, so panning draws from the
/// upload already on the GPU.
const MARGIN_VIEWS: f64 = 1.;
/// While the summary builds, how often shown columns are fetched again.
const REFRESH: Duration = Duration::from_millis(250);
/// While the summary builds, how often the whole-collection overview is.
const OVERVIEW_REFRESH: Duration = Duration::from_secs(1);
/// Whole-collection columns kept for the overview strip and series ranges.
const OVERVIEW_COLUMNS: usize = 2000;
/// Bytes a click reads at most to find the sample under the pointer.
const PICK_BYTES: usize = 64 << 20;

/// The columns on the GPU and what they were fetched for.
struct Shown {
    window: Range<usize>,
    /// Samples per column, as the pyramid spaced them.
    per_column: f64,
    /// Channels uploaded, in upload order.
    channels: Vec<usize>,
    columns: Columns,
    built: f32,
    at: Instant,
}

impl Shown {
    fn column_samples(&self, column: usize, count: usize) -> Range<usize> {
        let first = self.window.start + (column as f64 * self.per_column) as usize;
        let last = (self.window.start + ((column + 1) as f64 * self.per_column) as usize).min(count);
        first..last.max(first + 1)
    }

    /// Columns overlapping `view`.
    fn columns_in(&self, view: &Range<usize>) -> Range<usize> {
        let count = self.columns.values.first().map_or(0, Vec::len);
        let first = (view.start.saturating_sub(self.window.start) as f64 / self.per_column) as usize;
        let last = (view.end.saturating_sub(self.window.start) as f64 / self.per_column).ceil() as usize;
        first.min(count)..last.min(count)
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
    shown: Option<Shown>,
    /// A fetch running off the UI thread; the last one stays drawn meanwhile.
    fetching: Option<mpsc::Receiver<(Shown, ColumnUpload)>>,
    ctx: Option<egui::Context>,
    overview: Option<(f32, Instant, Columns)>,
    /// Each channel's range over the overview, when it has data.
    ranges: Vec<Option<[f64; 2]>>,
    gpu: GpuColumns,
    /// The whole collection at overview resolution, drawn under the detailed
    /// columns so a fast pan or zoom-out never shows empty ends; keyed by the
    /// overview it came from and its channels.
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

    /// Whether `view` is drawn from the summary: each pixel covers a whole bin.
    pub fn active(&self, view: &Range<usize>, pixel_width: usize) -> bool {
        self.builder.is_some() && view.len() >= BIN * pixel_width.max(1)
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

    /// Take a finished fetch; `wait` blocks for one in flight.
    fn take_fetch(&mut self, wait: bool) {
        let Some(receiver) = &self.fetching else { return };
        let result = if wait { receiver.recv().ok() } else { receiver.try_recv().ok() };
        if let Some((shown, upload)) = result {
            self.gpu.set(upload);
            self.shown = Some(shown);
            self.fetching = None;
        } else if wait || matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
            self.fetching = None;
        }
    }

    /// Fetch and upload columns for `view` when it leaves the fetched window,
    /// the zoom changes enough to matter, the shown channels change, or more of
    /// it has been built. Fetches run off the UI thread, one at a time.
    pub fn prepare(&mut self, view: Range<usize>, pixel_width: usize, visible: impl Fn(&str) -> bool) {
        self.take_fetch(false);
        if self.builder.is_none() { return; }
        // Stack channels in name order, as the line renderers do, so the colours
        // on top don't change when a zoom moves between them.
        let mut channels: Vec<usize> = (0..self.names.len()).filter(|&channel| visible(&self.names[channel])).collect();
        channels.sort_by(|&a, &b| self.names[a].cmp(&self.names[b]));
        self.update_whole(&channels);
        let Some(builder) = &self.builder else { return };
        builder.focus(view.clone());
        if self.fetching.is_some() { return; }
        let per_column = view.len().max(1) as f64 / pixel_width.max(1) as f64;
        let built = builder.built();
        // Fetch ahead once the view comes within a quarter view of the window's
        // edge, so a steady pan never reaches the end of what is on the GPU.
        let slack = view.len() / 4;
        let stale = self.shown.as_ref().is_none_or(|shown| shown.channels != channels
            || shown.window.start > 0 && view.start < shown.window.start + slack
            || shown.window.end < self.count && view.end + slack > shown.window.end
            || shown.per_column > per_column * 1.5 || shown.per_column < per_column / 2.
            || shown.built < built && (built >= 1. || shown.at.elapsed() >= REFRESH));
        if !stale { return; }
        let margin = (view.len() as f64 * MARGIN_VIEWS) as usize;
        let window = view.start.saturating_sub(margin)..view.end.saturating_add(margin).min(self.count);
        let count = ((window.len() as f64 / per_column).round() as usize).max(1);
        let colors: Vec<_> = channels.iter().enumerate()
            .map(|(slot, &channel)| (slot, crate::plot_trace::series_color(&self.names[channel]))).collect();
        let (query, repaint) = (builder.query(), self.ctx.clone());
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let columns = query.columns_of(window.clone(), count, &channels);
            let upload = ColumnUpload::new(&columns, &colors);
            let shown = Shown { per_column: window.len() as f64 / count as f64, window, channels, columns,
                built, at: Instant::now() };
            let _ = sender.send((shown, upload));
            if let Some(repaint) = repaint { repaint.request_repaint(); }
        });
        self.fetching = Some(receiver);
    }

    /// Upload the overview's columns for `channels` when either changed.
    fn update_whole(&mut self, channels: &[usize]) {
        self.overview();
        let Some((_, at, columns)) = &self.overview else { return };
        if self.whole_key.as_ref().is_some_and(|(seen, shown)| seen == at && shown == channels) { return; }
        let colors: Vec<_> = channels.iter()
            .map(|&channel| (channel, crate::plot_trace::series_color(&self.names[channel]))).collect();
        self.whole.set(ColumnUpload::new(columns, &colors));
        self.whole_key = Some((*at, channels.to_vec()));
    }

    fn placements(&self, channels: &[usize], presentation: &Presentation) -> Vec<[f32; 2]> {
        channels.iter().map(|&channel| {
            let placement = presentation.placement(&self.names[channel]);
            [placement.scale as f32, placement.offset as f32]
        }).collect()
    }

    /// Paint the shown columns, except over `skip` (sample coordinates), where
    /// another renderer already draws samples. Without `skip` the summary is
    /// the plot, and the whole-collection layer fills whatever the detailed
    /// columns don't reach yet.
    pub fn paint(&self, painter: &Painter, transform: &PlotTransform, presentation: &Presentation,
        skip: Option<Range<f64>>) {
        if skip.is_none() {
            if let Some((_, channels)) = &self.whole_key {
                let detailed = self.shown.as_ref()
                    .map(|shown| shown.window.start as f64 - 0.5..shown.window.end as f64 - 0.5);
                self.whole.paint(painter, transform, &self.placements(channels, presentation), detailed);
            }
        }
        let Some(shown) = &self.shown else { return };
        self.gpu.paint(painter, transform, &self.placements(&shown.channels, presentation), skip);
    }

    /// Displayed Y extent of the included channels across `view`.
    pub fn y_bounds(&self, view: &Range<usize>, include: impl Fn(&str) -> bool,
        presentation: &Presentation) -> Option<(f64, f64)> {
        let shown = self.shown.as_ref()?;
        let columns = shown.columns_in(view);
        let mut bounds: Option<(f64, f64)> = None;
        for (slot, &channel) in shown.channels.iter().enumerate() {
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

    /// Whether every shown column across `view` has data.
    pub fn complete(&self, view: &Range<usize>) -> bool {
        let Some(shown) = &self.shown else { return false };
        if shown.built >= 1. { return true; }
        let columns = shown.columns_in(view);
        shown.columns.values.iter().all(|values| values[columns.clone()].iter().all(Option::is_some))
    }

    /// The shown column nearest `pointer`, within `radius` points of its bar.
    pub fn hit(&self, pointer: Pos2, transform: &PlotTransform, presentation: &Presentation,
        radius: f32) -> Option<Hit> {
        let shown = self.shown.as_ref()?;
        let x = transform.value_from_position(pointer).x - shown.window.start as f64;
        let columns = shown.columns.values.first().map_or(0, Vec::len);
        if x < 0. || columns == 0 { return None; }
        let column = (x / shown.per_column) as usize;
        if column >= columns { return None; }
        let x = transform.value_from_position(pointer).x;
        shown.channels.iter().enumerate().filter_map(|(slot, &channel)| {
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
        summary.take_fetch(true);
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
    fn only_views_with_a_bin_per_pixel_use_the_summary() {
        let (summary, _store) = built(BIN * 1000);
        assert!(summary.active(&(0..BIN * 1000), 1000));
        assert!(summary.active(&(0..BIN * 100), 100));
        assert!(!summary.active(&(0..BIN * 100 - 1), 100), "closer in, the reader draws samples");
        assert!(!SummaryPlot::default().active(&(0..BIN * 1000), 10), "nothing to summarize");
    }

    #[test]
    fn shown_columns_follow_hidden_series_and_fit_the_visible_ones() {
        let count = BIN * 1000;
        let (mut summary, _store) = built(count);
        let presentation = Presentation::default();
        prepare(&mut summary, 0..count, 500, |name| name != "records / b");
        let shown = summary.shown.as_ref().unwrap();
        assert_eq!(shown.channels, [0, 2, 3], "hidden series are not uploaded");
        assert!(summary.complete(&(0..count)));
        // The spike and the ramp set the fit; the hidden channel near u16::MAX does not.
        let (low, high) = summary.y_bounds(&(0..count), |_| true, &presentation).unwrap();
        assert_eq!((low, high), (0., 60_000.));
        let (low, high) = summary.y_bounds(&(0..count), |name| name == "records / c", &presentation).unwrap();
        assert_eq!((low, high), (7., 7.));
        // Panning within the fetched window keeps the upload; leaving it refetches.
        let at = shown.at;
        prepare(&mut summary, count / 10..count / 10 + count / 4, 500, |name| name != "records / b");
        let at_zoomed = summary.shown.as_ref().unwrap().at;
        assert_ne!(at, at_zoomed, "zooming in refetches");
        prepare(&mut summary, count / 10 + 10..count / 10 + count / 4 + 10, 500, |name| name != "records / b");
        assert_eq!(summary.shown.as_ref().unwrap().at, at_zoomed, "a small pan reuses the upload");
        // Nearing the fetched window's edge fetches ahead, before the view runs out.
        let window = summary.shown.as_ref().unwrap().window.clone();
        let near_end = window.end - count / 4 - count / 32..window.end - count / 32;
        prepare(&mut summary, near_end, 500, |name| name != "records / b");
        assert_ne!(summary.shown.as_ref().unwrap().at, at_zoomed, "fetched ahead of the edge");
    }

    #[test]
    fn fetches_run_off_the_ui_thread_and_the_last_columns_stay_drawn() {
        let count = BIN * 1000;
        let (mut summary, _store) = built(count);
        prepare(&mut summary, 0..count, 500, |_| true);
        let first = summary.shown.as_ref().unwrap().at;
        // Zooming in starts a fetch; until it lands the previous columns remain.
        summary.prepare(0..count / 8, 500, |_| true);
        assert!(summary.fetching.is_some());
        assert_eq!(summary.shown.as_ref().unwrap().at, first);
        summary.take_fetch(true);
        assert_ne!(summary.shown.as_ref().unwrap().at, first);
        assert_eq!(summary.shown.as_ref().unwrap().window, 0..count / 4, "the view and a view width after it");
    }

    #[test]
    fn the_whole_collection_layer_follows_the_shown_channels() {
        let count = BIN * 1000;
        let (mut summary, _store) = built(count);
        prepare(&mut summary, 0..count / 8, 500, |name| name != "records / b");
        assert_eq!(summary.whole_key.as_ref().unwrap().1, [0, 2, 3], "ready before any detailed fetch lands");
        let generation = summary.whole.generation();
        prepare(&mut summary, count / 16..count / 16 + count / 8, 500, |name| name != "records / b");
        assert_eq!(summary.whole.generation(), generation, "panning keeps the whole-collection upload");
        prepare(&mut summary, 0..count / 8, 500, |_| true);
        assert_eq!(summary.whole_key.as_ref().unwrap().1, [0, 1, 2, 3]);
        assert_ne!(summary.whole.generation(), generation, "showing a series uploads it");
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
