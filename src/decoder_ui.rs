//! Decoder workflow presentation shared by the native and browser shells.
#[cfg(test)]
use crate::egui_compat::ContextRun as _;
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
    egui::Frame::NONE
        .fill(fill)
        .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.65)))
        .corner_radius(5.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RepeatSizeFailure {
    start: u64,
    region: u64,
    item_size: u64,
    complete: u64,
    remainder: u64,
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
    repeat_size: Option<RepeatSizeFailure>,
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
    let has_output = !stdout.is_empty() || error.is_some() || running;
    let console_id = ui.id().with("source_console_open");
    let requested_open = ui.data_mut(|data| data.get_persisted::<bool>(console_id).unwrap_or(false));
    let console_open = has_output || requested_open;
    let spacing = 6.0;
    let (source_height, console_height) = if console_open {
        source_console_heights(ui.available_height(), error.is_some())
    } else {
        ((ui.available_height() - 30.0 - spacing).max(0.0), 30.0)
    };
    let width = ui.available_width();
    ui.allocate_ui(egui::vec2(width, source_height), |ui| {
        ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
        source(ui);
    });
    ui.add_space(spacing);
    ui.allocate_ui(egui::vec2(width, console_height), |ui| {
        ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
        if console_open {
            let action = console(ui, stdout, error, running);
            if !has_output && ui.button("Hide console").clicked() {
                ui.data_mut(|data| data.insert_persisted(console_id, false));
            }
            action
        } else {
            if ui.button("Console").clicked() {
                ui.data_mut(|data| data.insert_persisted(console_id, true));
            }
            ui.weak("No output");
            ConsoleAction::default()
        }
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
        .inner_margin(egui::Margin::same(8))
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
                .id_salt("decoder_console_output")
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
                                        .wrap(),
                                    );
                                });
                            } else {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(error)
                                            .monospace()
                                            .color(ui.visuals().error_fg_color),
                                    )
                                    .wrap(),
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
        egui::Frame::NONE
            .fill(color.gamma_multiply(if ui.visuals().dark_mode { 0.10 } else { 0.06 }))
            .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.65)))
            .corner_radius(4.0)
            .inner_margin(egui::Margin::same(7))
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
    if let Some(repeat) = map.repeat_size {
        ui.add_space(6.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong("Repeat size check");
            ui.label(format!(
                "{} available bytes ÷ {} bytes per item = {} complete items, with {} bytes left over.",
                repeat.region, repeat.item_size, repeat.complete, repeat.remainder,
            ));
            if repeat.complete > 0 && repeat.remainder % repeat.complete == 0 {
                let extra = repeat.remainder / repeat.complete;
                ui.label(format!(
                    "That is {extra} extra {} per complete item if distributed evenly. Check for a missing field, header, or trailer; this is a clue, not proof of its position.",
                    if extra == 1 { "byte" } else { "bytes" },
                ));
            }
            ui.weak("The complete-sized item ranges form a tentative prefix in Decoded structure. Expand the first item to decode its fields.");
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("Show first item in Hex").clicked() {
                    action.byte = Some(repeat.start);
                }
                if let Some(next) = (repeat.complete >= 2).then(|| repeat.start.checked_add(repeat.item_size)).flatten() {
                    if ui.small_button("Show expected next item").clicked() {
                        action.byte = Some(next);
                    }
                    if repeat.complete > 0 && repeat.remainder % repeat.complete == 0 {
                        if let Some(candidate) = next.checked_add(repeat.remainder / repeat.complete) {
                            if ui.small_button("Show candidate next item").clicked() {
                                action.byte = Some(candidate);
                            }
                        }
                    }
                }
                if let Some(leftover_start) = repeat.start.checked_add(repeat.complete.saturating_mul(repeat.item_size)) {
                    if ui.small_button("Show unmatched tail in Hex").on_hover_text("Where the current item size leaves an unmatched tail; this does not prove the extra bytes are physically at the end.").clicked() {
                        action.byte = Some(leftover_start);
                    }
                }
            });
        });
    }
}

pub fn parsing_map_panel(ui: &mut Ui, error: Option<&str>) -> ConsoleAction {
    let mut action = ConsoleAction::default();
    if let Some(map) = error.and_then(parsing_map) {
        egui::ScrollArea::vertical().show(ui, |ui| parsing_map_ui(ui, &map, &mut action));
    } else {
        ui.weak("No parsing map is available. Run a decoder to see the offsets followed before a parse error.");
    }
    action
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
    map.repeat_size = parse_repeat_size_failure(error);
    (!map.steps.is_empty() || map.boundary.is_some() || map.repeat_size.is_some()).then_some(map)
}

fn parse_repeat_size_failure(error: &str) -> Option<RepeatSizeFailure> {
    let (_, rest) = error.split_once("fixed-size repeat at byte ")?;
    let (start, rest) = rest.split_once(" has ")?;
    let (region, rest) = rest.split_once(" bytes available; ")?;
    let (complete, rest) = rest.split_once(" complete ")?;
    let (item_size, rest) = rest.split_once("-byte items fit; ")?;
    let (remainder, _) = rest.split_once(" bytes remain")?;
    let failure = RepeatSizeFailure {
        start: start.parse().ok()?,
        region: region.parse().ok()?,
        item_size: item_size.parse().ok()?,
        complete: complete.parse().ok()?,
        remainder: remainder.parse().ok()?,
    };
    (failure.item_size > 0
        && failure.complete.checked_mul(failure.item_size)?.checked_add(failure.remainder)? == failure.region)
        .then_some(failure)
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
        .inner_margin(egui::Margin::same(12))
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
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
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

    #[test]
    fn repeat_size_error_shows_arithmetic_and_inspection_targets() {
        let error = "read records at byte 4096: records: fixed-size repeat at byte 4096 has 82320 bytes available; 40 complete 2048-byte items fit; 400 bytes remain. Their ranges are available as a tentative prefix in Decoded structure.";
        let repeat = parse_repeat_size_failure(error).unwrap();
        assert_eq!(repeat.complete, 40);
        assert_eq!(repeat.remainder / repeat.complete, 10);
        assert_eq!(repeat.start + repeat.complete * repeat.item_size, 86016);
        let ctx = egui::Context::default();
        let frame = || ctx.run(
            egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900., 600.))), ..Default::default() },
            |_ctx, ui| { egui::CentralPanel::default().show(ui, |ui| { console(ui, "", Some(error), false); }); },
        );
        let _ = frame();
        let output = frame();
        let labels: Vec<_> = output.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text()), _ => None,
        }).collect();
        assert!(labels.contains(&"Repeat size check"));
        assert!(labels.iter().any(|label| label.contains("10 extra bytes per complete item")));
        assert!(labels.contains(&"Show first item in Hex"));
        assert!(labels.contains(&"Show candidate next item"));
        assert!(labels.contains(&"Show unmatched tail in Hex"));
        assert!(parse_repeat_size_failure("fixed-size repeat at byte 3 has 10 bytes available; 2 complete 4-byte items fit; 3 bytes remain").is_none());
    }
}
