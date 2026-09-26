//! Cached screen-space rendering for many ordinary colored decoded traces.
//! Data and hit testing stay in the plot model; this replaces only painted traces.
use crate::plot_trace::series_color;
use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use egui_plot::{PlotImage, PlotPoint, PlotUi};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

struct PendingRaster {
    key: u64,
    receiver: Receiver<ColorImage>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct PlotRaster {
    texture: Option<TextureHandle>,
    key: Option<u64>,
    texture_bounds: Option<[f64; 4]>,
    texture_generation: Option<u64>,
    pending: Option<PendingRaster>,
    epoch: u64,
}

impl PlotRaster {
    pub fn invalidate(&mut self) {
        if let Some(pending) = &self.pending { pending.cancel.store(true, Ordering::Relaxed); }
        self.key = None;
        self.texture = None;
        self.texture_bounds = None;
        self.texture_generation = None;
        self.epoch = self.epoch.wrapping_add(1);
    }

    pub fn show(&mut self, ctx: &Context, plot: &mut PlotUi<'_>,
        series: &BTreeMap<String, Vec<PlotPoint>>, hidden: &HashSet<String>,
        data_generation: u64) {
        let bounds = plot.plot_bounds();
        let frame = plot.response().rect;
        let ppp = ctx.pixels_per_point();
        let requested_width = (frame.width() * ppp).ceil().max(1.) as usize;
        let requested_height = (frame.height() * ppp).ceil().max(1.) as usize;
        // Bound the texture and the per-worker coverage mask on large displays.
        let scale = (2_000_000_f64 / requested_width.saturating_mul(requested_height).max(1) as f64)
            .sqrt().min(1.);
        let width = (requested_width as f64 * scale).round().max(1.) as usize;
        let height = (requested_height as f64 * scale).round().max(1.) as usize;
        if width == 0 || height == 0
            || !bounds.min().iter().chain(bounds.max().iter()).all(|v| v.is_finite())
            || bounds.width() <= 0. || bounds.height() <= 0. { return; }
        if self.texture_generation.is_some_and(|generation| generation != data_generation) {
            self.invalidate();
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.epoch.hash(&mut hasher);
        data_generation.hash(&mut hasher);
        width.hash(&mut hasher);
        height.hash(&mut hasher);
        ppp.to_bits().hash(&mut hasher);
        for value in bounds.min().into_iter().chain(bounds.max()) { value.to_bits().hash(&mut hasher); }
        let key = hasher.finish();
        let requested_bounds = [bounds.min()[0], bounds.min()[1], bounds.max()[0], bounds.max()[1]];
        if let Some(pending) = &self.pending {
            if pending.key != key { pending.cancel.store(true, Ordering::Relaxed); }
        }
        if let Some(pending) = self.pending.take() {
            match pending.receiver.try_recv() {
                Ok(image) if pending.key == key => {
                    if let Some(texture) = &mut self.texture {
                        texture.set(image, TextureOptions::NEAREST);
                    } else {
                        self.texture = Some(ctx.load_texture("decoded-plot-traces", image, TextureOptions::NEAREST));
                    }
                    self.key = Some(key);
                    self.texture_bounds = Some(requested_bounds);
                    self.texture_generation = Some(data_generation);
                }
                Ok(_) | Err(TryRecvError::Disconnected) => {}
                Err(TryRecvError::Empty) => self.pending = Some(pending),
            }
        }
        if self.key != Some(key) && self.pending.is_none() {
            let visible: Vec<_> = series.iter().filter(|(name, _)| !hidden.contains(*name))
                .map(|(name, points)| (name.clone(), points.clone())).collect();
            let (sender, receiver) = mpsc::sync_channel(1);
            let repaint = ctx.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = cancel.clone();
            std::thread::spawn(move || {
                let borrowed: Vec<_> = visible.iter()
                    .map(|(name, points)| (name.as_str(), points.as_slice())).collect();
                if let Some(image) = rasterize_with_cancel(&borrowed, width, height,
                    requested_bounds, Some(&worker_cancel)) {
                    let _ = sender.send(image);
                }
                repaint.request_repaint();
            });
            self.pending = Some(PendingRaster { key, receiver, cancel });
        }
        if let (Some(texture), Some(image_bounds)) = (&self.texture, self.texture_bounds) {
            let center = PlotPoint::new((image_bounds[0] + image_bounds[2]) * 0.5,
                (image_bounds[1] + image_bounds[3]) * 0.5);
            plot.image(PlotImage::new("Rasterized traces", texture.id(), center,
                egui::vec2((image_bounds[2] - image_bounds[0]) as f32,
                    (image_bounds[3] - image_bounds[1]) as f32)).allow_hover(false));
        }
    }
}

#[cfg(test)]
fn rasterize(series: &[(&str, &[PlotPoint])], width: usize, height: usize, bounds: [f64; 4]) -> ColorImage {
    rasterize_with_cancel(series, width, height, bounds, None).unwrap()
}

fn rasterize_with_cancel(series: &[(&str, &[PlotPoint])], width: usize, height: usize,
    bounds: [f64; 4], cancel: Option<&AtomicBool>) -> Option<ColorImage> {
    let mut pixels = vec![[0u8; 4]; width * height];
    // Work on a few series at a time so parallel coverage masks stay bounded.
    // Each series contributes at most once per pixel; dense oscillations cannot
    // brighten themselves into a solid min/max rectangle.
    for batch in series.chunks(8) {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) { return None; }
        let prepared: Vec<_> = batch.par_iter().map(|(name, points)| {
            let mut covered = vec![false; width * height];
            let mut touched = Vec::new();
            let mut mark = |x: i32, y: i32| {
                let index = y as usize * width + x as usize;
                if !covered[index] {
                    covered[index] = true;
                    touched.push(index);
                }
            };
            for pair in points.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                if !a.x.is_finite() || !a.y.is_finite() || !b.x.is_finite() || !b.y.is_finite()
                    || b.x < a.x { continue; }
                let x0 = (a.x - bounds[0]) / (bounds[2] - bounds[0]) * (width - 1) as f64;
                let y0 = (bounds[3] - a.y) / (bounds[3] - bounds[1]) * (height - 1) as f64;
                let x1 = (b.x - bounds[0]) / (bounds[2] - bounds[0]) * (width - 1) as f64;
                let y1 = (bounds[3] - b.y) / (bounds[3] - bounds[1]) * (height - 1) as f64;
                if let Some(((x0, y0), (x1, y1))) = clip_line(x0, y0, x1, y1, width, height) {
                    raster_line(x0, y0, x1, y1, &mut mark);
                }
            }
            if points.len() == 1 {
                let point = points[0];
                if point.x.is_finite() && point.y.is_finite() {
                    let x = (point.x - bounds[0]) / (bounds[2] - bounds[0]) * (width - 1) as f64;
                    let y = (bounds[3] - point.y) / (bounds[3] - bounds[1]) * (height - 1) as f64;
                    if x >= 0. && x < width as f64 && y >= 0. && y < height as f64 {
                        mark(x.round() as i32, y.round() as i32);
                    }
                }
            }
            let [r, g, b, _] = series_color(name).to_array();
            (touched, [r, g, b])
        }).collect();
        for (touched, color) in prepared {
            for index in touched {
                blend(&mut pixels[index], color);
            }
        }
    }
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) { return None; }
    Some(ColorImage::new([width, height], pixels.into_iter().map(|p|
        Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3])).collect()))
}

