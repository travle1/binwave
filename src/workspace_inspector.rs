//! Inspector presentation shared by the native and browser shells.
use crate::structure::{Node, Program, Value};
use egui::{Align2, FontFamily, FontId, RichText, Sense, Ui};
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
    /// The value as Structure shows it, with the decoder's `cases` meaning: `3 · f64`.
    #[serde(default)]
    pub display: String,
    /// The decoder structure this field belongs to; `None` for top-level reads.
    #[serde(default)]
    pub structure: Option<String>,
    /// The node's own structure type, for items and nested records.
    #[serde(default)]
    pub own_structure: Option<String>,
    /// Byte ranges of the path's parts, when the host resolved them up front.
    #[serde(default)]
    pub crumbs: Vec<Crumb>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Crumb {
    pub path: String,
    pub start: u64,
    pub end: u64,
}

impl FieldInfo {
    /// `parent` is the node containing this one, used to find its structure and
    /// the meaning of enum values.
    pub fn from_node(node: &Node, sample: Option<(u64, Range<u64>)>,
        program: Option<&Program>, parent: Option<&Node>) -> Self {
        let structure_named = |kind: &str| {
            let kind = kind.trim_end_matches("[]");
            program.is_some_and(|program| program.structures.contains_key(kind)).then(|| kind.to_owned())
        };
        let parent_kind = parent.map(|parent| parent.kind.trim_end_matches("[]"));
        let display = match &node.value {
            Some(value) => {
                let text = match value {
                    Value::Signed(number) => crate::format::count(number),
                    Value::Unsigned(number) => crate::format::count(number),
                    Value::Text(text) => text.clone(),
                    _ => node.display(),
                };
                match program.and_then(|program| program.enum_meaning(node, parent_kind)) {
                    Some(meaning) => format!("{text} · {meaning}"),
                    None => text,
                }
            }
            None => node.display(),
        };
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
            display,
            structure: parent_kind.and_then(structure_named),
            own_structure: structure_named(&node.kind),
            crumbs: Vec::new(),
        }
    }
    pub fn sample_range(&self) -> Option<Range<u64>> {
        self.sample_start
            .zip(self.sample_end)
            .map(|(start, end)| start..end)
    }
    /// The byte range of a breadcrumb part, when the host supplied it.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // Native resolves parts on click.
    pub fn crumb(&self, path: &str) -> Option<Range<u64>> {
        self.crumbs.iter().find(|crumb| crumb.path == path).map(|crumb| crumb.start..crumb.end)
    }
}

/// The one-based decoder line that defines this field, found by name within
/// its structure. Items point at their collection; structured nodes at their type.
pub fn definition_line(source: &str, field: &FieldInfo) -> Option<usize> {
    let lines: Vec<&str> = source.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let structure_line = |name: &str| lines.iter().position(|line| {
        line.trim_start().strip_prefix("- name:")
            .is_some_and(|value| value.trim().trim_matches(['\'', '"']) == name)
    });
    if let Some(line) = field.own_structure.as_deref().and_then(structure_line) {
        return Some(line + 1);
    }
    let name = if field.name.starts_with('[') {
        crate::structure_selection::parent_path(&field.path)
            .map_or(field.name.as_str(), |collection| collection.rsplit('.').next().unwrap_or(collection))
    } else {
        field.name.as_str()
    };
    let (first, last) = match field.structure.as_deref() {
        Some(structure) => {
            let start = structure_line(structure)?;
            let end = lines[start + 1..].iter()
                .position(|line| !line.trim().is_empty() && indent(line) <= indent(lines[start]))
                .map_or(lines.len(), |offset| start + 1 + offset);
            (start + 1, end)
        }
        None => (lines.iter().position(|line| line.starts_with("actions:")).unwrap_or(0), lines.len()),
    };
    (first..last).find(|&index| declares(lines[index], name)).map(|index| index + 1)
        .or_else(|| field.structure.as_ref().map(|_| first))
}

