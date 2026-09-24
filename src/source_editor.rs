//! Shared decoder source editor and starter templates for native and browser shells.
use egui::{Color32, Ui};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
/// One catalog for the native and browser example menus.
pub const YAML_EXAMPLES: &[(&str, &str)] = &[
    ("Start here · Simple wave", include_str!("../samples/quickstart/simple_wave.yaml")),
    ("PNG · Chunk structure", include_str!("../samples/decoders/yaml/png_chunks.yaml")),
    ("WAV · RIFF structure", include_str!("../samples/decoders/yaml/riff_chunks.yaml")),
    ("Nested records · Byte walkthrough", include_str!("../samples/decoders/yaml/nested_records.yaml")),
    ("Advanced · Primitive records", include_str!("../samples/decoders/yaml/generic_primitives.yaml")),
    ("Advanced · Types from earlier sections", include_str!("../samples/decoders/yaml/generic_sections.yaml")),
    ("Advanced · Waveform container", include_str!("../samples/decoders/yaml/waveform.yaml")),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DecoderLanguage {
    #[default]
    Yaml,
    Python,
}

impl DecoderLanguage {
    pub fn byte_preview(self) -> &'static str {
        match self {
            Self::Yaml => {
                r#"# Byte preview: edit this to match your data.
# Built-in types: u8, i16, u16, i32, f32, f64.
# Declare: - name: Sample. Reference: type: {structure: Sample}.
# F5 updates the plot, samples, and Hex links.
endian: little
structures:
  - name: Sample
    fields:
      - {name: value, type: u8}
actions:
  - read:
      name: samples
      type: {structure: Sample}
      at: 0
      repeat: until end
"#
            }
            Self::Python => {
                r#"# Byte preview: edit this to match your data.
# Field types: u8, i16, u16, i32, f32, f64.
# Press ▶ Run (F5); print() uses Console.
from binwave import Struct

Sample = Struct("Sample", [
    ("value", "u8"),
], endian="little")

samples = file.records(
    "samples", Sample, offset=0,
)
print(f"Registered {len(samples):,} samples")
"#
            }
        }
    }

    pub fn starter(self) -> &'static str {
        match self {
            Self::Yaml => include_str!("../samples/quickstart/simple_wave.yaml"),
            Self::Python => self.byte_preview(),
        }
    }

    pub fn header_example(self) -> &'static str {
        match self {
            Self::Yaml => {
                r#"## Adapt this example to your binary file.
## Header: u32 count. Samples: u32 time, f32 value.
## Select all, then Ctrl+/ to uncomment.
## On Mac use Cmd+/. Press ▶ Run (F5).
#
# endian: little
# structures:
#   - name: Header
#     fields:
#       - {name: count, type: u32}
#   - name: Sample
#     fields:
#       - {name: time, type: u32}
#       - {name: value, type: f32}
# actions:
#   - read: {name: header, type: {structure: Header}}
#   - read:
#       name: samples
#       type: {structure: Sample}
#       repeat: {ref: header.count}
"#
            }
            Self::Python => {
                r#"## Adapt this example to your binary file.
## `file` is the open file. print() uses Console.
## Select all, then Ctrl+/ to uncomment.
## On Mac use Cmd+/. Press ▶ Run (F5).
#
# from binwave import Struct
#
# Header = Struct("Header", [
#     ("count", "u32"),
# ], endian="little")
#
# Sample = Struct("Sample", [
#     ("time", "u32"),
#     ("value", "f32"),
# ], endian="little")
#
# header = file.read_struct("header", Header)
# samples = file.records(
#     "samples", Sample, count=header.count,
# )
# print(f"Decoded {len(samples)} samples")
"#
            }
        }
    }
}

pub(crate) struct YamlLayoutCache {
    job: Option<egui::text::LayoutJob>,
    font: egui::FontId,
    dark: bool,
    foreground: Color32,
    language: DecoderLanguage,
    edit_action: Option<&'static str>,
    paste: Arc<std::sync::Mutex<Option<String>>>,
    paste_error: Option<String>,
    focused: bool,
    goto_line: Option<usize>,
    goto_column: Option<usize>,
    diagnostic_hash: Option<u64>,
    diagnostic: Option<SourceDiagnostic>,
    tab_width: usize,
}

impl Default for YamlLayoutCache {
    fn default() -> Self {
        Self {
            job: None,
            font: egui::FontId::default(),
            dark: false,
            foreground: Color32::default(),
            language: DecoderLanguage::default(),
            edit_action: None,
            paste: Default::default(),
            paste_error: None,
            focused: false,
            goto_line: None,
            goto_column: None,
            diagnostic_hash: None,
            diagnostic: None,
            tab_width: 2,
        }
    }
}

#[derive(Clone)]
struct SourceDiagnostic {
    line: Option<usize>,
    column: Option<usize>,
    message: String,
    hint: Option<String>,
}

