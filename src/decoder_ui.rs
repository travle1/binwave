//! Decoder workflow presentation shared by the native and browser shells.
use egui::{Color32, RichText, Ui};

pub const RUN_LABEL: &str = "▶ Run (F5)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusTone {
    Neutral,
    Ready,
    Success,
    Warning,
    Error,
}

fn tone_color(ui: &Ui, tone: StatusTone) -> Color32 {
    match tone {
        StatusTone::Neutral => ui.visuals().weak_text_color(),
        StatusTone::Ready => ui.visuals().selection.stroke.color,
        StatusTone::Success => Color32::from_rgb(80, 180, 120),
        StatusTone::Warning => ui.visuals().warn_fg_color,
        StatusTone::Error => ui.visuals().error_fg_color,
    }
}

pub fn run_button(ui: &mut Ui, enabled: bool, disabled_reason: Option<&str>) -> egui::Response {
    let mut button = egui::Button::new(RichText::new(RUN_LABEL).strong());
    if enabled {
        button = button.fill(ui.visuals().selection.bg_fill);
    }
    let response = ui.add_enabled(enabled, button);
    if !enabled {
        if let Some(reason) = disabled_reason {
            return response.on_hover_text(reason);
        }
    }
    response.on_hover_text("Run this decoder against the open data file")
}

pub fn status_banner(ui: &mut Ui, headline: &str, detail: &str, tone: StatusTone, busy: bool) {
    let color = tone_color(ui, tone);
    let fill = color.gamma_multiply(if ui.visuals().dark_mode { 0.10 } else { 0.06 });
    egui::Frame::none()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.65)))
        .rounding(5.0)
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                if busy {
                    ui.spinner();
                }
                ui.vertical(|ui| {
                    ui.label(RichText::new(headline).strong().color(color));
                    if !detail.is_empty() {
                        ui.label(detail);
                    }
                });
            });
        });
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ConsoleAction {
    pub copy: bool,
    pub line: Option<usize>,
    pub byte: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParseStep {
    label: String,
    byte: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BoundaryFailure {
    start: u64,
    requested_end: u64,
    region_end: u64,
}

impl BoundaryFailure {
    fn requested_bytes(self) -> u64 {
        self.requested_end.saturating_sub(self.start)
    }

    fn overrun(self) -> u64 {
        self.requested_end.saturating_sub(self.region_end)
    }

    fn available_bytes(self) -> u64 {
        self.region_end.saturating_sub(self.start)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ParsingMap {
    steps: Vec<ParseStep>,
    boundary: Option<BoundaryFailure>,
}

/// Keep the source editor and its console in one bounded vertical split.
///
/// Both application shells use this layout so a short Definition pane cannot
/// push Python errors below the visible tab. The child UIs are clipped to their
/// assigned regions; each contains its own scrolling surface.
pub fn source_console(
    ui: &mut Ui,
    stdout: &str,
    error: Option<&str>,
    running: bool,
    source: impl FnOnce(&mut Ui),
) -> ConsoleAction {
    let spacing = 6.0;
    let (source_height, console_height) =
        source_console_heights(ui.available_height(), error.is_some());
    let width = ui.available_width();
    ui.allocate_ui(egui::vec2(width, source_height), |ui| {
        ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
        source(ui);
    });
    ui.add_space(spacing);
    ui.allocate_ui(egui::vec2(width, console_height), |ui| {
        ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
        console(ui, stdout, error, running)
    })
    .inner
}

fn source_console_heights(available: f32, has_error: bool) -> (f32, f32) {
    let spacing = 6.0;
    let body = (available - spacing).max(0.0);
    let desired_console: f32 = if has_error { 260.0 } else { 125.0 };
    let minimum_console: f32 = if has_error { 96.0 } else { 64.0 };
    let minimum_source = 72.0_f32.min(body * 0.5);
    let maximum_console = (body - minimum_source).max(0.0);
    let effective_minimum = minimum_console.min(maximum_console);
    let console = (body * 0.55)
        .max(effective_minimum)
        .min(desired_console)
        .min(maximum_console);
    (body - console, console)
}

pub fn console(ui: &mut Ui, stdout: &str, error: Option<&str>, running: bool) -> ConsoleAction {
    let mut action = ConsoleAction::default();
    let error_count = usize::from(error.is_some());
    let failing_line = error.and_then(error_line);
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Console");
                if running {
                    ui.spinner();
                    ui.weak("Running…");
                } else if error_count > 0 {
                    ui.colored_label(ui.visuals().error_fg_color, "1 error");
                } else if stdout.is_empty() {
                    ui.weak("No output");
                } else {
                    ui.weak("Run finished");
                }
                if let Some(line) = failing_line {
                    if ui.small_button(format!("Go to line {line}")).clicked() {
                        action.line = Some(line);
                    }
                }
                if ui
                    .add_enabled(
                        !stdout.is_empty() || error.is_some(),
                        egui::Button::new("Copy console").small(),
                    )
                    .clicked()
                {
                    action.copy = true;
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .id_source("decoder_console_output")
                .auto_shrink([false, false])
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if stdout.is_empty() && error.is_none() {
                        ui.weak("print() output and decoder errors appear here.");
                    } else {
                        if !stdout.is_empty() {
                            ui.monospace(stdout);
                        }
                        if let Some(error) = error {
                            if let Some(map) = parsing_map(error) {
                                parsing_map_ui(ui, &map, &mut action);
                                ui.add_space(6.0);
                                ui.collapsing("Technical details", |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(error)
                                                .monospace()
                                                .color(ui.visuals().error_fg_color),
                                        )
                                        .wrap(true),
                                    );
                                });
                            } else {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(error)
                                            .monospace()
                                            .color(ui.visuals().error_fg_color),
                                    )
                                    .wrap(true),
                                );
                            }
                        }
                    }
                });
        });
    action
}

