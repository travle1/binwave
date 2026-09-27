//! Browser-side rendering of the decoded Structure panel. Values are supplied
//! by the decoder worker; large arrays are fetched a page at a time.
use egui::{Align2, FontFamily, FontId, Sense, Ui};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct Action {
    pub toggle: Option<(String, bool, bool)>,
    pub select: Option<(String, u64, u64)>,
}

fn number(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, digit) in text.chars().enumerate() {
        if index > 0 && (text.len() - index) % 3 == 0 { out.push(','); }
        out.push(digit);
    }
    out
}
fn size(bytes: u64) -> String {
    if bytes < 1024 { format!("{bytes} B") }
    else if bytes < 1024 * 1024 { format!("{:.2} KiB", bytes as f64 / 1024.) }
    else if bytes < 1024 * 1024 * 1024 { format!("{:.2} MiB", bytes as f64 / (1024. * 1024.)) }
    else { format!("{:.2} GiB", bytes as f64 / (1024. * 1024. * 1024.)) }
}
fn collect(node: &Value, depth: usize, expanded: &HashSet<String>, pages: &HashMap<String, Value>, rows: &mut Vec<(Value, usize)>) {
    rows.push((node.clone(), depth));
    let path = node["path"].as_str().unwrap_or("");
    if !expanded.contains(path) { return; }
    let children = pages.get(path).and_then(|page| page["nodes"].as_array())
        .or_else(|| node["children"].as_array());
    if let Some(children) = children {
        for child in children { collect(child, depth + 1, expanded, pages, rows); }
    }
}
fn row_value(node: &Value) -> String {
    if let Some(count) = node["count"].as_str().and_then(|s| s.parse::<u64>().ok()) {
        return format!("{} items", number(count));
    }
    if let Some(value) = node["value"].as_str().filter(|s| !s.is_empty()) {
        if let Ok(integer) = value.parse::<u64>() { return number(integer); }
        return value.to_owned();
    }
    if let Some(children) = node["children"].as_array() {
        if let Some(identifier) = children.iter().find(|child| matches!(child["name"].as_str(), Some("name" | "id" | "label")))
            .or_else(|| children.first()) {
            if let Some(value) = identifier["value"].as_str() { return value.to_owned(); }
        }
        if !children.is_empty() { return format!("{} items", number(children.len() as u64)); }
    }
    String::new()
}

pub fn show(ui: &mut Ui, decoded: &Value, expanded: &HashSet<String>, pages: &HashMap<String, Value>, selected: Option<&str>) -> Action {
    let mut rows = Vec::new();
    for node in decoded["nodes"].as_array().into_iter().flatten() {
        collect(node, 0, expanded, pages, &mut rows);
    }
    if rows.is_empty() {
        ui.label("Select bytes in Hex and run a decoder to inspect structure.");
        return Action::default();
    }
    let theme = &crate::theme::DARK;
    let widths = [300., 150., 210., 130., 95.];
    let total = widths.iter().sum::<f32>();
    let mut action = Action::default();
    let old_spacing = ui.spacing().item_spacing.y;
    ui.spacing_mut().item_spacing.y = 0.;
    egui::ScrollArea::both().id_salt("browser_structure_columns")
        .auto_shrink([false, false]).show_rows(ui, 24., rows.len() + 1, |ui, range| {
            for index in range {
                let (rect, response) = ui.allocate_exact_size(egui::vec2(total, 24.), Sense::click());
                if index == 0 {
                    ui.painter().rect_filled(rect, 5., theme.color.surface.raised);
                    let mut x = rect.left() + 6.;
                    for (heading, width) in ["Name", "Type", "Value", "Offset", "Size"].into_iter().zip(widths) {
                        ui.painter().text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER,
                            heading, FontId::proportional(11.), theme.color.text.muted);
                        x += width;
                    }
                    continue;
                }
                let (node, depth) = &rows[index - 1];
                let path = node["path"].as_str().unwrap_or("");
                let start = node["start"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let end = node["end"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(start);
                let count = node["count"].as_str().and_then(|s| s.parse::<u64>().ok());
                let expandable = count.is_some() || node["children"].as_array().is_some_and(|children| !children.is_empty());
                let open = expanded.contains(path);
                if selected == Some(path) {
                    ui.painter().rect_filled(rect.shrink2(egui::vec2(0., 1.)), 5.,
                        theme.color.accent.base.gamma_multiply(theme.color.accent.selection_alpha));
                } else if index % 2 == 0 {
                    ui.painter().rect_filled(rect, 0., theme.color.surface.raised.gamma_multiply(0.30));
                }
                let left = rect.left() + 6. + (*depth).min(32) as f32 * 16.;
                let arrow = egui::Rect::from_min_size(egui::pos2(left, rect.top() + 3.), egui::vec2(16., 18.));
                if expandable {
                    let center = arrow.center();
                    let stroke = egui::Stroke::new(1.35, theme.color.text.muted);
                    let points = if open {
                        [egui::pos2(center.x - 4., center.y - 2.), egui::pos2(center.x, center.y + 2.), egui::pos2(center.x + 4., center.y - 2.)]
                    } else {
                        [egui::pos2(center.x - 2., center.y - 4.), egui::pos2(center.x + 2., center.y), egui::pos2(center.x - 2., center.y + 4.)]
                    };
                    ui.painter().line_segment([points[0], points[1]], stroke);
                    ui.painter().line_segment([points[1], points[2]], stroke);
                }
                let toggle = ui.interact(arrow, ui.id().with((path, "toggle")), Sense::click());
                if expandable && toggle.clicked() { action.toggle = Some((path.to_owned(), !open, count.is_some())); }
                else if response.clicked() { action.select = Some((path.to_owned(), start, end)); }
                response.context_menu(|ui| {
                    if ui.button("Copy value").clicked() { ui.ctx().copy_text(row_value(node)); ui.close(); }
                    if ui.button("Copy path").clicked() { ui.ctx().copy_text(path.to_owned()); ui.close(); }
                });
                response.on_hover_text(format!("{path}\n{} bytes · 0x{start:08X}–0x{end:08X}", number(end.saturating_sub(start))));
                let fields = [
                    (node["name"].as_str().unwrap_or("" ).to_owned(), theme.color.text.primary, left + 16., widths[0] - (left - rect.left()) - 20.),
                    (node["kind"].as_str().unwrap_or("").to_owned(), theme.color.text.muted, rect.left() + widths[0] + 6., widths[1] - 10.),
                    (row_value(node), theme.color.text.primary, rect.left() + widths[0] + widths[1] + 6., widths[2] - 10.),
                    (format!("0x{start:08X}"), theme.color.text.muted, rect.left() + widths[0] + widths[1] + widths[2] + 6., widths[3] - 10.),
                    (size(end.saturating_sub(start)), theme.color.text.muted, rect.left() + widths[0] + widths[1] + widths[2] + widths[3] + 6., widths[4] - 10.),
                ];
                for (text, color, x, width) in fields {
                    let cell = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(width.max(0.), rect.height()));
                    ui.painter().with_clip_rect(cell).text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER,
                        text, FontId::new(11., FontFamily::Monospace), color);
                }
            }
        });
    ui.spacing_mut().item_spacing.y = old_spacing;
    action
}