impl YamlLayoutCache {
    pub(crate) fn tab_width(&self) -> usize { self.tab_width }
    pub(crate) fn set_tab_width(&mut self, width: usize) {
        self.tab_width = width.clamp(1, 8);
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn focused(&self) -> bool { self.focused }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn queue_paste(&mut self, text: String) {
        if text.len() <= 256 * 1024 {
            *self.paste.lock().unwrap() = Some(text);
            self.paste_error = None;
        } else {
            self.paste_error = Some("Paste is limited to 256 KiB.".into());
        }
    }
    pub fn command(&mut self, action: &'static str) { self.edit_action = Some(action); }
    pub fn go_to_line(&mut self, line: usize) {
        self.goto_line = Some(line.max(1));
        self.goto_column = Some(1);
    }
    fn go_to_location(&mut self, line: usize, column: usize) {
        self.goto_line = Some(line.max(1));
        self.goto_column = Some(column.max(1));
    }
    fn tab_width_menu(&mut self, ui: &mut Ui) {
        ui.menu_button(format!("Tab: {} spaces", self.tab_width), |ui| {
            ui.set_min_width(180.);
            ui.horizontal(|ui| {
                ui.label("Spaces");
                ui.add(
                    egui::DragValue::new(&mut self.tab_width)
                        .clamp_range(1..=8)
                        .speed(1),
                );
            });
            ui.weak("Tab indents; Shift+Tab dedents; Enter continues indentation.");
        });
    }
    fn edit_menu(&mut self, ui: &mut Ui) {
        for action in ["Undo", "Redo", "Cut", "Copy", "Paste", "Select all", "Toggle comments"] {
            if ui.button(action).clicked() {
                self.edit_action = Some(action);
                ui.close_menu();
            }
        }
        ui.separator();
        self.tab_width_menu(ui);
        if ui.button("Convert leading tabs to spaces").clicked() {
            self.edit_action = Some("Convert indentation");
            ui.close_menu();
        }
    }
    pub(crate) fn layout(
        &mut self,
        ui: &Ui,
        text: &str,
        font: &egui::FontId,
        language: DecoderLanguage,
    ) -> Arc<egui::Galley> {
        let dark = ui.visuals().dark_mode;
        let foreground = ui.visuals().text_color();
        if self.job.as_ref().is_none_or(|job| job.text != text)
            || self.font != *font
            || self.dark != dark
            || self.foreground != foreground
            || self.language != language
        {
            self.job = Some(source_layout(
                text,
                font.clone(),
                dark,
                foreground,
                language,
            ));
            self.language = language;
            self.font = font.clone();
            self.dark = dark;
            self.foreground = foreground;
        }
        ui.fonts(|fonts| fonts.layout_job(self.job.as_ref().unwrap().clone()))
    }

    pub(crate) fn editor(&mut self, ui: &mut Ui, yaml: &mut String, language: DecoderLanguage) {
        if let Some(error)=&self.paste_error { ui.colored_label(ui.visuals().error_fg_color,error); }
        if language == DecoderLanguage::Yaml {
            ui.horizontal_wrapped(|ui| {
                let dark = ui.visuals().dark_mode;
                let compact = ui.ctx().screen_rect().width() < 900.;
                if !compact { ui.colored_label(if dark { Color32::from_rgb(140,194,238) } else { Color32::from_rgb(25,89,149) }, "Keyword");
                ui.colored_label(builtin_color(dark), "Built-in type");
                ui.label("User-defined name"); }
                ui.menu_button("YAML help", |ui| {
                    ui.set_max_width(330.);
                    if compact {
                        ui.colored_label(builtin_color(dark), "Teal: built-in types");
                        ui.label("Blue: keywords · Normal text: user-defined names");
                    }
                    ui.label("Declare a reusable type with name; use structure to reference it.");
                    ui.code("structures:\n  - name: RecordType\n    fields:\n      - {name: value, type: u16}\nactions:\n  - read: {name: records, type: {structure: RecordType}, repeat: 2}");
                    ui.label("Declarations read no bytes. Actions start reads; fields consume bytes in order. A field using a structure creates nested values.");
                    ui.label("repeat counts items; length bounds each item in bytes. ref uses an earlier decoded value. Names are chosen by you.");
                    ui.separator();
                    ui.label("Open Help → Decoder guide for every supported type, field option, action, expression, and a complete Python API reference.");
                });
                self.tab_width_menu(ui);
            });
            self.refresh_diagnostic(yaml);
            if let Some(diagnostic) = self.diagnostic.clone() {
                egui::Frame::none()
                    .fill(ui.visuals().error_fg_color.gamma_multiply(0.08))
                    .stroke(egui::Stroke::new(1_f32, ui.visuals().error_fg_color.gamma_multiply(0.55)))
                    .inner_margin(egui::Margin::symmetric(8., 5.))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.colored_label(ui.visuals().error_fg_color, "YAML error");
                            if let Some(line) = diagnostic.line {
                                let location = diagnostic.column.map_or_else(
                                    || format!("Line {line}"),
                                    |column| format!("Line {line}:{column}"),
                                );
                                let column = diagnostic.column.unwrap_or(1);
                                if ui.small_button(location)
                                    .on_hover_text(format!("Jump to line {line}, column {column}"))
                                    .clicked()
                                {
                                    self.go_to_location(line, column);
                                }
                            }
                            ui.label(diagnostic.message);
                        });
                        if let Some(line) = diagnostic.line {
                            if let Some(source_line) = yaml.lines().nth(line.saturating_sub(1)) {
                                ui.monospace(format!("{line:>4} | {source_line}"));
                                if let Some(column) = diagnostic.column {
                                    ui.colored_label(
                                        ui.visuals().error_fg_color,
                                        format!("     | {}^", " ".repeat(column.saturating_sub(1))),
                                    );
                                }
                            }
                        }
                        if let Some(hint) = diagnostic.hint {
                            ui.label(format!("Fix: {hint}"));
                        }
                    });
            }
        } else {
            ui.horizontal(|ui| self.tab_width_menu(ui));
        }
        let mut font = egui::TextStyle::Monospace.resolve(ui.style());
        font.size = font.size.max(13.);
        let lines = yaml.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let gutter_width = ui.fonts(|fonts| fonts.glyph_width(&font, '0'))
            * lines.to_string().len().max(2) as f32
            + 20.;
        egui::Frame::none()
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(egui::Margin::same(6.))
            .show(ui, |ui| {
                let size = ui.available_size();
                let mut caret = None;
                let scroll = egui::ScrollArea::both()
                    .id_source(("decoder_source", language))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            let (gutter, _) = ui.allocate_exact_size(
                                egui::vec2(gutter_width, 0.),
                                egui::Sense::hover(),
                            );
                            let text_width =
                                (size.x - gutter_width - ui.spacing().item_spacing.x).max(80.);
                            let editor_id = ui.make_persistent_id(egui::Id::new((
                                "decoder_source_text",
                                language,
                            )));
                            let selection_before = egui::TextEdit::load_state(ui.ctx(), editor_id)
                                .and_then(|state| state.cursor.char_range());
                            if let Some(paste) = self.paste.lock().unwrap().take() {
                                edit_source(ui.ctx(), editor_id, yaml, "Paste", Some(&paste));
                            }
                            let focused = ui.memory(|memory| memory.has_focus(editor_id));
                            self.focused = focused;
                            if focused {
                                let shortcut = ui.input_mut(|input| {
                                    if input.consume_key(egui::Modifiers::COMMAND, egui::Key::C) {
                                        Some("Copy")
                                    } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::X) {
                                        Some("Cut")
                                    } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::V) {
                                        Some("Paste")
                                    } else {
                                        None
                                    }
                                });
                                if shortcut.is_some() {
                                    self.edit_action = shortcut;
                                }
                            }
                            if let Some(action) = self.edit_action.take() {
                                if action == "Paste" {
                                    #[cfg(not(target_arch = "wasm32"))]
                                    { match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
                                        Ok(text)=>{*self.paste.lock().unwrap()=Some(text);self.paste_error=None;ui.ctx().request_repaint();},
                                        Err(error)=>self.paste_error=Some(format!("Could not paste: {error}. You can also use Ctrl+V.")),
                                    }}
                                    #[cfg(target_arch = "wasm32")]
                                    {
                                        let pending = self.paste.clone();
                                        let ctx = ui.ctx().clone();
                                        wasm_bindgen_futures::spawn_local(async move {
                                            if let Ok(value) = wasm_bindgen_futures::JsFuture::from(crate::browser::paste_text()).await {
                                                *pending.lock().unwrap() = value.as_string();
                                                ctx.request_repaint();
                                            }
                                        });
                                    }

                                    ui.memory_mut(|m| m.request_focus(editor_id));
                                } else if action == "Convert indentation" {
                                    convert_source_indentation(
                                        ui.ctx(),
                                        editor_id,
                                        yaml,
                                        self.tab_width,
                                    );
                                } else {
                                    edit_source(ui.ctx(), editor_id, yaml, action, None);
                                }
                            }
                            if let Some(line) = self.goto_line.take() {
                                let column = self.goto_column.take().unwrap_or(1);
                                let index = source_char_index(yaml, line, column);
                                let mut state = egui::TextEdit::load_state(ui.ctx(), editor_id)
                                    .unwrap_or_default();
                                state.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                                    egui::text::CCursor::new(index),
                                )));
                                state.store(ui.ctx(), editor_id);
                                ui.memory_mut(|memory| memory.request_focus(editor_id));
                            }
                            let toggle = focused
                                && ui.input_mut(|input| {
                                    input.consume_key(egui::Modifiers::COMMAND, egui::Key::Slash)
                                });
                            // egui 0.27 also matches Shift+Command+Z as Undo; handle Redo
                            // first so the standard shortcut works in the source editors.
                            let redo = focused
                                && ui.input_mut(|input| {
                                    input.consume_key(
                                        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                                        egui::Key::Z,
                                    )
                                });
                            let indent = focused.then(|| {
                                ui.input_mut(|input| {
                                    if input.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab) {
                                        Some(true)
                                    } else if input.consume_key(
                                        egui::Modifiers::NONE,
                                        egui::Key::Tab,
                                    ) {
                                        Some(false)
                                    } else {
                                        None
                                    }
                                })
                            }).flatten();
                            let newline = focused
                                && ui.input_mut(|input| {
                                    input.consume_key(
                                        egui::Modifiers::NONE,
                                        egui::Key::Enter,
                                    )
                                });
                            let mut shortcut_changed = false;
                            if newline {
                                insert_auto_indented_newline(
                                    ui.ctx(),
                                    editor_id,
                                    yaml,
                                    self.tab_width,
                                    language,
                                );
                                shortcut_changed = true;
                            } else if let Some(dedent) = indent {
                                indent_source(
                                    ui.ctx(),
                                    editor_id,
                                    yaml,
                                    self.tab_width,
                                    dedent,
                                );
                                shortcut_changed = true;
                            } else if toggle || redo {
                                if let Some(mut state) =
                                    egui::TextEdit::load_state(ui.ctx(), editor_id)
                                {
                                    if let Some(selection) = state.cursor.char_range() {
                                        let mut undoer = state.undoer();
                                        let next = if toggle {
                                            undoer.add_undo(&(selection, yaml.clone()));
                                            let selection = toggle_source_comments(yaml, selection);
                                            undoer.feed_state(
                                                ui.input(|input| input.time),
                                                &(selection, yaml.clone()),
                                            );
                                            Some((selection, yaml.clone()))
                                        } else {
                                            undoer.redo(&(selection, yaml.clone())).cloned()
                                        };
                                        if let Some((selection, source)) = next {
                                            *yaml = source;
                                            state.cursor.set_char_range(Some(selection));
                                            shortcut_changed = true;
                                        }
                                        state.set_undoer(undoer);
                                        state.store(ui.ctx(), editor_id);
                                    }
                                }
                            }
                            let mut layouter = |ui: &Ui, text: &str, _width: f32| {
                                self.layout(ui, text, &font, language)
                            };
                            let mut output = egui::TextEdit::multiline(yaml)
                                .id(editor_id)
                                .code_editor()
                                .font(font.clone())
                                .frame(false)
                                .desired_width(text_width)
                                .min_size(egui::vec2(text_width, size.y))
                                .layouter(&mut layouter)
                                .hint_text(match language {
                                    DecoderLanguage::Yaml => {
                                        "Write decoder YAML here, or open a YAML file."
                                    }
                                    DecoderLanguage::Python => {
                                        "Write Python here, or open a Python decoder."
                                    }
                                })
                                .show(ui);
                            caret = output.cursor_range.map(|range| {
                                (
                                    range.primary.rcursor.row + 1,
                                    range.primary.rcursor.column + 1,
                                )
                            });
                            let secondary_pressed = ui.input(|input| {
                                input.pointer.button_pressed(egui::PointerButton::Secondary)
                                    && input.pointer.interact_pos().is_some_and(|position| {
                                        output.response.rect.contains(position)
                                    })
                            });
                            if (secondary_pressed || output.response.secondary_clicked())
                                && selection_before.is_some_and(|selection| {
                                    selection.primary.index != selection.secondary.index
                                })
                            {
                                let mut state = egui::TextEdit::load_state(ui.ctx(), editor_id)
                                    .unwrap_or_default();
                                state.cursor.set_char_range(selection_before);
                                state.store(ui.ctx(), editor_id);
                                ui.memory_mut(|memory| memory.request_focus(editor_id));
                                ui.ctx().request_repaint();
                            }
                            if language == DecoderLanguage::Yaml {
                                if let Some(diagnostic) = &self.diagnostic {
                                    paint_source_diagnostic(ui, &output, diagnostic);
                                }
                            }
                            output.response.context_menu(|ui| { self.edit_menu(ui); });
                            if shortcut_changed {
                                output.response.mark_changed();
                            }
                            let gutter = egui::Rect::from_min_max(
                                gutter.min,
                                egui::pos2(gutter.right(), output.response.rect.bottom()),
                            );
                            ui.painter().vline(
                                gutter.right(),
                                gutter.y_range(),
                                ui.visuals().widgets.noninteractive.bg_stroke,
                            );
                            let clip = ui.clip_rect();
                            for (index, row) in output.galley.rows.iter().enumerate() {
                                let y = output.galley_pos.y + row.rect.center().y;
                                if y >= clip.top() - row.rect.height()
                                    && y <= clip.bottom() + row.rect.height()
                                {
                                    ui.painter().text(
                                        egui::pos2(gutter.right() - 8., y),
                                        egui::Align2::RIGHT_CENTER,
                                        (index + 1).to_string(),
                                        font.clone(),
                                        ui.visuals().weak_text_color(),
                                    );
                                }
                            }
                        });
                    });
                paint_caret_status(ui, scroll.inner_rect, &font, caret);
            });
    }

    fn refresh_diagnostic(&mut self, source: &str) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hasher);
        let hash = hasher.finish();
        if self.diagnostic_hash == Some(hash) {
            return;
        }
        self.diagnostic_hash = Some(hash);
        self.diagnostic = match serde_yaml_ng::from_str::<crate::structure::Program>(source) {
            Err(error) => {
                let location = error.location();
                let line = location.as_ref().map(|location| location.line());
                let message = format!("read structure YAML: {error}");
                Some(SourceDiagnostic {
                    line,
                    column: location.as_ref().map(|location| location.column()),
                    hint: diagnostic_hint(source, line, &message),
                    message,
                })
            }
            Ok(program) if program.actions.is_empty() => None,
            Ok(_) => crate::structure::Program::parse(source).err().map(|error| {
                let location = error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<serde_yaml_ng::Error>())
                    .and_then(serde_yaml_ng::Error::location);
                let line = location.as_ref().map(|location| location.line());
                let message = format!("{error:#}");
                SourceDiagnostic {
                    line,
                    column: location.as_ref().map(|location| location.column()),
                    hint: diagnostic_hint(source, line, &message),
                    message,
                }
            }),
        };
    }
}