/// Whether a YAML line names `name`, in either `- name: x` or `{name: x, …}` form.
fn declares(line: &str, name: &str) -> bool {
    line.match_indices("name:").any(|(index, key)| {
        let before = line[..index].chars().next_back();
        let value = line[index + key.len()..].trim_start();
        let value = &value[..value.find([',', '}', ']', '#']).unwrap_or(value.len())];
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && value.trim().trim_matches(['\'', '"']) == name
    })
}

/// State of the Inspector's in-place value editor.
#[derive(Clone, Debug, Default)]
pub struct ValueEdit {
    pub text: String,
    pub original: String,
    pub error: Option<String>,
    pub editing: bool,
}
impl ValueEdit {
    pub fn new(value: &str) -> Self {
        Self { text: value.into(), original: value.into(), ..Self::default() }
    }
    fn cancel(&mut self) {
        self.text.clone_from(&self.original);
        self.error = None;
        self.editing = false;
    }
}

pub enum FieldAction {
    /// Select the node at this path (a breadcrumb part).
    SelectPath(String),
    JumpToDefinition,
    /// Encode `ValueEdit::text` into the field's bytes.
    ApplyValue,
}

const OPEN_ID: &str = "inspector_open";
const DOCKED_ID: &str = "inspector_docked";

/// Whether the docked Inspector is shown. Remembered between launches.
pub fn is_open(ctx: &egui::Context) -> bool {
    ctx.data_mut(|data| *data.get_persisted_mut_or(egui::Id::new(OPEN_ID), true))
}

pub fn set_open(ctx: &egui::Context, open: bool) {
    ctx.data_mut(|data| data.insert_persisted(egui::Id::new(OPEN_ID), open));
}

/// Hex's toolbar toggle for the docked Inspector (Ctrl+I). Hidden when Hex is too
/// narrow to dock it.
pub fn toggle_button(ui: &mut Ui) {
    let t = &crate::theme::DARK;
    if !ui.data(|data| data.get_temp::<bool>(egui::Id::new(DOCKED_ID))).unwrap_or(false) {
        return;
    }
    let open = is_open(ui.ctx());
    let icon = RichText::new(egui_phosphor::regular::SIDEBAR_SIMPLE)
        .font(FontId::new(t.size.info_icon, FontFamily::Name("phosphor".into())));
    if ui.add(egui::Button::selectable(open, icon))
        .on_hover_text(if open { "Hide Inspector (Ctrl+I)" } else { "Show Inspector (Ctrl+I)" })
        .clicked()
    {
        set_open(ui.ctx(), !open);
    }
}

/// The Inspector docked inside Hex. It starts right after Hex's Text column
/// (`grid_width` is the width the byte grid needs) and never gets narrower than
/// its minimum. Its content lays out at the panel's width, and long text ends in
/// "…" instead of widening it. Ctrl+I and the Hex toolbar toggle hide it.
pub fn docked_panel<T>(ui: &mut Ui, id: &str, state: &mut T, grid_width: f32,
    header: impl FnOnce(&mut T, &mut Ui), body: impl FnOnce(&mut T, &mut Ui)) {
    let t = &crate::theme::DARK;
    let ctx = ui.ctx().clone();
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(DOCKED_ID), true));
    if ctx.input_mut(|input| input.consume_shortcut(
        &egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::I))) {
        set_open(&ctx, !is_open(&ctx));
    }
    if !is_open(&ctx) {
        return;
    }
    let width = (ui.available_width() - grid_width).max(t.size.inspector_min_width);
    egui::Panel::right(id.to_owned())
        .resizable(false)
        .exact_size(width)
        .frame(egui::Frame::new()
            .fill(t.color.surface.raised)
            .inner_margin(egui::Margin::same(t.spacing.card_padding as i8))
            .corner_radius(t.radius.inset))
        .show(ui, |ui| {
            header(state, ui);
            // Content lays out at the panel's width; anything still wider scrolls
            // rather than widening the panel over Hex.
            let width = ui.available_width();
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(width);
                body(state, ui)
            });
        });
}

