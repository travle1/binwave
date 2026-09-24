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
    let (source_height, console_height) = source_console_heights(ui.available_height(), error.is_some());
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
    let desired_console: f32 = if has_error { 190.0 } else { 125.0 };
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
            egui::ScrollArea::both()
                .id_source("decoder_console_output")
                .auto_shrink([false, false])
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    if stdout.is_empty() && error.is_none() {
                        ui.weak("print() output and decoder errors appear here.");
                    } else {
                        if !stdout.is_empty() {
                            ui.monospace(stdout);
                        }
                        if let Some(error) = error {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(error)
                                        .monospace()
                                        .color(ui.visuals().error_fg_color),
                                )
                                .wrap(false),
                            );
                        }
                    }
                });
        });
    action
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
}