fn paint_caret_status(
    ui: &Ui,
    rect: egui::Rect,
    font: &egui::FontId,
    caret: Option<(usize, usize)>,
) {
    let Some((line, column)) = caret else { return; };
    let label = format!("Ln {line}, Col {column}");
    let digit = ui.fonts(|fonts| fonts.glyph_width(font, '0')).max(1.);
    let size = egui::vec2(digit * label.chars().count() as f32 + 14., font.size + 8.);
    let badge = egui::Rect::from_min_size(
        egui::pos2(rect.right() - size.x - 4., rect.bottom() - size.y - 4.),
        size,
    );
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(badge, 3., ui.visuals().extreme_bg_color);
    painter.rect_stroke(
        badge,
        3.,
        egui::Stroke::new(1_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
    painter.text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        label,
        font.clone(),
        ui.visuals().weak_text_color(),
    );
}

fn paint_source_diagnostic(
    ui: &Ui,
    output: &egui::text_edit::TextEditOutput,
    diagnostic: &SourceDiagnostic,
) {
    let Some(row_index) = diagnostic.line.and_then(|line| line.checked_sub(1)) else {
        return;
    };
    let Some(row) = output.galley.rows.get(row_index) else {
        return;
    };
    let row_rect = row.rect.translate(output.galley_pos.to_vec2());
    let line_rect = egui::Rect::from_min_max(
        egui::pos2(output.text_clip_rect.left(), row_rect.top()),
        egui::pos2(output.text_clip_rect.right(), row_rect.bottom()),
    )
    .intersect(output.text_clip_rect);
    if !line_rect.is_positive() {
        return;
    }
    let color = ui.visuals().error_fg_color;
    let painter = ui.painter().with_clip_rect(output.text_clip_rect);
    painter.rect_filled(line_rect, 0., color.gamma_multiply(0.10));
    painter.vline(
        line_rect.left() + 1.,
        line_rect.y_range(),
        egui::Stroke::new(2_f32, color),
    );
    if let Some(column) = diagnostic.column {
        let caret = output
            .galley
            .pos_from_rcursor(egui::epaint::text::cursor::RCursor {
            row: row_index,
            column: column.saturating_sub(1),
        });
        let x = output.galley_pos.x + caret.left();
        let width = ui
            .fonts(|fonts| fonts.glyph_width(&egui::TextStyle::Monospace.resolve(ui.style()), '0'))
            .max(3.);
        let column_rect = egui::Rect::from_min_max(
            egui::pos2(x, row_rect.top()),
            egui::pos2(x + width, row_rect.bottom()),
        )
        .intersect(output.text_clip_rect);
        painter.rect_stroke(column_rect, 1., egui::Stroke::new(1.5_f32, color));
        painter.hline(
            column_rect.x_range(),
            column_rect.bottom() - 1.,
            egui::Stroke::new(2_f32, color),
        );
    }
}

fn source_char_index(source: &str, line: usize, column: usize) -> usize {
    let target_line = line.max(1);
    let mut line_number = 1;
    let mut line_start = 0;
    let chars: Vec<char> = source.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if line_number == target_line {
            line_start = index;
            break;
        }
        if *ch == '\n' {
            line_number += 1;
            line_start = index + 1;
        }
    }
    if line_number < target_line {
        return chars.len();
    }
    let line_length = chars[line_start..]
        .iter()
        .position(|ch| *ch == '\n')
        .unwrap_or(chars.len() - line_start);
    line_start + column.saturating_sub(1).min(line_length)
}

fn diagnostic_hint(source: &str, line: Option<usize>, message: &str) -> Option<String> {
    let line = line?;
    let lines: Vec<&str> = source.lines().collect();
    let current = *lines.get(line.saturating_sub(1))?;
    if current.chars().take_while(|ch| ch.is_whitespace()).any(|ch| ch == '\t') {
        return Some("YAML indentation cannot contain tabs. Right-click the editor and choose Convert leading tabs to spaces.".into());
    }
    if current.chars().any(|ch| matches!(ch, '‘' | '’' | '“' | '”')) {
        return Some("Replace typographic quotes with plain ASCII ' or \" characters.".into());
    }
    if message.contains("mapping values are not allowed") {
        let current_indent = current.len() - current.trim_start_matches(' ').len();
        let current_key = current.trim_start().split_once(':').map(|(key, _)| key.trim());
        let previous = lines[..line.saturating_sub(1)]
            .iter()
            .rev()
            .copied()
            .find(|candidate| {
                let candidate = candidate.trim();
                !candidate.is_empty() && !candidate.starts_with('#')
            });
        if let (Some(current_key), Some(previous)) = (current_key, previous) {
            let previous_indent = previous.len() - previous.trim_start_matches(' ').len();
            if current_indent > previous_indent {
                if let Some((previous_key, value)) = previous.trim_start().split_once(':') {
                    if !value.trim().is_empty() {
                        return Some(format!(
                            "`{current_key}` is indented under `{}`, but `{}` already has the value `{}`. Remove {} leading spaces so `{current_key}` aligns with `{}`.",
                            previous_key.trim(),
                            previous_key.trim(),
                            value.trim(),
                            current_indent - previous_indent,
                            previous_key.trim(),
                        ));
                    }
                }
            }
        }
        return Some("Check this line's indentation and align it with the sibling key above it.".into());
    }
    None
}

