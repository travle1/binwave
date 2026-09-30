//! The whole-file overview under the Signal plot: a min/max envelope of one
//! series across the whole collection, the plot's viewport, and issue ticks.
//! The envelope is computed once in the background and cached in memory and on
//! disk; each frame only paints cached buckets, so it stays smooth on large files.
use crate::byte_store::ByteView;
use crate::structure::Node;
use crate::structure_plot::{PlotData, PlotReader};
use crate::theme::DARK;
use egui::{pos2, vec2, Align2, CornerRadius, FontId, Rect, Sense, Stroke, Ui};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

/// Envelope resolution across the whole collection.
const BUCKETS: usize = 2000;
/// Series scanned together in one pass; the picker offers these.
const MAX_SERIES: usize = 16;
/// The whole-file scan runs in this many chunks, so a pause for the plot loses
/// at most the chunk in progress instead of starting over.
const CHUNKS: usize = 32;
/// Collections this small are scanned in one go.
const SINGLE_CHUNK: usize = 65_536;

/// What the strip asks the plot to show.
#[derive(Debug, PartialEq)]
pub enum Action {
    View(Range<usize>),
    Fit,
}

/// Min/max per bucket for each series, over `total` samples.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Envelope {
    pub total: usize,
    pub bands: BTreeMap<String, Vec<Option<[f64; 2]>>>,
    pub scanned: usize,
    pub complete: bool,
}

impl Envelope {
    /// Bin whatever the plot reader produced (min/max bands, or exact points
    /// for small collections) into `BUCKETS` buckets.
    pub fn from_data(data: &PlotData, total: usize) -> Self {
        let buckets = BUCKETS.min(total.max(1));
        let bucket = |index: usize| (index * buckets / total.max(1)).min(buckets - 1);
        let mut bands: BTreeMap<String, Vec<Option<[f64; 2]>>> = BTreeMap::new();
        let mut add = |name: &str, index: usize, low: f64, high: f64| {
            if !low.is_finite() || !high.is_finite() {
                return;
            }
            let slot = &mut bands.entry(name.to_owned()).or_insert_with(|| vec![None; buckets])[bucket(index)];
            *slot = Some(merge(*slot, [low, high]));
        };
        if data.envelopes.values().any(|bands| !bands.is_empty()) {
            for (name, envelopes) in &data.envelopes {
                for envelope in envelopes {
                    add(name, envelope.range.start, envelope.min.value, envelope.max.value);
                }
            }
        } else {
            for (name, points) in &data.series {
                for point in points {
                    add(name, point.index, point.value, point.value);
                }
            }
        }
        Self { total, bands, scanned: data.scanned, complete: data.complete }
    }

    /// Widen each bucket by another scan's bands of the same collection.
    fn merge(&mut self, other: &Envelope) {
        let buckets = BUCKETS.min(self.total.max(1));
        for (name, bands) in &other.bands {
            let mine = self.bands.entry(name.clone()).or_insert_with(|| vec![None; buckets]);
            for (slot, band) in mine.iter_mut().zip(bands) {
                if let Some(band) = band {
                    *slot = Some(merge(*slot, *band));
                }
            }
        }
    }

    fn range(&self, series: &str) -> Option<[f64; 2]> {
        self.bands.get(series)?.iter().flatten().fold(None, |range, band| Some(merge(range, *band)))
    }

    /// A series whose every value is the same, such as all zeros.
    fn flat(&self, series: &str) -> bool {
        self.range(series).is_none_or(|[min, max]| min == max)
    }

    /// A series that swings through its whole range within every bucket, such as
    /// a fast wave: its envelope is one solid block with no shape to show.
    fn uniform(&self, series: &str) -> bool {
        let Some(range) = self.range(series) else { return false };
        self.bands.get(series).is_some_and(|bands| bands.iter().flatten().all(|band| *band == range))
    }

    /// Why a series makes a poor overview, for the picker.
    fn shapeless(&self, series: &str) -> Option<&'static str> {
        if self.flat(series) { Some("flat") } else if self.uniform(series) { Some("no shape") } else { None }
    }
}

