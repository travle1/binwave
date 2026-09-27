//! Display-only transforms. Source values and byte provenance never change.
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub scale: f64,
    pub offset: f64,
}
impl Default for Placement {
    fn default() -> Self {
        Self {
            scale: 1.,
            offset: 0.,
        }
    }
}
impl Placement {
    pub fn display(self, value: f64) -> f64 {
        value * self.scale + self.offset
    }
    pub fn original(self, value: f64) -> f64 {
        (value - self.offset) / self.scale
    }
}
#[derive(Default)]
pub struct Presentation {
    pub series: BTreeMap<String, Placement>,
}
impl Presentation {
    pub fn placement(&self, name: &str) -> Placement {
        self.series.get(name).copied().unwrap_or_default()
    }
    /// Move one trace in display coordinates; the source samples remain intact.
    pub fn pan_series(&mut self, name: &str, amount: f64) -> bool {
        let p = self.series.entry(name.to_owned()).or_default();
        let next = p.offset + amount;
        if !next.is_finite() || amount == 0. {
            return false;
        }
        p.offset = next;
        true
    }
    /// Zoom one trace around a display-space value, leaving that value fixed.
    pub fn zoom_series(&mut self, name: &str, factor: f64, center: f64) -> bool {
        if !factor.is_finite() || factor <= 0. || factor == 1. {
            return false;
        }
        let p = self.series.entry(name.to_owned()).or_default();
        let scale = p.scale * factor;
        let offset = center - (center - p.offset) * factor;
        if !scale.is_finite() || !(1e-12..=1e12).contains(&scale) || !offset.is_finite() {
            return false;
        }
        p.scale = scale;
        p.offset = offset;
        true
    }
    pub fn menu(&mut self, ui: &mut egui::Ui, ranges: &[(String, f64, f64)]) -> bool {
        let mut changed = false;
        ui.separator();
        if ui
            .button("Stack series")
            .on_hover_text(
                "Fit each series range into a separate vertical lane. Values stay unchanged.",
            )
            .clicked()
        {
            self.stack(ranges);
            changed = true;
        }
        if ui.button("Reset positions").clicked() {
            self.series.clear();
            changed = true;
        }
        let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for (name, _, _) in ranges {
            let field = name
                .rsplit_once(" / ")
                .map_or(name.as_str(), |(_, field)| field);
            let (group, label) = field.split_once('.').unwrap_or(("", field));
            groups
                .entry(group.to_owned())
                .or_default()
                .push((label.to_owned(), name.clone()));
        }
        ui.menu_button("Series positions", |ui| {
            egui::ScrollArea::vertical()
                .id_salt("plot-series-positions")
                .max_height(320.)
                .show(ui, |ui| {
                    for (group, entries) in groups {
                        let mut show_entries = |ui: &mut egui::Ui| {
                            for (label, name) in &entries {
                                ui.push_id(name, |ui| {
                                    ui.menu_button(label, |ui| {
                                        let p = self.series.entry(name.clone()).or_default();
                                        ui.horizontal(|ui| {
                                            ui.label("Vertical offset");
                                            changed |= ui.add(egui::DragValue::new(&mut p.offset).speed(0.1)).changed();
                                        });
                                        ui.horizontal(|ui| {
                                            ui.label("Scale");
                                            changed |= ui.add(egui::DragValue::new(&mut p.scale).speed(0.01).range(1e-12..=1e12)).changed();
                                        });
                                        if ui.button("Reset this series").clicked() {
                                            *p = Placement::default();
                                            changed = true;
                                        }
                                        ui.weak("Display only; original values and bytes are unchanged.");
                                    }).response.on_hover_text(name);
                                });
                            }
                        };
                        if group.is_empty() {
                            show_entries(ui);
                        } else {
                            ui.collapsing(format!("{group} ({})", entries.len()), |ui| show_entries(ui));
                        }
                    }
                });
        });
        changed
    }
    pub fn stack(&mut self, ranges: &[(String, f64, f64)]) {
        for (index, (name, min, max)) in ranges.iter().enumerate() {
            let span = max - min;
            let scale = if span.is_finite() && span > 1e-12 {
                2. / span
            } else {
                1.
            };
            let offset = index as f64 * 3. - (min / 2. + max / 2.) * scale;
            if scale.is_finite() && offset.is_finite() {
                self.series
                    .insert(name.clone(), Placement { scale, offset });
            }
        }
    }

}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stacking_separates_ranges_and_is_reversible() {
        let mut p = Presentation::default();
        p.stack(&[
            ("small".into(), -1., 1.),
            ("large".into(), 100., 300.),
            ("flat".into(), 7., 7.),
        ]);
        assert_eq!(p.placement("small").display(-1.), -1.);
        assert_eq!(p.placement("large").display(100.), 2.);
        assert_eq!(p.placement("large").display(300.), 4.);
        assert_eq!(p.placement("large").original(3.), 200.);
        assert_eq!(p.placement("flat").display(7.), 6.);
        p.series.clear();
        assert_eq!(p.placement("large").display(200.), 200.);
    }
    #[test]
    fn axis_navigation_changes_only_the_selected_trace() {
        let mut p = Presentation::default();
        assert!(p.zoom_series("sine[110]", 2., 32_000.));
        assert_eq!(p.placement("sine[110]").display(32_000.), 32_000.);
        assert_eq!(p.placement("sine[111]").display(32_000.), 32_000.);
        assert!(p.pan_series("sine[110]", 100.));
        assert_eq!(p.placement("sine[110]").display(32_000.), 32_100.);
        assert_eq!(p.placement("sine[111]").display(32_000.), 32_000.);
        assert!(!p.zoom_series("sine[110]", 0., 32_000.));
    }
}