/// The decoder language reference is rendered inside the application on every platform.
pub(crate) fn decoding_guide(ui: &mut Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.heading("Binwave decoder guide");
            ui.label("A decoder describes how bytes become named values. Open a data file, edit the Definition, then press ▶ Run (F5). A successful run publishes the same decoded structure to Plot, Samples, Inspector, and Hex.");
            ui.label("This guide documents Binwave's decoder types and cursor rules. YAML is the declarative format; Python is available when the layout needs loops, validation, or other program logic.");
            ui.separator();

            egui::CollapsingHeader::new("How parsing works")
                .default_open(true)
                .show(ui, |ui| {
                    ui.label("1. Binwave loads every static declaration under structures into a type catalog. Declaring a structure consumes no bytes, and static declaration order does not matter.");
                    ui.label("2. The byte cursor starts at offset 0. Top-level actions execute in order. A structure's fields then execute in their listed order, followed by that structure's actions.");
                    ui.label("3. A field reads at the current cursor unless at, relative, or align moves it. The decoded value is stored under the field name and the cursor advances by the bytes consumed.");
                    ui.label("4. ref retrieves an earlier decoded value. It does not reread, forward, or consume that value's bytes.");
                    ui.label("5. choose uses an earlier value to select one type for the current field. The chosen type begins at the current cursor.");
                    ui.label("6. length creates a bounded byte region. The chosen or nested type cannot escape it; unused bytes are consumed as padding so the next field starts at the region's end.");
                    ui.label("7. repeat reads multiple items. It belongs on the field or read action that actually repeats; choose by itself never repeats.");
                    ui.separator();
                    ui.strong("Small fixed-record example");
                    ui.code("endian: little\nstructures:\n  - name: Record\n    fields:\n      - {name: timestamp, type: u32}\n      - {name: measurement, type: f32}\nactions:\n  - read:\n      name: records\n      type: {structure: Record}\n      repeat: until end");
                    ui.label("Each Record consumes eight bytes. The read action repeats complete records until the file boundary.");
                });

            egui::CollapsingHeader::new("Built-in decoder types")
                .default_open(true)
                .show(ui, |ui| {
                    ui.label("These are Binwave schema types implemented by the decoder engine. Their names resemble Rust types, but they are not arbitrary Rust or Python types.");
                    egui::Grid::new("decoder-guide-types")
                        .striped(true)
                        .num_columns(3)
                        .show(ui, |ui| {
                            ui.strong("Type"); ui.strong("Bytes"); ui.strong("Meaning"); ui.end_row();
                            ui.code("u8 / i8"); ui.label("1"); ui.label("Unsigned / signed integer"); ui.end_row();
                            ui.code("u16 / i16"); ui.label("2"); ui.label("Unsigned / signed integer"); ui.end_row();
                            ui.code("u32 / i32"); ui.label("4"); ui.label("Unsigned / signed integer"); ui.end_row();
                            ui.code("u64 / i64"); ui.label("8"); ui.label("Unsigned / signed integer"); ui.end_row();
                            ui.code("f32"); ui.label("4"); ui.label("IEEE-754 floating point"); ui.end_row();
                            ui.code("f64"); ui.label("8"); ui.label("IEEE-754 floating point"); ui.end_row();
                            ui.code("bytes"); ui.label("length"); ui.label("Raw bytes; normally requires length"); ui.end_row();
                            ui.code("text"); ui.label("length"); ui.label("Fixed-length UTF-8 text"); ui.end_row();
                            ui.code("cstring"); ui.label("variable"); ui.label("Zero-terminated UTF-8 text"); ui.end_row();
                            ui.code("varuint"); ui.label("variable"); ui.label("Unsigned LEB128 integer"); ui.end_row();
                        });
                    ui.label("Integers remain exact in decoded data; plotting alone converts numeric values to floating point. Multi-byte numeric types inherit endian: little or endian: big from the file or structure, and a field can override it.");
                    ui.code("endian: little\n# ...\n- {name: network_value, type: u32, endian: big}");
                    ui.label("A declared structure is referenced explicitly: type: {structure: Record}. Bare user-defined type names are rejected so they cannot be confused with built-ins.");
                });

            egui::CollapsingHeader::new("Structures, nesting, and declaration order")
                .default_open(true)
                .show(ui, |ui| {
                    ui.label("structures is a reusable type catalog, not the file's top-level byte order. Static structures may reference declarations written earlier or later in that list because the complete catalog is loaded before parsing begins.");
                    ui.code("structures:\n  - name: Envelope\n    fields:\n      - {name: body, type: {structure: Body}}\n  - name: Body\n    fields:\n      - {name: value, type: u16}");
                    ui.label("Nesting does not automatically imply a count, size, pointer, tag, or terminator. Declare those fields only when the file actually stores them, and use them explicitly with ref, length, repeat, or choose.");
                    ui.label("Execution order still matters: fields and actions read bytes in order, references can only use values already decoded, and a runtime structure produced by define must be created before a later read uses it.");
                });

            egui::CollapsingHeader::new("Cursor movement, bounds, and field options")
                .default_open(true)
                .show(ui, |ui| {
                    egui::Grid::new("decoder-guide-field-options")
                        .striped(true)
                        .num_columns(2)
                        .show(ui, |ui| {
                            ui.code("length: 16"); ui.label("Bound one item to 16 bytes; unused bytes become padding"); ui.end_row();
                            ui.code("length: remaining"); ui.label("Consume through the enclosing region's end"); ui.end_row();
                            ui.code("repeat: 12"); ui.label("Read 12 consecutive items"); ui.end_row();
                            ui.code("repeat: {ref: count}"); ui.label("Use an earlier decoded item count"); ui.end_row();
                            ui.code("repeat: until end"); ui.label("Read complete items to the enclosing region's end"); ui.end_row();
                            ui.code("at: 128"); ui.label("Move to absolute file offset 128 before reading"); ui.end_row();
                            ui.code("relative: 4"); ui.label("Move four bytes from the current cursor"); ui.end_row();
                            ui.code("align: 8"); ui.label("Advance to the next absolute eight-byte boundary"); ui.end_row();
                            ui.code("endian: big"); ui.label("Override inherited byte order for this field"); ui.end_row();
                            ui.code("expect: 42"); ui.label("Fail unless the decoded numeric or text value matches"); ui.end_row();
                        });
                    ui.label("A field's length applies to each repeated item, not the array as a whole. To bound a whole repeated payload, put the repeated field inside a containing structure and apply length to that containing structure.");
                    ui.label("at and relative change where subsequent reading continues; they are not temporary peeks. No read may escape an explicitly bounded parent region. A partial final item is an error rather than silently ignored data.");
                });

            egui::CollapsingHeader::new("Tagged and length-delimited data with choose")
                .default_open(true)
                .show(ui, |ui| {
                    ui.label("choose selects the type for one field at the current cursor. The tag has already been consumed; ref only looks up its decoded value. The selected structure receives the bytes that follow the tag and length fields.");
                    ui.code("structures:\n  - name: VariantA\n    fields:\n      - {name: value, type: u32}\n\n  - name: Section\n    fields:\n      - {name: kind, type: u8}\n      - {name: payload_bytes, type: u16}\n      - name: payload\n        type:\n          choose:\n            ref: kind\n            cases:\n              '1': {structure: VariantA}\n            otherwise: bytes\n        length: {ref: payload_bytes}\n\nactions:\n  - read:\n      name: sections\n      type: {structure: Section}\n      repeat: until end");
                    ui.label("Here each outer Section repeats. Every section reads its own tag and length, chooses one payload type, and advances to the end of that payload's bounded region before the next section begins.");
                    ui.label("A tagged terminal payload can choose its length too. Use remaining for the terminal case and the stored byte count otherwise; this stays a field and does not require a choose action.");
                    ui.code("length:\n  choose:\n    ref: kind\n    cases:\n      '0': remaining\n    otherwise: {ref: payload_bytes}");
                    ui.label("remaining means the bytes from the payload's current cursor through its enclosing boundary. At top level that boundary is the file end; inside a length-bounded parent it is that parent's end.");
                    ui.label("If one tag selects a homogeneous array instead, put repeat on the payload field. If a chosen region contains its own mixed records, reference a container structure whose inner record field uses repeat: until end.");
                    ui.label("otherwise is recommended for forward-compatible formats. Using otherwise: bytes preserves unknown payloads and their exact byte ranges instead of failing on an unfamiliar tag.");
                });

            egui::CollapsingHeader::new("References and integer expressions")
                .show(ui, |ui| {
                    ui.label("An unqualified reference searches the current decoded element and then its enclosing scopes. A leading slash anchors the path at file level. References name decoded values, not structure declarations.");
                    ui.code("{ref: count}\n{ref: header.count}\n{ref: /header.entries[0].size}");
                    ui.label("Expressions use checked integer arithmetic. add, subtract, multiply, and divide accept two operands, and either operand may be another expression or reference. Division must be exact.");
                    ui.code("length: {subtract: [{ref: total_bytes}, 4]}\nrepeat: {divide: [{ref: payload_bytes}, 8]}\nrelative: {add: [{ref: gap}, 2]}");
                    ui.label("Missing or forward references, negative sizes, overflow, division remainders, and out-of-region reads are reported as decoder errors.");
                });

            egui::CollapsingHeader::new("YAML actions")
                .show(ui, |ui| {
                    ui.label("Every action contains exactly one of read, define, skip, align, calculate, or choose.");
                    egui::Grid::new("decoder-guide-actions")
                        .striped(true)
                        .num_columns(2)
                        .show(ui, |ui| {
                            ui.code("read"); ui.label("Decode a named field using the normal field options"); ui.end_row();
                            ui.code("skip"); ui.label("Advance the cursor by a byte count"); ui.end_row();
                            ui.code("align"); ui.label("Advance to an absolute alignment boundary"); ui.end_row();
                            ui.code("calculate"); ui.label("Publish a named checked-integer expression without reading bytes"); ui.end_row();
                            ui.code("choose"); ui.label("Run one action list selected by an earlier value"); ui.end_row();
                            ui.code("define"); ui.label("Create a new structure from previously decoded descriptions"); ui.end_row();
                        });
                    ui.code("actions:\n  - read: {name: header, type: {structure: Header}}\n  - calculate: {name: data_bytes, value: {multiply: [{ref: header.count}, 4]}}\n  - align: 8\n  - choose:\n      ref: header.kind\n      cases:\n        '1':\n          - read: {name: body, type: {structure: Body}}\n      otherwise:\n        - skip: {ref: data_bytes}");
                });

            egui::CollapsingHeader::new("Structures generated from file metadata")
                .show(ui, |ui| {
                    ui.label("define is different from choose. choose selects an existing type; define creates a new type from an earlier description collection. Creating the type consumes no payload bytes—the later read does that.");
                    ui.code("- define:\n    name: GeneratedRecord\n    from: description.fields\n    fields:\n      name: {ref: label}\n      type: {ref: format, cases: {'1': u16, '2': i16, '3': f32}}\n      repeat: {ref: count}");
                    ui.label("Optional mappings also include length, at, and relative. Generated names and formats come from decoded data, but the mapping from numeric format codes to Binwave types belongs to the decoder definition.");
                    ui.label("Because define executes during parsing, the description bytes and define action must occur before any read that references GeneratedRecord.");
                });

            egui::CollapsingHeader::new("Checksums, trailers, and validation")
                .show(ui, |ui| {
                    ui.label("Place a stored checksum after the bounded data it follows. It is a separate field; choose decodes only the payload region before it.");
                    ui.code("- name: Block\n  fields:\n    - {name: payload_bytes, type: u16}\n    - {name: payload, type: bytes, length: {ref: payload_bytes}}\n    - {name: stored_checksum, type: u32, endian: big}");
                    ui.label("If the stored length includes the checksum itself, subtract the checksum width when bounding the payload. The checksum field's byte order is a format rule separate from the checksum algorithm's byte range.");
                    ui.label("YAML currently displays stored checksum values but does not calculate, verify, or rebuild checksums. Use Python for calculated validation, and remember that some formats cover only payload bytes while others also cover tag or header bytes.");
                });

            egui::CollapsingHeader::new("Python quick start and API")
                .show(ui, |ui| {
                    ui.label("Python field strings such as 'u32' use the same Binwave schema names; they are not Python type annotations. Python is useful when a format needs loops, conditions, calculated validation, or structure discovery that is awkward in declarative YAML.");
                    ui.code("from binwave import Field, Struct\n\nHeader = Struct('Header', [('count', 'u32')], endian='little')\nRecord = Struct('Record', [\n    ('timestamp', 'u32'),\n    Field('measurement', 'f32'),\n])\nheader = file.read_struct('header', Header)\nrecords = file.records('records', Record, count=header.count)\nprint(f'{len(records)} records')");
                    ui.label("Field(name, type, length=None, count=None, offset=None, endian=None, expect=None) accepts a fixed numeric type, text, bytes, or another Struct. (name, type) is shorthand.");
                    ui.label("Struct(name, fields, endian=None) creates a fixed layout and exposes size and fields.");
                    ui.label("file.size is the byte length. file.tell() and file.seek(offset, whence=0) control the cursor. file.read(length) advances it; file.read_at(offset, length) does not. file.unpack(format, offset=None) uses Python struct formats.");
                    ui.label("file.read_struct(name, struct, offset=None) decodes metadata now. file.records(name, struct, count=None, offset=None) registers a lazy collection for Plot and Samples. Without count, records consumes complete records to the end of the file.");
                    ui.strong("Calculated checksum example");
                    ui.code("import zlib\n\ndef crc32_range(start, length):\n    crc = 0\n    while length:\n        size = min(length, 1024 * 1024)\n        crc = zlib.crc32(file.read_at(start, size), crc)\n        start += size\n        length -= size\n    return crc & 0xFFFFFFFF\n\nif crc32_range(payload_start, payload_bytes) != stored_checksum:\n    raise ValueError('checksum mismatch')");
                    ui.label("A single read/read_at call is limited to 1 MiB, so calculated validation over larger ranges must use bounded chunks as shown above.");
                    ui.label("Python supports ordinary imports, functions, loops, conditions, assertions, and print(). In the web app it runs locally in the browser; on desktop it runs with the selected Python interpreter.");
                });

            egui::CollapsingHeader::new("Large files and lazy decoding")
                .show(ui, |ui| {
                    ui.label("Fixed-size collections are kept lazy: Binwave stores the collection plan and decodes only the pages requested by Plot, Samples, Inspector, or Hex. This is the preferred representation for very large recordings.");
                    ui.label("Variable-size records must be scanned to discover their boundaries. A tagged record containing choose is variable-size unless every selected layout has a known common stride. Keep each record length-delimited when the format provides a length.");
                    ui.label("Use file.records in Python or a fixed-size repeated structure in YAML for large homogeneous arrays. Avoid turning a large raw payload into one decoded node per byte.");
                });

            egui::CollapsingHeader::new("Editing and troubleshooting")
                .show(ui, |ui| {
                    ui.label("YAML syntax and indentation are checked while you type. Select a reported line to jump there. Run-time errors remain in the Console because they depend on the open data.");
                    ui.label("Copy, cut, paste, undo, redo, select all, and toggle comments are available from Edit → Definition editing and from the editor context menu. Ctrl/Cmd+C, X, and V use the same selection.");
                    ui.label("Tab inserts spaces to the next indentation stop; Shift+Tab dedents selected lines. Enter preserves indentation and adds one level after YAML or Python block openers. Right-click the editor to choose a tab width from 1–8 spaces or convert existing leading tabs.");
                    ui.label("Use the line numbers and the bottom-right Ln/Col indicator to locate errors. The highlighted source line and exact reported column show where YAML stopped understanding the definition.");
                    ui.label("Start with Decoder → Examples, run after each small change, and compare the decoded byte ranges with the format specification. Keep an otherwise: bytes branch while investigating unknown variants.");
                    ui.label("Click a decoded value or plot point to reveal its exact bytes. Shift-click a plot sample to select the complete range from the anchor through that sample.");
                });
        });
}

