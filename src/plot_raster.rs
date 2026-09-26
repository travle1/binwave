//! Cached screen-space rendering for many ordinary colored decoded traces.
//! Data and hit testing stay in the plot model; this replaces only painted traces.
use crate::plot_trace::series_color;
use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use egui_plot::{PlotImage, PlotPoint, PlotUi};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};

#[derive(Default)]
pub struct PlotRaster {
    texture: Option<TextureHandle>,
    key: Option<u64>,
}

impl PlotRaster {
    pub fn invalidate(&mut self) { self.key = None; }

    pub fn show(&mut self, ctx: &Context, plot: &mut PlotUi<'_>,
        series: &BTreeMap<String, Vec<PlotPoint>>, hidden: &HashSet<String>,
        data_generation: u64) {
        let bounds = plot.plot_bounds();
        let frame = plot.response().rect;
        let ppp = ctx.pixels_per_point();
        let requested_width = (frame.width() * ppp).ceil().max(1.) as usize;
        let requested_height = (frame.height() * ppp).ceil().max(1.) as usize;
        // Rayon uses a private histogram per worker. Bound that temporary RAM
        // and let the texture scale on unusually large/high-DPI displays.
        let scale = (2_000_000_f64 / requested_width.saturating_mul(requested_height).max(1) as f64)
            .sqrt().min(1.);
        let width = (requested_width as f64 * scale).round().max(1.) as usize;
        let height = (requested_height as f64 * scale).round().max(1.) as usize;
        if width == 0 || height == 0
            || !bounds.min().iter().chain(bounds.max().iter()).all(|v| v.is_finite())
            || bounds.width() <= 0. || bounds.height() <= 0. { return; }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        data_generation.hash(&mut hasher);
        width.hash(&mut hasher);
        height.hash(&mut hasher);
        ppp.to_bits().hash(&mut hasher);
        for value in bounds.min().into_iter().chain(bounds.max()) { value.to_bits().hash(&mut hasher); }
        let key = hasher.finish();
        if self.key != Some(key) || self.texture.is_none() {
            let visible: Vec<_> = series.iter().filter(|(name, _)| !hidden.contains(*name))
                .map(|(name, points)| (name.as_str(), points.as_slice())).collect();
            let image = rasterize(&visible, width, height,
                [bounds.min()[0], bounds.min()[1], bounds.max()[0], bounds.max()[1]]);
            if let Some(texture) = &mut self.texture {
                texture.set(image, TextureOptions::NEAREST);
            } else {
                self.texture = Some(ctx.load_texture("decoded-plot-traces", image, TextureOptions::NEAREST));
            }
            self.key = Some(key);
        }
        if let Some(texture) = &self.texture {
            let center = PlotPoint::new((bounds.min()[0] + bounds.max()[0]) * 0.5,
                (bounds.min()[1] + bounds.max()[1]) * 0.5);
            plot.image(PlotImage::new("Rasterized traces", texture.id(), center,
                egui::vec2(bounds.width() as f32, bounds.height() as f32)).allow_hover(false));
        }
    }
}

fn rasterize(series: &[(&str, &[PlotPoint])], width: usize, height: usize, bounds: [f64; 4]) -> ColorImage {
    // Build each trace's per-pixel extrema independently, then composite in
    // stable series order so colors and crossings remain deterministic.
    let prepared: Vec<_> = series.par_iter().map(|(name, points)| {
            let mut columns = vec![(i32::MAX, i32::MIN); width];
            for point in *points {
                if !point.x.is_finite() || !point.y.is_finite()
                    || point.x < bounds[0] || point.x > bounds[2] { continue; }
                let x = (((point.x - bounds[0]) / (bounds[2] - bounds[0])) * width as f64)
                    .floor().clamp(0., (width - 1) as f64) as usize;
                let y = (((bounds[3] - point.y) / (bounds[3] - bounds[1])) * height as f64)
                    .floor().clamp(0., (height - 1) as f64) as i32;
                columns[x].0 = columns[x].0.min(y);
                columns[x].1 = columns[x].1.max(y);
            }
            let color = series_color(name).to_array();
            (columns, [color[0], color[1], color[2]])
        }).collect();
    let mut pixels = vec![[0u8; 4]; width * height];
    for (columns, color) in prepared {
        let mut previous: Option<(usize, i32, i32)> = None;
        for (x, &(top, bottom)) in columns.iter().enumerate() {
            if top > bottom { continue; }
            let (start, old_top, old_bottom) = previous.unwrap_or((x, top, bottom));
            for column in start..=x {
                if previous.is_some() && column == start { continue; }
                let fraction = if x == start { 1. } else {
                    (column - start) as f64 / (x - start) as f64
                };
                let line_top = (old_top as f64 + (top - old_top) as f64 * fraction).round() as i32;
                let line_bottom = (old_bottom as f64 + (bottom - old_bottom) as f64 * fraction).round() as i32;
                for y in line_top.min(line_bottom)..=line_top.max(line_bottom) {
                    blend(&mut pixels[y as usize * width + column], color);
                }
            }
            previous = Some((x, top, bottom));
        }
    }
    ColorImage::new([width, height], pixels.into_iter().map(|p|
        Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3])).collect())
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
}
