//! Browser adapter: shared egui docking, with asynchronous worker-backed byte views.
//! Native decoder modules are deliberately excluded until their I/O is portable.
use crate::workspace_layout::{Pane, WorkspaceLayout};
use egui_dock::{DockArea, TabViewer};
use serde::Deserialize;
use wasm_bindgen::prelude::*;

const PAGE: usize = 64 * 1024;
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = binwave, js_name = poll)]
    fn poll_event() -> String;
    #[wasm_bindgen(js_namespace = binwave, js_name = send)]
    fn send_command(command: &str) -> bool;
    #[wasm_bindgen(js_namespace = binwave, js_name = selection)]
    fn export_selection(start: &str, end: &str);
    #[wasm_bindgen(js_namespace = binwave, js_name = error)]
    pub fn startup_error(message: &str);
    #[wasm_bindgen(js_namespace = binwave, js_name = ready)]
    fn browser_ready();
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    size: String,
    offset: String,
    bytes: Vec<u8>,
    message: String,
    dirty: bool,
    undo: bool,
    redo: bool,
}

pub struct BrowserApp {
    layout: WorkspaceLayout,
    name: String,
    size: u64,
    offset: u64,
    bytes: Vec<u8>,
    needs_read: bool,
    jump: String,
    selected: Option<u64>,
    range_start: String,
    range_end: String,
    replacement: String,
    status: String,
    dirty: bool,
    undo: bool,
    redo: bool,
}
impl BrowserApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        browser_ready();
        Self {
            layout: cc
                .storage
                .map(|s| WorkspaceLayout::load(Some(s)))
                .unwrap_or_else(WorkspaceLayout::plot_and_bytes),
            name: String::new(),
            size: 0,
            offset: 0,
            bytes: Vec::new(),
            needs_read: false,
            jump: "0".into(),
            selected: None,
            range_start: "0".into(),
            range_end: "0".into(),
            replacement: String::new(),
            status: "Use Open file above to inspect a local file. Files stay on your computer."
                .into(),
            dirty: false,
            undo: false,
            redo: false,
        }
    }
    fn command(&self, value: serde_json::Value) -> bool {
        send_command(&value.to_string())
    }
    fn navigate(&mut self, offset: u64) {
        self.offset = offset.min(self.size.saturating_sub(1));
        self.bytes.clear();
        self.jump = self.offset.to_string();
        self.needs_read = true;
    }
    fn events(&mut self) {
        // At most one file operation is outstanding. Bound work in each UI frame.
        for _ in 0..8 {
            let json = poll_event();
            if json.is_empty() {
                break;
            }
            let event: Event = match serde_json::from_str(&json) {
                Ok(event) => event,
                Err(error) => {
                    self.status = error.to_string();
                    continue;
                }
            };
            match event.kind.as_str() {
                "opened" | "edited" | "exported" => {
                    self.dirty = event.dirty;
                    self.undo = event.undo;
                    self.redo = event.redo;
                    if event.kind == "opened" {
                        self.name = event.name;
                        self.size = event.size.parse().unwrap_or(0);
                        self.selected = None;
                        self.range_start = "0".into();
                        self.range_end = "0".into();
                        self.navigate(0);
                        self.status = "File opened. Byte preview shows the first 1,024 bytes of the current page.".into();
                    } else if event.kind == "edited" {
                        self.bytes.clear();
                        self.needs_read = true;
                        self.status = "Byte edit applied. Save edited copy to export.".into();
                    } else {
                        self.status = "Export complete.".into();
                    }
                }
                "page" => {
                    // A navigation made during a read must never show stale bytes.
                    if event.offset.parse::<u64>().ok() == Some(self.offset)
                        && event.bytes.len() <= PAGE
                    {
                        self.bytes = event.bytes;
                    }
                }
                "error" => {
                    self.status = event.message;
                }
                _ => {}
            }
        }
        if self.needs_read {
            if let Ok(length) = crate::file_range::bounded_length(self.offset, PAGE, self.size) {
                if self.command(serde_json::json!({"type":"read", "offset":self.offset.to_string(), "length":length})) {
                    self.needs_read = false;
                }
            }
        }
    }
    fn hex(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.offset > 0, egui::Button::new("Previous page"))
                .clicked()
            {
                self.navigate(self.offset.saturating_sub(PAGE as u64));
            }
            if ui
                .add_enabled(
                    self.offset.saturating_add(PAGE as u64) < self.size,
                    egui::Button::new("Next page"),
                )
                .clicked()
            {
                self.navigate(self.offset + PAGE as u64);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Byte offset");
            ui.add(egui::TextEdit::singleline(&mut self.jump).desired_width(160.0));
            if ui.button("Go").clicked() {
                match parse_offset(&self.jump) {
                    Some(at) if at < self.size || (at == 0 && self.size == 0) => self.navigate(at),
                    _ => {
                        self.status = "Enter an offset inside the file (decimal or 0x hex).".into()
                    }
                }
            }
        });
        ui.label(format!(
            "64 KiB page at 0x{:X} · {} bytes loaded",
            self.offset,
            self.bytes.len()
        ));
        let rows = self.bytes.len().div_ceil(16);
        egui::ScrollArea::both()
            .id_source("browser-hex")
            .show_rows(ui, 22.0, rows, |ui, range| {
                for row in range {
                    let start = row * 16;
                    let end = (start + 16).min(self.bytes.len());
                    ui.horizontal(|ui| {
                        ui.monospace(format!("{:016X}", self.offset + start as u64));
                        for i in start..end {
                            let at = self.offset + i as u64;
                            let label =
                                egui::RichText::new(format!("{:02X}", self.bytes[i])).monospace();
                            if ui
                                .selectable_label(self.selected == Some(at), label)
                                .clicked()
                            {
                                self.selected = Some(at);
                                self.replacement = format!("{:02X}", self.bytes[i]);
                                self.range_start = at.to_string();
                                self.range_end = (at + 1).to_string();
                                export_selection(&self.range_start, &self.range_end);
                            }
                        }
                        ui.monospace(
                            self.bytes[start..end]
                                .iter()
                                .map(|&b| crate::byte_display::ascii(b))
                                .collect::<String>(),
                        );
                    });
                }
            });
    }
    fn inspector(&mut self, ui: &mut egui::Ui) {
        if let Some(at) = self.selected {
            ui.monospace(format!("Selected byte: {at} / 0x{at:X}"));
            ui.horizontal(|ui| {
                ui.label("Replace with hex byte:");
                ui.add(egui::TextEdit::singleline(&mut self.replacement).desired_width(40.0).char_limit(2));
                if ui.button("Apply").clicked() {
                    if let Ok(value) = u8::from_str_radix(self.replacement.trim(), 16) {
                        if !self.command(serde_json::json!({"type":"replace","offset":at.to_string(),"value":value})) {
                            self.status = "Wait for the current file operation.".into();
                        }
                    } else { self.status = "Enter a hexadecimal byte, 00 through FF.".into(); }
                }
            });
        } else {
            ui.label("Select a byte in Hex to edit it.");
        }
        ui.separator();
        ui.label("Extraction range (start inclusive, end exclusive):");
        ui.text_edit_singleline(&mut self.range_start);
        ui.text_edit_singleline(&mut self.range_end);
        if ui.button("Set extraction range").clicked() {
            match (
                parse_offset(&self.range_start),
                parse_offset(&self.range_end),
            ) {
                (Some(start), Some(end)) if start <= end && end <= self.size => {
                    export_selection(&start.to_string(), &end.to_string());
                    self.status = format!(
                        "Extraction range {start}..{end} set. Use Extract selection above."
                    );
                }
                _ => self.status = "Extraction range must be inside the file.".into(),
            }
        }
        ui.label("Edits are sparse: up to 65,536 byte positions and 4,096 undo steps. Unsaved edits are lost on reload.");
    }
    fn decoder(&mut self, ui: &mut egui::Ui) {
        ui.heading("Browser preview");
        ui.label("Hex and Byte preview read only the current page. Navigate to any byte without scanning the file.");
        ui.separator();
        ui.label("YAML, CSV, and Python decoders are not yet available in this browser version. Use the native app for decoding, structure views, full-file searches, and large decoded plots.");
        ui.label("Native Python subprocesses cannot run in the browser.");
        ui.separator();
        self.inspector(ui);
    }
}
fn parse_offset(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}
impl eframe::App for BrowserApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.events();
        egui::TopBottomPanel::top("browser-menu").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Binwave · Browser preview");
                if ui.button("Reset workspace").clicked() {
                    self.layout = WorkspaceLayout::plot_and_bytes();
                }
                if ui
                    .add_enabled(self.undo, egui::Button::new("Undo"))
                    .clicked()
                {
                    self.command(serde_json::json!({"type":"undo"}));
                }
                if ui
                    .add_enabled(self.redo, egui::Button::new("Redo"))
                    .clicked()
                {
                    self.command(serde_json::json!({"type":"redo"}));
                }
                ui.label(format!(
                    "{} · {} bytes{}",
                    self.name,
                    self.size,
                    if self.dirty { " · unsaved edits" } else { "" }
                ));
            });
        });
        egui::TopBottomPanel::bottom("browser-status").show(ctx, |ui| {
            ui.label(&self.status);
        });
        // Move the dock state temporarily so its viewer can borrow the application.
        let mut layout = std::mem::take(&mut self.layout);
        DockArea::new(&mut layout.dock).show(ctx, self);
        self.layout = layout;
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.layout.save(storage);
    }
}
impl TabViewer for BrowserApp {
    type Tab = Pane;
    fn title(&mut self, pane: &mut Pane) -> egui::WidgetText {
        pane.title().into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, pane: &mut Pane) {
        match pane {
            Pane::Hex => self.hex(ui),
            Pane::Layout => self.decoder(ui),
            Pane::Inspector => self.inspector(ui),
            Pane::Plot => {
                ui.label("Byte preview · unsigned 8-bit · first 1,024 bytes of this page");
                let points: Vec<[f64; 2]> = self
                    .bytes
                    .iter()
                    .take(1024)
                    .enumerate()
                    .map(|(i, &b)| [(self.offset + i as u64) as f64, b as f64])
                    .collect();
                egui_plot::Plot::new(("browser-byte-preview", self.offset)).show(ui, |plot| {
                    plot.line(egui_plot::Line::new(points).name("Byte preview"));
                });
            }
            Pane::Table | Pane::Structures => {
                ui.label("Decoded results require the native application in this preview.");
            }
        }
    }
}
