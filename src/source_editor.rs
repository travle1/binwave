//! Shared decoder source editor and starter templates for native and browser shells.
use egui::{Color32, Ui};
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
# F5 runs this script; print() uses Console.
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
            Self::Python => self.header_example(),
        }
    }

    pub fn header_example(self) -> &'static str {
        match self {
            Self::Yaml => {
                r#"## Adapt this example to your binary file.
## Header: u32 count. Samples: u32 time, f32 value.
## Select all, then Ctrl+/ to uncomment.
## On Mac use Cmd+/. F5 decodes.
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
## On Mac use Cmd+/. F5 runs.
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
}
impl YamlLayoutCache {
    fn edit_menu(&mut self, ui: &mut Ui) {
        for action in ["Undo", "Redo", "Cut", "Copy", "Paste", "Select all", "Toggle comments"] {
            if action == "Paste" && !cfg!(target_arch = "wasm32") { continue; }
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
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("Edit", |ui| { self.edit_menu(ui); });
        });
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
                });
            });
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
                            if let Some(paste) = self.paste.lock().unwrap().take() {
                                edit_source(ui.ctx(), editor_id, yaml, "Paste", Some(&paste));
                            }
                            if let Some(action) = self.edit_action.take() {
                                if action == "Paste" {
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
                            let focused = ui.memory(|memory| memory.has_focus(editor_id));
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

/// Add an independent read without reserializing the user's comments or structures.
#[cfg(any(test, target_arch = "wasm32"))]
pub(crate) fn add_selected_read(source: &str, name: &str, kind: &str, start: u64, end: u64) -> Result<String, String> {
    if name.trim().is_empty() || end <= start { return Err("Choose a name and a nonempty byte selection.".into()); }
    let width = match kind { "bytes" | "text" | "u8" => 1, "u16" | "i16" => 2, "u32" | "i32" | "f32" => 4, "u64" | "f64" => 8, _ => return Err("Choose a supported type.".into()) };
    let span = end - start;
    if span % width != 0 { return Err(format!("The selection must contain a multiple of {width} bytes for {kind}.")); }
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(source).map_err(|e| format!("Fix YAML syntax before adding a read: {e}"))?;
    if value["actions"].as_sequence().is_some_and(|a| a.iter().any(|a| a["read"]["name"].as_str() == Some(name))) {
        return Err(format!("A read named {name} already exists. Choose another name."));
    }
    let extra = if matches!(kind, "bytes" | "text") { format!(", length: {span}") }
        else if span > width { format!(", repeat: {}", span / width) } else { String::new() };
    let action = format!("  - read: {{name: {}, type: {kind}, at: {start}{extra}}}\n", serde_json::to_string(name).unwrap());
    let mut result = String::new();
    let mut found = false;
    let mut in_actions = false;
    let mut inserted = false;
    for line in source.split_inclusive('\n') {
        if line.starts_with("actions:") {
            if found { return Err("Duplicate actions blocks.".into()); }
            let tail = line[8..].trim();
            if !(tail.is_empty() || tail.starts_with('#') || tail == "[]") { return Err("Expand actions into a YAML list before adding a read.".into()); }
            found = true; in_actions = true;
            if tail == "[]" { result.push_str("actions:\n"); } else { result.push_str(line); if !line.ends_with('\n') { result.push('\n'); } }
            continue;
        }
        if in_actions && !line.starts_with(char::is_whitespace) && !line.starts_with('#') && !line.trim().is_empty() {
            result.push_str(&action); inserted = true; in_actions = false;
        }
        result.push_str(line);
    }
    if !result.ends_with('\n') { result.push('\n'); }
    if !found { result.push_str("actions:\n"); }
    if !inserted { result.push_str(&action); }
    crate::structure::Program::parse(&result).map_err(|e| e.to_string())?;
    Ok(result)
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
    fn adds_reads_preserving_comments_and_checks_width_and_names() {
        let source = "# my notes\nendian: little\nactions: []\n";
        let first = add_selected_read(source, "signature", "text", 0, 4).unwrap();
        assert!(first.starts_with("# my notes\n"));
        let next = add_selected_read(&first, "version", "u16", 4, 6).unwrap();
        assert!(next.contains("at: 4"));
        assert!(next.find("signature").unwrap() < next.find("version").unwrap());
        assert!(add_selected_read(&next, "version", "u16", 6, 8).is_err());
        assert!(add_selected_read(&next, "bad", "u32", 6, 9).is_err());
    }
}