#[derive(Clone)]
struct SourceEdit {
    start: usize,
    remove: usize,
    insert: String,
}

fn apply_source_edits(
    text: &mut String,
    selection: egui::text::CCursorRange,
    edits: &[SourceEdit],
) -> egui::text::CCursorRange {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    for edit in edits {
        let start = edit.start.min(chars.len());
        result.extend(chars[copied..start].iter());
        result.push_str(&edit.insert);
        copied = (start + edit.remove).min(chars.len());
    }
    result.extend(chars[copied..].iter());
    *text = result;

    let transform = |index: usize| {
        let mut shift = 0_isize;
        for edit in edits {
            let end = edit.start + edit.remove;
            let inserted = edit.insert.chars().count();
            if index < edit.start {
                break;
            }
            if index < end {
                return edit.start
                    .checked_add_signed(shift)
                    .unwrap_or_default()
                    + inserted;
            }
            shift += inserted as isize - edit.remove as isize;
        }
        index.checked_add_signed(shift).unwrap_or_default()
    };
    egui::text::CCursorRange {
        primary: egui::text::CCursor::new(transform(selection.primary.index)),
        secondary: egui::text::CCursor::new(transform(selection.secondary.index)),
    }
}

fn selected_line_starts(text: &[char], selection: egui::text::CCursorRange) -> Vec<usize> {
    let [first, last] = selection.sorted();
    let first = first.index.min(text.len());
    let last = last.index.min(text.len());
    let included_last = if first < last { last - 1 } else { last };
    let mut start = text[..first]
        .iter()
        .rposition(|ch| *ch == '\n')
        .map_or(0, |position| position + 1);
    let mut starts = Vec::new();
    loop {
        starts.push(start);
        let Some(next) = text[start..]
            .iter()
            .position(|ch| *ch == '\n')
            .map(|position| start + position + 1)
        else {
            break;
        };
        if next > included_last {
            break;
        }
        start = next;
    }
    starts
}