/// Hex is too narrow to dock the Inspector this frame, so its toggle hides.
pub fn not_docked(ctx: &egui::Context) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(DOCKED_ID), false));
}

/// "Inspector" with the selection chip and a hide button on the right: `0x48 · 16 bytes`.
pub fn header(ui: &mut Ui, selection: Option<Range<u64>>) {
    let t = &crate::theme::DARK;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Inspector").size(t.font.card_title)
            .family(FontFamily::Name("Geist SemiBold".into())).color(t.color.text.primary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let icon = RichText::new(egui_phosphor::regular::CARET_DOUBLE_RIGHT)
                .font(FontId::new(t.size.info_icon, FontFamily::Name("phosphor".into())))
                .color(t.color.text.muted);
            if ui.add(egui::Button::new(icon).frame(false)).on_hover_text("Hide Inspector (Ctrl+I)").clicked() {
                set_open(ui.ctx(), false);
            }
            if let Some(range) = selection.filter(|range| !range.is_empty()) {
                ui.label(RichText::new(format!("0x{:X} · {}", range.start,
                    crate::format::plural(range.end - range.start, "byte", "bytes")))
                    .monospace().size(t.font.label).color(t.color.text.muted))
                    .on_hover_text(crate::format::byte_range(range.start, range.end));
            }
        });
    });
    ui.add_space(t.spacing.inspector_gap);
}

fn info_icon(ui: &mut Ui, help: &str) {
    let t = &crate::theme::DARK;
    ui.label(RichText::new(egui_phosphor::regular::INFO)
        .font(FontId::new(t.size.info_icon, FontFamily::Name("phosphor".into())))
        .color(t.color.text.muted))
        .on_hover_text(help);
}

pub fn empty_selection(ui: &mut Ui, has_file: bool, has_decoded: bool) {
    let (title, help) = if !has_file {
        ("Nothing to inspect yet", "Open a data file, then select bytes in Hex.")
    } else if has_decoded {
        ("Select a field or bytes",
            "Choose a field in Structure, a point in Signal, a row in Table, or bytes in Hex.")
    } else {
        ("Select bytes in Hex",
            "Drag across bytes in Hex to see their values, or press Run (F5) to inspect decoded fields.")
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).color(crate::theme::DARK.color.text.muted));
        info_icon(ui, help);
    });
}

/// A padded inset card, as used for the field card.
pub fn card<R>(ui: &mut Ui, contents: impl FnOnce(&mut Ui) -> R) -> R {
    let t = &crate::theme::DARK;
    egui::Frame::new()
        .fill(t.color.surface.card)
        .corner_radius(t.radius.inset)
        .inner_margin(egui::Margin::same(t.spacing.card_padding as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            contents(ui)
        })
        .inner
}

