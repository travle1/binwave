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
use std::sync::Arc;
use std::time::{Duration, Instant};

/// View widths fetched on each side of the view, so panning draws from the
/// upload already on the GPU.
const MARGIN_VIEWS: f64 = 1.;
/// While the summary builds, how often shown columns are fetched again.
const REFRESH: Duration = Duration::from_millis(200);
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
    overview: Option<(f32, Instant, Columns)>,
    gpu: GpuColumns,
}

impl SummaryPlot {
    /// Summarize the collection `start` describes (its plan, a series name per
    /// field, and its sample count) while `key` stays the same; `None` stops.
    pub fn sync(&mut self, ctx: &egui::Context, key: Option<u64>, focus: Range<usize>,
        start: impl FnOnce() -> Option<(ByteView, NumericPlan, Vec<String>, usize)>) {
        if self.key == key { return; }
        *self = Self { key, ..Self::default() };
        let Some((source, plan, names, count)) = key.and_then(|_| start()) else { return };
        let repaint = ctx.clone();
        self.builder = Some(PyramidBuilder::start(source.clone(), plan.clone(), count, focus,
            move || repaint.request_repaint()));
        (self.source, self.plan, self.names, self.count) = (Some(source), Some(plan), names, count);
    }

    /// Whether `view` is drawn from the summary: each pixel covers a whole bin.
    pub fn active(&self, view: &Range<usize>, pixel_width: usize) -> bool {
        self.builder.is_some() && view.len() >= BIN * pixel_width.max(1)
    }

    /// Share of the collection summarized, while building.
    pub fn building(&self) -> Option<f32> {
        self.builder.as_ref().map(PyramidBuilder::built).filter(|built| *built < 1.)
    }

    pub fn error(&self) -> Option<String> {
        self.builder.as_ref()?.error()
    }

    /// Fetch and upload columns for `view` when it leaves the fetched window,
    /// the zoom changes enough to matter, the shown channels change, or more of
    /// it has been built.
    pub fn prepare(&mut self, view: Range<usize>, pixel_width: usize, visible: impl Fn(&str) -> bool) {
        let Some(builder) = &self.builder else { return };
        builder.focus(view.clone());
        let per_column = view.len().max(1) as f64 / pixel_width.max(1) as f64;
        let channels: Vec<usize> = (0..self.names.len()).filter(|&channel| visible(&self.names[channel])).collect();
        let built = builder.built();
        let stale = self.shown.as_ref().is_none_or(|shown| shown.channels != channels
            || view.start < shown.window.start || view.end > shown.window.end
            || shown.per_column > per_column * 1.5 || shown.per_column < per_column / 2.
            || shown.built < built && (built >= 1. || shown.at.elapsed() >= REFRESH));
        if !stale { return; }
        let margin = (view.len() as f64 * MARGIN_VIEWS) as usize;
        let window = view.start.saturating_sub(margin)..view.end.saturating_add(margin).min(self.count);
        let count = ((window.len() as f64 / per_column).round() as usize).max(1);
        let columns = builder.columns_of(window.clone(), count, &channels);
        let colors: Vec<_> = channels.iter().enumerate()
            .map(|(slot, &channel)| (slot, crate::plot_trace::series_color(&self.names[channel]))).collect();
        self.gpu.set(ColumnUpload::new(&columns, &colors));
        self.shown = Some(Shown { per_column: window.len() as f64 / count as f64, window, channels, columns,
            built, at: Instant::now() });
    }

    pub fn paint(&self, painter: &Painter, transform: &PlotTransform, presentation: &Presentation) {
        let Some(shown) = &self.shown else { return };
        let placements: Vec<[f32; 2]> = shown.channels.iter().map(|&channel| {
            let placement = presentation.placement(&self.names[channel]);
            [placement.scale as f32, placement.offset as f32]
        }).collect();
        self.gpu.paint(painter, transform, &placements);
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
            *seen < built && (built >= 1. || at.elapsed() >= REFRESH));
        if stale {
            self.overview = Some((built, Instant::now(), builder.columns(0..self.count, OVERVIEW_COLUMNS)));
        }
        self.overview.as_ref().map(|(built, _, columns)| (*built, self.names.as_slice(), columns))
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
        summary.prepare(0..count, 500, |name| name != "records / b");
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
        summary.prepare(count / 10..count / 10 + count / 4, 500, |name| name != "records / b");
        let at_zoomed = summary.shown.as_ref().unwrap().at;
        assert_ne!(at, at_zoomed, "zooming in refetches");
        summary.prepare(count / 10 + 10..count / 10 + count / 4 + 10, 500, |name| name != "records / b");
        assert_eq!(summary.shown.as_ref().unwrap().at, at_zoomed, "a small pan reuses the upload");
    }

    #[test]
    fn a_click_on_a_column_picks_the_sample_nearest_the_pointer() {
        let count = BIN * 1000;
        let (mut summary, store) = built(count);
        let presentation = Presentation::default();
        summary.prepare(0..count, 1000, |_| true);
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