fn visual_column_at(text: &[char], index: usize, tab_width: usize) -> usize {
    let start = text[..index.min(text.len())]
        .iter()
        .rposition(|ch| *ch == '\n')
        .map_or(0, |position| position + 1);
    text[start..index.min(text.len())]
        .iter()
        .fold(0, |column, ch| {
            if *ch == '\t' {
                ((column / tab_width) + 1) * tab_width
            } else {
                column + 1
            }
        })
}

fn insert_auto_indented_newline(
    ctx: &egui::Context,
    id: egui::Id,
    text: &mut String,
    tab_width: usize,
    language: DecoderLanguage,
) {
    let tab_width = tab_width.clamp(1, 8);
    let state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let range = state.cursor.char_range().unwrap_or_default();
    let [first, last] = range.sorted();
    let chars: Vec<char> = text.chars().collect();
    let index = first.index.min(chars.len());
    let selection_end = last.index.min(chars.len());
    let line_start = chars[..index]
        .iter()
        .rposition(|ch| *ch == '\n')
        .map_or(0, |position| position + 1);
    let line_end = chars[selection_end..]
        .iter()
        .position(|ch| *ch == '\n')
        .map_or(chars.len(), |position| selection_end + position);

    let mut indentation = String::new();
    let mut column = 0;
    for ch in &chars[line_start..index] {
        match *ch {
            ' ' => {
                indentation.push(' ');
                column += 1;
            }
            '\t' => {
                let next = ((column / tab_width) + 1) * tab_width;
                indentation.push_str(&" ".repeat(next - column));
                column = next;
            }
            _ => break,
        }
    }

    let content: String = chars[line_start..index].iter().collect();
    let trimmed = content.trim();
    let at_logical_end = chars[selection_end..line_end]
        .iter()
        .all(|ch| ch.is_whitespace());
    let opens_block = at_logical_end
        && !trimmed.starts_with('#')
        && (trimmed.ends_with(':')
            || (language == DecoderLanguage::Yaml
                && trimmed.starts_with("- ")
                && trimmed.contains(':')));
    if opens_block {
        indentation.push_str(&" ".repeat(tab_width));
    }
    let insert = format!("\n{indentation}");
    edit_source(ctx, id, text, "Paste", Some(&insert));
}

fn indent_source(
    ctx: &egui::Context,
    id: egui::Id,
    text: &mut String,
    tab_width: usize,
    dedent: bool,
) {
    let tab_width = tab_width.clamp(1, 8);
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let selection = state.cursor.char_range().unwrap_or_default();
    let chars: Vec<char> = text.chars().collect();
    let collapsed = selection.primary.index == selection.secondary.index;
    let edits = if !dedent && collapsed {
        let index = selection.primary.index.min(chars.len());
        let column = visual_column_at(&chars, index, tab_width);
        let spaces = tab_width - column % tab_width;
        vec![SourceEdit {
            start: index,
            remove: 0,
            insert: " ".repeat(spaces),
        }]
    } else {
        selected_line_starts(&chars, selection)
            .into_iter()
            .filter_map(|start| {
                if !dedent {
                    return Some(SourceEdit {
                        start,
                        remove: 0,
                        insert: " ".repeat(tab_width),
                    });
                }
                let mut column = 0;
                let mut remove = 0;
                for ch in &chars[start..] {
                    let next = match *ch {
                        ' ' => column + 1,
                        '\t' => ((column / tab_width) + 1) * tab_width,
                        _ => break,
                    };
                    if next > tab_width {
                        break;
                    }
                    column = next;
                    remove += 1;
                    if column == tab_width {
                        break;
                    }
                }
                (remove > 0).then_some(SourceEdit {
                    start,
                    remove,
                    insert: String::new(),
                })
            })
            .collect()
    };
    if edits.is_empty() {
        return;
    }
    let mut undo = state.undoer();
    undo.add_undo(&(selection, text.clone()));
    let range = apply_source_edits(text, selection, &edits);
    undo.feed_state(ctx.input(|input| input.time), &(range, text.clone()));
    state.cursor.set_char_range(Some(range));
    state.set_undoer(undo);
    state.store(ctx, id);
    ctx.memory_mut(|memory| memory.request_focus(id));
}

fn convert_source_indentation(
    ctx: &egui::Context,
    id: egui::Id,
    text: &mut String,
    tab_width: usize,
) {
    let tab_width = tab_width.clamp(1, 8);
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let selection = state.cursor.char_range().unwrap_or_default();
    let chars: Vec<char> = text.chars().collect();
    let mut edits = Vec::new();
    let mut indentation = true;
    let mut column = 0;
    for (index, ch) in chars.iter().enumerate() {
        match *ch {
            '\n' => {
                indentation = true;
                column = 0;
            }
            ' ' if indentation => column += 1,
            '\t' if indentation => {
                let next = ((column / tab_width) + 1) * tab_width;
                edits.push(SourceEdit {
                    start: index,
                    remove: 1,
                    insert: " ".repeat(next - column),
                });
                column = next;
            }
            _ => indentation = false,
        }
    }
    if edits.is_empty() {
        return;
    }
    let mut undo = state.undoer();
    undo.add_undo(&(selection, text.clone()));
    let range = apply_source_edits(text, selection, &edits);
    undo.feed_state(ctx.input(|input| input.time), &(range, text.clone()));
    state.cursor.set_char_range(Some(range));
    state.set_undoer(undo);
    state.store(ctx, id);
    ctx.memory_mut(|memory| memory.request_focus(id));
}

// Menu actions use the same cursor and undo history as keyboard editing.
fn edit_source(ctx: &egui::Context, id: egui::Id, text: &mut String, action: &str, paste: Option<&str>) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let mut range = state.cursor.char_range().unwrap_or_default();
    let mut undo = state.undoer();
    if action != "Undo" && action != "Redo" { undo.add_undo(&(range, text.clone())); }
    let [first, last] = range.sorted();
    if matches!(action, "Copy" | "Cut") {
        let selected: String = text.chars().skip(first.index).take(last.index - first.index).collect();
        #[cfg(target_arch = "wasm32")]
        crate::browser::copy_text(&selected);
        #[cfg(not(target_arch = "wasm32"))]
        ctx.output_mut(|out| out.copied_text = selected);
    }
    match action {
        "Cut" | "Paste" => {
            let insert = paste.unwrap_or("");
            let chars: Vec<char> = text.chars().collect();
            *text = chars[..first.index.min(chars.len())].iter().collect::<String>()
                + insert + &chars[last.index.min(chars.len())..].iter().collect::<String>();
            range = egui::text::CCursorRange::one(egui::text::CCursor::new(first.index + insert.chars().count()));
        }
        "Select all" => range = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count())),
        "Toggle comments" => range = toggle_source_comments(text, range),
        "Undo" | "Redo" => {
            let current = (range, text.clone());
            let next = if action == "Undo" { undo.undo(&current) } else { undo.redo(&current) };
            if let Some((cursor, value)) = next.cloned() { range = cursor; *text = value; }
        }
        _ => {}
    }
    state.cursor.set_char_range(Some(range));
    state.set_undoer(undo);
    state.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
}