fn parsing_map_ui(ui: &mut Ui, map: &ParsingMap, action: &mut ConsoleAction) {
    ui.strong("Parsing map");
    ui.weak("The parser followed these byte offsets before it stopped.");
    ui.add_space(4.0);

    for (index, step) in map.steps.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{}", index + 1)).strong());
            ui.monospace(&step.label);
            ui.weak(format!("byte {} (0x{:X})", step.byte, step.byte));
            if ui.small_button("Show in Hex").clicked() {
                action.byte = Some(step.byte);
            }
        });
    }

    if let Some(boundary) = map.boundary {
        ui.add_space(6.0);
        let color = ui.visuals().error_fg_color;
        egui::Frame::none()
            .fill(color.gamma_multiply(if ui.visuals().dark_mode { 0.10 } else { 0.06 }))
            .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.65)))
            .rounding(4.0)
            .inner_margin(egui::Margin::same(7.0))
            .show(ui, |ui| {
                ui.colored_label(color, RichText::new("Read crossed its enclosing boundary").strong());
                ui.label(format!(
                    "The read starts at byte {} and requests {} bytes, ending at byte {}. Only {} bytes remain before this region ends at byte {}, so the read overruns it by {} bytes.",
                    boundary.start,
                    boundary.requested_bytes(),
                    boundary.requested_end,
                    boundary.available_bytes(),
                    boundary.region_end,
                    boundary.overrun(),
                ));
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button(format!("Show byte {} in Hex", boundary.start)).clicked() {
                        action.byte = Some(boundary.start);
                    }
                    ui.weak("Check whether the decoded length includes its header, whether a repeat should stop earlier, or whether an earlier field shifted the cursor.");
                });
            });
    }
}

fn parsing_map(error: &str) -> Option<ParsingMap> {
    let mut map = ParsingMap::default();

    for part in error.split(": ") {
        let Some((label, suffix)) = part.rsplit_once(" at byte ") else {
            continue;
        };
        let Some(byte) = parse_u64_prefix(suffix) else {
            continue;
        };
        let label = label.trim().strip_prefix("read ").unwrap_or(label.trim());
        if label.is_empty() {
            continue;
        }
        let step = ParseStep {
            label: label.to_owned(),
            byte,
        };
        if map.steps.last() != Some(&step) {
            map.steps.push(step);
        }
    }

    map.boundary = parse_boundary_failure(error);
    (!map.steps.is_empty() || map.boundary.is_some()).then_some(map)
}

