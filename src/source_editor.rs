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

#[derive(Default)]
pub(crate) struct YamlLayoutCache {
    job: Option<egui::text::LayoutJob>,
    font: egui::FontId,
    dark: bool,
    foreground: Color32,
    language: DecoderLanguage,
    edit_action: Option<&'static str>,
    paste: Arc<std::sync::Mutex<Option<String>>>,
    paste_error: Option<String>,
    goto_line: Option<usize>,
    diagnostic_hash: Option<u64>,
    diagnostic: Option<SourceDiagnostic>,
}

#[derive(Clone)]
struct SourceDiagnostic {
    line: Option<usize>,
    column: Option<usize>,
    message: String,
}

impl YamlLayoutCache {
    pub fn command(&mut self, action: &'static str) { self.edit_action = Some(action); }
    pub fn go_to_line(&mut self, line: usize) { self.goto_line = Some(line.max(1)); }
    fn edit_menu(&mut self, ui: &mut Ui) {
        for action in ["Undo", "Redo", "Cut", "Copy", "Paste", "Select all", "Toggle comments"] {
            if ui.button(action).clicked() {
                self.edit_action = Some(action);
                ui.close_menu();
            }
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
                                if ui.small_button(location).on_hover_text("Jump to this line").clicked() {
                                    self.go_to_line(line);
                                }
                            }
                            ui.label(diagnostic.message);
                        });
                    });
            }
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
                egui::ScrollArea::both()
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
                                } else {
                                    edit_source(ui.ctx(), editor_id, yaml, action, None);
                                }
                            }
                            if let Some(line) = self.goto_line.take() {
                                let index = if line <= 1 {
                                    0
                                } else {
                                    yaml.char_indices()
                                        .filter(|(_, ch)| *ch == '\n')
                                        .nth(line - 2)
                                        .map_or_else(|| yaml.chars().count(), |(index, _)| yaml[..=index].chars().count())
                                };
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
                            let mut shortcut_changed = false;
                            if toggle || redo {
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
                Some(SourceDiagnostic {
                    line: location.as_ref().map(|location| location.line()),
                    column: location.as_ref().map(|location| location.column()),
                    message: format!("read structure YAML: {error}"),
                })
            }
            Ok(program) if program.actions.is_empty() => None,
            Ok(_) => crate::structure::Program::parse(source).err().map(|error| {
                let location = error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<serde_yaml_ng::Error>())
                    .and_then(serde_yaml_ng::Error::location);
                SourceDiagnostic {
                    line: location.as_ref().map(|location| location.line()),
                    column: location.as_ref().map(|location| location.column()),
                    message: format!("{error:#}"),
                }
            }),
        };
    }
}