enum Drag {
    /// Moving the marker; `grab` is the pointer's sample offset from its start.
    Pan { grab: f64 },
    Start,
    End,
}

#[derive(Clone, PartialEq)]
struct Key {
    revision: u64,
    path: String,
    identity: u64,
    fields: Vec<String>,
}

#[derive(Default)]
pub struct OverviewStrip {
    reader: PlotReader,
    key: Option<Key>,
    envelope: Option<Envelope>,
    generation: u64,
    /// The next chunk to scan, and the one being read now.
    chunk: usize,
    reading: Option<Range<usize>>,
    /// The series chosen per collection path.
    chosen: HashMap<String, String>,
    /// The drag in progress and the view when it started.
    drag: Option<(Drag, Range<usize>)>,
    /// Where a drag has moved the box; the plot follows on release.
    preview: Option<Range<usize>>,
    /// The view last asked of the plot, until the plot shows it. Wheel and click
    /// steps build on it instead of the plot's lagging view.
    pending: Option<(Range<usize>, std::time::Instant)>,
}

/// Series names `fields` would scan: visible ones first, at most `MAX_SERIES`.
pub fn scanned_fields(catalog: &[String], hidden: impl Fn(&str) -> bool) -> Vec<String> {
    let mut fields: Vec<String> = catalog.iter().filter(|field| !hidden(field)).cloned().collect();
    fields.extend(catalog.iter().filter(|field| hidden(field)).cloned());
    if catalog.len() <= MAX_SERIES {
        // Scan everything so hiding a series never restarts the scan.
        fields.sort_by_key(|field| catalog.iter().position(|name| name == field));
    }
    fields.truncate(MAX_SERIES);
    fields
}