fn parse_boundary_failure(error: &str) -> Option<BoundaryFailure> {
    let (_, range) = error.split_once("byte range ")?;
    let (start, remainder) = range.split_once("..")?;
    let start = start.trim().parse().ok()?;
    let requested_end = parse_u64_prefix(remainder)?;
    let (_, region_end) = remainder.split_once("exceeds enclosing region end ")?;
    let region_end = parse_u64_prefix(region_end)?;
    (start <= requested_end && region_end < requested_end).then_some(BoundaryFailure {
        start,
        requested_end,
        region_end,
    })
}

fn parse_u64_prefix(text: &str) -> Option<u64> {
    let digits: String = text
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

pub fn empty_state(ui: &mut Ui, title: &str, detail: &str) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(12.0))
        .show(ui, |ui| {
            ui.strong(title);
            ui.label(detail);
        });
}

/// Return the first one-based source line mentioned by common Python/YAML errors.
pub fn error_line(error: &str) -> Option<usize> {
    for line in error.lines() {
        for marker in ["line ", "line: "] {
            let Some((_, suffix)) = line.split_once(marker) else {
                continue;
            };
            let digits: String = suffix.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(line) = digits.parse::<usize>() {
                if line > 0 {
                    return Some(line);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_python_and_yaml_error_lines() {
        assert_eq!(
            error_line("File \"decoder.py\", line 17, in <module>"),
            Some(17)
        );
        assert_eq!(error_line("invalid value at line 4 column 9"), Some(4));
        assert_eq!(error_line("decoder failed"), None);
    }

    #[test]
    fn source_console_keeps_errors_visible_in_short_panes() {
        let (source, console) = source_console_heights(260.0, true);
        assert!(source >= 72.0);
        assert!(console >= 96.0);
        assert!(source + console + 6.0 <= 260.0);

        let (source, console) = source_console_heights(150.0, true);
        assert!(source >= 72.0);
        assert!(console >= 72.0);
        assert!(source + console + 6.0 <= 150.0);
    }

    #[test]
    fn builds_a_parsing_map_from_decoder_context() {
        let error = "read Packets at byte 8: item 5 at byte 8332: read Packets[5].payload at byte 8344: byte range 8344..9000 exceeds enclosing region end 8500";
        let map = parsing_map(error).expect("parsing map");
        assert_eq!(
            map.steps,
            vec![
                ParseStep {
                    label: "Packets".into(),
                    byte: 8,
                },
                ParseStep {
                    label: "item 5".into(),
                    byte: 8332,
                },
                ParseStep {
                    label: "Packets[5].payload".into(),
                    byte: 8344,
                },
            ]
        );
        let boundary = map.boundary.expect("boundary");
        assert_eq!(boundary.requested_bytes(), 656);
        assert_eq!(boundary.available_bytes(), 156);
        assert_eq!(boundary.overrun(), 500);
    }

    #[test]
    fn renders_the_parsing_map_in_the_console() {
        let ctx = egui::Context::default();
        let error = "read Sections at byte 8: item 5 at byte 8332: read Sections[5].payload at byte 8344: byte range 8344..9000 exceeds enclosing region end 8500";
        let frame = || {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(760.0, 520.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        console(ui, "", Some(error), false);
                    });
                },
            )
        };
        let _ = frame();
        let output = frame();
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        for expected in [
            "Parsing map",
            "Sections[5].payload",
            "Read crossed its enclosing boundary",
            "Show byte 8344 in Hex",
        ] {
            assert!(labels.contains(&expected), "missing {expected}: {labels:?}");
        }
    }

    #[test]
    fn skips_a_parsing_map_for_unrelated_errors() {
        assert_eq!(parsing_map("unknown built-in type banana"), None);
        assert_eq!(
            parse_boundary_failure("byte range 100..90 exceeds enclosing region end 80"),
            None
        );
    }
}
