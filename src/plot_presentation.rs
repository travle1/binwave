//! Display-only transforms. Source values and byte provenance never change.
#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use egui_plot::{PlotPoint, PlotTransform};
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
    dragging: Option<String>,
}
impl Presentation {
    pub fn placement(&self, name: &str) -> Placement {
        self.series.get(name).copied().unwrap_or_default()
    }
    pub fn is_dragging(&self) -> bool {
        self.dragging.is_some()
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
            let field = name.rsplit_once(" / ").map_or(name.as_str(), |(_, field)| field);
            let (group, label) = field.split_once('.').unwrap_or(("", field));
            groups.entry(group.to_owned()).or_default().push((label.to_owned(), name.clone()));
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
    /// Points are already in display coordinates. Pick on press, then retain the
    /// selected trace until release so crossing another trace cannot switch it.
    pub fn drag(
        &mut self,
        ui: &egui::Ui,
        transform: &PlotTransform,
        points: impl Iterator<Item = (String, [f64; 2])>,
    ) {
        let (pressed, down, pointer, delta) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.interact_pos(),
                i.pointer.delta(),
            )
        });
        if !down {
            self.dragging = None;
            return;
        }
        if pressed {
            self.dragging = pointer
                .filter(|p| {
                    transform.frame().contains(*p)
                        && ui
                            .ctx()
                            .rect_contains_pointer(ui.layer_id(), *transform.frame())
                })
                .and_then(|pointer| {
                    let mut previous = BTreeMap::<String, egui::Pos2>::new();
                    points
                        .filter_map(|(name, p)| {
                            let position =
                                transform.position_from_point(&PlotPoint::new(p[0], p[1]));
                            let mut d = position.distance_sq(pointer);
                            if let Some(start) = previous.insert(name.clone(), position) {
                                let line = position - start;
                                if line.length_sq() > 0. {
                                    let t = ((pointer - start).dot(line) / line.length_sq())
                                        .clamp(0., 1.);
                                    d = d.min((start + line * t).distance_sq(pointer));
                                }
                            }
                            (d <= 100.).then_some((d, name))
                        })
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                        .map(|(_, name)| name)
                });
        } else if let Some(name) = &self.dragging {
            let center = transform.frame().center();
            let amount = transform
                .value_from_position(center + egui::vec2(0., delta.y))
                .y
                - transform.value_from_position(center).y;
            let p = self.series.entry(name.clone()).or_default();
            if (p.offset + amount).is_finite() {
                p.offset += amount;
            }
            if delta.y != 0. {
                ui.ctx().request_repaint();
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dragging_moves_only_the_pressed_series_and_releases() {
        let ctx = egui::Context::default();
        let mut p = Presentation::default();
        let transform = PlotTransform::new(
            egui::Rect::from_min_max(egui::pos2(20., 20.), egui::pos2(220., 220.)),
            egui_plot::PlotBounds::from_min_max([-1., -1.], [1., 1.]),
            false,
        );
        let mut frame = |events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(240., 240.),
                    )),
                    events,
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        p.drag(
                            ui,
                            &transform,
                            [("first".into(), [0., 0.]), ("second".into(), [0., 0.8])].into_iter(),
                        )
                    });
                },
            );
        };
        frame(vec![]);
        frame(vec![]);
        frame(vec![
            egui::Event::PointerMoved(egui::pos2(120., 120.)),
            egui::Event::PointerButton {
                pos: egui::pos2(120., 120.),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ]);
        frame(vec![egui::Event::PointerMoved(egui::pos2(120., 80.))]);
        frame(vec![egui::Event::PointerButton {
            pos: egui::pos2(120., 80.),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }]);
        assert!((p.placement("first").offset - 0.4).abs() < 1e-10);
        assert_eq!(p.placement("second").offset, 0.);
        assert!(!p.is_dragging());
    }
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
}