/// The selected field: clickable path, name and type badge, large value,
/// inclusive range and one action row. `menu` adds the host's items to the ⋯ menu.
pub fn field_card(ui: &mut Ui, field: &FieldInfo, mut edit: Option<&mut ValueEdit>, can_edit: bool,
    menu: impl FnOnce(&mut Ui)) -> Option<FieldAction> {
    let t = &crate::theme::DARK;
    let mut action = None;
    card(ui, |ui| {
        if let Some(path) = breadcrumb(ui, field) {
            action = Some(FieldAction::SelectPath(path));
        }
        ui.horizontal(|ui| {
            let size = field.end.saturating_sub(field.start);
            let badge = format!("{} · {}", field.kind, crate::format::size(size));
            // The badge keeps its size; the name gives way first.
            let badge_width = ui.fonts_mut(|fonts| fonts.layout_no_wrap(badge.clone(),
                FontId::monospace(t.font.label), t.color.status.success).size().x)
                .min(t.size.inspector_badge_max_width) + 2. * t.spacing.inspector_gap;
            ui.scope(|ui| {
                ui.set_max_width((ui.available_width() - badge_width - ui.spacing().item_spacing.x).max(0.));
                ui.add(egui::Label::new(RichText::new(&field.name).size(t.font.body)
                    .family(FontFamily::Name("Geist SemiBold".into())).color(t.color.text.primary)).truncate())
                    .on_hover_text(&field.name);
            });
            egui::Frame::new()
                .fill(t.color.status.success.gamma_multiply(t.color.status.soft_alpha))
                .corner_radius(t.radius.chip)
                .inner_margin(egui::Margin::symmetric(t.spacing.inspector_gap as i8, 1))
                .show(ui, |ui| {
                    ui.set_max_width(t.size.inspector_badge_max_width);
                    ui.add(egui::Label::new(RichText::new(&badge)
                        .monospace().size(t.font.label).color(t.color.status.success)).truncate())
                        .on_hover_text(format!("{} · {}\n{} endian", field.kind, crate::format::exact_bytes(size),
                            field.endian));
                });
        });
        let editable = field.editable && can_edit;
        match edit.as_deref_mut().filter(|edit| edit.editing && editable) {
            Some(edit) => {
                let response = ui.add(egui::TextEdit::singleline(&mut edit.text)
                    .font(FontId::monospace(t.font.value))
                    .desired_width(f32::INFINITY));
                if !response.has_focus() && !response.lost_focus() && edit.error.is_none() {
                    response.request_focus();
                }
                let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
                if response.lost_focus() && enter && edit.text != edit.original {
                    action = Some(FieldAction::ApplyValue);
                } else if ui.input(|input| input.key_pressed(egui::Key::Escape))
                    || (response.lost_focus() && edit.text == edit.original) {
                    edit.cancel();
                }
                if let Some(error) = &edit.error {
                    ui.colored_label(t.color.status.danger, error);
                }
            }
            None => {
                let full = if field.display.is_empty() { &field.value } else { &field.display };
                // A collection shows its count; the rest of its summary is on hover.
                let value = if field.kind.ends_with("[]") {
                    full.split(" · ").next().unwrap_or(full)
                } else {
                    full
                };
                let response = ui.add(egui::Label::new(RichText::new(value).size(t.font.value)
                    .color(t.color.text.primary)).truncate()
                    .sense(if editable { Sense::click() } else { Sense::hover() }));
                let response = if editable {
                    response.on_hover_cursor(egui::CursorIcon::Text)
                        .on_hover_text(format!("{full}\nClick to edit. Enter writes the bytes."))
                } else {
                    response.on_hover_text(full)
                };
                if response.clicked() {
                    if let Some(edit) = edit.as_deref_mut() {
                        edit.editing = true;
                    }
                }
            }
        }
        ui.add(egui::Label::new(RichText::new(crate::format::byte_range(field.start, field.end))
            .monospace().size(t.font.label).color(t.color.text.muted)).truncate());
        ui.add_space(t.spacing.inspector_gap);
        ui.horizontal(|ui| {
            if ui.button("Jump to definition").on_hover_text("Show this field in the decoder").clicked() {
                action = Some(FieldAction::JumpToDefinition);
            }
            crate::theme::more_menu(ui, |ui| {
                if editable && ui.button("Edit value").clicked() {
                    if let Some(edit) = edit {
                        edit.editing = true;
                    }
                    ui.close();
                }
                if ui.button("Copy value").clicked() {
                    ui.ctx().copy_text(field.value.clone());
                    ui.close();
                }
                if ui.button("Copy path").clicked() {
                    ui.ctx().copy_text(field.path.clone());
                    ui.close();
                }
                ui.separator();
                menu(ui);
            });
        });
    });
    action
}