/// Toggle complete source lines, using character indices to match egui's cursors.
pub(crate) fn toggle_source_comments(
    text: &mut String,
    mut selection: egui::text::CCursorRange,
) -> egui::text::CCursorRange {
    let chars: Vec<char> = text.chars().collect();
    selection.primary.index = selection.primary.index.min(chars.len());
    selection.secondary.index = selection.secondary.index.min(chars.len());
    let [first, last] = selection.sorted();
    // A selection ending at the start of a line excludes that following line.
    let last = if first.index < last.index {
        last.index - 1
    } else {
        last.index
    };
    let mut start = chars[..first.index]
        .iter()
        .rposition(|&ch| ch == '\n')
        .map_or(0, |position| position + 1);
    let mut positions = Vec::new();
    while start <= last {
        let end = chars[start..]
            .iter()
            .position(|&ch| ch == '\n')
            .map_or(chars.len(), |position| start + position);
        let content = (start..end).find(|&position| !matches!(chars[position], ' ' | '\t' | '\r'));
        if let Some(position) = content {
            positions.push(position);
        } else if selection.primary.index == selection.secondary.index {
            // On an empty current line, keep its indentation before inserting '# '.
            positions.push(if end > start && chars[end - 1] == '\r' {
                end - 1
            } else {
                end
            });
        }
        if end == chars.len() {
            break;
        }
        start = end + 1;
    }
    let uncomment = !positions.is_empty()
        && positions
            .iter()
            .all(|&position| chars.get(position) == Some(&'#'));
    let mut result = String::with_capacity(text.len() + positions.len() * 2);
    let mut copied = 0;
    let mut shift = 0_isize;
    for position in positions {
        result.extend(chars[copied..position].iter());
        let removed = if uncomment {
            1 + usize::from(chars.get(position + 1) == Some(&' '))
        } else {
            result.push_str("# ");
            0
        };
        let added = if uncomment { 0 } else { 2 };
        let adjusted_position = position.checked_add_signed(shift).unwrap();
        for cursor in [&mut selection.primary, &mut selection.secondary] {
            if cursor.index >= adjusted_position {
                cursor.index = adjusted_position
                    + added
                    + cursor.index.saturating_sub(adjusted_position + removed);
            }
        }
        shift += added as isize - removed as isize;
        copied = position + removed;
    }
    result.extend(chars[copied..].iter());
    *text = result;
    selection
}

/// A small, lossless lexer for display only. Unknown YAML remains editable and unmodified.
fn builtin_color(dark: bool) -> Color32 {
    if dark { Color32::from_rgb(97, 213, 200) } else { Color32::from_rgb(0, 113, 103) }
}

// Recognize type values, including numeric type-choice mappings. A field named
// "u32", a reference, or expected literal text must not become a built-in token.
fn yaml_type_position(text: &str, start: usize) -> bool {
    let prefix = text[..start].trim_end();
    let Some(before) = prefix.strip_suffix(':') else { return false; };
    let key = before.trim_end().rsplit(|c: char| c.is_whitespace() || "{},[".contains(c)).next().unwrap_or("")
        .trim_matches(['\'', '"']);
    key == "type" || key == "otherwise" || key.parse::<i64>().is_ok()
}

