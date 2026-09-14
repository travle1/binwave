//! Draw resolvable traces as lines and subpixel cycles as amplitude coverage.
use egui::{Color32, Id, Rect, Shape, Ui};
use egui_plot::{Line, PlotBounds, PlotGeometry, PlotItem, PlotPoint, PlotTransform};
use std::ops::RangeInclusive;

/// Render a decoded channel consistently across platform shells. Estimated
/// bands describe observed ranges, never a continuous path across unread data.
pub fn decoded_channel(
    plot: &mut egui_plot::PlotUi,
    name: &str,
    color: Color32,
    points: Vec<[f64; 2]>,
    estimated_bands: Option<&[[f64; 4]]>,
) {
    if let Some(bands) = estimated_bands {
        for &[left, right, low, high] in bands {
            if ![left, right, low, high].iter().all(|x| x.is_finite()) || left >= right || low > high {
                continue;
            }
            // Flat polygons have no area and can tessellate to non-finite vertices.
            if low < high {
                plot.polygon(egui_plot::Polygon::new(vec![
                    [left, low], [right, low], [right, high], [left, high],
                ]).name(name).fill_color(color.gamma_multiply(0.23))
                    .stroke(egui::Stroke::new(0_f32, color)));
            }
            for (i, value) in [low, high].into_iter().enumerate() {
                if i == 1 && low == high { continue; }
                plot.line(Line::new(vec![[left, value], [right, value]])
                    .name(name).color(color).width(0.8_f32));
            }
        }
    } else {
        plot.add(AdaptiveTrace::new(points, name, color));
    }
}

const MIN_CYCLE_PIXELS: f64 = 4.0;
const MAX_COLUMNS: usize = 16_384;
const ENVELOPE_RADIUS: usize = 2;

/// Retains the line's original point geometry for legends, hover and selection.
/// Only its painted representation changes when cycles cannot be resolved.
pub struct AdaptiveTrace {
    line: Line,
}

impl AdaptiveTrace {
    pub fn new(points: Vec<[f64; 2]>, name: impl Into<String>, color: Color32) -> Self {
        Self {
            line: Line::new(points).name(name.into()).color(color),
        }
    }
}

impl PlotItem for AdaptiveTrace {
    fn shapes(&self, ui: &Ui, transform: &PlotTransform, shapes: &mut Vec<Shape>) {
        let PlotGeometry::Points(points) = self.line.geometry() else {
            return;
        };
        let pixels_per_point = ui.ctx().pixels_per_point();
        if has_dense_cycles(points, transform, pixels_per_point) {
            let alpha = if self.highlighted() { 0.72 } else { 0.45 };
            let mesh = amplitude_mesh(
                points,
                transform,
                pixels_per_point,
                self.color().gamma_multiply(alpha),
            );
            if !mesh.is_empty() {
                shapes.push(Shape::Mesh(mesh));
            }
        } else {
            self.line.shapes(ui, transform, shapes);
        }
    }

    fn initialize(&mut self, x_range: RangeInclusive<f64>) {
        self.line.initialize(x_range);
    }
    fn name(&self) -> &str {
        PlotItem::name(&self.line)
    }
    fn color(&self) -> Color32 {
        PlotItem::color(&self.line)
    }
    fn highlight(&mut self) {
        PlotItem::highlight(&mut self.line);
    }
    fn highlighted(&self) -> bool {
        self.line.highlighted()
    }
    fn geometry(&self) -> PlotGeometry<'_> {
        self.line.geometry()
    }
    fn bounds(&self) -> PlotBounds {
        self.line.bounds()
    }
    fn id(&self) -> Option<Id> {
        PlotItem::id(&self.line)
    }
}

fn finite(point: &PlotPoint) -> bool {
    point.x.is_finite() && point.y.is_finite()
}