/// `sections[0] › payload › entries[1] › name`. Leading parts are dropped when the
/// path is too long for the card; the full path shows on hover.
fn breadcrumb(ui: &mut Ui, field: &FieldInfo) -> Option<String> {
    let t = &crate::theme::DARK;
    let parts = crate::structure_selection::path_parts(&field.path);
    let font = FontId::monospace(t.font.label);
    let width = |text: &str| ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.into(), font.clone(),
        t.color.text.muted).size().x);
    let separator = " › ";
    let available = ui.available_width();
    let mut first = 0;
    while first + 1 < parts.len() {
        let shown = parts[first..].iter().map(|(label, _)| *label).collect::<Vec<_>>().join(separator);
        let prefix = if first > 0 { width(&format!("…{separator}")) } else { 0. };
        if width(&shown) + prefix <= available { break; }
        first += 1;
    }
    let mut chosen = None;
    let response = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.;
        if first > 0 {
            ui.label(RichText::new(format!("…{separator}")).font(font.clone()).color(t.color.text.muted));
        }
        for (index, (label, path)) in parts.iter().enumerate().skip(first) {
            if index + 1 == parts.len() {
                ui.label(RichText::new(*label).font(font.clone()).color(t.color.text.secondary));
            } else {
                let link = ui.add(egui::Label::new(RichText::new(*label).font(font.clone())
                    .color(t.color.text.muted)).sense(Sense::click()));
                let link = link.on_hover_cursor(egui::CursorIcon::PointingHand);
                if link.hovered() {
                    ui.painter().line_segment([link.rect.left_bottom(), link.rect.right_bottom()],
                        egui::Stroke::new(t.size.frame_border, t.color.text.muted));
                }
                if link.clicked() {
                    chosen = Some((*path).to_owned());
                }
                ui.label(RichText::new(separator).font(font.clone()).color(t.color.text.faint));
            }
        }
    }).response;
    response.on_hover_text(&field.path);
    chosen
}

/// Values of the selected bytes as each type. `default_big` follows the field
/// or decoder byte order until the user picks one. `menu`, when given, adds a ⋯
/// menu for byte actions (used when no decoded field is selected).
pub fn selected_bytes(ui: &mut Ui, range: Range<u64>, bytes: Option<&[u8]>, default_big: bool,
    menu: Option<&mut dyn FnMut(&mut Ui)>) {
    let t = &crate::theme::DARK;
    let order_id = egui::Id::new("inspector_big_endian");
    let mut big = ui.data(|data| data.get_temp::<bool>(order_id)).unwrap_or(default_big);
    ui.add_space(t.spacing.inspector_gap);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Selected bytes").size(t.font.body).color(t.color.text.secondary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(menu) = menu {
                crate::theme::more_menu(ui, |ui| menu(ui));
            }
            egui::Frame::new().fill(t.color.surface.window).corner_radius(t.radius.control)
                .inner_margin(egui::Margin::same(2)).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 0.;
                    let before = big;
                    // Right-to-left: Big is added first so Little shows on the left.
                    ui.selectable_value(&mut big, true, "Big");
                    ui.selectable_value(&mut big, false, "Little");
                    if big != before {
                        ui.data_mut(|data| data.insert_temp(order_id, big));
                    }
                });
        });
    });
    ui.add_space(t.spacing.inspector_gap);
    let Some(bytes) = bytes.filter(|bytes| !bytes.is_empty()) else {
        ui.label(RichText::new("Loading selected bytes…").color(t.color.text.muted));
        return;
    };
    let length = (range.end - range.start).min(bytes.len() as u64) as usize;
    let bytes = &bytes[..length];
    let cells: Vec<Cell> = interpretations(bytes, big, range.end - range.start)
        .into_iter().filter(|cell| !cell.missing).collect();
    let gap = t.spacing.inspector_gap;
    let chip_width = ((ui.available_width() - gap) / 2.).max(0.);
    ui.spacing_mut().item_spacing = egui::vec2(gap, gap);
    for pair in cells.chunks(2) {
        ui.horizontal(|ui| {
            for cell in pair {
                chip(ui, chip_width, cell);
            }
        });
    }
    if let Some(note) = wider_types_note(range.end - range.start) {
        ui.add(egui::Label::new(RichText::new(note).size(t.font.label).color(t.color.text.muted)).truncate());
    }
}