impl OverviewStrip {
    /// Continue the whole-collection scan one chunk at a time. It pauses while the
    /// plot loads, so it never slows the view being looked at, and resumes where
    /// it left off. `group` is the plot's grouped scan plan, when it reads the
    /// collection as a group of channels; the strip then scans them the same way.
    #[allow(clippy::too_many_arguments)]
    pub fn update(&mut self, ctx: &egui::Context, source: &ByteView, node: &Node, count: usize,
        fields: Vec<String>, group: Option<(Node, crate::structure::NumericPlan)>, main_loading: bool) {
        let scanned = group.as_ref().map(|(group_node, _)| group_node).unwrap_or(node);
        let key = Key {
            revision: source.revision,
            path: scanned.path.clone(),
            identity: crate::structure_plot::identity(scanned),
            fields: match &group {
                Some((_, plan)) => plan.fields.iter().map(|field| field.name.clone()).collect(),
                None => fields,
            },
        };
        if self.key.as_ref() != Some(&key) {
            // A different collection, series set or bytes: start over.
            self.reader.stop();
            self.reading = None;
            self.chunk = 0;
            self.envelope = Some(load_cached(source, &key).filter(|cached| cached.total == count)
                .unwrap_or_else(|| Envelope { total: count, ..Default::default() }));
            self.key = Some(key.clone());
        }
        let chunks = if count <= SINGLE_CHUNK { 1 } else { CHUNKS };
        let complete = self.envelope.as_ref().is_some_and(|envelope| envelope.complete);
        if main_loading && self.reading.is_some() {
            // Only the chunk in progress is read again later.
            self.reader.stop();
            self.reading = None;
        }
        if !complete && !main_loading && self.reading.is_none() && self.chunk < chunks && count > 0 {
            let range = self.chunk * count / chunks..(self.chunk + 1) * count / chunks;
            let width = (BUCKETS / chunks).max(1);
            match group {
                Some((group_node, plan)) => self.reader.request_group(ctx, source.clone(), group_node, plan,
                    range.clone(), width, true),
                None => {
                    let fields = (!key.fields.is_empty()).then(|| key.fields.clone());
                    self.reader.request_filtered(ctx, source.clone(), node.clone(), range.clone(), width, true, fields);
                }
            }
            self.reading = Some(range);
        }
        self.reader.tick(ctx);
        if self.reader.data_generation() != self.generation {
            self.generation = self.reader.data_generation();
            if let (Some(data), Some(reading), Some(envelope)) = (self.reader.data(), self.reading.clone(), &mut self.envelope) {
                // Min/max only grow, so partial results merge safely.
                envelope.merge(&Envelope::from_data(data, count));
                if data.complete && data.range.start <= reading.start && data.range.end >= reading.end {
                    self.chunk += 1;
                    self.reading = None;
                    envelope.scanned = self.chunk * count / chunks;
                    if self.chunk == chunks {
                        envelope.complete = true;
                        store_cached(source, &key, envelope);
                    }
                }
            }
        }
        if self.reading.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    #[cfg(test)]
    pub(crate) fn envelope(&self) -> Option<&Envelope> {
        self.envelope.as_ref()
    }

    /// The series shown: the one chosen for this collection, else the first
    /// that isn't flat (all zeros or constant), else the first.
    fn series(&self, path: &str) -> Option<String> {
        let envelope = self.envelope.as_ref()?;
        let fields = self.key.as_ref().map(|key| key.fields.as_slice()).unwrap_or_default();
        let mut names: Vec<&String> = fields.iter().filter(|field| envelope.bands.contains_key(*field)).collect();
        if names.is_empty() {
            names = envelope.bands.keys().collect();
        }
        if let Some(chosen) = self.chosen.get(path).filter(|chosen| envelope.bands.contains_key(*chosen)) {
            return Some(chosen.clone());
        }
        names.iter().find(|name| envelope.shapeless(name).is_none()).or(names.first()).map(|name| (*name).clone())
    }

    /// Paint the strip under the plot and handle its interactions. `plot_x` is the
    /// plot area's horizontal extent; `issues` are sample indices with a reason.
    #[allow(clippy::too_many_arguments)]
    pub fn show(&mut self, ui: &mut Ui, plot_x: egui::Rangef, node: &Node, count: usize,
        view: Range<usize>, issues: &[(usize, String)]) -> Option<Action> {
        let t = &DARK;
        let plot_view = view.clone();
        if self.pending.as_ref().is_some_and(|(pending, when)| *pending == plot_view
            || when.elapsed() > std::time::Duration::from_millis(500)) {
            self.pending = None;
        }
        let view = self.preview.clone()
            .or_else(|| self.pending.as_ref().map(|(pending, _)| pending.clone()))
            .unwrap_or(view);
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), t.size.overview_height), Sense::hover());
        let rect = Rect::from_x_y_ranges(plot_x, row.y_range());
        if count == 0 || rect.width() <= 0. {
            return None;
        }
        let painter = ui.painter_at(rect.expand2(vec2(rect.left() - row.left(), 0.)));
        painter.text(pos2(rect.left() - t.spacing.card_padding, rect.center().y), Align2::RIGHT_CENTER, "File",
            FontId::proportional(t.font.label), t.color.text.muted);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(t.radius.well), t.color.surface.window);
        let x_of = |sample: f64| rect.left() + (sample / count as f64) as f32 * rect.width();
        let sample_of = |x: f32| ((x - rect.left()) / rect.width()).clamp(0., 1.) as f64 * count as f64;
        let series = self.series(&node.path);
        // The envelope: one column per point, spanning its buckets' min and max.
        if let (Some(envelope), Some(series)) = (&self.envelope, &series) {
            if let (Some(bands), Some([low, high])) = (envelope.bands.get(series), envelope.range(series)) {
                let inset = t.spacing.chip_padding_y;
                let height = rect.height() - 2. * inset;
                let y_of = |value: f64| rect.bottom() - inset
                    - if high > low { ((value - low) / (high - low)) as f32 * height } else { height / 2. };
                let columns = rect.width().ceil() as usize;
                for column in 0..columns {
                    let first = column * bands.len() / columns;
                    let last = ((column + 1) * bands.len() / columns).max(first + 1).min(bands.len());
                    let Some([min, max]) = bands[first..last].iter().flatten()
                        .fold(None, |range, band| Some(merge(range, *band))) else { continue };
                    let x = rect.left() + column as f32;
                    // At least one point tall so flat stretches still show.
                    let (top, bottom) = (y_of(max), y_of(min).max(y_of(max) + 1.));
                    painter.rect_filled(Rect::from_min_max(pos2(x, top), pos2(x + 1., bottom)), 0., t.color.overview_band);
                }
            }
            if !envelope.complete && envelope.total > 0 {
                let done = envelope.scanned as f32 / envelope.total as f32;
                painter.line_segment([pos2(rect.left(), rect.bottom() - 1.), pos2(rect.left() + done * rect.width(), rect.bottom() - 1.)],
                    Stroke::new(t.size.frame_border, t.color.accent.base.gamma_multiply(t.color.accent.selection_alpha)));
            }
        }
        for (sample, _) in issues {
            let x = x_of(*sample as f64);
            painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(2. * t.size.frame_border, t.color.status.warning));
        }
        // Viewport marker, never narrower than `overview_marker_min`.
        let marker_of = |view: &Range<usize>| {
            let (mut left, mut right) = (x_of(view.start as f64), x_of(view.end as f64));
            if right - left < t.size.overview_marker_min {
                let centre = (left + right) / 2.;
                left = (centre - t.size.overview_marker_min / 2.).max(rect.left());
                right = (left + t.size.overview_marker_min).min(rect.right());
            }
            Rect::from_x_y_ranges(left..=right, rect.y_range())
        };
        let marker = marker_of(&view);
        let font = FontId::proportional(t.font.label);
        let samples = crate::format::plural(count as u64, "sample", "samples");
        let label = match &series {
            Some(series) => format!("{series} · {samples}"),
            None => samples,
        };
        let label = painter.text(pos2(rect.right() - t.spacing.card_padding, rect.center().y), Align2::RIGHT_CENTER,
            label, font.clone(), t.color.text.muted);

        let id = ui.id().with("overview_strip");
        let response = ui.interact(rect, id, Sense::click_and_drag());
        let picker = ui.interact(label.expand(t.spacing.chip_padding_y), id.with("series"), Sense::click())
            .on_hover_text("Click to choose the series shown here");
        let mut action = None;
        let options: Vec<String> = self.envelope.as_ref().map(|envelope| envelope.bands.keys().cloned().collect())
            .unwrap_or_default();
        egui::Popup::menu(&picker).show(|ui| egui::ScrollArea::vertical()
            .max_height(DARK.size.overview_picker_height).show(ui, |ui| {
            for option in &options {
                let text = match self.envelope.as_ref().and_then(|envelope| envelope.shapeless(option)) {
                    Some(why) => format!("{option} ({why})"),
                    None => option.clone(),
                };
                if ui.selectable_label(series.as_ref() == Some(option), text).clicked() {
                    self.chosen.insert(node.path.clone(), option.clone());
                    ui.close();
                }
            }
        }));
        let span = view.len().max(1) as f64;
        let place = |start: f64, span: f64| {
            let span = span.clamp(1., count as f64);
            let start = start.clamp(0., count as f64 - span);
            start.round() as usize..(start + span).round().min(count as f64) as usize
        };
        let near = |x: f32, edge: f32| (x - edge).abs() <= t.size.overview_edge_grab;
        if let Some(pointer) = response.hover_pos() {
            if marker.width() > 2. * t.size.overview_edge_grab && (near(pointer.x, marker.left()) || near(pointer.x, marker.right())) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            } else if marker.contains(pointer) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
            let sample = (sample_of(pointer.x) as usize).min(count - 1);
            let offset = crate::structure_selection::sample_bytes(node, sample..sample + 1)
                .map(|bytes| format!(" · {}", crate::format::offset(bytes.start)))
                .unwrap_or_default();
            let mut text = format!("Sample {}{offset}", crate::format::count(sample));
            if let (Some(envelope), Some(series)) = (&self.envelope, &series) {
                if let Some(Some([min, max])) = envelope.bands.get(series).and_then(|bands|
                    bands.get((sample * bands.len() / count).min(bands.len() - 1))) {
                    text.push_str(&format!("\n{series}: {} to {}", crate::format::short_float(*min), crate::format::short_float(*max)));
                }
            }
            for (issue, reason) in issues {
                if (x_of(*issue as f64) - pointer.x).abs() <= t.size.overview_edge_grab {
                    text.push_str(&format!("\n{reason}"));
                }
            }
            response.clone().on_hover_text_at_pointer(text);
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll != 0. {
                let factor = (-(scroll as f64) * f64::from(t.size.overview_wheel_zoom)).exp();
                let anchor = sample_of(pointer.x);
                let ratio = (anchor - view.start as f64) / span;
                let next = span * factor;
                action = Some(Action::View(place(anchor - ratio * next, next)));
            }
        }
        if response.drag_started() {
            if let Some(origin) = ui.input(|input| input.pointer.press_origin()) {
                let kind = if marker.width() > 2. * t.size.overview_edge_grab && near(origin.x, marker.left()) {
                    Drag::Start
                } else if marker.width() > 2. * t.size.overview_edge_grab && near(origin.x, marker.right()) {
                    Drag::End
                } else if marker.contains(origin) {
                    Drag::Pan { grab: sample_of(origin.x) - view.start as f64 }
                } else {
                    // Grabbing outside the marker centres it there first.
                    Drag::Pan { grab: span / 2. }
                };
                self.drag = Some((kind, view.clone()));
            }
        }
        // Measured from the view when the drag started, so the box follows the
        // pointer exactly while the plot catches up.
        if let (Some((drag, start)), Some(pointer)) = (&self.drag, response.interact_pointer_pos().filter(|_| response.dragged())) {
            let at = sample_of(pointer.x);
            let (first, end) = (start.start as f64, start.end as f64);
            self.preview = Some(match drag {
                Drag::Pan { grab } => place(at - grab, (end - first).max(1.)),
                Drag::Start => place(at.min(end - 1.), end - at.min(end - 1.)),
                Drag::End => place(first, at.max(first + 1.) - first),
            });
        }
        if response.drag_stopped() {
            self.drag = None;
            if let Some(preview) = self.preview.take() {
                action = Some(Action::View(preview));
            }
        }
        if response.double_clicked() {
            action = Some(Action::Fit);
        } else if response.clicked() {
            if let Some(pointer) = response.interact_pointer_pos() {
                action = Some(Action::View(place(sample_of(pointer.x) - span / 2., span)));
            }
        }
        if let Some(Action::View(next)) = &action {
            self.pending = Some((next.clone(), std::time::Instant::now()));
        } else if action == Some(Action::Fit) {
            self.pending = Some((0..count, std::time::Instant::now()));
        }
        let shown = self.preview.clone().or_else(|| self.pending.as_ref().map(|(pending, _)| pending.clone()))
            .unwrap_or(plot_view);
        let marker = marker_of(&shown);
        painter.rect(marker, CornerRadius::same(t.radius.marker), t.color.accent.base.gamma_multiply(t.color.accent.selection_alpha),
            Stroke::new(t.size.frame_border, t.color.accent.base), egui::StrokeKind::Inside);
        painter.text(pos2(rect.left() + t.spacing.card_padding, rect.center().y), Align2::LEFT_CENTER,
            format!("Viewing {} – {}", crate::format::count(shown.start), crate::format::count(shown.end.saturating_sub(1))),
            font, t.color.text.secondary);
        if self.preview.is_some() || self.pending.is_some() {
            ui.ctx().request_repaint();
        }
        action
    }
}