fn has_dense_cycles(
    points: &[PlotPoint],
    transform: &PlotTransform,
    pixels_per_point: f32,
) -> bool {
    let bounds = transform.bounds();
    let visible = |point: &&PlotPoint| {
        finite(point) && point.x >= bounds.min()[0] && point.x <= bounds.max()[0]
    };
    let mut levels: Vec<_> = points.iter().filter(visible).map(|point| point.y).collect();
    if levels.len() < 4 {
        return false;
    }
    let tail = levels.len() / 20;
    let (_, low, _) = levels.select_nth_unstable_by(tail, f64::total_cmp);
    let low = *low;
    let high_index = levels.len() - 1 - tail;
    let (_, high, _) = levels.select_nth_unstable_by(high_index, f64::total_cmp);
    let high = *high;
    let (low, high) = if low < high {
        (low, high)
    } else {
        // Rare pulses can occupy less than the trimmed tails. Keep their full
        // amplitude when the rest of the trace is flat.
        levels
            .into_iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), value| {
                (low.min(value), high.max(value))
            })
    };
    // Robust hysteresis ignores tiny noise without letting one large spike hide
    // every ordinary cycle. Scale first to avoid overflowing a finite range.
    let threshold = high * 0.05 - low * 0.05;
    if !threshold.is_finite() || threshold <= 0.0 {
        return false;
    }
    let mut direction = 0_i8;
    let mut extreme: Option<PlotPoint> = None;
    let mut previous_turn = [None::<f64>; 2];
    let mut cycles = Vec::new();
    for point in points.iter().filter(visible) {
        let Some(mut peak) = extreme else {
            extreme = Some(*point);
            continue;
        };
        let mut turn = None;
        match direction {
            1 if point.y >= peak.y => peak = *point,
            1 if peak.y - point.y >= threshold => {
                turn = Some((0, peak.x));
                direction = -1;
                peak = *point;
            }
            -1 if point.y <= peak.y => peak = *point,
            -1 if point.y - peak.y >= threshold => {
                turn = Some((1, peak.x));
                direction = 1;
                peak = *point;
            }
            0 if (point.y - peak.y).abs() >= threshold => {
                direction = if point.y > peak.y { 1 } else { -1 };
                peak = *point;
            }
            _ => {}
        }
        if let Some((kind, x)) = turn {
            if let Some(previous) = previous_turn[kind] {
                let pixels = (x - previous) * transform.dpos_dvalue_x() * pixels_per_point as f64;
                if pixels.is_finite() && pixels > 0.0 {
                    cycles.push(pixels);
                }
            }
            previous_turn[kind] = Some(x);
        }
        extreme = Some(peak);
    }
    // Compare full peak-to-peak/trough-to-trough cycles. A sawtooth's abrupt
    // reset is a short half-cycle, not evidence that the entire wave is dense.
    if cycles.len() < 3 {
        return false;
    }
    let middle = cycles.len() / 2;
    let (_, median, _) = cycles.select_nth_unstable_by(middle, f64::total_cmp);
    *median < MIN_CYCLE_PIXELS
}

#[derive(Clone, Copy)]
struct Column {
    top: f32,
    bottom: f32,
}
impl Default for Column {
    fn default() -> Self {
        Self {
            top: f32::INFINITY,
            bottom: f32::NEG_INFINITY,
        }
    }
}
impl Column {
    fn include(&mut self, y: f32) {
        if !y.is_nan() {
            self.top = self.top.min(y);
            self.bottom = self.bottom.max(y);
        }
    }
    fn occupied(self) -> bool {
        self.top <= self.bottom
    }
}