/// One line in place of the types a short selection can't hold.
fn wider_types_note(selected: u64) -> Option<&'static str> {
    match selected {
        0 | 8.. => None,
        1 => Some("Select 2+ bytes for 16-bit and wider types"),
        2 | 3 => Some("Select 4+ bytes for 32- and 64-bit types"),
        _ => Some("Select 8+ bytes for 64-bit types"),
    }
}

struct Cell {
    label: &'static str,
    value: String,
    hover: String,
    missing: bool,
}

fn interpretations(bytes: &[u8], big: bool, selected: u64) -> Vec<Cell> {
    fn take<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
        bytes.get(..N).and_then(|slice| slice.try_into().ok())
    }
    let needs = |size: usize| Cell {
        label: "", value: format!("needs {size} B"),
        hover: format!("Select at least {} to read this type.", crate::format::plural(size, "byte", "bytes")),
        missing: true,
    };
    macro_rules! integer {
        ($label:expr, $ty:ty, $size:expr) => {
            match take::<$size>(bytes) {
                Some(raw) => {
                    let value = if big { <$ty>::from_be_bytes(raw) } else { <$ty>::from_le_bytes(raw) };
                    Cell { label: $label, value: crate::format::count(value),
                        hover: format!("{value} · 0x{value:X}"), missing: false }
                }
                None => Cell { label: $label, ..needs($size) },
            }
        };
    }
    macro_rules! float {
        ($label:expr, $ty:ty, $size:expr) => {
            match take::<$size>(bytes) {
                Some(raw) => {
                    let value = if big { <$ty>::from_be_bytes(raw) } else { <$ty>::from_le_bytes(raw) };
                    Cell { label: $label, value: crate::format::short_float(value as f64),
                        hover: value.to_string(), missing: false }
                }
                None => Cell { label: $label, ..needs($size) },
            }
        };
    }
    let text: String = String::from_utf8_lossy(bytes).chars()
        .map(|c| if c.is_control() { '·' } else { c }).collect();
    let hex = bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
    let more = if selected > bytes.len() as u64 {
        format!("\nFirst {} of {} shown.", bytes.len(), crate::format::exact_bytes(selected))
    } else {
        String::new()
    };
    vec![
        integer!("u8", u8, 1), integer!("i8", i8, 1),
        integer!("u16", u16, 2), integer!("i16", i16, 2),
        integer!("u32", u32, 4), integer!("i32", i32, 4),
        integer!("u64", u64, 8), integer!("i64", i64, 8),
        float!("f32", f32, 4), float!("f64", f64, 8),
        Cell { label: "text", value: format!("\"{text}\""), hover: format!("{text}{more}"), missing: false },
        Cell { label: "hex", value: hex.clone(), hover: format!("{hex}{more}"), missing: false },
    ]
}

fn chip(ui: &mut Ui, width: f32, cell: &Cell) {
    let t = &crate::theme::DARK;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, t.size.inspector_chip_height), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, t.radius.row, t.color.surface.card);
    let inner = rect.shrink2(egui::vec2(t.spacing.card_padding, 0.));
    let label = painter.text(inner.left_center(), Align2::LEFT_CENTER, cell.label,
        FontId::proportional(t.font.label), t.color.text.muted);
    let font = if cell.missing { FontId::proportional(t.font.label) } else { FontId::monospace(t.font.small) };
    let color = if cell.missing { t.color.text.faint } else { t.color.text.primary };
    let room = inner.right() - label.right() - t.spacing.card_padding;
    let mut value = cell.value.clone();
    let fits = |text: &str| ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.into(), font.clone(), color).size().x) <= room;
    if !fits(&value) {
        while !value.is_empty() && !fits(&format!("{value}…")) {
            value.pop();
        }
        value = format!("{}…", value.trim_end());
    }
    painter.text(inner.right_center(), Align2::RIGHT_CENTER, value, font, color);
    response.on_hover_text(&cell.hover);
}