fn clip_line(x0: f64, y0: f64, x1: f64, y1: f64, width: usize, height: usize)
    -> Option<((i32, i32), (i32, i32))> {
    if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) { return None; }
    let dx = x1 - x0;
    let dy = y1 - y0;
    if !dx.is_finite() || !dy.is_finite() { return None; }
    let mut first: f64 = 0.;
    let mut last: f64 = 1.;
    for (p, q) in [(-dx, x0), (dx, width as f64 - 1. - x0),
        (-dy, y0), (dy, height as f64 - 1. - y0)] {
        if p == 0. {
            if q < 0. { return None; }
        } else {
            let t = q / p;
            if p < 0. { first = first.max(t); } else { last = last.min(t); }
            if first > last { return None; }
        }
    }
    let endpoint = |t: f64| {
        ((x0 + t * dx).round().clamp(0., width as f64 - 1.) as i32,
         (y0 + t * dy).round().clamp(0., height as f64 - 1.) as i32)
    };
    Some((endpoint(first), endpoint(last)))
}

fn raster_line(mut x0: i32, mut y0: i32, x1: i32, y1: i32,
    mark: &mut impl FnMut(i32, i32)) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        mark(x0, y0);
        if x0 == x1 && y0 == y1 { break; }
        let doubled = 2 * error;
        if doubled >= dy { error += dy; x0 += sx; }
        if doubled <= dx { error += dx; y0 += sy; }
    }
}

