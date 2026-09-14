//! Browser adapter: shared egui docking, with asynchronous worker-backed byte views.
//! Uses the native decoder, plot reducer, trace renderer, and source editor.
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
    source: String,
    name: String,
    size: String,
    offset: String,
    bytes: Vec<u8>,
    message: String,
    dirty: bool,
    undo: bool,
    redo: bool,
    result: serde_json::Value,
    request: String,
    reset: bool,
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
    yaml: String,
    editor: crate::source_editor::YamlLayoutCache,
    decoded: serde_json::Value,
    table: serde_json::Value,
    plot: serde_json::Value,
    collection: String,
    table_start: u64,
    first: String,
    last: String,
    pending: std::collections::VecDeque<serde_json::Value>,
    hidden: std::collections::BTreeSet<String>,
    decoding: bool,
    reveal_results: bool,
    reveal_decoder: bool,
    plot_request: u64,
    reset_plot: bool,
    layout_mode: String,
    stacked: bool,
    mobile_target: Option<Pane>,
    viewport: crate::browser_viewport::ViewportRefresh,
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
            yaml: cc
                .storage
                .and_then(|s| s.get_string("browser-decoder-yaml"))
                .filter(|s| s.len() <= 256 * 1024)
                .unwrap_or_else(|| {
                    crate::source_editor::DecoderLanguage::Yaml
                        .byte_preview()
                        .into()
                }),
            editor: Default::default(),
            decoded: Default::default(),
            table: Default::default(),
            plot: Default::default(),
            collection: String::new(),
            table_start: 0,
            first: "0".into(),
            last: "0".into(),
            pending: Default::default(),
            hidden: Default::default(),
            decoding: false,
            reveal_results: false,
            reveal_decoder: false,
            plot_request: 0,
            reset_plot: false,
            layout_mode: cc.storage.and_then(|s|s.get_string("browser-layout-mode")).filter(|s|matches!(s.as_str(),"auto"|"stacked"|"docked")).unwrap_or_else(||"auto".into()),
            stacked: false,
            mobile_target: None,
            viewport: Default::default(),
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
                        self.clear_results();
                        self.name = event.name;
                        self.size = event.size.parse().unwrap_or(0);
                        self.selected = None;
                        self.range_start = "0".into();
                        self.range_end = "0".into();
                        self.navigate(0);
                        self.status =
                            "File opened. Choose a YAML example or write a decoder, then press F5."
                                .into();
                    } else if event.kind == "edited" {
                        self.clear_results();
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
                        if let Some(index) = self
                            .selected
                            .and_then(|at| at.checked_sub(self.offset))
                            .and_then(|at| usize::try_from(at).ok())
                        {
                            if let Some(byte) = self.bytes.get(index) {
                                self.replacement = format!("{byte:02X}");
                            }
                        }
                    }
                }
                "decoder-source" => {
                    if event.source.len() <= 256 * 1024 {
                        self.yaml = event.source;
                        self.reveal_decoder = true;
                        self.mobile_target = Some(Pane::Layout);
                        self.status = format!("Loaded {}. Tap Apply to decode the open data file.", event.name);
                    } else {
                        self.status = "Decoder source is limited to 256 KiB.".into();
                    }
                }
                "decoded" => {
                    self.decoding = false;
                    self.decoded = event.result;
                    self.table = Default::default();
                    self.plot = Default::default();
                    self.reveal_results = true;
                    self.mobile_target = Some(Pane::Plot);
                    let path = self.decoded["collections"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|v| v["path"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    self.select_collection(path);
                    self.status =
                        "Decoder applied. Select a collection to inspect samples and channels."
                            .into();
                }
                "table" => {
                    if event.result["path"].as_str() == Some(&self.collection) {
                        self.table = event.result;
                    }
                }
                "plot" => {
                    if event.result["path"].as_str() == Some(&self.collection)
                        && event.request == self.plot_request.to_string() {
                        self.reset_plot = event.reset;
                        self.plot = event.result;
                    }
                }
                "error" => {
                    self.decoding = false;
                    self.pending.clear();
                    self.status = event.message;
                }
                _ => {}
            }
        }
        if let Some(request) = self.pending.front() {
            if self.command(request.clone()) {
                self.pending.pop_front();
            }
        } else if self.needs_read {
            if let Ok(length) = crate::file_range::bounded_length(self.offset, PAGE, self.size) {
                if self.command(serde_json::json!({"type":"read", "offset":self.offset.to_string(), "length":length})) {
                    self.needs_read = false;
                }
            }
        }
    }
    fn hex(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
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
        ui.horizontal_wrapped(|ui| {
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
                    ui.horizontal_wrapped(|ui| {
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
            ui.horizontal_wrapped(|ui| {
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
    fn clear_results(&mut self) {
        self.decoded = Default::default();
        self.table = Default::default();
        self.plot = Default::default();
        self.pending.clear();
        self.collection.clear();
        self.decoding = false;
    }
    fn queue(&mut self, value: serde_json::Value) {
        self.pending.retain(|m| m["type"] != value["type"]);
        if self.pending.len() < 4 {
            self.pending.push_back(value);
        }
    }
    fn apply_decoder(&mut self) {
        if self.name.is_empty() {
            self.status = "Open a file first.".into();
            return;
        }
        if self.yaml.len() > 256 * 1024 {
            self.status = "Decoder source is limited to 256 KiB.".into();
            return;
        }
        self.pending.clear();
        self.decoding = true;
        self.queue(serde_json::json!({"type":"decode","yaml":self.yaml}));
        self.status = "Decoding in worker…".into();
    }
    fn count(&self) -> u64 {
        self.decoded["collections"]
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|v| v["path"].as_str() == Some(&self.collection))
            })
            .and_then(|v| v["count"].as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
    fn select_collection(&mut self, path: String) {
        self.collection = path;
        self.table_start = 0;
        self.first = "0".into();
        self.last = self.count().min(512).to_string();
        self.viewport.reset();
        self.table = Default::default();
        self.plot = Default::default();
        if !self.collection.is_empty() {
            self.request_table();
            self.request_plot(1200);
        }
    }
    fn request_table(&mut self) {
        self.queue(serde_json::json!({"type":"table","path":self.collection,"start":self.table_start.to_string()}));
    }
    fn request_plot(&mut self, width: u32) {
        self.viewport.reset();
        self.request_plot_view(width, true);
    }
    fn request_plot_view(&mut self, width: u32, reset: bool) {
        match (parse_offset(&self.first), parse_offset(&self.last)) {
            (Some(start), Some(end)) if start < end && end <= self.count() => {
                self.plot_request = self.plot_request.wrapping_add(1);
                self.queue(serde_json::json!({"type":"plot","path":self.collection,"start":start.to_string(),"end":end.to_string(),"width":width,"request":self.plot_request.to_string(),"reset":reset}));
            }
            _ => self.status = "Choose a nonempty sample range within the collection.".into(),
        }
    }
    fn select_range(&mut self, start: u64, end: u64) {
        self.selected = Some(start);
        self.range_start = start.to_string();
        self.range_end = end.to_string();
        export_selection(&self.range_start, &self.range_end);
        self.navigate(start);
    }
    fn decoder(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("YAML decoder");
            if ui
                .add_enabled(!self.decoding, egui::Button::new("Apply · F5"))
                .clicked()
            {
                self.apply_decoder();
            }
            if ui.button("Stop").clicked() {
                self.pending.clear();
                self.command(serde_json::json!({"type":"cancel"}));
                self.decoding = false;
            }
            egui::ComboBox::from_id_source("decoder-example")
                .selected_text("Examples")
                .show_ui(ui, |ui| {
                    if ui.button("Nested records · byte walkthrough").clicked() {
                    self.yaml = include_str!("../samples/decoders/yaml/nested_records.yaml").into();
                }
                if ui.button("Waveform demo").clicked() {
                    self.yaml = include_str!("../samples/decoders/yaml/waveform.yaml").into();
                }
                if ui.button("Unsigned byte samples").clicked() {
                        self.yaml = crate::source_editor::DecoderLanguage::Yaml
                            .byte_preview()
                            .into();
                    }
                    if ui.button("Signed 16-bit samples").clicked() {
                        self.yaml = crate::source_editor::DecoderLanguage::Yaml
                            .byte_preview()
                            .replace("type: u8", "type: i16");
                    }
                    if ui.button("Float32 samples").clicked() {
                        self.yaml = crate::source_editor::DecoderLanguage::Yaml
                            .byte_preview()
                            .replace("type: u8", "type: f32");
                    }
                    if ui.button("Header and records").clicked() {
                        self.yaml = crate::source_editor::DecoderLanguage::Yaml
                            .starter()
                            .replace("# ", "");
                    }
                });
        });
        ui.label("Shared YAML engine · bounded reads · Python subprocess decoders require the native app.");
        self.editor.editor(
            ui,
            &mut self.yaml,
            crate::source_editor::DecoderLanguage::Yaml,
        );
    }
    fn collection_picker(&mut self, ui: &mut egui::Ui) {
        let mut selected = self.collection.clone();
        egui::ComboBox::from_id_source("collection")
            .selected_text(if selected.is_empty() {
                "Apply a decoder"
            } else {
                &selected
            })
            .show_ui(ui, |ui| {
                if let Some(arrays) = self.decoded["collections"].as_array() {
                    for a in arrays {
                        let path = a["path"].as_str().unwrap_or("");
                        ui.selectable_value(
                            &mut selected,
                            path.into(),
                            format!("{} · {} samples", path, a["count"].as_str().unwrap_or("0")),
                        );
                    }
                }
            });
        if selected != self.collection {
            self.select_collection(selected);
        }
    }
    fn samples(&mut self, ui: &mut egui::Ui) {
        self.collection_picker(ui);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(self.table_start > 0, egui::Button::new("Previous 128"))
                .clicked()
            {
                self.table_start = self.table_start.saturating_sub(128);
                self.request_table();
            }
            if ui
                .add_enabled(
                    self.table_start.saturating_add(128) < self.count(),
                    egui::Button::new("Next 128"),
                )
                .clicked()
            {
                self.table_start += 128;
                self.request_table();
            }
            let max = self.count().saturating_sub(1);
            let changed = ui
                .add(egui::DragValue::new(&mut self.table_start).clamp_range(0..=max))
                .changed();
            if changed {
                self.request_table();
            }
        });
        let mut selection = None;
        egui::ScrollArea::both().show(ui, |ui| {
            if let Some(rows) = self.table["rows"].as_array() {
                let columns: std::collections::BTreeSet<_> = rows
                    .iter()
                    .filter_map(|r| r["cells"].as_object())
                    .flat_map(|m| m.keys())
                    .collect();
                egui::Grid::new("decoded-samples")
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("Sample");
                        for key in &columns {
                            ui.strong(*key);
                        }
                        ui.end_row();
                        for row in rows {
                            ui.monospace(row["index"].as_str().unwrap_or(""));
                            for key in &columns {
                                let cell = &row["cells"][*key];
                                if ui
                                    .selectable_label(false, cell["text"].as_str().unwrap_or("—"))
                                    .on_hover_text("Select bytes in Hex")
                                    .clicked()
                                {
                                    selection = cell["start"]
                                        .as_str()
                                        .and_then(parse_offset)
                                        .zip(cell["end"].as_str().and_then(parse_offset));
                                }
                            }
                            ui.end_row();
                        }
                    });
            } else {
                ui.label("Apply a decoder to populate samples.");
            }
        });
        if let Some((start, end)) = selection {
            self.select_range(start, end);
        }
    }
    fn structures(&mut self, ui: &mut egui::Ui) {
        fn tree(ui: &mut egui::Ui, node: &serde_json::Value, selection: &mut Option<(u64, u64)>) {
            let label = format!(
                "{}: {}  {}",
                node["name"].as_str().unwrap_or(""),
                node["kind"].as_str().unwrap_or(""),
                node["value"].as_str().unwrap_or("")
            );
            egui::CollapsingHeader::new(label)
                .id_source(node["path"].as_str().unwrap_or(""))
                .show(ui, |ui| {
                    if ui
                        .button(format!(
                            "Bytes {}..{}",
                            node["start"].as_str().unwrap_or(""),
                            node["end"].as_str().unwrap_or("")
                        ))
                        .clicked()
                    {
                        *selection = node["start"]
                            .as_str()
                            .and_then(parse_offset)
                            .zip(node["end"].as_str().and_then(parse_offset));
                    }
                    if let Some(count) = node["count"].as_str() {
                        ui.label(format!(
                            "{count} records · select collection in Samples or Plot"
                        ));
                    }
                    if let Some(children) = node["children"].as_array() {
                        for child in children {
                            tree(ui, child, selection);
                        }
                    }
                });
        }
        let mut selection = None;
        egui::ScrollArea::both().show(ui, |ui| {
            if let Some(nodes) = self.decoded["nodes"].as_array() {
                for node in nodes {
                    tree(ui, node, &mut selection);
                }
            } else {
                ui.label("Apply a decoder to inspect structures.");
            }
        });
        if let Some((start, end)) = selection {
            self.select_range(start, end);
        }
    }
    fn decoded_plot(&mut self, ui: &mut egui::Ui) {
        self.collection_picker(ui);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Fit all").clicked() {
                self.first = "0".into();
                self.last = self.count().to_string();
                self.request_plot(1200);
            }
            ui.label("Drag to pan · scroll or pinch to zoom");
        });
        ui.label(format!(
            "{} · {} records inspected",
            self.plot["quality"]
                .as_str()
                .unwrap_or("Apply a decoder to graph its numeric channels"),
            self.plot["scanned"]
        ));
        if let Some(series) = self.plot["series"].as_array() {
            ui.horizontal_wrapped(|ui| {
                for channel in series {
                    let name = channel["name"].as_str().unwrap_or("");
                    let mut enabled = !self.hidden.contains(name);
                    if ui.checkbox(&mut enabled, name).changed() {
                        if enabled {
                            self.hidden.remove(name);
                        } else {
                            self.hidden.insert(name.into());
                        }
                    }
                }
            });
            let mut plot = egui_plot::Plot::new(("decoded-plot", &self.collection))
                .legend(egui_plot::Legend::default()).allow_scroll(!self.stacked);
            if std::mem::take(&mut self.reset_plot) { plot = plot.reset(); }
            let response = plot
            .show(ui, |plot| {
                let colors = [
                    egui::Color32::LIGHT_BLUE,
                    egui::Color32::LIGHT_GREEN,
                    egui::Color32::LIGHT_RED,
                    egui::Color32::GOLD,
                ];
                for (i, channel) in series.iter().enumerate() {
                    let name = channel["name"].as_str().unwrap_or("");
                    if self.hidden.contains(name) {
                        continue;
                    }
                    let points: Vec<[f64; 2]> =
                        serde_json::from_value(channel["points"].clone()).unwrap_or_default();
                    let bands: Vec<[f64;4]> = serde_json::from_value(channel["bands"].clone()).unwrap_or_default();
                    crate::plot_trace::decoded_channel(plot, name, colors[i % colors.len()], points,
                        self.plot["estimated"].as_bool().unwrap_or(false).then_some(bands.as_slice()));
                }
            });
            let now = ui.input(|i| i.time);
            let interacting = response.response.dragged() || response.response.drag_stopped() ||
                (response.response.hovered() && ui.input(|i| (!self.stacked && i.smooth_scroll_delta != egui::Vec2::ZERO) || i.zoom_delta() != 1.0));
            if interacting {
                let bounds = response.transform.bounds();
                self.viewport.observe(bounds.min()[0], bounds.max()[0], self.count(), now);
            }
            if let Some((first, last)) = self.viewport.ready(now) {
                self.first = first.to_string(); self.last = last.to_string();
                self.request_plot_view(response.response.rect.width().max(100.) as u32, false);
            }
            if response.response.double_clicked() {
                self.first = "0".into(); self.last = self.count().to_string();
                self.request_plot(1200);
            }
        }
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
        self.stacked = self.layout_mode == "stacked" || (self.layout_mode == "auto" && ctx.screen_rect().width() < 900.);
        self.events();
        if self.reveal_decoder {
            self.layout.show_decoder();
            self.reveal_decoder = false;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F5)) {
            self.apply_decoder();
        }
        if self.reveal_results {
            self.layout.show_samples();
            self.layout.show_initial_plot();
            self.layout.show_decoder();
            self.reveal_results = false;
        }
        egui::TopBottomPanel::top("browser-menu").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Binwave");
                egui::ComboBox::from_id_source("browser-layout-mode").selected_text(match self.layout_mode.as_str() {
                    "stacked" => "Stacked", "docked" => "Docked", _ => "Layout: Auto",
                }).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.layout_mode,"auto".into(),"Auto");
                    ui.selectable_value(&mut self.layout_mode,"stacked".into(),"Stacked");
                    ui.selectable_value(&mut self.layout_mode,"docked".into(),"Docked");
                });
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
        if self.stacked {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.spacing_mut().interact_size.y = 40.;
                ui.spacing_mut().item_spacing.y = 8.;
                let target = self.mobile_target.take();
                egui::ScrollArea::vertical().id_source("mobile-workspace").auto_shrink([false,false]).show(ui, |ui| {
                    let width = ui.available_width();
                    for (pane, height, open) in [
                        (Pane::Layout, 400., true), (Pane::Plot, 430., true),
                        (Pane::Table, 310., true), (Pane::Hex, 290., true),
                        (Pane::Structures, 300., false), (Pane::Inspector, 300., false),
                    ] {
                        ui.push_id(("mobile-pane", pane), |ui| {
                            let header = egui::CollapsingHeader::new(egui::RichText::new(pane.title()).strong())
                                .default_open(open).open(if target == Some(pane) { Some(true) } else { None })
                                .show(ui, |ui| {
                                    ui.allocate_ui_with_layout(egui::vec2((width-24.).max(100.),height),egui::Layout::top_down(egui::Align::Min), |ui| {
                                        ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
                                        self.ui(ui, &mut pane.clone());
                                    });
                                });
                            if target == Some(pane) { header.header_response.scroll_to_me(Some(egui::Align::Min)); }
                            ui.separator();
                        });
                    }
                });
            });
        } else {
            // Desktop docking remains independently arranged while mobile stacks
            // the same pane implementations into one scrollable column.
            let mut layout = std::mem::take(&mut self.layout);
            DockArea::new(&mut layout.dock).show(ctx, self);
            self.layout = layout;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.layout.save(storage);
        storage.set_string("browser-layout-mode", self.layout_mode.clone());
        if self.yaml.len() <= 256 * 1024 {
            storage.set_string("browser-decoder-yaml", self.yaml.clone());
        }
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
            Pane::Plot => self.decoded_plot(ui),
            Pane::Table => self.samples(ui),
            Pane::Structures => self.structures(ui),
        }
    }
}