/// "Definition" row, collapsed by default: the field's structure and type.
/// Byte order is the Little/Big switch above it.
pub fn definition(ui: &mut Ui, field: &FieldInfo, line: Option<usize>) {
    let t = &crate::theme::DARK;
    ui.add_space(t.spacing.inspector_gap);
    let title = match line {
        Some(line) => format!("Definition · line {}", crate::format::count(line)),
        None => "Definition".into(),
    };
    egui::CollapsingHeader::new(RichText::new(title).color(t.color.text.muted))
        .id_salt("inspector_definition")
        .default_open(false)
        .show(ui, |ui| {
            let row = |ui: &mut Ui, label: &str, value: &str, hover: String| {
                ui.horizontal(|ui| {
                    ui.add_sized([t.size.inspector_definition_label, t.size.inspector_chip_height / 2.],
                        egui::Label::new(RichText::new(label).size(t.font.label).color(t.color.text.muted)));
                    ui.add(egui::Label::new(RichText::new(value).monospace().size(t.font.small)).truncate())
                        .on_hover_text(hover);
                });
            };
            if let Some(structure) = field.structure.as_deref() {
                row(ui, "Structure", structure, structure.to_owned());
            }
            let uses = if field.references.is_empty() {
                String::new()
            } else {
                format!("\nUses {}", field.references.join(", "))
            };
            row(ui, "Type", &field.kind, format!("{}{uses}", field.kind));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, path: &str, structure: Option<&str>, own: Option<&str>) -> FieldInfo {
        FieldInfo {
            name: name.into(), path: path.into(), kind: "u32".into(), value: String::new(),
            editable: false, endian: "little".into(), start: 0, end: 4, references: Vec::new(),
            sample: None, sample_start: None, sample_end: None, display: String::new(),
            structure: structure.map(Into::into), own_structure: own.map(Into::into), crumbs: Vec::new(),
        }
    }

    const DECODER: &str = "endian: little\nstructures:\n  - name: Header\n    fields:\n      - {name: count, type: u32}\n  - name: Field\n    fields:\n      - {name: name, type: string, length: 16}\n      - name: count\n        type: u32\nactions:\n  - read: {name: entries, type: {structure: Field}, repeat: 3}\n  - read: {name: count, type: u32}\n";

    #[test]
    fn definition_line_finds_the_field_in_its_own_structure() {
        assert_eq!(definition_line(DECODER, &field("name", "entries[1].name", Some("Field"), None)), Some(8));
        assert_eq!(definition_line(DECODER, &field("count", "entries[1].count", Some("Field"), None)), Some(9));
        assert_eq!(definition_line(DECODER, &field("count", "header.count", Some("Header"), None)), Some(5));
    }

    #[test]
    fn definition_line_uses_actions_for_top_level_reads_and_types_for_items() {
        assert_eq!(definition_line(DECODER, &field("count", "count", None, None)), Some(13));
        assert_eq!(definition_line(DECODER, &field("[1]", "entries[1]", None, Some("Field"))), Some(6));
        assert_eq!(definition_line(DECODER, &field("[1]", "entries[1]", None, None)), Some(12));
    }

    #[test]
    fn field_info_shows_enum_meanings_structures_and_parent_paths() {
        use crate::byte_store::ByteStore;
        let yaml = "structures:\n  - name: Field\n    fields:\n      - {name: name, type: string, length: 4}\n      - {name: format, type: u32}\n  - name: Header\n    fields:\n      - {name: entries, type: {structure: Field}, repeat: 2}\n    actions:\n      - define:\n          name: Sample\n          from: entries\n          fields:\n            name: {ref: name}\n            type: {ref: format, cases: {'3': f64, '5': u32}}\nactions:\n  - read: {name: header, type: {structure: Header}}\n";
        let program = Program::parse(yaml).unwrap();
        let mut bytes = Vec::new();
        for (name, format) in [(b"sine", 3_u32), (b"ramp", 5)] {
            bytes.extend_from_slice(name);
            bytes.extend_from_slice(&format.to_le_bytes());
        }
        let store = ByteStore::from_snapshot(crate::binary::Original::from_bytes(&bytes).unwrap());
        let source = store.view();
        let output = program.run(&source, &std::sync::atomic::AtomicBool::new(false)).unwrap();
        let node = |path: &str| crate::structure_selection::node_at_path(&output.nodes, &source, path).unwrap().unwrap();
        let info = |path: &str| {
            let parent = crate::structure_selection::parent_path(path).map(node);
            FieldInfo::from_node(&node(path), None, Some(&program), parent.as_ref())
        };
        let format = info("header.entries[0].format");
        assert_eq!(format.display, "3 · f64");
        assert_eq!(format.structure.as_deref(), Some("Field"));
        assert_eq!(definition_line(yaml, &format), Some(5));
        assert_eq!(info("header.entries[1].format").display, "5 · u32");
        let item = info("header.entries[1]");
        assert_eq!(item.own_structure.as_deref(), Some("Field"));
        assert_eq!(definition_line(yaml, &item), Some(2));
        assert_eq!(crate::structure_selection::path_parts("header.entries[1].format").iter()
            .map(|(label, _)| *label).collect::<Vec<_>>(), ["header", "entries[1]", "format"]);
    }

    #[test]
    fn selected_bytes_read_both_byte_orders_and_report_short_selections() {
        let cells = interpretations(&[0x74, 0x72], false, 2);
        let value = |label: &str| cells.iter().find(|cell| cell.label == label).unwrap();
        assert_eq!(value("u16").value, "29,300");
        assert_eq!(interpretations(&[0x74, 0x72], true, 2).iter()
            .find(|cell| cell.label == "u16").unwrap().value, "29,810");
        assert!(value("u32").missing);
        assert_eq!(value("u32").value, "needs 4 B");
        assert_eq!(value("text").value, "\"tr\"");
        assert_eq!(value("hex").value, "74 72");
    }

    #[test]
    fn short_selections_get_one_line_instead_of_a_row_per_missing_type() {
        assert_eq!(wider_types_note(1), Some("Select 2+ bytes for 16-bit and wider types"));
        assert_eq!(wider_types_note(3), Some("Select 4+ bytes for 32- and 64-bit types"));
        assert_eq!(wider_types_note(7), Some("Select 8+ bytes for 64-bit types"));
        assert_eq!(wider_types_note(8), None);
        let shown: Vec<_> = interpretations(&[1], false, 1).into_iter().filter(|cell| !cell.missing)
            .map(|cell| cell.label).collect();
        assert_eq!(shown, ["u8", "i8", "text", "hex"]);
    }

    #[test]
    fn ctrl_i_hides_and_shows_the_docked_inspector() {
        use crate::egui_compat::ContextRun as _;
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let frame = |events: Vec<egui::Event>| {
            let output = ctx.run(egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200., 600.))),
                ..Default::default()
            }, |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    docked_panel(ui, "inspector", &mut (), 600., |_, ui: &mut Ui| header(ui, None),
                        |_, ui: &mut Ui| { ui.label("body"); });
                });
            });
            output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == "body"))
        };
        let ctrl_i = || vec![egui::Event::Key { key: egui::Key::I, physical_key: None, pressed: true,
            repeat: false, modifiers: egui::Modifiers::COMMAND }];
        assert!(frame(vec![]), "open by default");
        assert!(!frame(ctrl_i()));
        assert!(!is_open(&ctx));
        assert!(!frame(vec![]), "stays hidden");
        assert!(frame(ctrl_i()));
    }
}
