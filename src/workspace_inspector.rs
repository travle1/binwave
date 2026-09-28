//! Inspector presentation shared by the native and browser shells.
use crate::structure::Node;
use egui::{RichText, Ui};
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FieldInfo {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub value: String,
    pub editable: bool,
    pub endian: String,
    pub start: u64,
    pub end: u64,
    pub references: Vec<String>,
    pub sample: Option<u64>,
    pub sample_start: Option<u64>,
    pub sample_end: Option<u64>,
}

impl FieldInfo {
    pub fn from_node(node: &Node, sample: Option<(u64, Range<u64>)>) -> Self {
        Self {
            name: node.name.clone(),
            path: node.path.clone(),
            kind: node.kind.clone(),
            value: node.display(),
            editable: crate::field_edit::editable_value(node),
            endian: format!("{:?}", node.endian).to_lowercase(),
            start: node.range.start as u64,
            end: node.range.end as u64,
            references: node.references.clone(),
            sample: sample.as_ref().map(|(index, _)| *index),
            sample_start: sample.as_ref().map(|(_, range)| range.start),
            sample_end: sample.as_ref().map(|(_, range)| range.end),
        }
    }
    #[cfg(any(test, target_arch = "wasm32"))]
    pub fn sample_range(&self) -> Option<Range<u64>> {
        self.sample_start
            .zip(self.sample_end)
            .map(|(start, end)| start..end)
    }
}

pub fn empty_selection(ui: &mut Ui, has_file: bool, has_decoded: bool) {
    let (title, detail) = if !has_file {
        (
            "Nothing to inspect yet",
            "Open a data file, then select bytes in Hex.",
        )
    } else if has_decoded {
        (
            "Select a field or byte range",
            "Choose a field in Structure, a point in Signal, a row in Table, or bytes in Hex. The linked selection appears here.",
        )
    } else {
        (
            "Select bytes to inspect",
            "Drag across bytes in Hex for raw interpretations, or press ▶ Run (F5) to inspect decoded fields.",
        )
    };
    crate::decoder_ui::empty_state(ui, title, detail);
}

/// `samples_collection` names the samples collection; indices in any other
/// collection are items.
pub fn field_heading(ui: &mut Ui, field: &FieldInfo, samples_collection: Option<&str>) {
    ui.heading(&field.name);
    if let Some(index) = field.sample {
        let collection = field.path.rfind('[').map(|bracket| &field.path[..bracket]);
        let noun = if collection.is_some() && collection == samples_collection { "Sample" } else { "Item" };
        ui.weak(format!("{noun} {}", crate::format::count(index)));
    }
}

pub fn field_details(ui: &mut Ui, field: &FieldInfo, definition: Option<(u64, usize)>) {
    if let Some((consumed, types)) = definition {
        ui.label(format!(
            "Decoder covers {} · {}",
            crate::format::size(consumed),
            crate::format::plural(types as u64, "reusable type", "reusable types")
        )).on_hover_text(crate::format::exact_bytes(consumed));
    }
    ui.label("Definition path");
    ui.monospace(&field.path);
    ui.label(format!("Type: {} · {} endian", field.kind, field.endian));
    ui.label(format!("Field bytes: {}", crate::format::byte_range(field.start, field.end)));
    if !field.references.is_empty() {
        ui.label(format!("Uses: {}", field.references.join(", ")));
    }
}

/// Draw the editable-value controls identically in native and browser Inspectors.
pub fn value_editor(
    ui: &mut Ui,
    text: &mut String,
    original: &str,
    error: &mut Option<String>,
    enabled: bool,
) -> bool {
    let mut apply = false;
    ui.add_enabled_ui(enabled, |ui| {
        let response = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
        let changed = text != original;
        ui.horizontal_wrapped(|ui| {
            apply = ui
                .add_enabled(changed, egui::Button::new("Write value to bytes"))
                .clicked()
                || (changed
                    && response.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)));
            if changed && ui.button("Cancel").clicked() {
                text.clear();
                text.push_str(original);
                *error = None;
            }
        });
        if response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            text.clear();
            text.push_str(original);
            *error = None;
        }
    });
    if let Some(message) = error.as_deref() {
        ui.colored_label(ui.visuals().error_fg_color, message);
    }
    apply
}

/// The supplied bytes must start at range.start; previews stay bounded to 64.
pub fn byte_selection(ui: &mut Ui, range: Range<u64>, bytes: Option<&[u8]>) {
    if range.is_empty() {
        ui.weak("Select a byte or range in Hex.");
        return;
    }
    ui.label(RichText::new("Byte selection").strong());
    ui.monospace(crate::format::byte_range(range.start, range.end));
    ui.label(format!(
        "{} · absolute offset {}",
        crate::format::exact_bytes(range.end - range.start),
        crate::format::count(range.start)
    ));
    let Some(bytes) = bytes.filter(|bytes| !bytes.is_empty()) else {
        ui.weak("Loading selected bytes…");
        return;
    };
    let preview_len = (range.end - range.start).min(64) as usize;
    let bytes = &bytes[..bytes.len().min(preview_len)];
    egui::Grid::new("hex_numeric_inspector")
        .striped(true)
        .show(ui, |ui| {
            ui.strong("Type");
            ui.strong("Little endian");
            ui.strong("Big endian");
            ui.end_row();
            if let Some(&b) = bytes.first() {
                ui.label("u8 / i8");
                ui.monospace(format!("{b} / {}", b as i8));
                ui.weak("—");
                ui.end_row();
            }
            macro_rules! numeric {
                ($ty:ty, $count:expr, $name:expr) => {
                    if bytes.len() >= $count {
                        let b: [u8; $count] = bytes[..$count].try_into().unwrap();
                        ui.label($name);
                        ui.monospace(format!("{}", <$ty>::from_le_bytes(b)));
                        ui.monospace(format!("{}", <$ty>::from_be_bytes(b)));
                        ui.end_row();
                    }
                };
            }
            numeric!(u16, 2, "u16");
            numeric!(i16, 2, "i16");
            numeric!(u32, 4, "u32");
            numeric!(i32, 4, "i32");
            numeric!(f32, 4, "f32");
            numeric!(u64, 8, "u64");
            numeric!(i64, 8, "i64");
            numeric!(f64, 8, "f64");
        });
    if let Some(b) = bytes.first() {
        ui.monospace(format!("Bits  {b:08b}"));
    }
    ui.label(format!(
        "Text  {}",
        String::from_utf8_lossy(bytes)
            .chars()
            .map(|c| if c.is_control() { '·' } else { c })
            .collect::<String>()
    ));
}