pub(crate) fn source_layout(
    text: &str,
    font: egui::FontId,
    dark: bool,
    foreground: Color32,
    language: DecoderLanguage,
) -> egui::text::LayoutJob {
    let (key, string, literal, comment, punctuation) = if dark {
        (
            Color32::from_rgb(140, 194, 238),
            Color32::from_rgb(173, 203, 145),
            Color32::from_rgb(218, 175, 225),
            Color32::from_rgb(130, 144, 139),
            Color32::from_rgb(173, 178, 190),
        )
    } else {
        (
            Color32::from_rgb(25, 89, 149),
            Color32::from_rgb(59, 112, 41),
            Color32::from_rgb(132, 52, 140),
            Color32::from_rgb(107, 117, 111),
            Color32::from_rgb(95, 100, 111),
        )
    };
    let mut job = egui::text::LayoutJob::default();
    // Long expressions stay on their source line; the surrounding area scrolls horizontally.
    job.wrap.max_width = f32::INFINITY;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let color = match bytes[i] {
            byte if byte.is_ascii_whitespace() => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                foreground
            }
            b'#' if language == DecoderLanguage::Python
                || i == 0
                || bytes[i - 1].is_ascii_whitespace() =>
            {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                comment
            }
            quote @ (b'\'' | b'"') => {
                let triple = language == DecoderLanguage::Python
                    && bytes.get(i + 1) == Some(&quote)
                    && bytes.get(i + 2) == Some(&quote);
                i += if triple { 3 } else { 1 };
                while i < bytes.len() {
                    if triple {
                        if bytes[i] == b'\\' {
                            i = (i + 2).min(bytes.len());
                        } else if bytes[i] == quote
                            && bytes.get(i + 1) == Some(&quote)
                            && bytes.get(i + 2) == Some(&quote)
                        {
                            i += 3;
                            break;
                        } else {
                            i += 1;
                        }
                        continue;
                    }
                    if quote == b'"' && bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if bytes[i] == quote {
                        i += 1;
                        if quote == b'\'' && bytes.get(i) == Some(&quote) {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                let token = text[start..i].trim_matches(['\'', '"']);
                if language == DecoderLanguage::Yaml && crate::structure::is_builtin_type(token)
                    && yaml_type_position(text, start) {
                    builtin_color(dark)
                } else { string }
            }
            b'{' | b'}' | b'[' | b']' | b',' | b':' | b'|' | b'>' => {
                i += 1;
                punctuation
            }
            byte if language == DecoderLanguage::Python && b"()=+-*/%<!@;".contains(&byte) => {
                i += 1;
                punctuation
            }
            _ => {
                i += 1;
                let delimiters: &[u8] = if language == DecoderLanguage::Python {
                    b"{}[],:()=+-*/%<>!@;\"'"
                } else {
                    b"{}[],:"
                };
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && !delimiters.contains(&bytes[i])
                {
                    i += 1;
                }
                let token = &text[start..i];
                let followed_by_colon = text[i..].trim_start().starts_with(':');
                if language == DecoderLanguage::Python
                    && matches!(
                        token,
                        "and"
                            | "as"
                            | "assert"
                            | "async"
                            | "await"
                            | "break"
                            | "class"
                            | "continue"
                            | "def"
                            | "del"
                            | "elif"
                            | "else"
                            | "except"
                            | "finally"
                            | "for"
                            | "from"
                            | "global"
                            | "if"
                            | "import"
                            | "in"
                            | "is"
                            | "lambda"
                            | "nonlocal"
                            | "not"
                            | "or"
                            | "pass"
                            | "raise"
                            | "return"
                            | "try"
                            | "while"
                            | "with"
                            | "yield"
                    )
                {
                    key
                } else if language == DecoderLanguage::Yaml && followed_by_colon {
                    key
                } else if language == DecoderLanguage::Yaml
                    && crate::structure::is_builtin_type(token) && yaml_type_position(text, start) {
                    builtin_color(dark)
                } else if matches!(
                    token,
                    "true"
                        | "false"
                        | "null"
                        | "~"
                        | "True"
                        | "False"
                        | "None"
                        | "remaining"
                ) || token.parse::<f64>().is_ok()
                    || token.starts_with("0x")
                    || token.starts_with("0b")
                {
                    literal
                } else {
                    foreground
                }
            }
        };
        job.append(
            &text[start..i],
            0.,
            egui::TextFormat {
                font_id: font.clone(),
                color,
                line_height: Some(font.size * 1.4),
                ..Default::default()
            },
        );
    }
    job
}

#[cfg(test)]
mod highlighting_tests {
    use super::*;
    #[test]
    fn builtin_type_values_are_distinct_in_both_themes() {
        let text = "type: u32\ntype: \"f32\"\ntype: {structure: RecordType}\nname: u32\nexpect: 'u32'\n# type: u32\ncases: {'1': i16, '2': f64}\notherwise: bytes\n";
        for dark in [true, false] {
            let normal=if dark {Color32::WHITE} else {Color32::BLACK};
            let job=source_layout(text,egui::FontId::monospace(13.),dark,normal,DecoderLanguage::Yaml);
            assert_eq!(job.text,text);
            let color_at=|index|job.sections.iter().find(|s|s.byte_range.contains(&index)).unwrap().format.color;
            for needle in ["u32", "\"f32\"", "i16", "f64", "bytes"] {
                assert_eq!(color_at(text.find(needle).unwrap()),builtin_color(dark));
            }
            assert_eq!(color_at(text.find("RecordType").unwrap()),normal);
            assert_eq!(color_at(text.find("name: u32").unwrap()+6),normal);
            assert_ne!(color_at(text.find("'u32'").unwrap()),builtin_color(dark));
            assert_ne!(color_at(text.find("# type").unwrap()),builtin_color(dark));
            assert_ne!(color_at(0),builtin_color(dark));
        }
    }
}

#[cfg(test)]
mod selected_read_tests {
    use super::*;
    #[test]
    fn menu_edits_use_unicode_cursor_and_support_undo_redo() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("test-editor");
        let mut text = "aé猫z".to_string();
        let mut state = egui::text_edit::TextEditState::default();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(1), egui::text::CCursor::new(3))));
        state.store(&ctx, id);
        edit_source(&ctx, id, &mut text, "Paste", Some("hello"));
        assert_eq!(text, "ahelloz");
        edit_source(&ctx, id, &mut text, "Undo", None);
        assert_eq!(text, "aé猫z");
        edit_source(&ctx, id, &mut text, "Redo", None);
        assert_eq!(text, "ahelloz");
        edit_source(&ctx, id, &mut text, "Select all", None);
        edit_source(&ctx, id, &mut text, "Cut", None);
        assert_eq!(text, "");
        edit_source(&ctx, id, &mut text, "Undo", None);
        assert_eq!(text, "ahelloz");
    }

    #[test]
    fn yaml_diagnostics_are_live_and_include_parser_locations() {
        let mut editor = YamlLayoutCache::default();
        editor.refresh_diagnostic(
            "actions:\n  - read:\n      name: value\n       type: u8\n",
        );
        let diagnostic = editor.diagnostic.as_ref().expect("invalid indentation");
        assert!(diagnostic.line.is_some());
        assert!(diagnostic.column.is_some());
        assert!(diagnostic.message.contains("read structure YAML"));

        editor.refresh_diagnostic("actions: [{read: {name: value, type: u8}}]\n");
        assert!(editor.diagnostic.is_none());
        editor.refresh_diagnostic("endian: little\nactions: []\n");
        assert!(editor.diagnostic.is_none(), "The new-decoder placeholder is not an error");
    }

    #[test]
    fn overindented_cases_reports_the_actual_alignment_fix() {
        let mut editor = YamlLayoutCache::default();
        let source = "structures:\n  - name: Metadata\n    fields:\n      - name: payload\n        type:\n          choose:\n            ref: metadata_type\n              cases: {'1': u8}\nactions: []\n";
        editor.refresh_diagnostic(source);
        let diagnostic = editor.diagnostic.as_ref().expect("invalid cases indentation");
        assert_eq!(diagnostic.line, Some(8));
        assert_eq!(diagnostic.column, Some(20));
        let hint = diagnostic.hint.as_deref().expect("actionable indentation hint");
        assert!(hint.contains("`cases` is indented under `ref`"), "{hint}");
        assert!(hint.contains("Remove 2 leading spaces"), "{hint}");

        editor.refresh_diagnostic(&source.replace("              cases", "            cases"));
        assert!(editor.diagnostic.is_none());
    }

    #[test]
    fn source_locations_use_one_based_unicode_columns() {
        let source = "first\nαβ猫 value\nlast";
        assert_eq!(source_char_index(source, 1, 1), 0);
        assert_eq!(source_char_index(source, 2, 4), 9);
        assert_eq!(source_char_index(source, 2, 200), 15);
        assert_eq!(source_char_index(source, 20, 1), source.chars().count());
    }

    #[test]
    fn tab_indents_to_configured_space_stop_and_shift_tab_dedents_lines() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("tab-editor");
        let mut text = "  first\n  second\n".to_owned();
        let mut state = egui::text_edit::TextEditState::default();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::new(2),
        )));
        state.store(&ctx, id);

        indent_source(&ctx, id, &mut text, 4, false);
        assert_eq!(text, "    first\n  second\n");

        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(text.chars().count()),
        )));
        state.store(&ctx, id);
        indent_source(&ctx, id, &mut text, 3, false);
        assert_eq!(text, "       first\n     second\n");
        indent_source(&ctx, id, &mut text, 3, true);
        assert_eq!(text, "    first\n  second\n");
    }

    #[test]
    fn enter_preserves_indentation_and_indents_block_openers() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("auto-indent-editor");
        let run = |text: &mut String, language, width| {
            let mut state = egui::text_edit::TextEditState::default();
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(text.chars().count()),
            )));
            state.store(&ctx, id);
            insert_auto_indented_newline(&ctx, id, text, width, language);
        };

        let mut yaml = "structures:".to_owned();
        run(&mut yaml, DecoderLanguage::Yaml, 4);
        assert_eq!(yaml, "structures:\n    ");

        let mut list_mapping = "  - name: Record".to_owned();
        run(&mut list_mapping, DecoderLanguage::Yaml, 2);
        assert_eq!(list_mapping, "  - name: Record\n    ");

        let mut ordinary = "    value".to_owned();
        run(&mut ordinary, DecoderLanguage::Yaml, 2);
        assert_eq!(ordinary, "    value\n    ");

        let mut python = "  if ready:".to_owned();
        run(&mut python, DecoderLanguage::Python, 2);
        assert_eq!(python, "  if ready:\n    ");
    }

    #[test]
    fn enter_replaces_selection_and_expands_existing_indent_tabs() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("auto-indent-selection-editor");
        let mut text = "\tname: selected".to_owned();
        let mut state = egui::text_edit::TextEditState::default();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(5),
            egui::text::CCursor::new(text.chars().count()),
        )));
        state.store(&ctx, id);
        insert_auto_indented_newline(
            &ctx,
            id,
            &mut text,
            4,
            DecoderLanguage::Yaml,
        );
        assert_eq!(text, "\tname\n    ");
    }

    #[test]
    fn indentation_conversion_changes_only_leading_tabs() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("convert-tabs-editor");
        let mut text = "\tkey: \"keep\tvalue\"\n \tchild: 1\n".to_owned();
        convert_source_indentation(&ctx, id, &mut text, 4);
        assert_eq!(text, "    key: \"keep\tvalue\"\n    child: 1\n");
    }

    #[test]
    fn editor_tab_width_defaults_to_two_and_is_bounded() {
        let mut editor = YamlLayoutCache::default();
        assert_eq!(editor.tab_width(), 2);
        editor.set_tab_width(0);
        assert_eq!(editor.tab_width(), 1);
        editor.set_tab_width(99);
        assert_eq!(editor.tab_width(), 8);
    }

    #[test]
    fn caret_position_is_the_only_editor_column_label() {
        let ctx = egui::Context::default();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(700., 100.),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(650., 80.),
                        egui::Sense::hover(),
                    );
                    paint_caret_status(
                        ui,
                        rect,
                        &egui::FontId::monospace(13.),
                        Some((27, 20)),
                    );
                });
            },
        );
        let labels: Vec<_> = output.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        }).collect();
        assert!(labels.contains(&"Ln 27, Col 20".to_owned()), "{labels:?}");
        assert!(!labels.contains(&"20".to_owned()), "{labels:?}");
    }

    #[test]
    fn right_click_preserves_source_selection_and_copy_shortcut_uses_it() {
        let ctx = egui::Context::default();
        let mut editor = YamlLayoutCache::default();
        let mut source = "actions: [{read: {name: value, type: u8}}]\n".to_owned();
        let frame = |editor: &mut YamlLayoutCache, source: &mut String, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(700., 400.),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        editor.editor(ui, source, DecoderLanguage::Yaml);
                    });
                },
            )
        };
        let first = frame(&mut editor, &mut source, vec![]);
        let position = first
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == source => {
                    Some(text.pos + egui::vec2(12., 6.))
                }
                _ => None,
            })
            .expect("source position");
        frame(&mut editor, &mut source, vec![egui::Event::PointerMoved(position)]);
        for pressed in [true, false] {
            frame(&mut editor, &mut source, vec![egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }]);
        }
        let id = ctx.memory(|memory| memory.focused()).expect("editor focus");
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        frame(&mut editor, &mut source, vec![key(egui::Key::A)]);
        frame(&mut editor, &mut source, vec![egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Secondary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        let selection = egui::TextEdit::load_state(&ctx, id)
            .unwrap()
            .cursor
            .char_range()
            .unwrap();
        assert_eq!(selection.sorted()[0].index, 0);
        assert_eq!(selection.sorted()[1].index, source.chars().count());
        let copied = frame(&mut editor, &mut source, vec![key(egui::Key::C)]);
        assert_eq!(copied.platform_output.copied_text, source);
    }
}