fn amplitude_mesh(
    points: &[PlotPoint],
    transform: &PlotTransform,
    pixels_per_point: f32,
    color: Color32,
) -> egui::epaint::Mesh {
    let mut mesh = egui::epaint::Mesh::default();
    let frame = *transform.frame();
    if !frame.is_finite() || frame.width() <= 0.0 || frame.height() <= 0.0 {
        return mesh;
    }
    let ppp = pixels_per_point.max(0.01);
    let left = (frame.left() * ppp).floor() / ppp;
    let right = (frame.right() * ppp).ceil() / ppp;
    let count = (((right - left) * ppp).round() as usize).clamp(1, MAX_COLUMNS);
    let step = (right - left) / count as f32;
    let column_at = |x: f32| (((x - left) / step).floor() as usize).min(count - 1);
    let mut columns = vec![Column::default(); count];
    let x_bounds = transform.bounds().min()[0]..=transform.bounds().max()[0];
    for point in points.iter().filter(|point| finite(point)) {
        if x_bounds.contains(&point.x) {
            let x = transform.position_from_point_x(point.x);
            columns[column_at(x)].include(transform.position_from_point_y(point.y));
        }
    }
    for pair in points.windows(2) {
        let [a, b] = [pair[0], pair[1]];
        if !finite(&a) || !finite(&b) || a.x >= b.x {
            continue;
        }
        let start = a.x.max(*x_bounds.start());
        let end = b.x.min(*x_bounds.end());
        if start > end {
            continue;
        }
        let start_x = transform.position_from_point_x(start);
        let end_x = transform.position_from_point_x(end);
        for (index, column) in columns
            .iter_mut()
            .enumerate()
            .take(column_at(end_x) + 1)
            .skip(column_at(start_x))
        {
            // Include the whole connecting segment inside each pixel footprint,
            // not just vertices: a narrow spike crossing a column must survive.
            for x in [
                start_x.max(left + index as f32 * step),
                end_x.min(left + (index + 1) as f32 * step),
            ] {
                let data_x = transform.value_from_position(egui::pos2(x, frame.top())).x;
                let t = ((data_x - a.x) / (b.x - a.x)).clamp(0.0, 1.0);
                let y = a.y * (1.0 - t) + b.y * t;
                column.include(transform.position_from_point_y(y));
            }
        }
    }
    mesh.reserve_vertices(count * 4);
    mesh.reserve_triangles(count * 2);
    for (index, original) in columns.iter().copied().enumerate() {
        if !original.occupied() {
            continue;
        }
        let mut band = original;
        // Cover two neighboring pixels in either direction, wider than the
        // unresolved four-pixel cycle threshold. This avoids phase-dependent
        // teeth at the density boundary without averaging away an extremum.
        for neighbor in
            index.saturating_sub(ENVELOPE_RADIUS)..=(index + ENVELOPE_RADIUS).min(count - 1)
        {
            if columns[neighbor].occupied() {
                band.top = band.top.min(columns[neighbor].top);
                band.bottom = band.bottom.max(columns[neighbor].bottom);
            }
        }
        if band.bottom < frame.top() || band.top > frame.bottom() {
            continue;
        }
        let mut top = band.top.max(frame.top());
        let mut bottom = band.bottom.min(frame.bottom());
        if bottom - top < 1.0 / ppp {
            let center = (top + bottom) * 0.5;
            top = (center - 0.5 / ppp).max(frame.top());
            bottom = (center + 0.5 / ppp).min(frame.bottom());
        }
        let rect = Rect::from_min_max(
            egui::pos2((left + index as f32 * step).max(frame.left()), top),
            egui::pos2(
                (left + (index + 1) as f32 * step).min(frame.right()),
                bottom,
            ),
        );
        if rect.is_positive() && rect.is_finite() {
            mesh.add_colored_rect(rect, color);
        }
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave(count: usize, period: usize) -> Vec<PlotPoint> {
        (0..count)
            .map(|index| {
                PlotPoint::new(
                    index as f64,
                    (std::f64::consts::TAU * index as f64 / period as f64).sin(),
                )
            })
            .collect()
    }
    fn transform(width: f32, count: usize) -> PlotTransform {
        PlotTransform::new(
            Rect::from_min_size(egui::pos2(10.25, 20.5), egui::vec2(width, 400.0)),
            PlotBounds::from_min_max([0.0, -4.0], [count as f64 - 1.0, 4.0]),
            false,
            false,
        )
    }

    #[test]
    fn unresolved_cycles_use_bands_but_resolved_waves_and_sawtooth_resets_do_not() {
        let count = 12_496;
        let transform = transform(1_250.0, count);
        assert!(has_dense_cycles(&wave(count, 16), &transform, 1.0));
        let mut with_spike = wave(count, 16);
        with_spike[3_333].y = 1_000.0;
        assert!(has_dense_cycles(&with_spike, &transform, 1.0));
        assert!(!has_dense_cycles(&wave(count, 64), &transform, 1.0));
        let ramp: Vec<_> = (0..count)
            .map(|index| PlotPoint::new(index as f64, (index % 128) as f64))
            .collect();
        assert!(!has_dense_cycles(&ramp, &transform, 1.0));
        let flat = vec![PlotPoint::new(0.0, 1.0), PlotPoint::new(count as f64, 1.0)];
        assert!(!has_dense_cycles(&flat, &transform, 1.0));
    }

    #[test]
    fn density_uses_physical_pixels_and_the_visible_interval() {
        let points = wave(12_496, 16);
        let full = transform(1_250.0, points.len());
        assert!(has_dense_cycles(&points, &full, 1.0));
        assert!(!has_dense_cycles(&points, &full, 3.0));
        let mut zoomed = full;
        zoomed.set_bounds(PlotBounds::from_min_max([0.0, -4.0], [1_000.0, 4.0]));
        assert!(!has_dense_cycles(&points, &zoomed, 1.0));
    }

    #[test]
    fn envelope_has_shared_edges_stable_amplitude_and_retains_a_single_sample_peak() {
        let mut points = wave(12_496, 16);
        points[3_333].y = 3.5;
        let transform = transform(1_250.0, points.len());
        let mesh = amplitude_mesh(&points, &transform, 1.0, Color32::RED);
        assert!(mesh.is_valid());
        assert!(mesh.vertices.len() <= 1_252 * 4);
        let rects: Vec<_> = mesh
            .vertices
            .chunks_exact(4)
            .map(|vertices| {
                vertices.iter().fold(Rect::NOTHING, |mut rect, vertex| {
                    rect.extend_with(vertex.pos);
                    rect
                })
            })
            .collect();
        for pair in rects.windows(2) {
            assert!((pair[0].right() - pair[1].left()).abs() < 0.001);
            assert!((pair[0].right() - pair[0].right().round()).abs() < 0.001);
        }
        let peak = transform.position_from_point(&points[3_333]);
        assert!(rects.iter().any(|rect| rect.contains(peak)));
        // Away from the isolated peak and clipped edges, unresolved sine cycles
        // have uniform coverage instead of a false low-frequency beat pattern.
        let top = transform.position_from_point_y(1.0);
        let bottom = transform.position_from_point_y(-1.0);
        for rect in &rects[20..200] {
            assert!((rect.top() - top).abs() < 0.01);
            assert!((rect.bottom() - bottom).abs() < 0.01);
        }
    }

    #[test]
    fn cycles_just_below_four_pixels_have_uniform_amplitude_coverage() {
        let points = wave(12_496, 38);
        let transform = transform(1_250.0, points.len());
        assert!(has_dense_cycles(&points, &transform, 1.0));
        let mesh = amplitude_mesh(&points, &transform, 1.0, Color32::RED);
        assert!(mesh.is_valid());
        assert!(mesh.vertices.len() <= 1_252 * 4);
        let amplitude = points
            .iter()
            .take(38)
            .map(|point| point.y.abs())
            .fold(0.0, f64::max);
        let top = transform.position_from_point_y(amplitude);
        let bottom = transform.position_from_point_y(-amplitude);
        for vertices in mesh.vertices.chunks_exact(4).skip(5).take(1_230) {
            let rect = vertices.iter().fold(Rect::NOTHING, |mut rect, vertex| {
                rect.extend_with(vertex.pos);
                rect
            });
            assert!((rect.top() - top).abs() < 0.01, "{rect:?}");
            assert!((rect.bottom() - bottom).abs() < 0.01, "{rect:?}");
        }
    }

    #[test]
    fn mesh_is_finite_clipped_and_screen_bounded_even_with_gaps_and_extreme_values() {
        let points = vec![
            PlotPoint::new(-1.0, 0.0),
            PlotPoint::new(0.0, -1e100),
            PlotPoint::new(100.0, 1e100),
            PlotPoint::new(150.0, f64::NAN),
            PlotPoint::new(180.0, 0.0),
            PlotPoint::new(200.0, 0.0),
        ];
        for ppp in [1.0, 1.5, 2.0, 100.0] {
            let transform = transform(1_250.0, 201);
            let mesh = amplitude_mesh(&points, &transform, ppp, Color32::RED);
            assert!(mesh.is_valid());
            assert!(mesh.vertices.len() <= MAX_COLUMNS * 4);
            assert!(!mesh.vertices.is_empty());
            assert!(mesh.vertices.iter().all(|vertex| {
                vertex.pos.is_finite() && transform.frame().contains(vertex.pos)
            }));
        }
    }

    #[test]
    fn line_identity_and_original_point_geometry_are_unchanged() {
        let mut trace = AdaptiveTrace::new(vec![[1.0, 4.0], [2.0, 7.0]], "signal", Color32::RED);
        assert_eq!(trace.name(), "signal");
        assert_eq!(trace.color(), Color32::RED);
        assert_eq!(trace.bounds().min(), [1.0, 4.0]);
        assert_eq!(trace.bounds().max(), [2.0, 7.0]);
        let PlotGeometry::Points(points) = trace.geometry() else {
            panic!("actual samples must remain available for picking");
        };
        assert_eq!(points, [PlotPoint::new(1.0, 4.0), PlotPoint::new(2.0, 7.0)]);
        trace.highlight();
        assert!(trace.highlighted());
    }
}
