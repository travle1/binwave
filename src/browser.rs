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
    #[wasm_bindgen(js_namespace = binwave, js_name = menus)]
    fn publish_menus(model: &str, file: &str, status: &str, version: &str);
    #[wasm_bindgen(js_namespace = binwave, js_name = pasteText)]
    pub fn paste_text() -> js_sys::Promise;
    #[wasm_bindgen(js_namespace = binwave, js_name = copyText)]
    pub fn copy_text(text: &str);
    #[wasm_bindgen(js_namespace = binwave, js_name = saveDecoder)]
    fn save_decoder(text: &str, language: &str);
    #[wasm_bindgen(js_namespace = binwave, js_name = decoderDraft)]
    fn decoder_draft(text: &str, language: &str);
    #[wasm_bindgen(js_namespace = binwave, js_name = sourceEditorFocus)]
    fn source_editor_focus(focused: bool);
    #[wasm_bindgen(js_namespace = binwave, js_name = clearDecoder)]
    fn clear_decoder_status();
    #[wasm_bindgen(js_namespace = binwave, js_name = send)]
    fn send_command(command: &str) -> bool;
    #[wasm_bindgen(js_namespace = binwave, js_name = selection)]
    fn export_selection(start: &str, end: &str);
    #[wasm_bindgen(js_namespace = binwave, js_name = error)]
    pub fn startup_error(message: &str);
    #[wasm_bindgen(js_namespace = binwave, js_name = ready)]
    fn browser_ready();
    #[wasm_bindgen(catch, js_namespace = binwave, js_name = freshCanvas)]
    pub fn fresh_canvas() -> Result<(), JsValue>;
    #[wasm_bindgen(js_namespace = binwave, js_name = openDemo)]
    fn open_embedded_demo(bytes: &[u8], replace_decoder: bool, name: &str);
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
    operation: String,
    command: Option<crate::app_menu::Command>,
    argument: usize,
    dirty: bool,
    demo: bool,
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
    selection_anchor: Option<u64>,
    hex_visible: Option<std::ops::Range<u64>>,
    hex_reveal: Option<u64>,
    hex_columns: usize,
    authoring: crate::direct_authoring::DirectAuthoring,
    authoring_pane: Pane,
    inline_decode: bool,
    direct_pending: Option<(String, crate::direct_authoring::Interpretation)>,
    direct_error: Option<String>,
    selected_sample: Option<u64>,
    selected_samples: Option<(u64, u64)>,
    plot_anchor: Option<u64>,
    inspected: Option<crate::workspace_inspector::FieldInfo>,
    inspector_definition: Option<(u64, usize)>,
    value_text: String,
    value_original: String,
    value_error: Option<String>,
    value_request: u64,
    plot_visible: Option<[f64; 2]>,
    probe_request: u64,
    range_start: String,
    range_end: String,
    replacement: String,
    status: String,
    dirty: bool,
    undo: bool,
    redo: bool,
    language: crate::source_editor::DecoderLanguage,
    decoder_name: String,
    other_draft: String,
    applied_language: crate::source_editor::DecoderLanguage,
    yaml: String,
    sent_yaml: Option<String>,
    applied_yaml: String,
    decoding_yaml: String,
    decoding_language: crate::source_editor::DecoderLanguage,
    python_stdout: String,
    decoder_error: Option<String>,
    editor: crate::source_editor::YamlLayoutCache,
    decoded: serde_json::Value,
    tree_pages: std::collections::HashMap<String, serde_json::Value>,
    tree_expanded: std::collections::HashSet<String>,
    tree_selected: Option<String>,
    tree_page_path: Option<String>,
    tree_page_index: String,
    table: serde_json::Value,
    plot: serde_json::Value,
    collection: String,
    plot_sources: std::collections::BTreeSet<String>,
    plot_sources_initialized: bool,
    table_start: u64,
    table_jump: String,
    first: String,
    last: String,
    pending: std::collections::VecDeque<serde_json::Value>,
    hidden: std::collections::BTreeSet<String>,
    presentation: crate::plot_presentation::Presentation,
    plot_sidebar_open: bool,
    plot_sidebar_series: bool,
    plot_filter: String,
    plot_axis_series: Option<String>,
    plot_axis_group: Option<String>,
    plot_groups_expanded: std::collections::HashSet<String>,
    plot_groups_collapsed: std::collections::HashSet<String>,
    plot_axis_panning: bool,
    plot_jump: String,
    plot_jump_open: bool,
    fit_plot_y: bool,
    fit_all_plot_y: bool,
    decoding: bool,
    reveal_results: bool,
    reveal_decoder: bool,
    reveal_inspector: bool,
    show_decoding_guide: bool,
    plot_request: u64,
    reset_plot: bool,
    layout_mode: String,
    stacked: bool,
    mobile_target: Option<Pane>,
    viewport: crate::browser_viewport::ViewportRefresh,
}
impl BrowserApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::install(&cc.egui_ctx);
        browser_ready();
        let saved_yaml = cc.storage.and_then(|s| s.get_string("browser-decoder-yaml"))
            .filter(|s| s.len() <= 256 * 1024)
            .unwrap_or_else(|| "endian: little\nactions: []\n".into());
        let saved_python = cc.storage.and_then(|s| s.get_string("browser-decoder-python"))
            .filter(|s| s.len() <= 256 * 1024)
            .unwrap_or_else(|| crate::source_editor::DecoderLanguage::Python.byte_preview().into());
        let language = if cc.storage.and_then(|s| s.get_string("browser-decoder-language")).as_deref() == Some("python") {
            crate::source_editor::DecoderLanguage::Python
        } else { crate::source_editor::DecoderLanguage::Yaml };
        let (active_draft, other_draft) = if language == crate::source_editor::DecoderLanguage::Python {
            (saved_python, saved_yaml)
        } else { (saved_yaml, saved_python) };
        let mut editor = crate::source_editor::YamlLayoutCache::default();
        if let Some(width) = cc
            .storage
            .and_then(|storage| storage.get_string("browser-editor-tab-width"))
            .and_then(|width| width.parse().ok())
        {
            editor.set_tab_width(width);
        }
        Self {
            layout: cc
                .storage
                .map(|s| WorkspaceLayout::load(Some(s)))
                .unwrap_or_default(),
            name: String::new(),
            size: 0,
            offset: 0,
            bytes: Vec::new(),
            needs_read: false,
            jump: "0".into(),
            selected: None,
            selection_anchor: None,
            hex_visible: None,
            hex_reveal: None,
            hex_columns: 0,
            authoring: Default::default(),
            authoring_pane: Pane::Hex,
            inline_decode: false,
            direct_pending: None,
            direct_error: None,
            selected_sample: None,
            selected_samples: None,
            plot_anchor: None,
            inspected: None,
            inspector_definition: None,
            value_text: String::new(),
            value_original: String::new(),
            value_error: None,
            value_request: 0,
            plot_visible: None,
            probe_request: 0,
            range_start: "0".into(),
            range_end: "0".into(),
            replacement: String::new(),
            status: "Use Open file above to inspect a local file. Files stay on your computer."
                .into(),
            dirty: false,
            undo: false,
            redo: false,
            language,
            decoder_name: "decoder.yaml".into(),
            other_draft,
            applied_language: crate::source_editor::DecoderLanguage::Yaml,
            yaml: active_draft,
            sent_yaml: None,
            applied_yaml: String::new(),
            decoding_yaml: String::new(),
            decoding_language: language,
            python_stdout: String::new(),
            decoder_error: None,
            editor,
            decoded: Default::default(),
            tree_pages: Default::default(),
            tree_expanded: Default::default(),
            tree_selected: None,
            tree_page_path: None,
            tree_page_index: "0".into(),
            table: Default::default(),
            plot: Default::default(),
            collection: String::new(),
            plot_sources: Default::default(),
            plot_sources_initialized: false,
            table_start: 0,
            table_jump: "0".into(),
            first: "0".into(),
            last: "0".into(),
            pending: Default::default(),
            hidden: Default::default(),
            presentation: Default::default(),
            plot_sidebar_open: true,
            plot_sidebar_series: true,
            plot_filter: String::new(),
            plot_axis_series: None,
            plot_axis_group: None,
            plot_groups_expanded: Default::default(),
            plot_groups_collapsed: Default::default(),
            plot_axis_panning: false,
            plot_jump: "0".into(),
            plot_jump_open: false,
            fit_plot_y: false,
            fit_all_plot_y: false,
            decoding: false,
            reveal_results: false,
            reveal_decoder: false,
            reveal_inspector: false,
            show_decoding_guide: false,
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
        self.hex_visible = None;
        self.hex_reveal = None;
    }
    fn reveal_hex_selection(&mut self, start: u64) {
        let loaded_end = self.offset.saturating_add(self.bytes.len() as u64);
        let loaded = start >= self.offset && start < loaded_end;
        if !loaded {
            self.navigate(start / PAGE as u64 * PAGE as u64);
            self.hex_reveal = Some(start);
        } else if !self
            .hex_visible
            .as_ref()
            .is_some_and(|visible| visible.contains(&start))
        {
            self.hex_reveal = Some(start);
        }
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
                            "Select bytes to preview interpretations and name fields."
                                .into();
                        if event.demo {
                            self.switch_language(crate::source_editor::DecoderLanguage::Yaml);
                            self.decoder_name = format!("{}.yaml", self.name.trim_end_matches(".bin"));
                            self.yaml = if self.name == crate::demos::Demo::Advanced.name() {
                                crate::demos::Demo::Advanced.yaml()
                            } else { crate::demos::Demo::Simple.yaml() }.into();
                            self.apply_decoder();
                        }
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
                "menu" => if let Some(command)=event.command { self.menu_action(command,event.argument); },
                "decoder-source" => {
                    if event.source.len() <= 256 * 1024 {
                        self.decoder_name = event.name.clone();
                        self.switch_language(if event.name.to_ascii_lowercase().ends_with(".py") {
                            crate::source_editor::DecoderLanguage::Python
                        } else { crate::source_editor::DecoderLanguage::Yaml });
                        self.yaml = event.source;
                        self.reveal_decoder = true;
                        self.mobile_target = Some(Pane::Layout);
                        self.status = format!("Loaded {}. Press ▶ Run (F5) to decode the open data file.", event.name);
                    } else {
                        self.status = "Decoder source is limited to 256 KiB.".into();
                    }
                }
                "decoder-paste" => {
                    if event.source.len() <= 256 * 1024 {
                        self.editor.queue_paste(event.source);
                    } else {
                        self.status = "Paste is limited to 256 KiB.".into();
                    }
                }
                "decode_failed" => {
                    self.direct_pending = None;
                    if self.inline_decode { self.direct_error = Some(event.message.clone()); }
                    self.applied_yaml = self.decoding_yaml.clone();
                    self.applied_language = self.decoding_language;
                    self.python_stdout = event.result["stdout"].as_str().unwrap_or("").to_owned();
                    self.decoder_error = Some(event.message.clone());
                    self.decoding = false;
                    self.inline_decode = false;
                    self.decoded = event.result;
                    self.table = Default::default();
                    self.plot = Default::default();
                    self.collection.clear();
                    self.plot_sources.clear();
                    self.reveal_results = true;
                    self.mobile_target = Some(Pane::Structures);
                    let consumed = self.decoded["consumed"].as_str().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
                    self.status = format!("Partial decode through byte {}. Inspect Structure and the Console error together.", crate::format::count(consumed));
                    if let Some((start, end)) = parse_offset(&self.range_start).zip(parse_offset(&self.range_end)) {
                        self.request_inspect(start, end);
                    }
                }
                "decoded" => {
                    if let Some((before, interpretation)) = self.direct_pending.take() {
                        if self.yaml != before { self.decoding = false; continue; }
                        self.yaml = self.decoding_yaml.clone();
                        self.authoring.created(interpretation.name.clone());
                        if let Some(node) = event.result["nodes"].as_array().and_then(|nodes| nodes.iter().find(|n| n["name"].as_str() == Some(&interpretation.name))) {
                            if let Some((start, end)) = node["start"].as_str().and_then(parse_offset).zip(node["end"].as_str().and_then(parse_offset)) { self.select_range(start, end); }
                        }
                        self.authoring_pane = Pane::Structures;
                        self.layout.reveal(Pane::Structures);
                        self.mobile_target = Some(Pane::Structures);
                    } else if self.decoder_empty() { continue; }
                    self.direct_error = None;
                    self.applied_yaml = self.decoding_yaml.clone();
                    self.applied_language = self.decoding_language;
                    self.python_stdout = event.result["stdout"].as_str().unwrap_or("").to_owned();
                    self.decoder_error = None;
                    self.decoding = false;
                    self.decoded = event.result;
                    if self.decoded["noData"].as_bool() == Some(true) {
                        self.table = Default::default();
                        self.plot = Default::default();
                        self.collection.clear();
                        self.plot_sources.clear();
                        self.status = "Python script ran, but registered no decoded fields.".into();
                        continue;
                    }
                    self.plot_sources.retain(|p| self.decoded["collections"].as_array().into_iter().flatten().any(|c| c["path"].as_str()==Some(p)));

                    self.table = Default::default();
                    self.plot = Default::default();
                    let inline = std::mem::take(&mut self.inline_decode);
                    self.reveal_results |= !inline;
                    let path = self.samples_path().unwrap_or_default();
                    if inline && !self.collection.is_empty() && self.decoded["collections"].as_array().is_some_and(|a| a.iter().any(|v| v["path"].as_str() == Some(&self.collection))) {
                        self.table_start = self.table_start.min(self.count().saturating_sub(1));
                        self.request_table();
                        self.request_plot_view(1200, false);
                    } else { self.select_collection(path); if !self.plot_sources.is_empty() { self.request_plot_view(1200, false); } }
                    if !inline { self.mobile_target = Some(if self.collection.is_empty() { Pane::Structures } else { Pane::Plot }); }
                    let consumed = self.decoded["consumed"].as_str().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
                    self.status = format!("Decoded {} of {}. {}", crate::format::count(consumed), crate::format::exact_bytes(self.size),
                        if self.collection.is_empty() { "Inspect the fields in Structure; unread bytes are allowed." }
                        else if self.samples_path().as_deref() == Some(self.collection.as_str()) { "Select samples to inspect their values and bytes." }
                        else { "Select items to inspect their values and bytes." });
                    if let Some((start, end)) = parse_offset(&self.range_start).zip(parse_offset(&self.range_end)) {
                        self.request_inspect(start, end);
                    }
                }
                "pick" => {
                    if event.request == self.probe_request.to_string()
                        && event.result["path"].as_str() == Some(&self.collection) {
                        if let Some((start, end)) = event.result["start"].as_str().and_then(parse_offset)
                            .zip(event.result["end"].as_str().and_then(parse_offset)) {
                            self.selected = Some(start);
                            self.range_start = start.to_string();
                            self.range_end = end.to_string();
                            export_selection(&self.range_start, &self.range_end);
                            self.request_inspect(start, end);
                            self.navigate(start);
                            let first = event.result["first"].as_str().and_then(parse_offset)
                                .or_else(|| event.result["index"].as_str().and_then(parse_offset));
                            let last = event.result["last"].as_str().and_then(parse_offset)
                                .or(first);
                            self.selected_sample = first;
                            self.selected_samples = first.zip(last).map(|(first, last)| (first.min(last), first.max(last).saturating_add(1)));
                            self.table_start = first.unwrap_or(0) / 128 * 128;
                            self.request_table();
                        }
                    }
                }
                "probe" => {
                    if event.request == self.probe_request.to_string()
                        && event.result["path"].as_str() == Some(&self.collection) {
                        self.selected_sample = event.result["first"].as_str().and_then(parse_offset);
                        self.selected_samples = self.selected_sample.zip(
                            event.result["end"].as_str().and_then(parse_offset)
                        );
                        if let Some(index) = self.selected_sample {
                            self.table_start = index / 128 * 128;
                            self.request_table();
                            let first = parse_offset(&self.first).unwrap_or(0);
                            let last = parse_offset(&self.last).unwrap_or(0);
                            if self.plot_visible.map_or(index < first || index >= last, |[left, right]| (index as f64) < left || (index as f64) > right) {
                                let width = 512.min(self.count());
                                let start = index.saturating_sub(width / 2).min(self.count().saturating_sub(width));
                                self.first = start.to_string();
                                self.last = (start + width).to_string();
                                self.request_plot(1200);
                            }
                        }
                    }
                }
                "inspect" => {
                    if event.request == self.probe_request.to_string() {
                        self.inspected = serde_json::from_value(event.result["field"].clone()).ok();
                        self.inspector_definition = event.result["consumed"].as_str()
                            .and_then(parse_offset).zip(event.result["types"].as_u64())
                            .map(|(consumed, types)| (consumed, types as usize));
                        self.value_original = self.inspected.as_ref().map(|field| field.value.clone()).unwrap_or_default();
                        self.value_text = self.value_original.clone();
                        self.value_error = None;
                    }
                }
                "encoded" => {
                    if event.request == format!("{}:{}", self.probe_request, self.value_request) {
                        if let Some(field) = self.inspected.clone() {
                            if event.result["start"].as_str() == Some(&field.start.to_string())
                                && event.result["end"].as_str() == Some(&field.end.to_string()) {
                                self.queue(serde_json::json!({"type":"replace_range",
                                    "offset":field.start.to_string(),"bytes":event.result["bytes"]}));
                                self.status = format!("Applying {} to {}…",
                                    event.result["displayed"].as_str().unwrap_or("value"), field.name);
                            }
                        }
                    }
                }
                "table" => {
                    if event.result["path"].as_str() == Some(&self.collection) {
                        self.table = event.result;
                    }
                }
                "tree_page" => {
                    if let Some(path) = event.result["path"].as_str() {
                        self.tree_pages.insert(path.to_owned(), event.result);
                    }
                }
                "plot" => {
                    if event.request == self.plot_request.to_string() {
                        self.reset_plot = event.reset;
                        self.plot = event.result;
                    }
                }
                "error" => {
                    if event.operation == "encode" {
                        self.value_error = Some(event.message);
                        continue;
                    }
                    if self.direct_pending.is_none() && self.decoder_empty() && matches!(event.operation.as_str(), "decode" | "decode_python" | "table" | "plot" | "probe" | "pick" | "inspect") { continue; }
                    self.direct_pending = None;
                    if self.inline_decode { self.direct_error = Some(event.message.clone()); }
                    let decoder_failure = matches!(event.operation.as_str(), "decode" | "decode_python");
                    if decoder_failure {
                        self.decoder_error = Some(event.message.clone());
                    }
                    self.inline_decode = false;
                    self.decoding = false;
                    self.pending.clear();
                    self.status = if decoder_failure {
                        "Decoder failed. See Console for details.".into()
                    } else {
                        event.message
                    };
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
        if self.name.is_empty() {
            ui.heading("Explore your data");
            ui.label("Open data above, then select bytes to discover their meaning.");
            if ui.button("Try simple demo").clicked() { self.open_simple_demo(); }
            ui.small("512 synthetic samples · 2 KB");
            return;
        }
        let embedded_inspector = !self.stacked && ui.available_width() >= crate::theme::DARK.size.inspector_min_host_width;
        if embedded_inspector {
            // The header and body both need `self`; the header runs first, then the body.
            let this = std::cell::RefCell::new(&mut *self);
            crate::workspace_inspector::docked_panel(ui, "browser_hex_inspector",
                |ui| { ui.strong("Inspector"); this.borrow_mut().direct_controls(ui, Pane::Hex); },
                |ui| this.borrow_mut().inspector(ui));
        }
        if !embedded_inspector { self.direct_controls(ui, Pane::Hex); }
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
            "64 KiB page at 0x{:X} · {} loaded",
            self.offset,
            crate::format::exact_bytes(self.bytes.len() as u64)
        ));
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("Selection", |ui| self.hex_menu(ui));
            if let Some((start, end)) = parse_offset(&self.range_start).zip(parse_offset(&self.range_end)) {
                ui.label(format!("Bytes {}", crate::format::byte_range(start, end)));
            }
        });
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let digit = ui.fonts_mut(|fonts| fonts.layout_no_wrap("0".into(), font.clone(), egui::Color32::WHITE).size().x);
        // Keep full addresses, but omit padding that adds no information.
        let digits = format!("{:X}", self.size.saturating_sub(1)).len().max(4);
        let address_width = digits as f32 * digit + 8.;
        let cell_width = (2. * digit + 12.).max(if self.stacked { 30. } else { 24. });
        let available = ui.available_width();
        let pointer_in_grid = ui.input(|input| input.pointer.primary_down()
            && input.pointer.interact_pos().is_some_and(|position| {
                ui.available_rect_before_wrap().contains(position)
            }));
        let fitted_columns = if !self.stacked && available >= address_width + 16. * cell_width {
            16
        } else if available >= address_width + 8. * cell_width {
            8
        } else { 4 };
        let columns = if pointer_in_grid && self.hex_columns > 0 {
            self.hex_columns
        } else {
            fitted_columns
        };
        let previous_visible = self.hex_visible.as_ref().map(|range| range.start);
        let columns_changed = self.hex_columns > 0 && self.hex_columns != columns;
        let ascii_width = columns as f32 * digit + 12.;
        let show_ascii = available >= address_width + columns as f32 * cell_width + ascii_width;
        let row_height = ui.spacing().interact_size.y.max(if self.stacked { 32. } else { 22. });
        let rows = self.bytes.len().div_ceil(columns);
        let selected_start = parse_offset(&self.range_start);
        let selected_end = parse_offset(&self.range_end);
        let width = address_width + columns as f32 * cell_width
            + if show_ascii { ascii_width } else { 0. };
        let (header, _) = ui.allocate_exact_size(
            egui::vec2(width.max(ui.available_width()), row_height),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(header);
        painter.text(
            header.left_center(),
            egui::Align2::LEFT_CENTER,
            "Offset",
            font.clone(),
            ui.visuals().weak_text_color(),
        );
        for column in 0..columns {
            let cell = egui::Rect::from_min_size(
                egui::pos2(
                    header.left() + address_width + column as f32 * cell_width,
                    header.top(),
                ),
                egui::vec2(cell_width, row_height),
            );
            painter.text(
                cell.center(),
                egui::Align2::CENTER_CENTER,
                format!("{column:02X}"),
                font.clone(),
                ui.visuals().weak_text_color(),
            );
        }
        if show_ascii {
            painter.text(
                egui::pos2(
                    header.left() + address_width + columns as f32 * cell_width + 12.,
                    header.center().y,
                ),
                egui::Align2::LEFT_CENTER,
                "Text",
                font.clone(),
                ui.visuals().weak_text_color(),
            );
        }
        painter.line_segment(
            [header.left_bottom(), header.right_bottom()],
            egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
        );

        // Keep the displayed byte grid fixed while a press begins in it. A
        // pending reveal from Decoder/Plot must not scroll different bytes under
        // the pointer before the click resolves.
        if pointer_in_grid {
            self.hex_reveal = None;
        }
        let reveal = self.hex_reveal.filter(|at| {
            *at >= self.offset && *at < self.offset.saturating_add(self.bytes.len() as u64)
        });
        let desired_scroll = reveal.map(|at| {
            let row = (at - self.offset) / columns as u64;
            (row as f32 * row_height - ui.available_height() * 0.35).max(0.0)
        }).or_else(|| columns_changed.then(|| previous_visible.and_then(|at| at.checked_sub(self.offset))
            .map(|at| (at / columns as u64) as f32 * row_height)
            .unwrap_or(0.)));
        if reveal.is_some() {
            self.hex_reveal = None;
        }
        let mut scroll = egui::ScrollArea::both().id_salt("browser-hex");
        if let Some(offset) = desired_scroll {
            scroll = scroll.vertical_scroll_offset(offset);
        }
        let mut visible_bytes = None;
        scroll.show_rows(ui, row_height, rows, |ui, range| {
            if !range.is_empty() {
                visible_bytes = Some(
                    self.offset + (range.start * columns) as u64
                        ..(self.offset + (range.end * columns) as u64)
                            .min(self.offset.saturating_add(self.bytes.len() as u64)),
                );
            }
            for row in range {
                    let start = row * columns;
                    let end = (start + columns).min(self.bytes.len());
                    // A row always occupies exactly one line. Painting fixed cells
                    // also gives each mobile byte a predictable touch target.
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_height), egui::Sense::hover());
                    ui.painter().text(rect.left_center(), egui::Align2::LEFT_CENTER,
                        format!("{:0digits$X}", self.offset + start as u64), font.clone(), ui.visuals().weak_text_color());
                    for i in start..end {
                        let at = self.offset + i as u64;
                        let cell = egui::Rect::from_min_size(
                            egui::pos2(rect.left() + address_width + (i-start) as f32 * cell_width, rect.top()),
                            egui::vec2(cell_width, row_height));
                        let response = ui.interact(cell, ui.id().with(("byte", at)), egui::Sense::click_and_drag());
                        let selected = selected_start.zip(selected_end).is_some_and(|(first,last)| at >= first && at < last);
                        if selected || response.hovered() {
                            ui.painter().rect_filled(cell.shrink(1.), 3., if selected {
                                ui.visuals().selection.bg_fill
                            } else { ui.visuals().widgets.hovered.bg_fill });
                        }
                        ui.painter().text(cell.center(), egui::Align2::CENTER_CENTER,
                            format!("{:02X}", self.bytes[i]), font.clone(),
                            if selected { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() });
                        if response.drag_started() || response.clicked() {
                            if !ui.input(|i| i.modifiers.shift) || self.selection_anchor.is_none() {
                                self.selection_anchor = Some(at);
                            }
                            let anchor = self.selection_anchor.unwrap_or(at);
                            self.set_hex_range(anchor.min(at), anchor.max(at) + 1);
                            self.replacement = format!("{:02X}", self.bytes[i]);
                        }
                        if ui.input(|i| i.pointer.primary_down() && i.pointer.is_decidedly_dragging()
                            && i.pointer.interact_pos().is_some_and(|p| cell.contains(p))) {
                            if let Some(anchor) = self.selection_anchor {
                                self.set_hex_range(anchor.min(at), anchor.max(at) + 1);
                            }
                        }
                        response.context_menu(|ui| {
                            if !selected { self.selection_anchor = Some(at); self.set_hex_range(at, at + 1); }
                            self.hex_menu(ui);
                        });
                    }
                    if show_ascii {
                        ui.painter().text(egui::pos2(rect.left() + address_width + columns as f32 * cell_width + 12., rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            self.bytes[start..end].iter().map(|&b| crate::byte_display::ascii(b)).collect::<String>(),
                            font.clone(), ui.visuals().weak_text_color());
                    }
            }
        });
        self.hex_visible = visible_bytes;
        self.hex_columns = columns;
    }
    fn direct_controls(&mut self, ui: &mut egui::Ui, pane: Pane) {
        if pane != self.authoring_pane || self.language == crate::source_editor::DecoderLanguage::Python { return; }
        let range = parse_offset(&self.range_start).zip(parse_offset(&self.range_end)).map(|(a,b)| a..b);
        let preview = range.as_ref().and_then(|r| r.start.checked_sub(self.offset).and_then(|n| usize::try_from(n).ok())
            .and_then(|start| self.bytes.get(start..).map(|bytes| bytes[..bytes.len().min((r.end.saturating_sub(r.start)).min(64) as usize)].to_vec())));
        let placeholder = self.yaml == crate::source_editor::DecoderLanguage::Yaml.starter() && self.decoded.is_null();
        let source = if placeholder { "endian: little\nactions: []\n" } else { &self.yaml };
        let mut edit = None;
        ui.add_enabled_ui(!self.decoding, |ui| {
            edit = self.authoring.ui(ui, source, range, preview.as_deref(), self.size);
        });
        if let Some(error) = &self.direct_error { ui.colored_label(ui.visuals().error_fg_color, error); }
        match edit {
            Some(crate::direct_authoring::Edit::Commit(source)) => {
                self.yaml = source;
                let empty = crate::direct_authoring::empty_draft(&self.yaml);
                if empty { self.reset_decoder(); }
                else { self.apply_decoder(); self.inline_decode = true; }
            }
            Some(crate::direct_authoring::Edit::Interpret(source, interpretation)) => {
                self.direct_pending = Some((self.yaml.clone(), interpretation.clone()));
                self.direct_error = None;
                self.pending.clear(); self.decoding = true; self.inline_decode = true;
                self.decoding_yaml = source.clone();
                self.queue(serde_json::json!({"type":"decode", "yaml":source, "interpretation": {
                    "name": interpretation.name, "start": interpretation.range.start.to_string(), "end": interpretation.range.end.to_string()
                }}));
            },
            Some(crate::direct_authoring::Edit::Code) => self.reveal_decoder = true,
            None => {}
        }
    }
    fn set_hex_range(&mut self, start: u64, end: u64) {
        self.authoring_pane = Pane::Hex;
        self.selected = Some(start);
        self.range_start = start.to_string();
        self.range_end = end.to_string();
        export_selection(&self.range_start, &self.range_end);
        self.probe_selection(start, end);
    }
    fn hex_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Copy selected hex").clicked() {
            self.copy_selected_hex();
            ui.close();
        }
        if ui.button("Interpret selected bytes").clicked() {
            self.status = "Choose a type beside the byte selection, then name the field and press Enter.".into();
            ui.close();
        }
        if ui.button("Select loaded page").clicked() {
        if self.inspected.is_none() && self.selected.is_none() {
            crate::workspace_inspector::empty_selection(
                ui,
                !self.name.is_empty(),
                !self.decoded.is_null(),
            );
        }
            self.selection_anchor = Some(self.offset);
            self.set_hex_range(self.offset, self.offset + self.bytes.len() as u64);
            ui.close();
        }
    }
    fn inspector(&mut self, ui: &mut egui::Ui) {
        let mut reveal = None;
        if let Some(field) = self.inspected.clone() {
            crate::workspace_inspector::field_heading(ui, &field, self.samples_path().as_deref());
            ui.label("Value");
            if field.editable {
                let apply = crate::workspace_inspector::value_editor(
                    ui, &mut self.value_text, &self.value_original, &mut self.value_error, !self.decoding);
                if apply {
                    self.value_request = self.value_request.wrapping_add(1);
                    self.value_error = None;
                    self.queue(serde_json::json!({"type":"encode",
                        "start":field.start.to_string(),"end":field.end.to_string(),
                        "value":self.value_text,
                        "request":format!("{}:{}",self.probe_request,self.value_request)}));
                }
            } else {
                ui.label(&field.value);
                ui.weak("Edit this field in Hex.");
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("Reveal in Hex").clicked() {
                    reveal = Some((field.start, field.end));
                }
                if let Some(range) = field.sample_range() {
                    ui.menu_button("Sample actions", |ui| {
                        if ui.button("Select all bytes of sample").clicked() {
                            reveal = Some((range.start, range.end));
                            ui.close();
                        }
                    });
                }
            });
            egui::CollapsingHeader::new("Technical details").show(ui, |ui| {
                crate::workspace_inspector::field_details(ui, &field, self.inspector_definition);
            });
            ui.separator();
        } else if !self.decoded.is_null() && self.selected.is_some() {
            ui.weak("No decoded field covers this selection.");
        }
        let selection = parse_offset(&self.range_start).zip(parse_offset(&self.range_end))
            .filter(|(start, end)| start < end && *end <= self.size);
        let preview = selection.and_then(|(start, end)| {
            let index = usize::try_from(start.checked_sub(self.offset)?).ok()?;
            let bytes = self.bytes.get(index..)?;
            Some(bytes[..bytes.len().min(64).min((end - start).min(64) as usize)].to_vec())
        });
        let raw = |ui: &mut egui::Ui| {
            if let Some((start, end)) = selection {
                crate::workspace_inspector::byte_selection(ui, start..end, preview.as_deref());
            } else {
                ui.weak("Select a byte or range in Hex.");
            }
        };
        if self.inspected.is_some() {
            egui::CollapsingHeader::new("Raw bytes").show(ui, raw);
        } else {
            raw(ui);
        }
        if let Some((start, end)) = reveal {
            self.select_range(start, end);
        }
        if let Some(at) = self.selected {
            ui.horizontal_wrapped(|ui| {
                ui.label("Replace selected byte with hex:");
                ui.add(egui::TextEdit::singleline(&mut self.replacement).desired_width(40.0).char_limit(2));
                if ui.button("Apply").clicked() {
                    if let Ok(value) = u8::from_str_radix(self.replacement.trim(), 16) {
                        if !self.command(serde_json::json!({"type":"replace","offset":at.to_string(),"value":value})) {
                            self.status = "Wait for the current file operation.".into();
                        }
                    } else { self.status = "Enter a hexadecimal byte, 00 through FF.".into(); }
                }
            });
        }
        egui::CollapsingHeader::new("Extraction range")
            .default_open(false).show(ui, |ui| {
                ui.weak("Start inclusive · end exclusive");
                ui.horizontal(|ui| {
                    ui.label("Start");
                    ui.add(egui::TextEdit::singleline(&mut self.range_start).desired_width(100.));
                });
                ui.horizontal(|ui| {
                    ui.label("End");
                    ui.add(egui::TextEdit::singleline(&mut self.range_end).desired_width(100.));
                });
                if ui.button("Set range").clicked() {
                    match (parse_offset(&self.range_start), parse_offset(&self.range_end)) {
                        (Some(start), Some(end)) if start <= end && end <= self.size => {
                            self.selection_anchor = Some(start);
                            self.set_hex_range(start, end);
                            self.status = format!("Extraction range {} set. Use Extract selection above.",
                                crate::format::byte_range(start, end));
                        }
                        _ => self.status = "Extraction range must be inside the file.".into(),
                    }
                }
            });
    }
    fn decoder_empty(&self) -> bool {
        match self.language {
            crate::source_editor::DecoderLanguage::Yaml => crate::direct_authoring::empty_draft(&self.yaml),
            crate::source_editor::DecoderLanguage::Python => self.yaml.trim().is_empty(),
        }
    }
    fn switch_language(&mut self, language: crate::source_editor::DecoderLanguage) {
        if self.language == language { return; }
        std::mem::swap(&mut self.yaml, &mut self.other_draft);
        self.language = language;
        let stem = self.decoder_name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or("decoder");
        self.decoder_name = format!("{stem}.{}", if language == crate::source_editor::DecoderLanguage::Python { "py" } else { "yaml" });
        self.editor = Default::default();
        self.sent_yaml = None;
        self.reset_decoder();
    }
    fn reset_decoder(&mut self) {
        if self.decoding { self.command(serde_json::json!({"type":"cancel"})); }
        self.direct_pending = None;
        self.clear_results();
        self.applied_yaml.clear();
        self.decoding_yaml.clear();
        self.direct_error = None;
        self.inline_decode = false;
        self.reveal_results = false;
        self.python_stdout.clear();
        self.decoder_error = None;
        self.status = if self.language == crate::source_editor::DecoderLanguage::Python {
            "Open a data file, then press ▶ Run (F5). Python runs locally in this browser."
        } else { "No fields yet. Select bytes, choose a type, and name the field." }.into();
        clear_decoder_status();
    }
    fn clear_results(&mut self) {
        self.direct_pending = None;
        self.selected_sample = None;
        self.selected_samples = None;
        self.plot_anchor = None;
        self.inspected = None;
        self.inspector_definition = None;
        self.value_text.clear();
        self.value_original.clear();
        self.value_error = None;
        self.probe_request = self.probe_request.wrapping_add(1);
        self.decoder_error = None;
        self.decoded = Default::default();
        self.tree_pages.clear();
        self.tree_expanded.clear();
        self.tree_selected = None;
        self.tree_page_path = None;
        self.table = Default::default();
        self.plot = Default::default();
        self.plot_visible = None;
        self.pending.clear();
        self.collection.clear();
        self.plot_sources.clear();
        self.plot_sources_initialized = false;
        self.presentation = Default::default();
        self.decoding = false;
    }
    fn queue(&mut self, value: serde_json::Value) {
        let important = matches!(value["type"].as_str(), Some("decode" | "decode_python" | "encode" | "replace_range" | "pick"));
        self.pending.retain(|m| m["type"] != value["type"]);
        if self.pending.len() >= 8 {
            if !important { return; }
            if let Some(index) = self.pending.iter().rposition(|m|
                matches!(m["type"].as_str(), Some("plot" | "table" | "probe" | "inspect"))) {
                self.pending.remove(index);
            } else {
                self.pending.pop_back();
            }
        }
        if important {
            self.pending.push_front(value);
        } else {
            self.pending.push_back(value);
        }
    }
    fn apply_decoder(&mut self) {
        self.direct_pending = None;
        if self.decoder_empty() { self.reset_decoder(); return; }
        self.inline_decode = false;
        self.direct_error = None;
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
        self.decoding_yaml = self.yaml.clone();
        self.decoding_language = self.language;
        self.python_stdout.clear();
        self.decoder_error = None;
        if self.language == crate::source_editor::DecoderLanguage::Python {
            self.queue(serde_json::json!({"type":"decode_python","script":self.yaml,"filename":"<browser-decoder.py>"}));
            self.status = "Loading browser Python and decoding locally…".into();
        } else {
            self.queue(serde_json::json!({"type":"decode","yaml":self.yaml}));
            self.status = "Decoding in worker…".into();
        }
    }
    /// The samples collection is the largest one; rows elsewhere are items.
    fn samples_path(&self) -> Option<String> {
        self.decoded["collections"]
            .as_array()?
            .iter()
            .max_by_key(|c| c["count"].as_str().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0))
            .and_then(|v| v["path"].as_str())
            .map(str::to_owned)
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
    fn plot_count(&self) -> u64 {
        self.decoded["collections"].as_array().into_iter().flatten()
            .filter(|c| c["path"].as_str().is_some_and(|p| self.plot_sources.contains(p)))
            .filter_map(|c| c["count"].as_str()?.parse::<u64>().ok()).max().unwrap_or(0)
    }
    fn select_collection(&mut self, path: String) {
        self.selected_sample = None;
        self.selected_samples = None;
        self.plot_anchor = None;
        self.probe_request = self.probe_request.wrapping_add(1);
        self.collection = path;
        self.table_start = 0;
        self.table = Default::default();
        if !self.collection.is_empty() {
            self.request_table();
            if !self.plot_sources_initialized {
                self.plot_sources_initialized = true;
                self.plot_sources.insert(self.collection.clone());
                self.first = "0".into();
                self.last = self.plot_count().min(512).to_string();
                self.viewport.reset();
                self.request_plot(1200);
            }
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
            (Some(start), Some(end)) if start < end && end <= self.plot_count() => {
                self.plot_request = self.plot_request.wrapping_add(1);
                self.queue(serde_json::json!({"type":"plot","paths":self.plot_sources,"start":start.to_string(),"end":end.to_string(),"width":width,"request":self.plot_request.to_string(),"reset":reset}));
            }
            _ => self.status = "Choose a nonempty sample range within the collection.".into(),
        }
    }
    fn request_inspect(&mut self, start: u64, end: u64) {
        self.inspected = None;
        self.inspector_definition = None;
        self.value_error = None;
        if !self.decoded.is_null() && self.decoded["noData"].as_bool() != Some(true) && start < end {
            self.queue(serde_json::json!({"type":"inspect",
                "start":start.to_string(),"end":end.to_string(),
                "request":self.probe_request.to_string()}));
        }
    }
    fn probe_selection(&mut self, start: u64, end: u64) {
        self.selected_sample = None;
        self.selected_samples = None;
        self.probe_request = self.probe_request.wrapping_add(1);
        if !self.collection.is_empty() {
            self.queue(serde_json::json!({"type":"probe","path":self.collection,
                "start":start.to_string(),"end":end.to_string(),"request":self.probe_request.to_string()}));
        }
        self.request_inspect(start, end);
    }
    fn select_range(&mut self, start: u64, end: u64) {
        self.probe_selection(start, end);
        self.selected = Some(start);
        self.range_start = start.to_string();
        self.range_end = end.to_string();
        export_selection(&self.range_start, &self.range_end);
        self.reveal_hex_selection(start);
    }
    fn menu_action(&mut self, command: crate::app_menu::Command, argument: usize) {
        use crate::app_menu::Command::*;
        if command == TogglePane {
            if let Some(&pane) = Pane::PANELS.get(argument) {
                if let Some(path) = self.layout.dock.find_tab(&pane) {
                    self.layout.dock.remove_tab(path);
                } else {
                    self.layout.reveal(pane);
                    self.mobile_target = Some(pane);
                }
            }
            return;
        }
        if let Some(pane)=command.pane() { self.layout.reveal(pane); self.mobile_target=Some(pane); return; }
        if let Some(action)=command.editor_action() { self.editor.command(action); self.layout.reveal(Pane::Layout); self.mobile_target=Some(Pane::Layout); return; }
        match command {
            SimpleDemo=>self.open_simple_demo(),
            AdvancedDemo=>self.open_demo(crate::demos::Demo::Advanced),
            SaveDecoder | SaveDecoderAs=>save_decoder(&self.yaml, if self.language == crate::source_editor::DecoderLanguage::Python { "python" } else { "yaml" }),
            NewDecoder=>{self.switch_language(crate::source_editor::DecoderLanguage::Yaml);self.decoder_name="decoder.yaml".into();self.yaml="endian: little\nactions: []\n".into();self.reset_decoder();self.layout.reveal(Pane::Layout);self.mobile_target=Some(Pane::Layout);},
            Python=>{self.switch_language(crate::source_editor::DecoderLanguage::Python);self.decoder_name="decoder.py".into();self.yaml=crate::source_editor::DecoderLanguage::Python.starter().into();self.reset_decoder();self.layout.reveal(Pane::Layout);self.mobile_target=Some(Pane::Layout);},
            PythonExample=>{self.switch_language(crate::source_editor::DecoderLanguage::Python);self.yaml=crate::source_editor::DecoderLanguage::Python.byte_preview().into();self.layout.reveal(Pane::Layout);self.mobile_target=Some(Pane::Layout);},
            Apply=>self.apply_decoder(),
            Example=>if let Some((_,yaml))=crate::source_editor::YAML_EXAMPLES.get(argument) {self.switch_language(crate::source_editor::DecoderLanguage::Yaml);self.yaml=(*yaml).into();self.layout.reveal(Pane::Layout);self.mobile_target=Some(Pane::Layout);},
            Undo=>{self.command(serde_json::json!({"type":"undo"}));},
            Redo=>{self.command(serde_json::json!({"type":"redo"}));},
            CopyHex=>self.copy_selected_hex(),
            Cancel=>{self.pending.clear();self.command(serde_json::json!({"type":"cancel"}));self.decoding=false;self.direct_pending=None;},
            Reset=>{self.layout=WorkspaceLayout::default();self.layout_mode="auto".into();},
            Auto=>self.layout_mode="auto".into(), Stacked=>self.layout_mode="stacked".into(), Docked=>self.layout_mode="docked".into(),
            Help=>self.show_decoding_guide=true,
            _=>{}
        }
    }
    fn copy_selected_hex(&mut self) {
        let start=parse_offset(&self.range_start).unwrap_or(0);
        let end=parse_offset(&self.range_end).unwrap_or(start);
        if start>=self.offset && end<=self.offset+self.bytes.len() as u64 && end>=start {
            copy_text(&self.bytes[(start-self.offset) as usize..(end-self.offset) as usize].iter().map(|b|format!("{b:02X}")).collect::<Vec<_>>().join(" "));
        } else {self.status="Copy supports the loaded 64 KiB page; use Extract selection for larger ranges.".into();}
    }
    fn open_simple_demo(&self) {
        self.open_demo(crate::demos::Demo::Simple);
    }
    fn open_demo(&self, demo: crate::demos::Demo) {
        open_embedded_demo(demo.bytes(), self.language != crate::source_editor::DecoderLanguage::Yaml || self.yaml != demo.yaml(), demo.name());
    }
    fn decoder(&mut self, ui: &mut egui::Ui) {
        let disabled_reason = if self.decoding {
            Some("A decoder is already running. Stop it before starting another run.")
        } else if self.name.is_empty() {
            Some("Open a data file first. The decoder runs against the open file.")
        } else if self.decoder_empty() {
            Some("Enter decoder source or open a decoder file first.")
        } else { None };
        let mut run = false;
        let mut save = false;
        let mut stop = false;
        ui.horizontal(|ui| {
            ui.strong(&self.decoder_name).on_hover_text("Current decoder source");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                run = crate::decoder_ui::run_button(ui, disabled_reason.is_none(), disabled_reason).clicked();
                save = ui.add_enabled(!self.yaml.trim().is_empty(), egui::Button::new("Save"))
                    .on_hover_text("Download this decoder source").clicked();
                if self.decoding { stop = ui.button("Stop").clicked(); }
            });
        });
        if run { self.apply_decoder(); }
        if save { save_decoder(&self.yaml, if self.language == crate::source_editor::DecoderLanguage::Python { "python" } else { "yaml" }); }
        if stop { self.menu_action(crate::app_menu::Command::Cancel, 0); }
        ui.horizontal(|ui| {
            let mut language = self.language;
            ui.selectable_value(&mut language, crate::source_editor::DecoderLanguage::Yaml, "YAML");
            ui.selectable_value(&mut language, crate::source_editor::DecoderLanguage::Python, "Python");
            self.switch_language(language);
        });
        if self.size == 0 {
            if ui.button("Open simple demo").clicked() { self.open_simple_demo(); }
            ui.weak("512 synthetic samples · 2 KB");
        }
        let current = !self.decoded.is_null()
            && self.yaml == self.applied_yaml
            && self.language == self.applied_language;
        let (headline, detail, color) = if self.decoder_error.is_some() {
            ("Run stopped · see Console", self.decoder_error.as_deref().unwrap_or(""), crate::theme::DARK.color.status.danger)
        } else if self.decoding {
            ("Running decoder…", "The last results remain visible.", crate::theme::DARK.color.accent.base)
        } else if current {
            ("Results are current", "Structure, Signal, Table, Inspector and Hex use this run.", crate::theme::DARK.color.status.success)
        } else if self.name.is_empty() {
            ("Open a data file to run this decoder", "Use Open file in the sidebar.", crate::theme::DARK.color.text.muted)
        } else {
            ("Ready to run", "Press Run (F5) to update the workspace.", crate::theme::DARK.color.text.muted)
        };
        ui.colored_label(color, headline).on_hover_text(detail);
        if self.language == crate::source_editor::DecoderLanguage::Python {
            ui.weak("Python runs locally in this browser. Scripts can access the network.");
        }
        self.editor.editor(ui, &mut self.yaml, self.language);
        source_editor_focus(self.editor.focused());
    }
    /// Collections in decoder order with their counts, as on the desktop Table.
    fn collection_picker(&mut self, ui: &mut egui::Ui) {
        let mut selected = self.collection.clone();
        let samples = self.samples_path();
        ui.horizontal_wrapped(|ui| {
            let Some(arrays) = self.decoded["collections"].as_array() else {
                ui.weak("Run a decoder to list its collections.");
                return;
            };
            for a in arrays {
                let path = a["path"].as_str().unwrap_or("");
                let count = a["count"].as_str().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
                let name = path.rsplit('.').next().unwrap_or(path);
                let mut chars = name.chars();
                let name: String = chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect());
                let noun = if samples.as_deref() == Some(path) {
                    crate::format::plural(count, "sample", "samples")
                } else {
                    crate::format::plural(count, "item", "items")
                };
                ui.selectable_value(&mut selected, path.into(), format!("{name} ({})", crate::format::count(count)))
                    .on_hover_text(format!("{path}\n{noun}"));
            }
        });
        if selected != self.collection {
            self.select_collection(selected);
        }
    }
    fn samples(&mut self, ui: &mut egui::Ui) {
        self.collection_picker(ui);
        let samples = self.samples_path().as_deref() == Some(self.collection.as_str());
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
            ui.label(if samples { "Go to sample" } else { "Go to item" });
            let enter = ui.add_sized([86., 24.], egui::TextEdit::singleline(&mut self.table_jump)).lost_focus()
                && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if ui.button("Go").clicked() || enter {
                if let Some(index) = parse_offset(&self.table_jump).filter(|index| *index < self.count()) {
                    self.table_start = index;
                    self.request_table();
                } else {
                    self.status = if samples { "Enter a sample index inside this collection." }
                        else { "Enter an item index inside this collection." }.into();
                }
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
                        ui.strong(if samples { "Sample" } else { "Item" });
                        for key in &columns {
                            ui.strong(*key);
                        }
                        ui.end_row();
                        for row in rows {
                            ui.monospace(row["index"].as_str().unwrap_or(""));
                            for key in &columns {
                                let cell = &row["cells"][*key];
                                if ui
                                    .selectable_label(self.selected_sample == row["index"].as_str().and_then(parse_offset), cell["text"].as_str().unwrap_or("—"))
                                    .on_hover_text("Select in Hex and Signal")
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
                ui.label(if self.decoded.is_null() { "Press ▶ Run (F5) to fill this table." } else { "No collections yet. Your decoded fields are in Structure." });
            }
        });
        if let Some((start, end)) = selection {
            self.select_range(start, end);
        }
    }
    fn structures(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.weak("Structure");
            ui.small_button("i").on_hover_text("Expand an item to inspect its fields. Click a row to highlight its bytes in Hex. Hover a number for decimal, hex, and binary.");
        });
        if self.decoded["partial"].as_bool() == Some(true) {
            egui::Frame::group(ui.style()).fill(crate::theme::DARK.color.surface.raised)
                .show(ui, |ui| {
                    ui.colored_label(crate::theme::DARK.color.status.warning, "Partial decode");
                    ui.label(self.decoder_error.as_deref().unwrap_or("The decoder stopped before the end of the file."));
                });
        }
        if let Some(path) = self.tree_page_path.clone().filter(|path| self.tree_expanded.contains(path)) {
            let page = self.tree_pages.get(&path);
            let start = page.and_then(|p| p["start"].as_str()).and_then(parse_offset).unwrap_or(0);
            let count = page.and_then(|p| p["count"].as_str()).and_then(parse_offset).unwrap_or(0);
            let displayed = page.and_then(|p| p["nodes"].as_array()).map_or(0, |nodes| nodes.len() as u64);
            let page_size = page.and_then(|p| p["page_size"].as_u64()).unwrap_or(64);
            let mut next = None;
            ui.horizontal_wrapped(|ui| {
                ui.weak(format!("{}–{} / {}", crate::format::count(start),
                    crate::format::count(start.saturating_add(displayed).min(count)), crate::format::count(count)));
                if ui.add_enabled(start > 0, egui::Button::new("Previous")).clicked() { next = Some(start.saturating_sub(page_size)); }
                if ui.add_enabled(start.saturating_add(displayed) < count, egui::Button::new("Next")).clicked() { next = Some(start.saturating_add(displayed.max(1))); }
                ui.label("Go to index");
                ui.add_sized([72., 24.], egui::TextEdit::singleline(&mut self.tree_page_index));
                if ui.button("Go").clicked() {
                    next = parse_offset(&self.tree_page_index).filter(|index| *index < count);
                }
            });
            if let Some(next) = next {
                self.queue(serde_json::json!({"type":"tree_page","path":path,"start":next.to_string()}));
            }
        }
        let action = crate::browser_tree::show(ui, &self.decoded, &self.tree_expanded, &self.tree_pages, self.tree_selected.as_deref());
        if let Some((path, open, lazy)) = action.toggle {
            if open {
                self.tree_expanded.insert(path.clone());
                let needs_page = lazy && self.tree_pages.get(&path).is_none();
                if needs_page {
                    self.tree_page_path = Some(path.clone());
                    self.tree_page_index = "0".into();
                    self.queue(serde_json::json!({"type":"tree_page","path":path,"start":"0"}));
                }
            } else {
                self.tree_expanded.remove(&path);
            }
        }
        if let Some((path, start, end)) = action.select {
            self.tree_selected = Some(path);
            if start < end { self.select_range(start, end); }
        }
    }
    fn console(&mut self, ui: &mut egui::Ui) {
        let action = crate::decoder_ui::console(ui, &self.python_stdout,
            self.decoder_error.as_deref(), self.decoding);
        if action.copy {
            let mut output = self.python_stdout.clone();
            if let Some(error) = &self.decoder_error {
                if !output.is_empty() { output.push('\n'); }
                output.push_str(error);
            }
            ui.ctx().copy_text(output);
        }
        if action.byte.is_some() { self.layout.reveal(Pane::Hex); }
        if let Some(byte) = action.byte { self.reveal_hex_selection(byte); }
        if action.line.is_some() { self.layout.show_decoder(); }
        if let Some(line) = action.line { self.editor.go_to_line(line); }
    }
    fn parsing_map(&mut self, ui: &mut egui::Ui) {
        let action = crate::decoder_ui::parsing_map_panel(ui, self.decoder_error.as_deref());
        if let Some(byte) = action.byte {
            self.layout.reveal(Pane::Hex);
            self.reveal_hex_selection(byte);
        }
    }
    fn plot_sidebar(&mut self, ui: &mut egui::Ui, names: &[String]) -> bool {
        let mut sources_changed = false;
        let mut close = false;
        egui::Panel::right("browser_plot_series_sidebar")
            .default_size(260.).min_size(220.).max_size(380.).resizable(true)
            .show_collapsible(ui, &mut self.plot_sidebar_open, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_label(!self.plot_sidebar_series, "Collections").clicked() {
                        self.plot_sidebar_series = false;
                    }
                    if ui.selectable_label(self.plot_sidebar_series, "Series").clicked() {
                        self.plot_sidebar_series = true;
                    }
                    if ui.button("×").on_hover_text("Close sidebar").clicked() { close = true; }
                });
                ui.separator();
                if !self.plot_sidebar_series {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for item in self.decoded["collections"].as_array().into_iter().flatten() {
                            let path = item["path"].as_str().unwrap_or("");
                            let mut enabled = self.plot_sources.contains(path);
                            if ui.checkbox(&mut enabled, path).changed() {
                                if enabled { self.plot_sources.insert(path.into()); }
                                else { self.plot_sources.remove(path); }
                                sources_changed = true;
                            }
                        }
                    });
                    return;
                }
                if let Some(group) = &self.plot_axis_group {
                    ui.label(format!("Y axis: {} group", crate::plot_series::group_label(group)))
                        .on_hover_text(group);
                } else if let Some(series) = &self.plot_axis_series {
                    ui.label(format!("Y axis: {}", series.rsplit('.').next().unwrap_or(series)))
                        .on_hover_text(series);
                }
                ui.horizontal(|ui| {
                    if ui.small_button("Fit selected Y").on_hover_text("Fit the selected series or group; keep the sample range").clicked() {
                        self.fit_plot_y = true;
                        self.fit_all_plot_y = false;
                    }
                    if ui.small_button("Fit all Y").on_hover_text("Fit every visible series; keep the sample range").clicked() {
                        self.fit_all_plot_y = true;
                        self.fit_plot_y = false;
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.plot_filter).hint_text("Find series…"));
                let filter = self.plot_filter.trim().to_lowercase();
                let matching: Vec<_> = names.iter().filter(|name|
                    filter.is_empty() || name.to_lowercase().contains(&filter)).map(String::as_str).collect();
                ui.horizontal(|ui| {
                    if ui.small_button("Show all").clicked() {
                        for name in &matching { self.hidden.remove(*name); }
                        self.fit_plot_y = true;
                    }
                    if ui.small_button("Hide all").clicked() {
                        for name in &matching { self.hidden.insert((*name).to_owned()); }
                        self.fit_plot_y = true;
                    }
                });
                egui::ScrollArea::vertical().id_salt("browser-plot-series")
                    .show(ui, |ui| {
                        for (group, entries) in crate::plot_series::grouped(matching) {
                            let open = !filter.is_empty()
                                || (entries.len() <= 32 && !self.plot_groups_collapsed.contains(&group))
                                || self.plot_groups_expanded.contains(&group);
                            let visible = entries.iter().filter(|(_, name)| !self.hidden.contains(*name)).count();
                            ui.push_id(&group, |ui| {
                                ui.horizontal(|ui| {
                                    let arrow = ui.add_sized([18.,18.], egui::Button::new(""));
                                    let c = arrow.rect.center();
                                    let (a,b,d) = if open {
                                        (egui::pos2(c.x-4.,c.y-2.),egui::pos2(c.x,c.y+2.),egui::pos2(c.x+4.,c.y-2.))
                                    } else {
                                        (egui::pos2(c.x-2.,c.y-4.),egui::pos2(c.x+2.,c.y),egui::pos2(c.x-2.,c.y+4.))
                                    };
                                    let stroke=egui::Stroke::new(1.5,ui.visuals().weak_text_color());
                                    ui.painter().line_segment([a,b],stroke);
                                    ui.painter().line_segment([b,d],stroke);
                                    if arrow.clicked() {
                                        if open { self.plot_groups_expanded.remove(&group); self.plot_groups_collapsed.insert(group.clone()); }
                                        else { self.plot_groups_collapsed.remove(&group); self.plot_groups_expanded.insert(group.clone()); }
                                    }
                                    let label = format!("{}  {visible}/{}", crate::plot_series::group_label(&group), entries.len());
                                    if ui.selectable_label(self.plot_axis_group.as_deref() == Some(group.as_str()), label)
                                        .on_hover_text(format!("{group}\nClick to use the group for the Y axis")).clicked() {
                                        self.plot_axis_group = Some(group.clone());
                                    }
                                    let all_shown = visible == entries.len();
                                    if ui.small_button(if all_shown { "Hide" } else { "Show" }).clicked() {
                                        for (_, name) in &entries {
                                            if all_shown { self.hidden.insert((*name).to_owned()); }
                                            else { self.hidden.remove(*name); }
                                        }
                                        if all_shown && self.plot_axis_group.as_deref() == Some(group.as_str()) {
                                            self.plot_axis_group = None;
                                        }
                                        self.fit_plot_y = true;
                                    }
                                });
                                if open {
                                    for (label, name) in entries {
                                        ui.push_id(name, |ui| { ui.horizontal(|ui| {
                                            ui.add_space(16.);
                                            let mut shown = !self.hidden.contains(name);
                                            if ui.checkbox(&mut shown, "").changed() {
                                                if shown { self.hidden.remove(name); }
                                                else { self.hidden.insert(name.to_owned()); }
                                                self.fit_plot_y = true;
                                            }
                                            let active = self.plot_axis_group.is_none()
                                                && self.plot_axis_series.as_deref() == Some(name);
                                            if ui.selectable_label(active, label).on_hover_text(name).clicked() {
                                                self.plot_axis_series = Some(name.to_owned());
                                                self.plot_axis_group = None;
                                            }
                                        }); });
                                    }
                                }
                            });
                        }
                    });
            });
        if close { self.plot_sidebar_open = false; }
        sources_changed
    }
    fn decoded_plot(&mut self, ui: &mut egui::Ui) {
        if !self.decoded.is_null() && (self.yaml != self.applied_yaml || self.language != self.applied_language) {
            ui.weak("Previous results. Press ▶ Run (F5) to refresh.");
        }
        if self.collection.is_empty() {
            let (title, detail) = if self.decoded.is_null() {
                ("Nothing to plot yet", "Press ▶ Run (F5). Repeated numeric fields become plot series.")
            } else {
                ("No sample series yet", "The decoder ran successfully. Add a repeated numeric field, then run again.")
            };
            crate::decoder_ui::empty_state(ui, title, detail);
            return;
        }
        let mut changed = false;
        let mut series=self.plot["series"].as_array().cloned().unwrap_or_default();
        let ranges:Vec<_>=series.iter().filter_map(|c| {
            let values:Vec<_>=c["points"].as_array()?.iter().filter_map(|p|p[1].as_f64()).filter(|v|v.is_finite()).collect();
            if values.is_empty() {return None;}
            Some((c["name"].as_str()?.to_owned(),values.iter().copied().fold(f64::INFINITY,f64::min),values.iter().copied().fold(f64::NEG_INFINITY,f64::max)))
        }).collect();
        let visible_series = series.iter().filter(|channel| {
            !self.hidden.contains(channel["name"].as_str().unwrap_or(""))
        }).count();
        let names: Vec<String> = series.iter().filter_map(|channel|
            channel["name"].as_str().map(str::to_owned)).collect();
        if self.plot_axis_series.as_ref().is_none_or(|name| !names.contains(name)) {
            self.plot_axis_series = names.iter().find(|name| !self.hidden.contains(*name)).cloned();
            self.fit_plot_y = true;
        }
        if self.plot_axis_group.as_ref().is_some_and(|group|
            !names.iter().any(|name| crate::plot_series::group_key(name) == *group)) {
            self.plot_axis_group = None;
        }
        let axis_names = crate::plot_series::axis_targets(&names,
            self.plot_axis_group.as_deref(), self.plot_axis_series.as_deref());
        ui.horizontal(|ui| {
            if ui.button(format!("Series {visible_series}/{}", series.len())).clicked() {
                self.plot_sidebar_open = true;
                self.plot_sidebar_series = true;
            }
            if ui.button("Collections…").clicked() {
                self.plot_sidebar_open = true;
                self.plot_sidebar_series = false;
            }
            if ui.button("Fit").on_hover_text("Show the full sample range and fit all visible series").clicked() { self.first="0".into();self.last=self.plot_count().to_string();self.fit_all_plot_y=true;self.request_plot(1200); }
            if ui.button("Go to sample…").clicked() { self.plot_jump_open = !self.plot_jump_open; }
            if self.plot_jump_open {
                let entry=ui.add(egui::TextEdit::singleline(&mut self.plot_jump).desired_width(86.)
                    .hint_text("Sample index"));
                let enter=entry.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                if ui.button("Go").clicked() || enter {
                    let count=self.plot_count();
                    if let Some(index) = parse_offset(&self.plot_jump).filter(|index| *index < count) {
                        let start = index.saturating_sub(128);
                        self.first=start.to_string();self.last=start.saturating_add(256).min(count).to_string();
                        self.request_plot(1200);
                        self.plot_jump_open = false;
                    } else { self.status = "Enter a sample index inside this collection.".into(); }
                }
            }
            ui.menu_button("Plot options", |ui| {
                ui.label("Drag to pan; scroll or pinch to zoom.");
                if self.presentation.menu(ui,&ranges) {self.fit_plot_y=true;}
                if ui.button("Fit selected Y").clicked() { self.fit_plot_y=true; self.fit_all_plot_y=false; ui.close(); }
                if ui.button("Fit all visible Y").clicked() { self.fit_all_plot_y=true; self.fit_plot_y=false; ui.close(); }
                if ui.button("Inspect selected bytes").clicked() {self.layout.reveal(Pane::Inspector);self.mobile_target=Some(Pane::Inspector);ui.close();}
                ui.weak("Use Open data → Extract selection to export bytes.");
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let number = |text: &str| text.trim().parse::<u64>().map_or_else(|_| text.to_owned(), crate::format::count);
                ui.weak(format!("Samples {}–{} of {}", number(&self.first), number(&self.last),
                    crate::format::count(self.plot_count())));
            });
        });
        if !self.stacked && self.plot_sidebar_open {
            changed |= self.plot_sidebar(ui, &names);
        }
        if changed {
            self.first="0".into();self.last=self.plot_count().min(512).to_string();
            self.plot=Default::default();self.viewport.reset();
            if self.plot_count()>0 {self.request_plot(1200);}
        }
        if self.plot_sources.is_empty() {ui.label("Use Series to add collections.");return;}
        let reduced = self.plot["quality"].as_str().is_some_and(|quality| quality != "Exact samples");
        if reduced {
            ui.weak(self.plot["quality"].as_str().unwrap_or("Reduced samples"));
        }
        {
            for channel in &mut series {
                let p=self.presentation.placement(channel["name"].as_str().unwrap_or(""));
                if let Some(points)=channel["points"].as_array_mut() {for point in points {if let Some(v)=point[1].as_f64() {point[1]=serde_json::json!(p.display(v));}}}
                if let Some(bands)=channel["bands"].as_array_mut() {for band in bands {for i in [2,3] {if let Some(v)=band[i].as_f64(){band[i]=serde_json::json!(p.display(v));}}}}
            }
            let placements=self.presentation.series.clone();
            let axis_placement = axis_names.first().map(|name|self.presentation.placement(name)).unwrap_or_default();
            let axis_uniform = axis_names.iter().all(|name| {
                let p=self.presentation.placement(name);
                p.scale == axis_placement.scale && p.offset == axis_placement.offset
            });
            let fit_selected=std::mem::take(&mut self.fit_plot_y);
            let fit_all=std::mem::take(&mut self.fit_all_plot_y);
            let fit_y=(fit_selected || fit_all).then(|| {
                let values=series.iter().filter(|c|{
                    let name=c["name"].as_str().unwrap_or("");
                    !self.hidden.contains(name) && (fit_all || axis_names.is_empty() || axis_names.iter().any(|axis| axis == name))
                })
                    .flat_map(|c|c["points"].as_array().into_iter().flatten().filter_map(|p|p[1].as_f64())
                        .chain(c["bands"].as_array().into_iter().flatten().flat_map(|band| [band[2].as_f64(), band[3].as_f64()].into_iter().flatten())))
                    .filter(|v|v.is_finite());
                values.fold((f64::INFINITY,f64::NEG_INFINITY),|(min,max),v|(min.min(v),max.max(v)))
            });

            let plot = egui_plot::Plot::new("decoded-plot-overlay").x_axis_label("Sample index")
                .allow_scroll(false)
                .allow_drag(egui::Vec2b::new(true,false))
                .allow_zoom(false)
                .allow_axis_zoom_drag(false)
                .custom_y_axes(vec![egui_plot::AxisHints::new_y()
                    .label(if axis_uniform {"Selected series value"} else {"Display position"})
                    .formatter(move |mark,_|format!("{:.2}",if axis_uniform {axis_placement.original(mark.value)} else {mark.value}))])
                .label_formatter(move |hover| {
                    if let egui_plot::HoverPosition::NearDataPoint { plot_name, position, .. } = hover {
                        let p=placements.get(*plot_name).copied().unwrap_or_default();
                        Some(format!("{plot_name}\nSample: {:.0}\nValue: {}", position.x, p.original(position.y)))
                    } else { None }
                });
            let reset_bounds = std::mem::take(&mut self.reset_plot) || self.plot_visible.is_none();
            let plot_ctx = ui.ctx().clone();
            let plot_outer = ui.available_rect_before_wrap();
            let plot_clip = ui.clip_rect();
            let plot_layer = ui.layer_id();
            let response = plot
            .show(ui, |plot| {
                if reset_bounds {
                    if let (Some(first),Some(last))=(parse_offset(&self.first),parse_offset(&self.last)) {
                        if first < last { plot.set_plot_bounds_x(first as f64..=last as f64); }
                    }
                }
                let y_range = fit_y.or_else(|| reset_bounds.then(|| {
                    series.iter().filter(|c| !self.hidden.contains(c["name"].as_str().unwrap_or("")))
                        .flat_map(|c| c["points"].as_array().into_iter().flatten().filter_map(|p| p[1].as_f64())
                            .chain(c["bands"].as_array().into_iter().flatten().flat_map(|band| [band[2].as_f64(), band[3].as_f64()].into_iter().flatten())))
                        .filter(|v| v.is_finite())
                        .fold((f64::INFINITY,f64::NEG_INFINITY), |(min,max),v| (min.min(v),max.max(v)))
                }));
                if let Some((min,max))=y_range.filter(|(min,max)|min.is_finite()&&max.is_finite()) {
                    let pad=((max-min)*0.05).max(0.1);
                    plot.set_plot_bounds_y(min-pad..=max+pad);
                }
                for channel in &series {
                    let name = channel["name"].as_str().unwrap_or("");
                    if self.hidden.contains(name) {
                        continue;
                    }
                    let points: Vec<[f64; 2]> =
                        serde_json::from_value(channel["points"].clone()).unwrap_or_default();
                    let bands: Vec<[f64;4]> = serde_json::from_value(channel["bands"].clone()).unwrap_or_default();
                    crate::plot_trace::decoded_channel(plot, name, crate::plot_trace::series_color(name), points,
                        self.plot["estimated"].as_bool().unwrap_or(false).then_some(bands.as_slice()));
                }
                if !self.stacked && plot.response().contains_pointer() {
                    let (wheel, pinch, shift)=plot_ctx.input(|input| {
                        (input.smooth_scroll_delta, input.zoom_delta_2d(), input.modifiers.shift)
                    });
                    let delta=if wheel.y != 0. {wheel.y} else {wheel.x};
                    if shift && delta != 0. {
                        let width=plot.plot_bounds().width();
                        plot.translate_bounds(egui::vec2((-(delta as f64)*width/500.) as f32,0.));
                    } else {
                        let factor=if pinch.x != 1. {pinch.x} else {(delta.clamp(-240.,240.)*0.0025).exp()};
                        if factor != 1. {
                            if let Some(center)=plot.pointer_coordinate() {
                                plot.zoom_bounds(egui::vec2(factor,1.),center);
                            }
                        }
                    }
                }
                if !self.stacked && !axis_names.is_empty() {
                    let inner=plot.response().rect;
                    let axis=egui::Rect::from_min_max(
                        egui::pos2(plot_outer.left(),inner.top()),egui::pos2(inner.left(),inner.bottom()));
                    let axis=axis.intersect(plot_clip);
                    let (pressed,down,delta,origin,pointer,shift,wheel,pinch)=plot_ctx.input(|input| {
                        let wheel=input.raw.events.iter().filter_map(|event| {
                            if let egui::Event::MouseWheel {unit,delta,..}=event {
                                let scale=match unit {egui::MouseWheelUnit::Point=>1.,egui::MouseWheelUnit::Line=>48.,egui::MouseWheelUnit::Page=>240.};
                                Some(*delta*scale)
                            } else {None}
                        }).fold(egui::Vec2::ZERO,|sum,delta|sum+delta);
                        (input.pointer.button_pressed(egui::PointerButton::Primary),
                            input.pointer.button_down(egui::PointerButton::Primary),
                            input.pointer.delta(),input.pointer.press_origin(),input.pointer.hover_pos(),
                            input.modifiers.shift,wheel,input.zoom_delta_2d().y)
                    });
                    if pressed { self.plot_axis_panning=origin.is_some_and(|pos|axis.contains(pos)); }
                    if !down { self.plot_axis_panning=false; }
                    else if self.plot_axis_panning && !pressed && delta.y != 0. {
                        let amount=-(delta.y as f64)*plot.plot_bounds().height()/inner.height().max(1.) as f64;
                        for name in &axis_names {self.presentation.pan_series(name,amount);}
                        plot_ctx.request_repaint();
                    }
                    if pointer.is_some_and(|pos|axis.contains(pos)
                        && plot_ctx.rect_contains_pointer(plot_layer,axis)) {
                        let delta=if shift {wheel.x+wheel.y} else if wheel.y!=0. {wheel.y} else {wheel.x};
                        let center=pointer.map(|pos|egui_plot::PlotTransform::new(inner,plot.plot_bounds(),false)
                            .value_from_position(pos).y).unwrap_or(0.);
                        if shift && delta!=0. {
                            let amount=-(f64::from(delta)/120.).clamp(-1.,1.)*plot.plot_bounds().height();
                            for name in &axis_names {self.presentation.pan_series(name,amount);}
                            plot_ctx.request_repaint();
                        } else if pinch!=1. || delta!=0. {
                            let factor=if pinch!=1. {f64::from(pinch)} else {f64::from((delta*0.012).clamp(-1.,1.).exp())};
                            for name in &axis_names {self.presentation.zoom_series(name,factor,center);}
                            plot_ctx.request_repaint();
                        }
                    }
                }
            });
            if let Some((first, end)) = self.selected_samples.filter(|(first, end)| first < end) {
                let frame = *response.transform.frame();
                let left = response.transform.position_from_point(
                    &egui_plot::PlotPoint::new(first as f64 - 0.5, 0.),
                ).x;
                let right = response.transform.position_from_point(
                    &egui_plot::PlotPoint::new(end as f64 - 0.5, 0.),
                ).x;
                let painter = ui.painter().with_clip_rect(frame.intersect(ui.clip_rect()));
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(left, frame.top()),
                        egui::pos2(right.max(left + 2.), frame.bottom()),
                    ),
                    0.,
                    ui.visuals().selection.bg_fill.gamma_multiply(0.18),
                );
                painter.line_segment(
                    [egui::pos2(left, frame.top()), egui::pos2(left, frame.bottom())],
                    egui::Stroke::new(1_f32, ui.visuals().selection.stroke.color),
                );
            }
            self.plot_visible = Some([response.transform.bounds().min()[0], response.transform.bounds().max()[0]]);
            let now = ui.input(|i| i.time);
            let interacting = (response.response.dragged() || response.response.drag_stopped()) ||
                (response.response.hovered() && ui.input(|i| (!self.stacked && i.smooth_scroll_delta != egui::Vec2::ZERO) || i.zoom_delta() != 1.0));
            if interacting {
                let bounds = response.transform.bounds();
                self.viewport.observe(bounds.min()[0], bounds.max()[0], self.plot_count(), now);
            }
            if let Some((first, last)) = self.viewport.ready(now) {
                self.first = first.to_string(); self.last = last.to_string();
                self.request_plot_view(response.response.rect.width().max(100.) as u32, false);
            }
            if response.response.clicked() && !response.response.double_clicked() && self.plot_count() > 0 {
                if let Some(pointer) = response.response.interact_pointer_pos() {
                    let transform = &response.transform;
                    let x = transform.value_from_position(pointer).x;
                    let shift = ui.input(|input| input.modifiers.shift);
                    if shift {
                        if let Some(index) = crate::browser_viewport::clicked_sample(x, self.count()) {
                            let anchor = self.plot_anchor.or(self.selected_sample).unwrap_or(index);
                            self.probe_request = self.probe_request.wrapping_add(1);
                            self.queue(serde_json::json!({"type":"pick","path":self.collection,
                                "anchor":anchor.to_string(),"index":index.to_string(),
                                "request":self.probe_request.to_string()}));
                        }
                        return;
                    }
                    let nearest = series.iter().filter(|c| !self.hidden.contains(c["name"].as_str().unwrap_or("")))
                        .flat_map(|c| c["points"].as_array().into_iter().flatten().enumerate().filter_map(move |(i,p)| {
                            let x=p[0].as_f64()?; let y=p[1].as_f64()?;
                            let distance=transform.position_from_point(&egui_plot::PlotPoint::new(x,y)).distance_sq(pointer);
                            Some((distance,c,i,x))
                        })).min_by(|a,b| a.0.total_cmp(&b.0));
                    if let Some((_, channel, i, x)) = nearest {
                        if let (Some(start), Some(end)) = (channel["bytes"][i][0].as_str().and_then(parse_offset), channel["bytes"][i][1].as_str().and_then(parse_offset)) {
                            let path=channel["path"].as_str().unwrap_or("").to_owned();
                            self.collection=path;
                            self.select_range(start,end);
                            let index = x.round().max(0.) as u64;
                            self.plot_anchor=Some(index);
                            self.selected_sample=Some(index);
                            self.selected_samples=Some((index,index.saturating_add(1)));
                            self.table_start=index / 128 * 128;
                            self.request_table();
                        }
                    } else {
                    if let Some(index) = crate::browser_viewport::clicked_sample(x, self.count()) {
                        self.plot_anchor = Some(index);
                        self.probe_request = self.probe_request.wrapping_add(1);
                        self.queue(serde_json::json!({"type":"pick","path":self.collection,
                            "index":index.to_string(),"request":self.probe_request.to_string()}));
                    }
                    }
                }
            }
            if response.response.double_clicked() {
                self.first = "0".into(); self.last = self.plot_count().to_string();
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
    fn ui(&mut self, root_ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        source_editor_focus(false);
        self.stacked = self.layout_mode == "stacked" || (self.layout_mode == "auto" && ctx.content_rect().width() < 900.);
        self.events();
        if self.reveal_decoder {
            self.layout.show_decoder();
            self.mobile_target = Some(Pane::Layout);
            self.reveal_decoder = false;
        }
        if self.reveal_inspector {
            self.layout.reveal(Pane::Inspector);
            self.reveal_inspector = false;
        }
        if self.show_decoding_guide {
            egui::Window::new("Decoder guide")
                .open(&mut self.show_decoding_guide)
                .default_width(560.)
                .default_height(640.)
                .resizable(true)
                .show(&ctx, |ui| crate::source_editor::decoding_guide(ui));
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F5)) {
            self.apply_decoder();
        }
        if self.reveal_results {
            if self.collection.is_empty() {
                self.layout.reveal(Pane::Structures);
            } else {
                self.layout.show_samples();
                self.layout.show_initial_plot();
                self.layout.show_decoder();
            }
            self.reveal_results = false;
        }
        let mut menus=crate::app_menu::menus(&crate::app_menu::State {
            available:!self.decoding, file:!self.name.is_empty(), selection:parse_offset(&self.range_start).zip(parse_offset(&self.range_end)).is_some_and(|(a,b)| a<b),
            undo:self.undo, redo:self.redo, save_decoder:!self.yaml.trim().is_empty(), apply:!self.decoding && !self.name.is_empty() && !self.decoder_empty(), busy:self.decoding, ..Default::default()
        });
        let panels = Pane::PANELS.iter().enumerate().map(|(index, pane)| crate::app_menu::Item {
            label:pane.title().into(), command:Some(crate::app_menu::Command::TogglePane),
            argument:index, enabled:true, checked:Some(self.layout.dock.find_tab(pane).is_some()), children:vec![],
        }).collect();
        menus.insert(0,crate::app_menu::Item {label:"Panels".into(),command:None,argument:0,enabled:true,checked:None,children:panels});
        menus.insert(1,crate::app_menu::Item {label:"Reset layout".into(),command:Some(crate::app_menu::Command::Reset),argument:0,enabled:true,checked:None,children:vec![]});
        publish_menus(&serde_json::to_string(&menus).unwrap(), &if self.name.is_empty() { String::new() } else { format!("{} · {}{}",self.name,crate::format::size(self.size),if self.dirty {" · unsaved changes"}else{""}) }, &self.status, crate::APP_VERSION);
        if self.stacked {
            egui::CentralPanel::default().show(root_ui, |ui| {
                ui.spacing_mut().interact_size.y = 32.;
                ui.spacing_mut().item_spacing.y = 8.;
                let target = self.mobile_target.take();
                egui::ScrollArea::vertical().id_salt("mobile-workspace").auto_shrink([false,false]).show(ui, |ui| {
                    let width = ui.available_width();
                    let editor_height = (ui.clip_rect().height()-56.).clamp(300.,560.);
                    for (pane, height, open) in [
                        (Pane::Hex, 380., true), (Pane::Structures, 380., true),
                        (Pane::Layout, editor_height, false), (Pane::Plot, 430., true),
                        (Pane::Table, 310., false), (Pane::Inspector, 300., false),
                    ] {
                        if pane != Pane::Layout && self.name.is_empty() { continue; }
                        if matches!(pane, Pane::Plot | Pane::Table) && self.collection.is_empty() { continue; }
                        ui.push_id(("mobile-pane", pane), |ui| {
                            let header = egui::CollapsingHeader::new(egui::RichText::new(pane.title()).strong())
                                .default_open(open).open(if target == Some(pane) { Some(true) } else { None })
                                .show(ui, |ui| {
                                    ui.allocate_ui_with_layout(egui::vec2((width-24.).max(100.),height),egui::Layout::top_down(egui::Align::Min), |ui| {
                                        ui.set_clip_rect(ui.max_rect().intersect(ui.clip_rect()));
                                        TabViewer::ui(self, ui, &mut pane.clone());
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
            DockArea::new(&mut layout.dock)
                .id(egui::Id::new("workspace_dock"))
                .style(crate::theme::dock_style(root_ui.style().as_ref()))
                .show_close_buttons(true)
                .show_leaf_close_all_buttons(false)
                .show_leaf_collapse_buttons(false)
                .tab_context_menus(true)
                .show_inside(root_ui, self);
            self.layout = layout;
        }
        if self.sent_yaml.as_ref() != Some(&self.yaml) {
            if self.decoder_empty() { self.reset_decoder(); }
            decoder_draft(&self.yaml, if self.language == crate::source_editor::DecoderLanguage::Python { "python" } else { "yaml" });
            self.sent_yaml = Some(self.yaml.clone());
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.layout.save(storage);
        storage.set_string("browser-layout-mode", self.layout_mode.clone());
        let (yaml, python) = if self.language == crate::source_editor::DecoderLanguage::Yaml {
            (&self.yaml, &self.other_draft)
        } else { (&self.other_draft, &self.yaml) };
        if yaml.len() <= 256 * 1024 { storage.set_string("browser-decoder-yaml", yaml.clone()); }
        if python.len() <= 256 * 1024 { storage.set_string("browser-decoder-python", python.clone()); }
        storage.set_string("browser-decoder-language", if self.language == crate::source_editor::DecoderLanguage::Python { "python" } else { "yaml" }.into());
        storage.set_string(
            "browser-editor-tab-width",
            self.editor.tab_width().to_string(),
        );
    }
}
impl TabViewer for BrowserApp {
    type Tab = Pane;
    fn id(&mut self, pane: &mut Pane) -> egui::Id {
        egui::Id::new(*pane)
    }
    fn title(&mut self, pane: &mut Pane) -> egui::WidgetText {
        pane.title().into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, pane: &mut Pane) {
        match pane {
            Pane::Hex => self.hex(ui),
            Pane::Layout => self.decoder(ui),
            Pane::Inspector => { egui::ScrollArea::vertical().show(ui, |ui| self.inspector(ui)); },
            Pane::Plot => self.decoded_plot(ui),
            Pane::Table => self.samples(ui),
            Pane::Structures => self.structures(ui),
            Pane::Console => self.console(ui),
            Pane::ParsingMap => self.parsing_map(ui),
        }
    }
}