/// The decoder language reference is rendered inside the application on every platform.
pub(crate) fn decoding_guide(ui: &mut Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.heading("Decode a file with YAML or Python");
            ui.label("Open a data file, describe its byte layout in Definition, then press ▶ Run (F5). Successful runs populate Decoded structure, Plot, Samples, Inspector, and Hex.");
            ui.label("Click a decoded value or plot point to locate its exact bytes. Shift-click a plot sample to select every record from the anchor through that sample.");
            ui.separator();

            egui::CollapsingHeader::new("YAML quick start")
                .default_open(true)
                .show(ui, |ui| {
                    ui.code("endian: little\nstructures:\n  - name: Sample\n    fields:\n      - {name: time, type: u32}\n      - {name: value, type: f32}\nactions:\n  - read:\n      name: samples\n      type: {structure: Sample}\n      repeat: until end");
                    ui.label("structures declares reusable record layouts but reads no bytes. actions performs reads from the file. Fields consume bytes in order unless an offset option changes the cursor.");
                });

            egui::CollapsingHeader::new("YAML types and field options")
                .default_open(true)
                .show(ui, |ui| {
                    ui.strong("Built-in types");
                    ui.code("u8  i8  u16  i16  u32  i32  u64  i64  f32  f64\nbytes  text  cstring  varuint");
                    ui.label("Use type: {structure: Name} for a declared structure. A structure or the whole file can set endian: little or endian: big.");
                    ui.strong("Field options");
                    ui.code("repeat: 12\nrepeat: {ref: header.count}\nrepeat: until end\nlength: 16\nat: 128\nrelative: 4\nalign: 8\nendian: big\nexpect: 42");
                    ui.label("length bounds one value in bytes. at is an absolute offset; relative moves from the current cursor; align advances to a boundary. expect validates the decoded value.");
                    ui.strong("Choose a type from an earlier value");
                    ui.code("type:\n  choose:\n    ref: stream_id\n    cases:\n      '1': {structure: Imu}\n      '2': {structure: Motors}\n    otherwise: {structure: Raw}");
                });

            egui::CollapsingHeader::new("YAML actions and expressions")
                .show(ui, |ui| {
                    ui.label("Every action contains exactly one of read, define, skip, align, calculate, or choose.");
                    ui.code("actions:\n  - read: {name: header, type: {structure: Header}}\n  - calculate: {name: payload_bytes, value: {multiply: [{ref: header.count}, 4]}}\n  - align: 8\n  - skip: 16");
                    ui.label("Expressions can be an integer, {ref: path}, or add, subtract, multiply, and divide pairs. References must point to values decoded earlier.");
                    ui.strong("Choose different actions");
                    ui.code("- choose:\n    ref: id\n    cases:\n      '1':\n        - read: {name: header, type: {structure: Header}}\n    otherwise:\n      - skip: {ref: length}");
                    ui.strong("Create a structure from decoded metadata");
                    ui.code("- define:\n    name: Discovered\n    from: metadata.entries\n    fields:\n      name: {ref: name}\n      type: {ref: format, cases: {'1': u16, '2': i16}}\n      repeat: {ref: count}");
                    ui.label("define maps an earlier description collection into reusable fields. Optional mappings also include length, at, and relative.");
                });

            egui::CollapsingHeader::new("Python quick start and API")
                .default_open(true)
                .show(ui, |ui| {
                    ui.code("from binwave import Field, Struct\n\nHeader = Struct('Header', [('count', 'u32')], endian='little')\nSample = Struct('Sample', [\n    ('time', 'u32'),\n    Field('value', 'f32'),\n])\nheader = file.read_struct('header', Header)\nsamples = file.records('samples', Sample, count=header.count)\nprint(f'{len(samples)} samples')");
                    ui.label("Field(name, type, length=None, count=None, offset=None, endian=None, expect=None) accepts a fixed-size numeric type, text, bytes, or another Struct. (name, type) is shorthand.");
                    ui.label("Struct(name, fields, endian=None) creates a fixed layout and exposes size and fields.");
                    ui.label("file.size is the byte length. file.tell() and file.seek(offset, whence=0) control the cursor. file.read(length) advances it; file.read_at(offset, length) does not. file.unpack(format, offset=None) uses Python struct formats.");
                    ui.label("file.read_struct(name, struct, offset=None) decodes metadata now. file.records(name, struct, count=None, offset=None) registers a lazy collection for Plot and Samples. Without count, records consumes complete records to the end of the file.");
                    ui.label("Python supports ordinary imports, functions, loops, conditions, assertions, and print(). In the web app it runs locally in the browser; on desktop it runs with the selected Python interpreter.");
                });

            egui::CollapsingHeader::new("Editing and troubleshooting")
                .show(ui, |ui| {
                    ui.label("YAML syntax and indentation are checked while you type. Select a reported line to jump there. Run-time errors remain in the Console because they depend on the open data.");
                    ui.label("Copy, cut, paste, undo, redo, select all, and toggle comments are available from Edit → Definition editing and from the editor context menu. Ctrl/Cmd+C, X, and V use the same selection.");
                    ui.label("Start with Decoder → Examples, then adapt one field at a time. Keep the last successful result visible while correcting a definition.");
                });
        });
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
                    "true" | "false" | "null" | "~" | "True" | "False" | "None"
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