fn blend(pixel: &mut [u8; 4], color: [u8; 3]) {
    const ALPHA: u32 = 180;
    let old_alpha = u32::from(pixel[3]);
    let remaining = 255 - ALPHA;
    let combined = ALPHA + old_alpha * remaining / 255;
    for channel in 0..3 {
        let source = u32::from(color[channel]) * ALPHA;
        let old = u32::from(pixel[channel]) * old_alpha * remaining / 255;
        pixel[channel] = ((source + old) / combined).min(255) as u8;
    }
    pixel[3] = combined as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egui_compat::ContextRun as _;
    #[test]
    fn raster_keeps_a_single_sample_spike() {
        let points = [PlotPoint::new(0., 0.), PlotPoint::new(1., 10.), PlotPoint::new(2., 0.)];
        let image = rasterize(&[("first", &points)], 2, 11, [0., 0., 3., 10.]);
        assert!(image.pixels.iter().any(|pixel| pixel != &Color32::TRANSPARENT));
        assert_ne!(image.pixels[0], Color32::TRANSPARENT);
    }
    #[test]
    fn raster_connects_samples_across_empty_pixel_columns() {
        let points = [PlotPoint::new(0., 5.), PlotPoint::new(10., 5.)];
        let image = rasterize(&[("first", &points)], 11, 11, [0., 0., 10., 10.]);
        for x in 0..11 {
            assert_ne!(image.pixels[5 * 11 + x], Color32::TRANSPARENT);
        }
    }
    #[test]
    fn raster_preserves_distinct_series_colors() {
        let upper = [PlotPoint::new(0., 8.), PlotPoint::new(10., 8.)];
        let lower = [PlotPoint::new(0., 2.), PlotPoint::new(10., 2.)];
        let image = rasterize(&[("upper", &upper), ("lower", &lower)], 11, 11, [0., 0., 10., 10.]);
        assert_ne!(image.pixels[2 * 11 + 5], image.pixels[8 * 11 + 5]);
    }
    #[test]
    fn a_spike_near_one_column_does_not_fill_the_following_columns() {
        let points = [PlotPoint::new(0., 0.), PlotPoint::new(0.1, 10.),
            PlotPoint::new(0.2, 0.), PlotPoint::new(10., 0.)];
        let image = rasterize(&[("trace", &points)], 11, 11, [0., 0., 10., 10.]);
        assert_ne!(image.pixels[0], Color32::TRANSPARENT);
        assert_ne!(image.pixels[10 * 11 + 5], Color32::TRANSPARENT);
        assert_eq!(image.pixels[5 * 11 + 5], Color32::TRANSPARENT);
    }
    #[test]
    fn one_trace_blends_only_once_per_pixel() {
        let straight = [PlotPoint::new(0., 5.), PlotPoint::new(10., 5.)];
        let retraced = [PlotPoint::new(0., 5.), PlotPoint::new(10., 5.),
            PlotPoint::new(0., 5.), PlotPoint::new(10., 5.)];
        let once = rasterize(&[("trace", &straight)], 11, 11, [0., 0., 10., 10.]);
        let repeated = rasterize(&[("trace", &retraced)], 11, 11, [0., 0., 10., 10.]);
        assert_eq!(once.pixels, repeated.pixels);
    }
    #[test]
    fn cancelled_raster_does_not_publish_a_partial_image() {
        let points = [PlotPoint::new(0., 0.), PlotPoint::new(10., 10.)];
        let cancelled = AtomicBool::new(true);
        assert!(rasterize_with_cancel(&[("trace", &points)], 11, 11,
            [0., 0., 10., 10.], Some(&cancelled)).is_none());
    }
    #[test]
    fn raster_worker_publishes_on_a_later_ui_frame() {
        let ctx = egui::Context::default();
        let mut raster = PlotRaster::default();
        let series = BTreeMap::from([("trace".to_owned(), vec![
            PlotPoint::new(0., 0.), PlotPoint::new(10., 10.),
        ])]);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO,
                egui::vec2(400., 300.))),
            ..Default::default()
        };
        for _ in 0..100 {
            ctx.run(input.clone(), |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    egui_plot::Plot::new("raster-worker-test").show(ui, |plot| {
                        plot.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                            [0., 0.], [10., 10.]));
                        raster.show(&ctx, plot, &series, &HashSet::new(), 1);
                    });
                });
            });
            if raster.key.is_some() { break; }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(raster.texture.is_some(), "background raster did not publish");
        assert_eq!(raster.texture_bounds, Some([0., 0., 10., 10.]));
        raster.invalidate();
        assert!(raster.texture.is_none());
    }
}