/// The smallest min and largest max of two bands.
fn merge(range: Option<[f64; 2]>, [low, high]: [f64; 2]) -> [f64; 2] {
    match range {
        Some([min, max]) => [min.min(low), max.max(high)],
        None => [low, high],
    }
}

fn cache_path(source: &ByteView, key: &Key) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let stamp = source.plot_cache_key(&("overview-v1", &key.path, key.identity, &key.fields))?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    stamp.hash(&mut hash);
    Some(crate::cache::root().join("overview-v1").join(format!("{:016x}.json", hash.finish())))
}

/// A finished envelope saved for this file, decoder and series. Unsaved edits
/// never read or write the cache.
fn load_cached(source: &ByteView, key: &Key) -> Option<Envelope> {
    let text = std::fs::read_to_string(cache_path(source, key)?).ok()?;
    serde_json::from_str::<Envelope>(&text).ok().filter(|envelope| envelope.complete)
}

fn store_cached(source: &ByteView, key: &Key, envelope: &Envelope) {
    let Some(path) = cache_path(source, key) else { return };
    let Ok(text) = serde_json::to_string(envelope) else { return };
    std::thread::spawn(move || {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, text);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structure_plot::{PlotEnvelope, PlotQuality, PlotSample};

    fn sample(index: usize, value: f64) -> PlotSample {
        PlotSample { index, value, bytes: index..index + 1, path: "x".into() }
    }

    #[test]
    fn envelopes_bin_bands_and_flat_series_are_skipped_by_default() {
        let bands = |values: &[(usize, f64, f64)]| values.iter().map(|&(index, low, high)| PlotEnvelope {
            range: index..index + 10, min: sample(index, low), max: sample(index + 1, high) }).collect::<Vec<_>>();
        let data = PlotData {
            range: 0..40_000,
            series: BTreeMap::new(),
            envelopes: BTreeMap::from([
                ("zeros".into(), bands(&[(0, 0., 0.), (20_000, 0., 0.)])),
                ("wave".into(), bands(&[(0, -1., 1.), (39_990, 2., 3.)])),
            ]),
            bounds: BTreeMap::new(),
            quality: PlotQuality::MinMax,
            scanned: 40_000,
            total: 40_000,
            complete: true,
        };
        let envelope = Envelope::from_data(&data, 40_000);
        assert_eq!(envelope.bands["wave"].len(), BUCKETS);
        assert_eq!(envelope.bands["wave"][0], Some([-1., 1.]));
        assert_eq!(envelope.bands["wave"][BUCKETS - 1], Some([2., 3.]));
        assert!(envelope.flat("zeros") && !envelope.flat("wave"));
        let mut fast = envelope.clone();
        fast.bands.insert("fast".into(), vec![Some([-1., 1.]); BUCKETS]);
        assert_eq!(fast.shapeless("fast"), Some("no shape"), "a full-range band in every bucket has no shape");
        assert_eq!(fast.shapeless("wave"), None);
        let mut strip = OverviewStrip { envelope: Some(envelope), ..Default::default() };
        strip.key = Some(Key { revision: 0, path: "records".into(), identity: 0, fields: vec!["zeros".into(), "wave".into()] });
        assert_eq!(strip.series("records").as_deref(), Some("wave"), "an all-zero series isn't the default");
        strip.chosen.insert("records".into(), "zeros".into());
        assert_eq!(strip.series("records").as_deref(), Some("zeros"), "a chosen series wins");
    }

    #[test]
    fn exact_points_fill_buckets_for_small_collections() {
        let data = PlotData {
            range: 0..4, series: BTreeMap::from([("v".into(), (0..4).map(|i| sample(i, i as f64)).collect())]),
            envelopes: BTreeMap::new(), bounds: BTreeMap::new(), quality: PlotQuality::Exact,
            scanned: 2, total: 4, complete: false,
        };
        let envelope = Envelope::from_data(&data, 4);
        assert_eq!(envelope.bands["v"], [Some([0., 0.]), Some([1., 1.]), Some([2., 2.]), Some([3., 3.])]);
        assert!(!envelope.complete);
    }

    #[test]
    fn scanning_covers_every_series_when_there_are_few_so_hiding_one_does_not_rescan() {
        let catalog: Vec<String> = ["a", "b", "c"].map(String::from).to_vec();
        assert_eq!(scanned_fields(&catalog, |name| name == "b"), catalog);
        let many: Vec<String> = (0..40).map(|i| format!("s{i}")).collect();
        let fields = scanned_fields(&many, |name| name != "s30");
        assert_eq!(fields.len(), MAX_SERIES);
        assert_eq!(fields[0], "s30", "visible series come first when not all fit");
    }

    fn strip_frame(ctx: &egui::Context, strip: &mut OverviewStrip, view: Range<usize>, events: Vec<egui::Event>)
        -> Option<Action> {
        use crate::egui_compat::ContextRun as _;
        let node = Node { name: "records".into(), path: "records".into(), range: 0..1000, kind: "collection".into(),
            value: None, children: Vec::new(), lazy: None, endian: Default::default(), references: Vec::new(), preview: None };
        let mut action = None;
        let _ = ctx.run(egui::RawInput {
            events,
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1100., 100.))),
            ..Default::default()
        }, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                action = strip.show(ui, egui::Rangef::new(100., 1100.), &node, 1000, view.clone(), &[]);
            });
        });
        action
    }

    #[test]
    fn clicks_centre_drags_pan_or_resize_and_double_click_fits() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut strip = OverviewStrip::default();
        let _ = strip_frame(&ctx, &mut strip, 0..100, vec![]);
        let y = 8. + DARK.size.overview_height / 2.;
        let press = |x: f32, pressed: bool| egui::Event::PointerButton { pos: pos2(x, y), button: egui::PointerButton::Primary,
            pressed, modifiers: Default::default() };
        let moved = |x: f32| egui::Event::PointerMoved(pos2(x, y));
        // Click at sample 600 (x = 700) centres the 100-sample view there.
        let _ = strip_frame(&ctx, &mut strip, 0..100, vec![moved(700.), press(700., true)]);
        let action = strip_frame(&ctx, &mut strip, 0..100, vec![press(700., false)]);
        assert_eq!(action, Some(Action::View(550..650)));
        // The plot shows what was asked; wheel and clicks build on it meanwhile.
        assert_eq!(strip.pending.as_ref().map(|(view, _)| view.clone()), Some(550..650));
        let _ = strip_frame(&ctx, &mut strip, 550..650, vec![]);
        assert!(strip.pending.is_none(), "cleared once the plot shows it");
        // Dragging the marker from sample 590 to 790 moves the box with the
        // pointer, then pans the plot by 200 on release.
        let _ = strip_frame(&ctx, &mut strip, 550..650, vec![moved(690.), press(690., true)]);
        let _ = strip_frame(&ctx, &mut strip, 550..650, vec![moved(750.)]);
        let action = strip_frame(&ctx, &mut strip, 550..650, vec![moved(890.)]);
        assert_eq!(action, None, "the plot moves once, on release");
        assert_eq!(strip.preview, Some(750..850));
        let action = strip_frame(&ctx, &mut strip, 550..650, vec![press(890., false)]);
        assert_eq!(action, Some(Action::View(750..850)));
        let _ = strip_frame(&ctx, &mut strip, 750..850, vec![]);
        // Dragging the right edge of 750..850 (x = 950) to x = 1050 widens it to 750..950.
        let _ = strip_frame(&ctx, &mut strip, 750..850, vec![moved(950.), press(950., true)]);
        let _ = strip_frame(&ctx, &mut strip, 750..850, vec![moved(1000.)]);
        let _ = strip_frame(&ctx, &mut strip, 750..850, vec![moved(1050.)]);
        let action = strip_frame(&ctx, &mut strip, 750..850, vec![press(1050., false)]);
        assert_eq!(action, Some(Action::View(750..950)));
        // Double-click fits the whole file. A fresh context keeps the earlier
        // clicks from making this a triple-click.
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut actions = Vec::new();
        for event in [moved(900.), press(900., true), press(900., false), press(900., true), press(900., false)] {
            actions.push(strip_frame(&ctx, &mut strip, 200..500, vec![event]));
        }
        assert_eq!(actions.last(), Some(&Some(Action::Fit)), "{actions:?}");
    }

    #[test]
    fn whole_file_scan_skips_a_flat_series_and_reopening_uses_the_saved_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairs.bin");
        let count = 50_000usize;
        let bytes: Vec<u8> = (0..count).flat_map(|index| {
            let wave = (index % 100) as i16 - 50;
            [0i16.to_le_bytes(), wave.to_le_bytes()].concat()
        }).collect();
        std::fs::write(&path, bytes).unwrap();
        let yaml = "endian: little\nstructures:\n  - name: Pair\n    fields: [{name: zero, type: i16}, {name: wave, type: i16}]\n\
            actions:\n  - read: {name: samples, type: {structure: Pair}, repeat: until end}\n";
        let store = crate::byte_store::ByteStore::from_file(std::sync::Arc::new(
            crate::file_source::FileSource::open(&path).unwrap()));
        let source = store.view();
        let node = crate::structure::Program::parse(yaml).unwrap()
            .run(&source, &std::sync::atomic::AtomicBool::new(false)).unwrap().nodes.remove(0);
        let fields = vec!["zero".to_owned(), "wave".to_owned()];
        let ctx = egui::Context::default();
        let mut strip = OverviewStrip::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !strip.envelope.as_ref().is_some_and(|envelope| envelope.complete) {
            strip.update(&ctx, &source, &node, count, fields.clone(), None, false);
            assert!(std::time::Instant::now() < deadline, "the whole-file scan finishes");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let envelope = strip.envelope.clone().unwrap();
        assert_eq!(envelope.range("wave"), Some([-50., 49.]));
        assert!(envelope.flat("zero"));
        assert_eq!(strip.series(&node.path).as_deref(), Some("wave"));
        // The saved envelope is written in the background.
        let key = strip.key.clone().unwrap();
        while load_cached(&source, &key).is_none() {
            assert!(std::time::Instant::now() < deadline, "the envelope is saved");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut reopened = OverviewStrip::default();
        reopened.update(&ctx, &source, &node, count, fields.clone(), None, true);
        assert_eq!(reopened.envelope.as_ref(), Some(&envelope), "no rescan, even while the plot loads");
        let _ = std::fs::remove_file(cache_path(&source, &key).unwrap());
    }

    #[test]
    fn plot_loads_pause_the_scan_and_it_resumes_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.bin");
        let count = 200_000usize;
        let bytes: Vec<u8> = (0..count).flat_map(|index| ((index / 1000) as i16).to_le_bytes()).collect();
        std::fs::write(&path, bytes).unwrap();
        let store = crate::byte_store::ByteStore::from_file(std::sync::Arc::new(
            crate::file_source::FileSource::open(&path).unwrap()));
        let source = store.view();
        let node = crate::structure::Program::parse("actions:\n  - read: {name: level, type: i16, repeat: until end}\n")
            .unwrap().run(&source, &std::sync::atomic::AtomicBool::new(false)).unwrap().nodes.remove(0);
        let ctx = egui::Context::default();
        let mut strip = OverviewStrip::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while strip.chunk < 3 {
            strip.update(&ctx, &source, &node, count, Vec::new(), None, false);
            assert!(std::time::Instant::now() < deadline, "the first chunks finish");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let before = (strip.chunk, strip.envelope.clone().unwrap());
        // The plot starts loading: the scan pauses without losing anything.
        for _ in 0..5 {
            strip.update(&ctx, &source, &node, count, Vec::new(), None, true);
        }
        assert!(strip.reading.is_none(), "paused while the plot loads");
        assert_eq!(strip.chunk, before.0, "finished chunks are kept");
        assert!(before.1.bands.values().next().unwrap().iter().flatten().count() > 0);
        assert!(strip.envelope.as_ref().unwrap().bands.values().next().unwrap().iter().flatten().count()
            >= before.1.bands.values().next().unwrap().iter().flatten().count(), "the band never shrinks");
        while !strip.envelope.as_ref().is_some_and(|envelope| envelope.complete) {
            strip.update(&ctx, &source, &node, count, Vec::new(), None, false);
            assert!(std::time::Instant::now() < deadline, "the scan resumes and finishes");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let envelope = strip.envelope.clone().unwrap();
        assert_eq!(envelope.range(envelope.bands.keys().next().unwrap()), Some([0., 199.]));
        assert!(envelope.bands.values().next().unwrap().iter().all(Option::is_some), "every bucket filled");
        let _ = std::fs::remove_file(cache_path(&source, strip.key.as_ref().unwrap()).unwrap());
    }
}
