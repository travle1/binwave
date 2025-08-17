use crate::config::AppConfig;
use crate::data::{decimate_minmax, DataStore};
use egui_plot::{Line, PlotPoints};

pub struct PlotBuilder<'a> {
    pub store: &'a DataStore,
    pub cfg: &'a AppConfig,
}

impl<'a> PlotBuilder<'a> {
    pub fn line_for_range(
        &self,
        name: &str,
        tmin: f64,
        tmax: f64,
        pixel_width: f32,
    ) -> Option<Line> {
        let sig = self.store.get(name)?;
        let target = (self.cfg.view.target_points_per_line as f32)
            .min(pixel_width.max(100.0)) as usize;
        let pts = decimate_minmax(&sig.points, tmin, tmax, target);
        if pts.is_empty() {
            return None;
        }
        let plot_points: PlotPoints = pts.into_iter().map(|p| [p[0], p[1]]).collect();
        Some(Line::new(plot_points).name(name.to_string()))
    }
}
