//! Shared, bounded-memory byte editing surface. All file reads run off the UI thread.
#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use crate::byte_store::ByteView;
use egui::{Color32, FontId, Key, RichText, Sense};
use std::ops::Range;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

const PAGE: usize = 64 * 1024;
pub(crate) const WINDOW_ROWS: usize = 65_536;
const COPY_LIMIT: usize = 1024 * 1024;
const SEARCH_CHUNK: usize = 256 * 1024;
const MATCH_LIMIT: usize = 10_000;

#[derive(Clone)]
pub struct HexHighlight {
    pub range: Range<usize>,
    pub name: String,
    pub color: Color32,
}

#[derive(Debug)]
pub enum HexAction {
    Replace { offset: usize, bytes: Vec<u8> },
    Insert { offset: usize, bytes: Vec<u8> },
    Delete(Range<usize>),
    Undo,
    Redo,
    Define(Range<usize>),
    Extract(Range<usize>),
    Selection(Range<usize>),
}

struct PageData {
    offset: usize,
    revision: u64,
    result: Result<(Vec<u8>, Vec<bool>), String>,
}

struct SearchUpdate {
    scanned: usize,
    matches: Vec<usize>,
    complete: bool,
    error: Option<String>,
}

enum Transfer {
    Hex,
    Text,
}

pub struct HexEditor {
    python_mode: bool,
    anchor: Option<usize>,
    caret: usize,
    reveal: bool,
    bytes_per_row: usize,
    window_start: usize,
    pending_scroll: Option<f32>,
    /// Grid's horizontal scroll offset last frame, so the column header follows it.
    horizontal_scroll: f32,
    visible_range: Option<Range<usize>>,
    jump: String,
    focus_jump: bool,
    history: Vec<usize>,
    history_pos: usize,
    page: Option<PageData>,
    page_rx: Option<mpsc::Receiver<PageData>>,
    revision: Option<u64>,
    selection_bytes: Option<(Range<usize>, u64, Vec<u8>)>,
    selection_rx: Option<mpsc::Receiver<(Range<usize>, u64, Result<Vec<u8>, String>)>>,
    bookmarks: Vec<HexHighlight>,
    bookmark_name: String,
    bookmark_color: Color32,
    show_bookmark: bool,
    show_search: bool,
    focus_find: bool,
    focus_replace: bool,
    query: String,
    search_text: bool,
    search_rx: Option<mpsc::Receiver<SearchUpdate>>,
    search_cancel: Option<Arc<AtomicBool>>,
    search_matches: Vec<usize>,
    search_index: usize,
    search_scanned: usize,
    search_width: usize,
    search_complete: bool,
    show_replace: bool,
    replacement: String,
    replace_text: bool,
    edit_mode: bool,
    /// Typing inserts bytes before the cursor instead of overwriting them.
    insert_mode: bool,
    /// False while a decoder reads fixed offsets, so the length must not change.
    can_resize: bool,
    /// The bytes row inserts instead of overwriting the selection.
    replace_inserts: bool,
    ascii_input: bool,
    high_nibble: Option<u8>,
    transfer_rx: Option<mpsc::Receiver<Result<String, String>>>,
    status: String,
    focused: bool,
    drag_selecting: bool,
    show_decoded_fields: bool,
}

impl Default for HexEditor {
    fn default() -> Self {
        Self {
            python_mode: false,
            anchor: None,
            caret: 0,
            reveal: false,
            bytes_per_row: 16,
            window_start: 0,
            pending_scroll: None,
            horizontal_scroll: 0.0,
            visible_range: None,
            jump: String::new(),
            focus_jump: false,
            history: vec![0],
            history_pos: 0,
            page: None,
            page_rx: None,
            revision: None,
            selection_bytes: None,
            selection_rx: None,
            bookmarks: Vec::new(),
            bookmark_name: String::new(),
            bookmark_color: Color32::from_rgb(71, 137, 188),
            show_bookmark: false,
            show_search: false,
            focus_find: false,
            focus_replace: false,
            query: String::new(),
            search_text: false,
            search_rx: None,
            search_cancel: None,
            search_matches: Vec::new(),
            search_index: 0,
            search_scanned: 0,
            search_width: 0,
            search_complete: false,
            show_replace: false,
            replacement: String::new(),
            replace_text: false,
            edit_mode: false,
            insert_mode: false,
            can_resize: true,
            replace_inserts: false,
            ascii_input: false,
            high_nibble: None,
            transfer_rx: None,
            status: String::new(),
            focused: false,
            drag_selecting: false,
            show_decoded_fields: true,
        }
    }
}

impl Drop for HexEditor {
    fn drop(&mut self) {
        self.cancel_search();
    }
}

impl HexEditor {
    pub fn set_python_mode(&mut self, enabled: bool) {
        self.python_mode = enabled;
    }

    pub fn show_jump(&mut self) {
        self.focus_jump = true;
    }

    pub fn show_find(&mut self) {
        self.show_search = true;
        self.focus_find = true;
    }

    pub fn show_replace(&mut self) {
        self.show_replace = true;
        self.replace_inserts = false;
        self.focus_replace = true;
    }

    pub fn show_insert(&mut self) {
        self.show_replace = true;
        self.replace_inserts = true;
        self.focus_replace = true;
    }

    pub fn set_can_resize(&mut self, can_resize: bool) {
        self.can_resize = can_resize;
        self.insert_mode &= can_resize;
    }

    /// Move offsets after an insert or delete: bytes before `at` stay, bytes after
    /// the removed ones move by the size change, and removed ones collapse to `at`.
    pub fn shift(&mut self, at: usize, removed: usize, inserted: usize) {
        let map = |offset: usize| {
            if offset < at {
                offset
            } else if offset >= at + removed {
                offset - removed + inserted
            } else {
                at
            }
        };
        if let Some(anchor) = self.anchor {
            // Keep a caret that sat on the edit point after inserted bytes.
            self.anchor = Some(map(anchor));
            self.caret = map(self.caret);
        }
        for offset in &mut self.history {
            *offset = map(*offset);
        }
        self.bookmarks.retain_mut(|bookmark| {
            bookmark.range = map(bookmark.range.start)..map(bookmark.range.end);
            !bookmark.range.is_empty()
        });
        self.cancel_search();
        self.search_matches.clear();
        self.high_nibble = None;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Reuse the asynchronous page cache; never read a file on the UI thread.
    #[allow(dead_code)] // The Interpret control is hidden for now; kept for when it returns.
    pub fn interpretation_preview(&self, range: Range<usize>) -> Option<Vec<u8>> {
        let page = self.page.as_ref()?;
        if self.revision != Some(page.revision) { return None; }
        let (bytes, _) = page.result.as_ref().ok()?;
        let start = range.start.checked_sub(page.offset)?;
        let available = bytes.get(start..)?;
        Some(available[..available.len().min(range.len()).min(64)].to_vec())
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        self.anchor
            .map(|anchor| anchor.min(self.caret)..anchor.max(self.caret).saturating_add(1))
    }

    pub fn select_range(&mut self, range: Range<usize>, reveal: bool) {
        let already_visible = self
            .visible_range
            .as_ref()
            .is_some_and(|visible| visible.contains(&range.start));
        if range.is_empty() {
            self.anchor = None;
            if reveal {
                self.caret = range.start;
                self.reveal |= !already_visible;
                self.high_nibble = None;
            }
            return;
        }
        self.anchor = Some(range.start);
        self.caret = range.end - 1;
        self.reveal |= reveal && !already_visible;
        self.high_nibble = None;
    }

    fn cancel_search(&mut self) {
        if let Some(cancel) = self.search_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.search_rx = None;
    }

    fn remember_location(&mut self, offset: usize) {
        if self.history.get(self.history_pos).copied() != Some(offset) {
            self.history.truncate(self.history_pos + 1);
            self.history.push(offset);
            if self.history.len() > 128 {
                self.history.remove(0);
            }
            self.history_pos = self.history.len() - 1;
        }
    }

    fn visit(&mut self, offset: usize) {
        // A clicked byte or a table selection can move the cursor without adding
        // history. Save that actual origin before an explicit jump.
        self.remember_location(self.caret);
        self.remember_location(offset);
        self.select_range(offset..offset.saturating_add(1), true);
        self.status.clear();
    }

    fn navigate_history(&mut self, backwards: bool) {
        let next = if backwards {
            self.history_pos.checked_sub(1)
        } else {
            (self.history_pos + 1 < self.history.len()).then_some(self.history_pos + 1)
        };
        if let Some(next) = next {
            self.history_pos = next;
            let offset = self.history[next];
            self.select_range(offset..offset.saturating_add(1), true);
            self.status.clear();
        }
    }

    fn jump_to_input(&mut self, len: usize) {
        match parse_jump(&self.jump, self.caret, len) {
            Ok(offset) => self.visit(offset),
            Err(error) => self.status = error,
        }
    }

    fn poll(&mut self, ui: &mut egui::Ui, source: &ByteView) {
        if self.revision != Some(source.revision) {
            self.page = None;
            self.selection_bytes = None;
            self.revision = Some(source.revision);
            self.cancel_search();
            self.search_matches.clear();
            if source.len() == 0 {
                self.anchor = None;
            } else if self.caret >= source.len() {
                self.select_range(source.len() - 1..source.len(), true);
            }
        }
        if let Some(rx) = &self.page_rx {
            match rx.try_recv() {
                Ok(page) => {
                    if page.revision == source.revision {
                        self.page = Some(page);
                    }
                    self.page_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.page_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.selection_rx {
            match rx.try_recv() {
                Ok((range, revision, result)) => {
                    if revision == source.revision {
                        match result {
                            Ok(bytes) => self.selection_bytes = Some((range, revision, bytes)),
                            Err(error) => self.status = error,
                        }
                    }
                    self.selection_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.selection_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.search_rx {
            if let Ok(update) = rx.try_recv() {
                self.search_matches = update.matches;
                self.search_scanned = update.scanned;
                self.search_complete = update.complete;
                if let Some(error) = update.error {
                    self.status = error;
                }
                if update.complete {
                    self.search_rx = None;
                    self.search_cancel = None;
                }
            }
        }
        if let Some(rx) = &self.transfer_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(text) => {
                        ui.ctx().copy_text(text);
                        self.status = "Copied selection".into();
                    }
                    Err(error) => self.status = error,
                }
                self.transfer_rx = None;
            }
        }
        if self.page_rx.is_some()
            || self.selection_rx.is_some()
            || self.search_rx.is_some()
            || self.transfer_rx.is_some()
        {
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
    }

    fn request_page(&mut self, source: &ByteView, offset: usize, ctx: &egui::Context) {
        let offset = offset / PAGE * PAGE;
        if self.page_rx.is_some()
            || self
                .page
                .as_ref()
                .is_some_and(|p| p.offset == offset && p.revision == source.revision)
        {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.page_rx = Some(rx);
        let source = source.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // One page of lookahead avoids a blank row at a page boundary.
            let result = source
                .read(offset, (PAGE * 2).min(source.len().saturating_sub(offset)))
                .map(|bytes| {
                    let edited = source.edited(offset, bytes.len());
                    (bytes, edited)
                })
                .map_err(|error| error.to_string());
            let _ = tx.send(PageData {
                offset,
                revision: source.revision,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn byte(&self, offset: usize) -> Option<(u8, bool)> {
        let page = self.page.as_ref()?;
        let (bytes, edited) = page.result.as_ref().ok()?;
        let index = offset.checked_sub(page.offset)?;
        Some((*bytes.get(index)?, edited.get(index).copied().unwrap_or(false)))
    }

    pub fn copy_hex(&mut self, source: &ByteView) { self.copy(source, Transfer::Hex); }
    fn copy(&mut self, source: &ByteView, kind: Transfer) {
        let Some(range) = self.selection() else {
            return;
        };
        if range.len() > COPY_LIMIT {
            self.status =
                "Clipboard limit is 1 MiB. Use Extract selection for larger ranges.".into();
            return;
        }
        if self.transfer_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.transfer_rx = Some(rx);
        let source = source.clone();
        std::thread::spawn(move || {
            let result = source
                .read(range.start, range.len())
                .map(|bytes| match kind {
                    Transfer::Hex => bytes
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    Transfer::Text => String::from_utf8_lossy(&bytes).into_owned(),
                })
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
    }

    fn start_search(&mut self, source: &ByteView, ctx: &egui::Context) {
        let pattern = if self.search_text {
            Ok(self.query.as_bytes().to_vec())
        } else {
            parse_hex(&self.query)
        };
        let pattern = match pattern {
            Ok(pattern) if !pattern.is_empty() => pattern,
            Ok(_) => {
                self.status = "Enter bytes or text to find.".into();
                return;
            }
            Err(error) => {
                self.status = error;
                return;
            }
        };
        if pattern.len() > 4096 {
            self.status = "Search pattern limit is 4096 bytes.".into();
            return;
        }
        self.cancel_search();
        self.search_width = pattern.len();
        self.search_matches.clear();
        self.search_scanned = 0;
        self.search_index = 0;
        self.search_complete = false;
        self.status.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        self.search_cancel = Some(cancel.clone());
        let (tx, rx) = mpsc::sync_channel(1);
        self.search_rx = Some(rx);
        let source = source.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut matches = Vec::new();
            let mut scanned = 0;
            let mut last_update = Instant::now();
            let mut error = None;
            while scanned < source.len() && matches.len() < MATCH_LIMIT {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let count = SEARCH_CHUNK.min(source.len() - scanned);
                let lookahead = count
                    .saturating_add(pattern.len() - 1)
                    .min(source.len() - scanned);
                match source.read(scanned, lookahead) {
                    Ok(bytes) => find_in_chunk(&bytes, &pattern, scanned, count, &mut matches),
                    Err(err) => {
                        error = Some(err.to_string());
                        break;
                    }
                }
                scanned += count;
                if last_update.elapsed() >= Duration::from_millis(100) {
                    let _ = tx.try_send(SearchUpdate {
                        scanned,
                        matches: matches.clone(),
                        complete: false,
                        error: None,
                    });
                    ctx.request_repaint();
                    last_update = Instant::now();
                }
            }
            if !cancel.load(Ordering::Relaxed) {
                let _ = tx.send(SearchUpdate {
                    scanned,
                    matches,
                    complete: true,
                    error,
                });
                ctx.request_repaint();
            }
        });
    }

    fn search_move(&mut self, backwards: bool) {
        if self.search_matches.is_empty() {
            return;
        }
        let index = if backwards {
            self.search_matches
                .partition_point(|offset| *offset < self.caret)
                .checked_sub(1)
                .unwrap_or(self.search_matches.len() - 1)
        } else {
            let index = self
                .search_matches
                .partition_point(|offset| *offset <= self.caret);
            if index == self.search_matches.len() {
                0
            } else {
                index
            }
        };
        self.search_index = index;
        let start = self.search_matches[index];
        self.visit(start);
        self.select_range(start..start + self.search_width, true);
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        source: Option<ByteView>,
        highlights: &[HexHighlight],
    ) -> Vec<HexAction> {
        self.ui_with_tools(ui, source, highlights, &mut |_| {})
    }

    /// `tools` adds controls (such as Interpret) to the toolbar row, after Find.
    pub fn ui_with_tools(
        &mut self,
        ui: &mut egui::Ui,
        source: Option<ByteView>,
        highlights: &[HexHighlight],
        tools: &mut dyn FnMut(&mut egui::Ui),
    ) -> Vec<HexAction> {
        let mut actions = Vec::new();
        let Some(source) = source else {
            ui.centered_and_justified(|ui| {
                ui.label("Open a file to inspect, select, and edit its bytes.");
            });
            return actions;
        };
        self.poll(ui, &source);
        // A delete can leave the cursor past the new end of the file.
        if let Some(last) = source.len().checked_sub(1) {
            self.caret = self.caret.min(last);
            self.anchor = self.anchor.map(|anchor| anchor.min(last));
        } else {
            self.anchor = None;
            self.caret = 0;
        }
        let navigation_origin = self.selection();
        // 16 bytes per row by default; 32 is a remembered setting.
        let wide_id = egui::Id::new("hex_32_bytes_per_row");
        let mut wide = ui.ctx().data_mut(|data| *data.get_persisted_mut_or(wide_id, false));
        let per_row = if wide { 32 } else { 16 };
        if self.bytes_per_row != per_row {
            self.bytes_per_row = per_row;
            self.reveal = true;
        }
        let shown_highlights = if self.show_decoded_fields { highlights } else { &[] };
        self.readout(ui, &source, shown_highlights);
        ui.horizontal_wrapped(|ui| {
            let can_back = self.history_pos > 0;
            if ui.add_enabled(can_back, egui::Button::new("‹"))
                .on_hover_text(if can_back { "Return to the previous visited byte." } else { "No previous location. Jump to another byte to start navigation history." })
                .on_disabled_hover_text("No previous location. Jump to another byte to start navigation history.")
                .clicked() {
                self.navigate_history(true);
            }
            let can_forward = self.history_pos + 1 < self.history.len();
            if ui.add_enabled(can_forward, egui::Button::new("›"))
                .on_hover_text("Return to the byte you left with Back.")
                .on_disabled_hover_text("No next location. Use Back first.")
                .clicked() {
                self.navigate_history(false);
            }
            let mut jump = egui::TextEdit::singleline(&mut self.jump)
                .id_salt("hex_jump")
                .desired_width(150.0)
                .hint_text("Go to  0x100  or  +256")
                .show(ui);
            if self.focus_jump {
                jump.response.request_focus();
                jump.state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(self.jump.chars().count()),
                )));
                jump.state.store(ui.ctx(), jump.response.id);
                self.focus_jump = false;
            }
            if jump.response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && source.len() > 0 {
                self.jump_to_input(source.len());
            }
            if ui.toggle_value(&mut self.show_search, "Find").changed() && self.show_search { self.focus_find = true; }
            tools(ui);
            self.edit_mode_switch(ui);
            crate::theme::more_menu(ui, |ui| {
                self.selection_menu(ui, &source, &mut actions);
                ui.separator();
                if ui.checkbox(&mut wide, "32 bytes per row").changed() {
                    ui.ctx().data_mut(|data| data.insert_persisted(wide_id, wide));
                }
                ui.checkbox(&mut self.show_decoded_fields, "Highlight decoded fields");
                if self.edit_mode { ui.checkbox(&mut self.ascii_input, "Type text instead of hex"); }
                ui.separator();
                ui.label("Saved ranges");
                let mut visit = None;
                let mut remove = None;
                for (index, bookmark) in self.bookmarks.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.colored_label(bookmark.color, &bookmark.name).interact(Sense::click()).clicked() { visit = Some(bookmark.range.clone()); }
                        if ui.small_button("×").clicked() { remove = Some(index); }
                    });
                }
                if let Some(range) = visit { self.visit(range.start); self.select_range(range, true); ui.close(); }
                if let Some(index) = remove { self.bookmarks.remove(index); }
                if self.bookmarks.is_empty() { ui.weak("Select bytes to name and color a range."); }
            });
        });
        if self.show_search {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.search_text, "Text");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .desired_width(170.0)
                        .hint_text(if self.search_text {
                            "Find text"
                        } else {
                            "DE AD BE EF"
                        }),
                );
                if self.focus_find {
                    response.request_focus();
                    self.focus_find = false;
                }
                if ui.button("Search").clicked()
                    || (response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)))
                {
                    self.start_search(&source, ui.ctx());
                }
                if self.search_rx.is_some() && ui.small_button("Stop").clicked() {
                    self.cancel_search();
                    self.status = "Search stopped; found matches remain available.".into();
                }
                if ui
                    .add_enabled(
                        !self.search_matches.is_empty(),
                        egui::Button::new("Previous"),
                    )
                    .clicked()
                {
                    self.search_move(true);
                }
                if ui
                    .add_enabled(!self.search_matches.is_empty(), egui::Button::new("Next"))
                    .clicked()
                {
                    self.search_move(false);
                }
                if self.search_width > 0 {
                    let suffix = if self.search_matches.len() == MATCH_LIMIT {
                        "+ (limit)"
                    } else {
                        ""
                    };
                    ui.weak(format!(
                        "{}{suffix} matches · {:.0}%",
                        self.search_matches.len(),
                        100.0 * self.search_scanned as f64 / source.len().max(1) as f64
                    ));
                }
            });
        }
        if self.show_bookmark {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.bookmark_name)
                        .desired_width(140.0)
                        .hint_text("Range name"),
                );
                ui.color_edit_button_srgba(&mut self.bookmark_color);
                if ui.button("Save range").clicked() {
                    if let Some(range) = self.selection() {
                        let name = if self.bookmark_name.trim().is_empty() {
                            crate::format::byte_range(range.start as u64, range.end as u64)
                        } else {
                            self.bookmark_name.trim().to_owned()
                        };
                        self.bookmarks.push(HexHighlight {
                            range,
                            name,
                            color: self.bookmark_color,
                        });
                        self.show_bookmark = false;
                        self.bookmark_name.clear();
                    }
                }
                if ui.small_button("Cancel").clicked() {
                    self.show_bookmark = false;
                }
            });
        }
        if self.show_replace {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.replace_text, "Text");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.replacement)
                        .desired_width(200.0)
                        .hint_text(if self.replace_inserts { "Bytes to insert" } else { "Replacement bytes" }),
                );
                if self.focus_replace {
                    response.request_focus();
                    self.focus_replace = false;
                }
                let label = if self.replace_inserts { "Insert bytes" } else { "Overwrite bytes" };
                if ui.button(label).clicked() {
                    let result = if self.replace_inserts {
                        typed_bytes(&self.replacement, self.replace_text).map(|bytes| {
                            let offset = self.selection().map_or(0, |range| range.start);
                            (offset, bytes)
                        })
                    } else {
                        replacement_bytes(&self.replacement, self.replace_text, self.selection(), source.len())
                    };
                    match result {
                        Ok((offset, bytes)) => {
                            actions.push(if self.replace_inserts {
                                HexAction::Insert { offset, bytes }
                            } else {
                                HexAction::Replace { offset, bytes }
                            });
                            self.show_replace = false;
                            self.replacement.clear();
                        }
                        Err(error) => self.status = error,
                    }
                }
                if ui.small_button("Cancel").clicked() {
                    self.show_replace = false;
                }
            });
        }
        let highlights = shown_highlights;
        let selection = self.selection();
        if selection != navigation_origin {
            if let Some(range) = &selection {
                actions.push(HexAction::Selection(range.clone()));
            }
            ui.ctx().request_repaint();
        }
        ui.separator();
        if source.len() == 0 {
            ui.centered_and_justified(|ui| {
                ui.label("This file is empty.");
            });
            return actions;
        }
        self.bytes_ui(ui, &source, highlights, &mut actions);
        actions
    }

    /// The Hex card's one readout: `Cursor 0x6B · 16 bytes · entries[1].name`.
    fn readout(&self, ui: &mut egui::Ui, source: &ByteView, highlights: &[HexHighlight]) {
        ui.horizontal_wrapped(|ui| {
            if let Some(range) = self.selection() {
                ui.monospace(format!("Cursor 0x{:X} · {}", self.caret,
                    crate::format::plural(range.len(), "byte", "bytes")))
                    .on_hover_text(crate::format::byte_range(range.start, range.end));
            } else {
                ui.weak(format!("{} · select bytes to inspect", crate::format::size(source.len())))
                    .on_hover_text(crate::format::exact_bytes(source.len()));
            }
            if let Some(field) = narrowest_highlight(highlights.iter(), self.caret) {
                ui.weak("·");
                ui.label(RichText::new(short_field_name(&field.name)).monospace().color(field.color))
                    .on_hover_text(format!("{}\nDecoded field · {}\nField tint does not change your byte selection.",
                        field.name, crate::format::byte_range(field.range.start, field.range.end)));
            }
            if self.high_nibble.is_some() {
                ui.colored_label(crate::theme::DARK.color.status.warning, "Type the second hex digit");
            }
            if !self.status.is_empty() {
                ui.colored_label(crate::theme::DARK.color.status.warning, &self.status);
            }
        });
    }

    /// Overwrite | Insert. Clicking the active mode stops editing.
    fn edit_mode_switch(&mut self, ui: &mut egui::Ui) {
        let t = &crate::theme::DARK;
        // No vertical margin, so the switch lines up with the toolbar's buttons.
        egui::Frame::new().fill(t.color.surface.window).corner_radius(t.radius.control)
            .inner_margin(egui::Margin::symmetric(2, 0)).show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 0.;
                let overwrite = self.edit_mode && !self.insert_mode;
                if ui.add(egui::Button::selectable(overwrite, "Overwrite"))
                    .on_hover_text("Type hex digits (or text) over the selected bytes.")
                    .clicked()
                {
                    self.edit_mode = !overwrite;
                    self.insert_mode = false;
                    self.high_nibble = None;
                }
                let insert = self.edit_mode && self.insert_mode;
                if ui.add_enabled(self.can_resize, egui::Button::selectable(insert, "Insert"))
                    .on_hover_text("Type to insert bytes before the cursor. Delete and Backspace remove bytes; the Insert key switches modes.")
                    .on_disabled_hover_text("This decoder reads fixed offsets, so bytes can't be inserted or deleted.")
                    .clicked()
                {
                    self.edit_mode = !insert;
                    self.insert_mode = !insert;
                    self.high_nibble = None;
                }
            });
    }

    fn selection_menu(
        &mut self,
        ui: &mut egui::Ui,
        source: &ByteView,
        actions: &mut Vec<HexAction>,
    ) {
        ui.add_enabled_ui(self.selection().is_some(), |ui| {
            if ui
                .button(if self.python_mode {
                    "Copy Python read"
                } else {
                    "Describe selected bytes…"
                })
                .clicked()
            {
                actions.push(HexAction::Define(self.selection().unwrap()));
                ui.close();
            }
            ui.separator();
            if ui.button("Copy hex").clicked() {
                self.copy(source, Transfer::Hex);
                ui.close();
            }
            if ui.button("Copy text").clicked() {
                self.copy(source, Transfer::Text);
                ui.close();
            }
            if ui.button("Extract selection…").clicked() {
                actions.push(HexAction::Extract(self.selection().unwrap()));
                ui.close();
            }
            if ui.button("Replace bytes…").clicked() {
                self.show_replace();
                ui.close();
            }
            if ui.button("Name / color range…").clicked() {
                self.show_bookmark = true;
                ui.close();
            }
        });
        let resize_blocked = "This decoder reads fixed offsets, so bytes can't be inserted or deleted.";
        if ui.add_enabled(self.can_resize, egui::Button::new("Insert bytes…"))
            .on_hover_text("Insert bytes before the selection.")
            .on_disabled_hover_text(resize_blocked)
            .clicked()
        {
            self.show_insert();
            ui.close();
        }
        let selected = self.selection();
        let delete_label = selected.as_ref().map_or("Delete bytes".into(), |range| {
            format!("Delete {}", crate::format::exact_bytes(range.len() as u64))
        });
        if ui.add_enabled(self.can_resize && selected.is_some(), egui::Button::new(delete_label))
            .on_disabled_hover_text(if self.can_resize { "Select bytes to delete." } else { resize_blocked })
            .clicked()
        {
            actions.push(HexAction::Delete(selected.unwrap()));
            ui.close();
        }
        ui.separator();
        if ui.button("Undo byte edit").clicked() {
            actions.push(HexAction::Undo);
            ui.close();
        }
        if ui.button("Redo byte edit").clicked() {
            actions.push(HexAction::Redo);
            ui.close();
        }
    }

    fn bytes_ui(
        &mut self,
        ui: &mut egui::Ui,
        source: &ByteView,
        highlights: &[HexHighlight],
        actions: &mut Vec<HexAction>,
    ) {
        let theme = &crate::theme::DARK;
        let font = FontId::monospace(theme.font.hex);
        let char_width = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let row_height = theme.size.hex_row_height;
        let offset_chars = format!("{:X}", source.len()).len().max(8);
        let offset_width = (offset_chars + 2) as f32 * char_width;
        let cell_width = char_width * 3.0;
        // Keep the text column visually separate from the last hex byte at any DPI.
        let ascii_gutter = (char_width * 3.0).max(24.0);
        let row_width = |columns: usize| {
            offset_width + columns as f32 * (cell_width + char_width) + ascii_gutter + 8.0
        };
        // A decoder/plot selection can request a reveal while the user is already
        // pressing a byte in the currently visible grid. In that frame the click
        // must target the bytes under the pointer, not a newly scrolled viewport.
        let pointer_in_grid = ui.input(|input| {
            input.pointer.primary_down()
                && input.pointer.interact_pos().is_some_and(|position| {
                    ui.available_rect_before_wrap().contains(position)
                })
        });
        if pointer_in_grid {
            self.reveal = false;
            self.pending_scroll = None;
        }
        let total_rows = source.len().div_ceil(self.bytes_per_row);
        let caret_row = self.caret / self.bytes_per_row;
        let mut desired_scroll = self.pending_scroll.take();
        if self.reveal {
            self.window_start = window_start(caret_row, total_rows);
            desired_scroll = Some(
                ((caret_row - self.window_start) as f32 * row_height
                    - ui.available_height() * 0.35)
                    .max(0.0),
            );
            self.reveal = false;
        }
        self.window_start = self.window_start.min(total_rows.saturating_sub(1));
        let rows = WINDOW_ROWS.min(total_rows - self.window_start);
        let hex_end = offset_width + self.bytes_per_row as f32 * cell_width;
        let ascii_start = hex_end + ascii_gutter;
        let content_width = row_width(self.bytes_per_row);
        // Keep the header to the visible width: a wider allocation would widen
        // everything below it, drawing rows over a docked Inspector. It scrolls
        // horizontally with the grid instead.
        let (header, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_height),
            Sense::hover(),
        );
        let header_painter = ui.painter_at(header);
        let header_left = header.left() - self.horizontal_scroll;
        header_painter.text(
            egui::pos2(header_left, header.center().y),
            egui::Align2::LEFT_CENTER,
            "Offset",
            font.clone(),
            ui.visuals().weak_text_color(),
        );
        for column in 0..self.bytes_per_row {
            let cell = egui::Rect::from_min_size(
                egui::pos2(
                    header_left + offset_width + column as f32 * cell_width,
                    header.top(),
                ),
                egui::vec2(cell_width, row_height),
            );
            header_painter.text(
                cell.center(),
                egui::Align2::CENTER_CENTER,
                format!("{column:02X}"),
                font.clone(),
                ui.visuals().weak_text_color(),
            );
        }
        header_painter.text(
            egui::pos2(header_left + ascii_start, header.center().y),
            egui::Align2::LEFT_CENTER,
            "Text",
            font.clone(),
            ui.visuals().weak_text_color(),
        );
        header_painter.line_segment(
            [header.left_bottom(), header.right_bottom()],
            egui::Stroke::new(
                1.0_f32,
                ui.visuals().widgets.noninteractive.bg_stroke.color,
            ),
        );
        let id = ui.make_persistent_id("hex_byte_surface");
        let mut clicked_offset = None;
        let mut drag_offset = None;
        let mut focus_requested = false;
        let mut requested_page = None;
        let mut wants_context = false;
        let mut byte_tooltip = None;
        let mut scroll = egui::ScrollArea::both()
            .id_salt("hex_continuous_bytes")
            .auto_shrink([false, false]);
        if let Some(offset) = desired_scroll {
            scroll = scroll.vertical_scroll_offset(offset);
        }
        let old_spacing = ui.spacing().item_spacing.y;
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut visible_range = None;
        let output = scroll.show_rows(ui, row_height, rows, |ui, visible| {
            if !visible.is_empty() {
                visible_range = Some(
                    (self.window_start + visible.start) * self.bytes_per_row
                        ..((self.window_start + visible.end) * self.bytes_per_row).min(source.len()),
                );
            }
            ui.set_min_width(content_width);
            if let Some(row) = visible.clone().next() {
                requested_page = Some((self.window_start + row) * self.bytes_per_row);
            }
            for local_row in visible {
                let offset = (self.window_start + local_row) * self.bytes_per_row;
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(content_width.max(ui.available_width()), row_height),
                    Sense::click_and_drag(),
                );
                let painter = ui.painter_at(rect);
                let text_y = rect.center().y;
                if (self.window_start + local_row) % 2 == 0 {
                    painter.rect_filled(rect, 0.0, Color32::from_white_alpha(3));
                }
                let divider_x = rect.left() + hex_end + ascii_gutter * 0.5;
                painter.line_segment(
                    [
                        egui::pos2(divider_x, rect.top()),
                        egui::pos2(divider_x, rect.bottom()),
                    ],
                    egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
                );
                painter.text(
                    egui::pos2(rect.left(), text_y),
                    egui::Align2::LEFT_CENTER,
                    format!("{offset:0offset_chars$X}"),
                    font.clone(),
                    ui.visuals().weak_text_color(),
                );
                let pointer = response
                    .interact_pointer_pos()
                    .or_else(|| ui.input(|i| i.pointer.hover_pos()));
                let pointer_byte = pointer.filter(|p| rect.contains(*p)).and_then(|p| {
                    let x = p.x - rect.left();
                    let (column, ascii) = if x >= ascii_start {
                        (((x - ascii_start) / char_width).floor() as usize, true)
                    } else if x >= offset_width && x < hex_end {
                        (((x - offset_width) / cell_width).floor() as usize, false)
                    } else {
                        return None;
                    };
                    (column < self.bytes_per_row && offset + column < source.len())
                        .then_some((offset + column, ascii))
                });
                // Anchor on press, before egui reaches its drag threshold. Re-anchoring
                // at drag_started loses the initial bytes and makes selection jump.
                if response.hovered() && ui.input(|i| i.pointer.primary_pressed()) {
                    if let Some((byte, ascii)) = pointer_byte {
                        clicked_offset = Some(byte);
                        self.drag_selecting = true;
                        self.ascii_input = ascii;
                        ui.memory_mut(|m| m.request_focus(id));
                        focus_requested = true;
                    }
                }
                // A completed click gives egui focus to its row response; retain the
                // stable surface focus without moving the established selection anchor.
                if (response.clicked() || response.drag_started()) && pointer_byte.is_some() {
                    ui.memory_mut(|m| m.request_focus(id));
                    focus_requested = true;
                }
                if self.drag_selecting && ui.input(|i| i.pointer.primary_down()) {
                    if let Some((byte, _)) = pointer_byte {
                        drag_offset = Some(byte);
                    }
                }
                if response.double_clicked() {
                    self.edit_mode = true;
                }
                if response.secondary_clicked() {
                    if let Some((byte, _)) = pointer_byte {
                        if !self.selection().is_some_and(|range| range.contains(&byte)) {
                            clicked_offset = Some(byte);
                        }
                    }
                }
                response.context_menu(|ui| {
                    wants_context = true;
                    self.selection_menu(ui, source, actions);
                });
                for column in 0..self.bytes_per_row {
                    let index = offset + column;
                    if index >= source.len() {
                        break;
                    }
                    let hex_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            rect.left() + offset_width + column as f32 * cell_width,
                            rect.top(),
                        ),
                        egui::vec2(cell_width, row_height),
                    );
                    let ascii_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            rect.left() + ascii_start + column as f32 * char_width,
                            rect.top(),
                        ),
                        egui::vec2(char_width, row_height),
                    );
                    let selected = self.selection().is_some_and(|range| range.contains(&index));
                    let region =
                        narrowest_highlight(highlights.iter().chain(&self.bookmarks), index);
                    let match_index = self.search_matches.partition_point(|start| *start <= index);
                    let matched = match_index > 0
                        && index - self.search_matches[match_index - 1] < self.search_width;
                    let byte = self.byte(index);
                    let modified = byte.is_some_and(|(_, modified)| modified);
                    let style = byte_style(ui.visuals(), selected, region, matched, modified);
                    if let Some(fill) = style.fill {
                        // Adjacent bytes form one range in both columns, including hex spacing.
                        painter.rect_filled(hex_rect, 0.0, fill);
                        painter.rect_filled(ascii_rect, 0.0, fill);
                    }
                    if !selected && region.is_some_and(|h| h.range.start == index) {
                        let stroke = egui::Stroke::new(1.0_f32, region.unwrap().color);
                        painter.line_segment([hex_rect.left_top(), hex_rect.left_bottom()], stroke);
                        painter.line_segment(
                            [ascii_rect.left_top(), ascii_rect.left_bottom()],
                            stroke,
                        );
                    }
                    for cell in [hex_rect, ascii_rect] {
                        if let Some(color) = style.search_mark {
                            painter.line_segment(
                                [
                                    cell.left_bottom() - egui::vec2(0.0, 1.0),
                                    cell.right_bottom() - egui::vec2(0.0, 1.0),
                                ],
                                egui::Stroke::new(2.0_f32, color),
                            );
                        }
                        if let Some(color) = style.edit_mark {
                            painter.line_segment(
                                [
                                    cell.left_top() + egui::vec2(0.0, 1.0),
                                    cell.right_top() + egui::vec2(0.0, 1.0),
                                ],
                                egui::Stroke::new(2.0_f32, color),
                            );
                        }
                    }
                    let color = if !selected && byte.is_some_and(|(value, _)| value == 0) {
                        theme.color.text.zero_byte
                    } else {
                        style.text
                    };
                    let (hex, ascii) = if let Some((byte, _)) = byte {
                        (
                            format!("{byte:02X}"),
                            crate::byte_display::ascii(byte).to_string(),
                        )
                    } else {
                        ("··".into(), "·".into())
                    };
                    painter.text(
                        egui::pos2(hex_rect.left() + char_width * 0.25, text_y),
                        egui::Align2::LEFT_CENTER,
                        hex,
                        font.clone(),
                        color,
                    );
                    painter.text(
                        egui::pos2(ascii_rect.left(), text_y),
                        egui::Align2::LEFT_CENTER,
                        ascii,
                        font.clone(),
                        color,
                    );
                    if self.anchor.is_some() && index == self.caret {
                        painter.rect_stroke(
                            if self.ascii_input {
                                ascii_rect
                            } else {
                                hex_rect
                            },
                            0.0,
                            egui::Stroke::new(1.5_f32, ui.visuals().selection.stroke.color),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if pointer_byte.is_some_and(|(byte, _)| byte == index) {
                        byte_tooltip = Some((response.clone(), index, selected, matched));
                    }
                }
            }
        });
        self.visible_range = visible_range;
        // Menus can cover a different row from the one that opened them. Wait until
        // every row has handled its menu, then let egui check hover layers and delay.
        if !wants_context && !ui.input(|i| i.pointer.any_down()) {
            if let Some((response, index, selected, matched)) = byte_tooltip {
                response.on_hover_ui_at_pointer(|ui| {
                    ui.monospace(format!("0x{index:X} · byte {}", crate::format::count(index)));
                    if let Some(field) = narrowest_highlight(highlights.iter(), index) {
                        ui.colored_label(field.color, &field.name);
                        ui.weak(format!(
                            "Decoded field · {}",
                            crate::format::byte_range(field.range.start as u64, field.range.end as u64)
                        ));
                    }
                    for bookmark in self.bookmarks.iter().filter(|h| h.range.contains(&index)) {
                        ui.colored_label(bookmark.color, format!("Saved range: {}", bookmark.name));
                    }
                    if selected {
                        ui.label("Selected byte");
                    }
                    if matched {
                        ui.colored_label(SEARCH_COLOR, "Search match (gold underline)");
                    }
                    if let Some((byte, modified)) = self.byte(index) {
                        ui.label(format!("u8 {byte} · i8 {} · {byte:08b}", byte as i8));
                        if modified {
                            ui.colored_label(EDIT_COLOR, "Modified byte (orange top edge)");
                        }
                    }
                });
            }
        }
        self.horizontal_scroll = output.state.offset.x;
        // Recenter only near a local scroll boundary. Keep the same absolute
        // first visible row and fractional pixel position, so wheel scrolling
        // continues through arbitrarily large files without exposing paging.
        // Let a scrollbar drag finish before changing its local coordinate space.
        if !ui.input(|i| i.pointer.primary_down()) {
            if let Some((start, offset)) = recenter_scroll_window(
                self.window_start,
                total_rows,
                output.state.offset.y,
                row_height,
            ) {
                self.window_start = start;
                self.pending_scroll = Some(offset);
                ui.ctx().request_repaint();
            }
        }
        ui.spacing_mut().item_spacing.y = old_spacing;
        // Keep one stable focus target independently of the virtualized rows.
        ui.interact(output.inner_rect, id, Sense::focusable_noninteractive());
        if let Some(offset) = requested_page {
            self.request_page(source, offset, ui.ctx());
        }
        if let Some(offset) = clicked_offset {
            if !ui.input(|i| i.modifiers.shift) || self.anchor.is_none() {
                self.anchor = Some(offset);
            }
            self.caret = offset;
            self.high_nibble = None;
            actions.push(HexAction::Selection(self.selection().unwrap()));
        }
        if let Some(offset) = drag_offset {
            if self.anchor.is_some() && self.caret != offset {
                self.caret = offset;
                actions.push(HexAction::Selection(self.selection().unwrap()));
            }
        }
        if !ui.input(|i| i.pointer.primary_down()) {
            self.drag_selecting = false;
        }
        if focus_requested {
            ui.memory_mut(|m| m.request_focus(id));
        }
        self.focused = ui.memory(|m| m.has_focus(id));
        if self.focused && !wants_context {
            self.keyboard(ui, source, output.inner_rect.height(), row_height, actions);
        }
        if let Some(page) = &self.page {
            if let Err(error) = &page.result {
                self.status = error.clone();
            }
        }
    }

    fn keyboard(
        &mut self,
        ui: &mut egui::Ui,
        source: &ByteView,
        height: f32,
        row_height: f32,
        actions: &mut Vec<HexAction>,
    ) {
        let events = ui.input(|i| i.events.clone());
        let old = self.selection();
        let page = ((height / row_height).floor() as usize).max(1) * self.bytes_per_row;
        for event in events {
            match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.command {
                        match key {
                            Key::A => {
                                self.select_range(0..source.len(), false);
                            }
                            Key::C => self.copy(
                                source,
                                if self.ascii_input {
                                    Transfer::Text
                                } else {
                                    Transfer::Hex
                                },
                            ),
                            Key::F => {
                                self.show_find();
                            }
                            Key::Z => actions.push(if modifiers.shift {
                                HexAction::Redo
                            } else {
                                HexAction::Undo
                            }),
                            Key::Y => actions.push(HexAction::Redo),
                            Key::Home => self.move_caret(0, modifiers.shift),
                            Key::End => self.move_caret(source.len() - 1, modifiers.shift),
                            _ => {}
                        }
                        continue;
                    }
                    let target = match key {
                        Key::ArrowLeft => Some(self.caret.saturating_sub(1)),
                        Key::ArrowRight => Some((self.caret + 1).min(source.len() - 1)),
                        Key::ArrowUp => Some(self.caret.saturating_sub(self.bytes_per_row)),
                        Key::ArrowDown => Some(
                            self.caret
                                .saturating_add(self.bytes_per_row)
                                .min(source.len() - 1),
                        ),
                        Key::PageUp => Some(self.caret.saturating_sub(page)),
                        Key::PageDown => {
                            Some(self.caret.saturating_add(page).min(source.len() - 1))
                        }
                        Key::Home => Some(self.caret / self.bytes_per_row * self.bytes_per_row),
                        Key::End => Some(
                            ((self.caret / self.bytes_per_row + 1) * self.bytes_per_row - 1)
                                .min(source.len() - 1),
                        ),
                        Key::Escape => {
                            self.edit_mode = false;
                            self.high_nibble = None;
                            None
                        }
                        Key::Insert if self.can_resize => {
                            self.insert_mode = !self.edit_mode || !self.insert_mode;
                            self.edit_mode = true;
                            self.high_nibble = None;
                            None
                        }
                        Key::Delete | Key::Backspace if self.edit_mode && self.can_resize => {
                            if let Some(range) = self.selection() {
                                let range = if range.len() > 1 {
                                    Some(range)
                                } else if key == Key::Delete {
                                    Some(self.caret..self.caret + 1)
                                } else {
                                    self.caret.checked_sub(1).map(|before| before..self.caret)
                                };
                                if let Some(range) = range {
                                    actions.push(HexAction::Delete(range));
                                }
                            }
                            self.high_nibble = None;
                            None
                        }
                        _ => None,
                    };
                    if let Some(target) = target {
                        self.move_caret(target, modifiers.shift);
                    }
                }
                egui::Event::Text(text) if self.edit_mode && self.insert_mode && self.anchor.is_some() => {
                    // The host shifts the cursor past the inserted bytes once they land.
                    let bytes = if self.ascii_input {
                        text.into_bytes()
                    } else {
                        let mut bytes = vec![];
                        for nibble in text.chars().filter_map(|c| c.to_digit(16).map(|n| n as u8)) {
                            match self.high_nibble.take() {
                                Some(high) => bytes.push(high * 16 + nibble),
                                None => self.high_nibble = Some(nibble),
                            }
                        }
                        bytes
                    };
                    if !bytes.is_empty() {
                        actions.push(HexAction::Insert { offset: self.caret, bytes });
                    }
                }
                egui::Event::Text(text) if self.edit_mode && self.anchor.is_some() => {
                    if self.ascii_input {
                        let bytes = text.into_bytes();
                        if !bytes.is_empty() && bytes.len() <= source.len() - self.caret {
                            let next = self.caret.saturating_add(bytes.len()).min(source.len() - 1);
                            actions.push(HexAction::Replace {
                                offset: self.caret,
                                bytes,
                            });
                            self.move_caret(next, false);
                        }
                    } else {
                        for nibble in text.chars().filter_map(|c| c.to_digit(16).map(|n| n as u8)) {
                            if let Some(high) = self.high_nibble.take() {
                                actions.push(HexAction::Replace {
                                    offset: self.caret,
                                    bytes: vec![high * 16 + nibble],
                                });
                                self.move_caret((self.caret + 1).min(source.len() - 1), false);
                            } else {
                                self.high_nibble = Some(nibble);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if self.selection() != old {
            if let Some(range) = self.selection() {
                actions.push(HexAction::Selection(range));
            }
        }
    }

    fn move_caret(&mut self, caret: usize, extend: bool) {
        if !extend || self.anchor.is_none() {
            self.anchor = Some(caret);
        }
        self.caret = caret;
        self.reveal = true;
        self.high_nibble = None;
    }

    /// Width of the byte grid (offsets, bytes and Text column) plus its scroll bar,
    /// so a docked Inspector can start right after the Text column.
    pub fn grid_width(&self, ui: &egui::Ui, len: usize) -> f32 {
        let theme = &crate::theme::DARK;
        let char_width = ui.fonts_mut(|f| f.glyph_width(&FontId::monospace(theme.font.hex), '0'));
        let offset_width = (format!("{len:X}").len().max(8) + 2) as f32 * char_width;
        let ascii_gutter = (char_width * 3.0).max(24.0);
        offset_width + self.bytes_per_row as f32 * (char_width * 4.0) + ascii_gutter + 8.0
            + ui.spacing().scroll.allocated_width()
    }

    pub fn name_selected_range(&mut self) {
        self.show_bookmark = true;
    }

    /// The Inspector's "Selected bytes" section. `byte_menu` adds its ⋯ menu of
    /// byte actions, for selections without a decoded field.
    pub fn ui_inspector(&mut self, ui: &mut egui::Ui, source: Option<ByteView>, default_big: bool,
        byte_menu: bool) -> Vec<HexAction> {
        let mut actions = Vec::new();
        let Some(source) = source else {
            return actions;
        };
        self.poll(ui, &source);
        let Some(range) = self.selection() else {
            return actions;
        };
        let preview_range = range.start..range.end.min(range.start.saturating_add(64));
        if self.selection_rx.is_none()
            && !self
                .selection_bytes
                .as_ref()
                .is_some_and(|(cached, revision, _)| {
                    *cached == preview_range && *revision == source.revision
                })
        {
            let (tx, rx) = mpsc::channel();
            self.selection_rx = Some(rx);
            let source = source.clone();
            let range = preview_range.clone();
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                let result = source
                    .read(range.start, range.len())
                    .map_err(|e| e.to_string());
                let _ = tx.send((range, source.revision, result));
                ctx.request_repaint();
            });
        }
        let preview = self.selection_bytes.as_ref()
            .filter(|(cached, revision, _)| *cached == preview_range && *revision == source.revision)
            .map(|(_, _, bytes)| bytes.clone());
        let python = self.python_mode;
        let (mut name_range, mut copy) = (false, false);
        let mut menu = |ui: &mut egui::Ui| {
            if ui.button(if python { "Copy Python read" } else { "Describe bytes…" }).clicked() {
                actions.push(HexAction::Define(range.clone()));
                ui.close();
            }
            if ui.button("Name / color range…").clicked() {
                name_range = true;
                ui.close();
            }
            if ui.button("Copy hex").clicked() {
                copy = true;
                ui.close();
            }
            if ui.button("Export bytes…").clicked() {
                actions.push(HexAction::Extract(range.clone()));
                ui.close();
            }
        };
        crate::workspace_inspector::selected_bytes(ui, range.start as u64..range.end as u64,
            preview.as_deref(), default_big, byte_menu.then_some(&mut menu as &mut dyn FnMut(&mut egui::Ui)));
        self.show_bookmark |= name_range;
        if copy {
            self.copy(&source, Transfer::Hex);
        }
        actions
    }
}

fn parse_jump(input: &str, cursor: usize, len: usize) -> Result<usize, String> {
    let input = input.trim();
    let Some(direction) = input.chars().next().filter(|c| matches!(c, '+' | '-')) else {
        return parse_offset(input, len);
    };
    if len == 0 {
        return Err("This file is empty.".into());
    }
    let amount = parse_byte_number(input[1..].trim())?;
    let offset = if direction == '+' {
        cursor.checked_add(amount)
    } else {
        cursor.checked_sub(amount)
    };
    offset.filter(|offset| *offset < len).ok_or_else(|| {
        format!(
            "That move is outside the file. Valid offsets are 0x0–0x{:X}.",
            len - 1
        )
    })
}

fn parse_byte_number(input: &str) -> Result<usize, String> {
    let digits = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"));
    if let Some(hex) = digits {
        if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return usize::from_str_radix(hex, 16)
                .map_err(|_| "The byte number is too large.".into());
        }
    } else if !input.is_empty() && input.bytes().all(|b| b.is_ascii_digit()) {
        return input
            .parse()
            .map_err(|_| "The byte number is too large.".into());
    }
    Err("Enter an offset (256 or 0x100), or a signed byte move (+256 or -0x10).".into())
}

fn parse_offset(input: &str, len: usize) -> Result<usize, String> {
    if len == 0 {
        return Err("This file is empty.".into());
    }
    let offset = parse_byte_number(input.trim())?;
    if offset >= len {
        return Err(format!(
            "Offset is outside the file. Valid offsets are 0x0–0x{:X}.",
            len - 1
        ));
    }
    Ok(offset)
}

pub(crate) fn recenter_scroll_window(
    start: usize,
    total_rows: usize,
    scroll_y: f32,
    row_height: f32,
) -> Option<(usize, f32)> {
    if total_rows <= WINDOW_ROWS {
        return None;
    }
    let local_row = (scroll_y / row_height).floor() as usize;
    let first_row = start.saturating_add(local_row).min(total_rows - 1);
    let edge = WINDOW_ROWS / 8;
    if (local_row < edge && start > 0)
        || (local_row > WINDOW_ROWS - edge && start + WINDOW_ROWS < total_rows)
    {
        let next_start = window_start(first_row, total_rows);
        if next_start != start {
            let sub_row = scroll_y - local_row as f32 * row_height;
            return Some((
                next_start,
                (first_row - next_start) as f32 * row_height + sub_row,
            ));
        }
    }
    None
}

fn parse_hex(input: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .collect();
    if cleaned.len() % 2 != 0 {
        return Err("Enter complete hexadecimal byte pairs, for example 00 A4 FF.".into());
    }
    if !cleaned.is_ascii() {
        return Err("Use hexadecimal digits 0–9 and A–F.".into());
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&cleaned[i..i + 2], 16)
                .map_err(|_| "Use hexadecimal digits 0–9 and A–F.".to_owned())
        })
        .collect()
}

fn typed_bytes(input: &str, text: bool) -> Result<Vec<u8>, String> {
    let bytes = if text { input.as_bytes().to_vec() } else { parse_hex(input)? };
    if bytes.is_empty() {
        return Err("Enter bytes to insert.".into());
    }
    if bytes.len() > COPY_LIMIT {
        return Err("One insert is limited to 1 MiB.".into());
    }
    Ok(bytes)
}

fn replacement_bytes(
    input: &str,
    text: bool,
    selection: Option<Range<usize>>,
    len: usize,
) -> Result<(usize, Vec<u8>), String> {
    let selection = selection.ok_or("Select bytes to replace.")?;
    let bytes = if text {
        input.as_bytes().to_vec()
    } else {
        parse_hex(input)?
    };
    if bytes.is_empty() {
        return Err("Enter replacement bytes.".into());
    }
    if bytes.len() > COPY_LIMIT {
        return Err("One replacement is limited to 1 MiB.".into());
    }
    if bytes.len() != selection.len() {
        return Err(format!(
            "Select exactly {} to overwrite; {} selected.",
            crate::format::exact_bytes(bytes.len() as u64),
            crate::format::exact_bytes(selection.len() as u64)
        ));
    }
    if selection.end > len {
        return Err("Replacement is outside the file.".into());
    }
    Ok((selection.start, bytes))
}

/// `sections[1].payload.entries[1].name` → `entries[1].name`: from the innermost collection on.
fn short_field_name(name: &str) -> &str {
    name.rfind('[')
        .and_then(|bracket| name[..bracket].rfind('.'))
        .map_or(name, |dot| &name[dot + 1..])
}

pub(crate) fn window_start(caret_row: usize, total_rows: usize) -> usize {
    if total_rows <= WINDOW_ROWS {
        0
    } else {
        caret_row
            .saturating_sub(WINDOW_ROWS / 2)
            .min(total_rows.saturating_sub(WINDOW_ROWS))
    }
}

fn find_in_chunk(
    bytes: &[u8],
    pattern: &[u8],
    base: usize,
    owned_len: usize,
    matches: &mut Vec<usize>,
) {
    if pattern.is_empty() {
        return;
    }
    for (index, window) in bytes.windows(pattern.len()).enumerate().take(owned_len) {
        if matches.len() >= MATCH_LIMIT {
            break;
        }
        if window == pattern {
            matches.push(base + index);
        }
    }
}

const SEARCH_COLOR: Color32 = Color32::from_rgb(235, 197, 84);
const EDIT_COLOR: Color32 = Color32::from_rgb(255, 159, 75);

fn narrowest_highlight<'a>(
    highlights: impl Iterator<Item = &'a HexHighlight>,
    offset: usize,
) -> Option<&'a HexHighlight> {
    highlights
        .filter(|h| h.range.contains(&offset))
        .min_by_key(|h| h.range.len())
}

struct ByteStyle {
    fill: Option<Color32>,
    text: Color32,
    search_mark: Option<Color32>,
    edit_mark: Option<Color32>,
}

fn byte_style(
    visuals: &egui::Visuals,
    selected: bool,
    field: Option<&HexHighlight>,
    matched: bool,
    modified: bool,
) -> ByteStyle {
    ByteStyle {
        fill: if selected {
            Some(visuals.selection.bg_fill)
        } else {
            field.map(|field| {
                Color32::from_rgba_unmultiplied(
                    field.color.r(),
                    field.color.g(),
                    field.color.b(),
                    38,
                )
            })
        },
        text: if selected {
            visuals.selection.stroke.color
        } else {
            visuals.text_color()
        },
        search_mark: matched.then_some(SEARCH_COLOR),
        edit_mark: modified.then_some(EDIT_COLOR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ByteView) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.unknown");
        std::fs::write(&path, vec![0x41; 128 * 1024]).unwrap();
        let file = Arc::new(crate::file_source::FileSource::open(&path).unwrap());
        (dir, crate::byte_store::ByteStore::from_file(file).view())
    }

    #[test]
    fn overlapping_fields_use_the_narrowest_range_without_changing_selection_style() {
        let sample = HexHighlight {
            range: 0..128,
            name: "sample".into(),
            color: Color32::RED,
        };
        let field = HexHighlight {
            range: 4..8,
            name: "temperature".into(),
            color: Color32::GREEN,
        };
        assert_eq!(
            narrowest_highlight([&sample, &field].into_iter(), 5)
                .unwrap()
                .name,
            "temperature"
        );
        assert_eq!(
            narrowest_highlight([&field, &sample].into_iter(), 5)
                .unwrap()
                .name,
            "temperature"
        );
        assert_eq!(
            narrowest_highlight([&sample, &field].into_iter(), 8)
                .unwrap()
                .name,
            "sample"
        );
        assert!(narrowest_highlight([&sample, &field].into_iter(), 128).is_none());
        for visuals in [egui::Visuals::dark(), egui::Visuals::light()] {
            let style = byte_style(&visuals, true, Some(&field), true, true);
            assert_eq!(style.fill, Some(visuals.selection.bg_fill));
            assert_eq!(style.text, visuals.selection.stroke.color);
            assert_eq!(style.search_mark, Some(SEARCH_COLOR));
            assert_eq!(style.edit_mark, Some(EDIT_COLOR));
            let field_style = byte_style(&visuals, false, Some(&field), false, false);
            assert_eq!(field_style.fill.unwrap().a(), 38);
            assert_eq!(field_style.text, visuals.text_color());
        }
    }

    #[test]
    fn drag_keeps_the_pressed_byte_as_anchor_across_semantic_highlights() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        let highlights = [
            HexHighlight {
                range: 0..64,
                name: "sample".into(),
                color: Color32::RED,
            },
            HexHighlight {
                range: 6..8,
                name: "value".into(),
                color: Color32::GREEN,
            },
        ];
        let frame = |editor: &mut HexEditor, events: Vec<egui::Event>| {
            let mut actions = Vec::new();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        actions = editor.ui(ui, Some(source.clone()), &highlights);
                    });
                },
            );
            (output, actions)
        };
        let (output, _) = frame(&mut editor, vec![]);
        let row = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "00000000" => Some(text.pos),
                _ => None,
            })
            .unwrap();
        let width = ctx.fonts_mut(|f| f.glyph_width(&FontId::monospace(crate::theme::DARK.font.hex), '0'));
        let at_byte = |index: usize| row + egui::vec2(width * (11.5 + index as f32 * 3.0), 6.5);
        let press = at_byte(3);
        frame(
            &mut editor,
            vec![
                egui::Event::PointerMoved(press),
                egui::Event::PointerButton {
                    pos: press,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
            ],
        );
        assert_eq!(
            editor.selection(),
            Some(3..4),
            "selection anchors on press, before drag threshold"
        );
        let (_, actions) = frame(&mut editor, vec![egui::Event::PointerMoved(at_byte(12))]);
        assert_eq!(editor.selection(), Some(3..13));
        assert!(actions
            .iter()
            .any(|action| matches!(action, HexAction::Selection(range) if range == &(3..13))));
        frame(&mut editor, vec![egui::Event::PointerMoved(at_byte(1))]);
        assert_eq!(editor.selection(), Some(1..4));
        frame(
            &mut editor,
            vec![egui::Event::PointerButton {
                pos: at_byte(1),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
        );
        assert_eq!(editor.selection(), Some(1..4));
        assert!(!editor.drag_selecting);
    }

    #[test]
    fn byte_tooltips_yield_to_context_menus_and_return_after_menu_actions() {
        let (_dir, source) = fixture();
        // Exercise menus below and above their originating row. The latter visits
        // covered rows before it renders the menu that must suppress their tooltip.
        for near_bottom in [false, true] {
            let ctx = egui::Context::default();
            ctx.global_style_mut(|style| {
                style.interaction.tooltip_delay = 0.0;
                style.interaction.show_tooltips_only_when_still = false;
            });
            let mut editor = HexEditor::default();
            let time = std::cell::Cell::new(0.0);
            let frame = |editor: &mut HexEditor, events: Vec<egui::Event>| {
                time.set(time.get() + 0.2);
                let mut actions = Vec::new();
                let output = ctx.run(
                    egui::RawInput {
                        time: Some(time.get()),
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(900.0, 700.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |_ctx, ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            actions = editor.ui(ui, Some(source.clone()), &[]);
                        });
                    },
                );
                (output, actions)
            };
            let text_pos = |output: &egui::FullOutput, label: &str| {
                output.shapes.iter().find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => Some(text.pos),
                    _ => None,
                })
            };
            let has_tooltip = |output: &egui::FullOutput| {
                output.shapes.iter().any(|shape| match &shape.shape {
                    egui::Shape::Text(text) => {
                        text.galley.text().starts_with("0x")
                            && text.galley.text().contains(" · byte ")
                    }
                    _ => false,
                })
            };
            let pointer_button = |pos, button, pressed| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: Default::default(),
            };
            let (output, _) = frame(&mut editor, vec![]);
            let rows: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text)
                        if text.galley.text().len() == 8
                            && usize::from_str_radix(text.galley.text(), 16).is_ok() =>
                    {
                        Some(text.pos)
                    }
                    _ => None,
                })
                .collect();
            let width = ctx.fonts_mut(|f| f.glyph_width(&FontId::monospace(13.0), '0'));
            let at_byte = |row| rows[row] + egui::vec2(width * 20.5, 6.5);
            let low_row = rows.len() - 3;
            let origin = at_byte(if near_bottom { low_row } else { 0 });
            let outside = at_byte(if near_bottom { 0 } else { low_row });
            frame(&mut editor, vec![egui::Event::PointerMoved(origin)]);
            let (output, _) = frame(&mut editor, vec![]);
            assert!(
                has_tooltip(&output),
                "normal byte hover still shows details"
            );

            let (output, _) = frame(
                &mut editor,
                vec![pointer_button(origin, egui::PointerButton::Secondary, true)],
            );
            assert!(!has_tooltip(&output), "hide details as right-click starts");
            let (output, _) = frame(
                &mut editor,
                vec![pointer_button(
                    origin,
                    egui::PointerButton::Secondary,
                    false,
                )],
            );
            assert!(!has_tooltip(&output), "hide details in the opening frame");
            let (output, _) = frame(&mut editor, vec![]);
            let menu_point = text_pos(&output, "Copy hex").unwrap() + egui::vec2(4.0, 4.0);
            assert!((menu_point.y - origin.y).abs() > 21.0);
            let (output, _) = frame(&mut editor, vec![egui::Event::PointerMoved(menu_point)]);
            assert!(!has_tooltip(&output), "a menu item covers another byte row");
            let (output, _) = frame(&mut editor, vec![]);
            assert!(text_pos(&output, "Copy hex").is_some());
            assert!(!has_tooltip(&output));
            let (output, _) = frame(&mut editor, vec![egui::Event::PointerMoved(outside)]);
            assert!(text_pos(&output, "Copy hex").is_some());
            assert!(
                !has_tooltip(&output),
                "keep details hidden outside an open menu"
            );

            let action_point = text_pos(&output, "Undo byte edit").unwrap() + egui::vec2(4.0, 4.0);
            frame(
                &mut editor,
                vec![
                    egui::Event::PointerMoved(action_point),
                    pointer_button(action_point, egui::PointerButton::Primary, true),
                ],
            );
            let (_, actions) = frame(
                &mut editor,
                vec![pointer_button(
                    action_point,
                    egui::PointerButton::Primary,
                    false,
                )],
            );
            assert!(actions
                .iter()
                .any(|action| matches!(action, HexAction::Undo)));
            frame(&mut editor, vec![egui::Event::PointerMoved(outside)]);
            let mut output = frame(&mut editor, vec![]).0;
            for _ in 0..3 {
                if has_tooltip(&output) { break; }
                output = frame(&mut editor, vec![]).0;
            }
            assert!(text_pos(&output, "Copy hex").is_none());
            assert!(has_tooltip(&output), "details return after the menu closes");
        }
    }

    #[test]
    fn selected_bytes_render_as_contiguous_bands_in_hex_and_ascii() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        editor.select_range(2..6, false);
        editor.revision = Some(source.revision);
        editor.search_matches = vec![2];
        editor.search_width = 4;
        editor.page = Some(PageData {
            offset: 0,
            revision: source.revision,
            result: Ok((vec![0x42; 1024], vec![true; 1024])),
        });
        let field = HexHighlight {
            range: 0..8,
            name: "sample.value".into(),
            color: Color32::RED,
        };
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 700.0),
                )),
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    editor.ui(ui, Some(source.clone()), &[field.clone()]);
                });
            },
        );
        let fill = ctx.global_style().visuals.selection.bg_fill;
        let mut bands: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect)
                    if rect.fill == fill && (rect.rect.height() - crate::theme::DARK.size.hex_row_height).abs() < 0.1 =>
                {
                    Some(rect.rect)
                }
                _ => None,
            })
            .collect();
        bands.sort_by(|a, b| a.left().total_cmp(&b.left()));
        assert_eq!(bands.len(), 8, "four selected bytes in each column");
        for band in bands.chunks_exact(4) {
            for pair in band.windows(2) {
                assert!((pair[0].right() - pair[1].left()).abs() < 0.01);
            }
        }
        assert_eq!(editor.selection(), Some(2..6));
    }

    fn hex_frame(ctx: &egui::Context, editor: &mut HexEditor, source: &ByteView, highlights: &[HexHighlight])
        -> egui::FullOutput {
        ctx.run(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 600.0))),
            ..Default::default()
        }, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| { editor.ui(ui, Some(source.clone()), highlights); });
        })
    }

    #[test]
    fn hex_card_has_one_readout_above_a_single_toolbar_row() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut editor = HexEditor::default();
        editor.select_range(0x60..0x70, false);
        let field = HexHighlight { range: 0x60..0x70, name: "sections[0].payload.entries[1].name".into(),
            color: Color32::LIGHT_BLUE };
        let _ = hex_frame(&ctx, &mut editor, &source, std::slice::from_ref(&field));
        let output = hex_frame(&ctx, &mut editor, &source, std::slice::from_ref(&field));
        let texts: Vec<(String, egui::Rect)> = output.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some((text.galley.text().to_owned(), text.galley.rect.translate(text.pos.to_vec2()))),
            _ => None,
        }).collect();
        let find = |label: &str| texts.iter().find(|(text, _)| text == label).map(|(_, rect)| *rect)
            .unwrap_or_else(|| panic!("missing {label} in {:?}", texts.iter().map(|(t, _)| t).collect::<Vec<_>>()));
        let readouts: Vec<_> = texts.iter().filter(|(text, _)| text.starts_with("Cursor")).collect();
        assert_eq!(readouts.len(), 1, "one cursor readout: {readouts:?}");
        assert_eq!(readouts[0].0, "Cursor 0x6F · 16 bytes");
        find("entries[1].name");
        assert!(!texts.iter().any(|(text, _)| text == "Go"), "Enter jumps; there is no Go button");
        assert!(!texts.iter().any(|(text, _)| text.contains('⋯')), "⋯ is painted, not a missing glyph");
        let row = find("Find").center().y;
        for label in ["‹", "›", "Overwrite", "Insert"] {
            assert!((find(label).center().y - row).abs() < 2., "{label} is not on the toolbar row: {:?} vs {row}", find(label));
        }
        assert!(readouts[0].1.bottom() <= find("Find").top(), "the readout heads the card");
    }

    #[test]
    fn rows_hold_sixteen_bytes_unless_thirty_two_is_chosen() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        hex_frame(&ctx, &mut editor, &source, &[]);
        assert_eq!(editor.bytes_per_row, 16);
        ctx.data_mut(|data| data.insert_persisted(egui::Id::new("hex_32_bytes_per_row"), true));
        hex_frame(&ctx, &mut editor, &source, &[]);
        assert_eq!(editor.bytes_per_row, 32);
    }

    #[test]
    fn fixed_width_rows_stay_left_of_a_docked_inspector_that_starts_after_the_text_column() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        editor.bytes_per_row = 16;
        // A selection inside a named field makes the long cursor readout appear.
        editor.select_range(0x18..0x20, false);
        let field = HexHighlight { range: 0x18..0x20, name: "sections[2].payload.records[104].chirp".into(),
            color: Color32::LIGHT_BLUE };
        // Narrow: the Inspector keeps its minimum width and Hex scrolls under it.
        // Wide: the Inspector grows so it starts right after the Text column.
        for width in [740.0, 1400.0] {
            let mut frame = || ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 600.0))),
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let grid = editor.grid_width(ui, source.len());
                        crate::workspace_inspector::docked_panel(ui, "inspector", &mut (), grid,
                            |_, ui: &mut egui::Ui| { ui.label("Inspector"); }, |_, ui| {
                            // Inspector values can be wider than the panel, e.g. a full-precision f32.
                            ui.add(egui::Label::new(egui::RichText::new("f32  -27110105000000000000000000000000000000.0").monospace())
                                .wrap_mode(egui::TextWrapMode::Extend));
                        });
                        editor.ui(ui, Some(source.clone()), std::slice::from_ref(&field));
                    });
                },
            );
            let mut output = frame();
            for _ in 0..50 {
                if output.shapes.iter().any(|shape| matches!(&shape.shape,
                    egui::Shape::Text(text) if text.galley.text() == "00000000")) { break; }
                std::thread::sleep(std::time::Duration::from_millis(10));
                output = frame();
            }
            let text_rects: Vec<_> = output.shapes.iter().filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some((text.galley.text().to_owned(),
                    text.galley.rect.translate(text.pos.to_vec2()).intersect(shape.clip_rect))),
                _ => None,
            }).collect();
            let inspector = text_rects.iter().find(|(text, _)| text == "Inspector").unwrap().1.left();
            let overlapping: Vec<_> = text_rects.iter()
                .filter(|(text, rect)| text != "Inspector" && !text.starts_with("f32")
                    && rect.is_positive() && rect.right() > inspector)
                .map(|(text, _)| text.clone()).collect();
            assert!(overlapping.is_empty(), "hex text drawn over the inspector: {overlapping:?}");
            if width > 1000.0 {
                // The Text column is the last one drawn in each row: one glyph per byte.
                let text_column = text_rects.iter()
                    .filter(|(text, rect)| text.chars().count() == 1 && rect.is_positive() && rect.right() < inspector)
                    .map(|(_, rect)| rect.right()).fold(f32::MIN, f32::max);
                let gap = inspector - text_column;
                assert!(gap < 48.0, "gap between the Text column and the Inspector: {gap}");
            }
        }
    }
    #[test]
    fn rendered_view_fills_height_and_reads_a_bounded_page_asynchronously() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 800.0),
                )),
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    editor.ui(ui, Some(source.clone()), &[]);
                });
            },
        );
        let offsets: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text().len() == 8 => {
                    usize::from_str_radix(text.galley.text(), 16).ok()
                }
                _ => None,
            })
            .collect();
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"Offset"));
        assert!(labels.contains(&"00"));
        assert!(labels.contains(&"0F"));
        assert!(
            offsets.iter().copied().max().unwrap() > 256,
            "the panel must show more than a fixed 256-byte page"
        );
        assert!(
            output.shapes.len() < 10_000,
            "offscreen rows must not be rendered"
        );
        let page = editor
            .page_rx
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let (bytes, edited) = page.result.unwrap();
        assert_eq!(bytes.len(), PAGE * 2);
        assert_eq!(edited, vec![false; PAGE * 2]);
    }

    #[test]
    fn decoded_selection_only_scrolls_when_the_target_is_offscreen() {
        let mut editor = HexEditor::default();
        editor.visible_range = Some(64..192);
        editor.select_range(96..100, true);
        assert_eq!(editor.selection(), Some(96..100));
        assert!(!editor.reveal, "a visible selection must not move the viewport");

        editor.select_range(512..516, true);
        assert_eq!(editor.selection(), Some(512..516));
        assert!(editor.reveal, "an offscreen selection must be revealed");
    }

    #[test]
    fn clicking_a_virtualized_byte_keeps_keyboard_focus_across_frames() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0));
        let frame = |editor: &mut HexEditor, events: Vec<egui::Event>| {
            let mut actions = Vec::new();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        actions = editor.ui(ui, Some(source.clone()), &[]);
                    });
                },
            );
            (output, actions)
        };
        let (output, _) = frame(&mut editor, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "00000000" => Some(text.pos),
                _ => None,
            })
            .unwrap();
        let width = ctx.fonts_mut(|f| f.glyph_width(&FontId::monospace(13.0), '0'));
        let pos = pos + egui::vec2(width * 11.5, 6.5);
        frame(
            &mut editor,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
            ],
        );
        frame(
            &mut editor,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
        );
        assert_eq!(editor.selection(), Some(0..1));
        assert!(editor.focused);
        editor.edit_mode = true;
        let (_, actions) = frame(&mut editor, vec![egui::Event::Text("AB".into())]);
        assert!(actions.iter().any(
            |action| matches!(action, HexAction::Replace { offset: 0, bytes } if bytes == &[0xAB])
        ));
    }

    #[test]
    fn clicking_visible_byte_cancels_pending_external_reveal_before_hit_testing() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        let frame = |editor: &mut HexEditor, events: Vec<egui::Event>| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        editor.ui(ui, Some(source.clone()), &[]);
                    });
                },
            )
        };
        let initial = frame(&mut editor, vec![]);
        let address = initial.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "00000000" => Some(text.pos),
            _ => None,
        }).expect("first byte row");
        let width = ctx.fonts_mut(|fonts| fonts.glyph_width(&FontId::monospace(13.0), '0'));
        let byte = address + egui::vec2(width * 11.5, 6.5);
        editor.select_range(8192..8193, true);
        assert!(editor.reveal);
        frame(&mut editor, vec![
            egui::Event::PointerMoved(byte),
            egui::Event::PointerButton {
                pos: byte,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ]);
        assert_eq!(editor.selection(), Some(0..1));
        assert!(!editor.reveal);
        let settled = frame(&mut editor, vec![egui::Event::PointerButton {
            pos: byte,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }]);
        assert!(settled.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "00000000")));
    }

    #[test]
    fn keyboard_overwrites_only_after_complete_hex_pair_and_keeps_ascii_bytes() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        editor.select_range(5..6, false);
        editor.edit_mode = true;
        let mut actions = Vec::new();
        for text in ["A", "5"] {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Text(text.into())],
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        editor.keyboard(ui, &source, 500.0, 21.0, &mut actions)
                    });
                },
            );
            if text == "A" {
                assert!(actions.is_empty());
            }
        }
        assert!(matches!(&actions[0], HexAction::Replace { offset: 5, bytes } if bytes == &[0xA5]));
        assert_eq!(editor.selection(), Some(6..7));
        actions.clear();
        editor.ascii_input = true;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Text("é".into())],
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    editor.keyboard(ui, &source, 500.0, 21.0, &mut actions)
                });
            },
        );
        assert!(
            matches!(&actions[0], HexAction::Replace { offset: 6, bytes } if bytes == "é".as_bytes())
        );
        assert_eq!(editor.selection(), Some(8..9));
    }

    #[test]
    fn insert_mode_inserts_typed_bytes_and_delete_keys_remove_bytes() {
        let (_dir, source) = fixture();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        let mut press = |editor: &mut HexEditor, event: egui::Event| {
            let mut actions = Vec::new();
            let _ = ctx.run(egui::RawInput { events: vec![event], ..Default::default() }, |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| editor.keyboard(ui, &source, 500.0, 21.0, &mut actions));
            });
            actions
        };
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false,
            modifiers: egui::Modifiers::NONE };
        editor.select_range(5..6, false);
        assert!(press(&mut editor, key(Key::Delete)).is_empty(), "Delete needs an edit mode");
        press(&mut editor, key(Key::Insert));
        assert!(editor.edit_mode && editor.insert_mode);
        assert!(press(&mut editor, egui::Event::Text("A".into())).is_empty());
        let actions = press(&mut editor, egui::Event::Text("5".into()));
        assert!(matches!(&actions[..], [HexAction::Insert { offset: 5, bytes }] if bytes == &[0xA5]));
        editor.ascii_input = true;
        let actions = press(&mut editor, egui::Event::Text("hi".into()));
        assert!(matches!(&actions[..], [HexAction::Insert { offset: 5, bytes }] if bytes == b"hi"));
        editor.shift(5, 0, 2);
        assert_eq!(editor.selection(), Some(7..8), "the cursor stays on the byte it was on");
        let actions = press(&mut editor, key(Key::Backspace));
        assert!(matches!(&actions[..], [HexAction::Delete(range)] if range == &(6..7)));
        let actions = press(&mut editor, key(Key::Delete));
        assert!(matches!(&actions[..], [HexAction::Delete(range)] if range == &(7..8)));
        editor.select_range(2..6, false);
        let actions = press(&mut editor, key(Key::Delete));
        assert!(matches!(&actions[..], [HexAction::Delete(range)] if range == &(2..6)));
        press(&mut editor, key(Key::Insert));
        assert!(editor.edit_mode && !editor.insert_mode, "Insert switches back to Overwrite");
        editor.set_can_resize(false);
        assert!(press(&mut editor, key(Key::Delete)).is_empty());
        press(&mut editor, key(Key::Insert));
        assert!(!editor.insert_mode, "fixed-offset decoders keep the length");
    }

    #[test]
    fn shifting_offsets_moves_named_ranges_and_drops_deleted_ones() {
        let mut editor = HexEditor::default();
        let bookmark = |range| HexHighlight { range, name: "range".into(), color: Color32::RED };
        editor.bookmarks = vec![bookmark(0..4), bookmark(10..14), bookmark(20..30)];
        editor.select_range(22..25, false);
        editor.shift(8, 8, 0);
        let ranges: Vec<_> = editor.bookmarks.iter().map(|b| b.range.clone()).collect();
        assert_eq!(ranges, [0..4, 12..22], "the range inside the deleted bytes is gone");
        assert_eq!(editor.selection(), Some(14..17));
        editor.shift(12, 3, 0);
        assert_eq!(editor.bookmarks[1].range, 12..19);
        assert_eq!(editor.selection(), Some(12..14));
    }

    #[test]
    fn selection_direction_shift_and_empty_ranges() {
        let mut editor = HexEditor::default();
        editor.select_range(10..20, true);
        assert_eq!(editor.selection(), Some(10..20));
        editor.move_caret(4, true);
        assert_eq!(editor.selection(), Some(4..11));
        editor.move_caret(7, false);
        assert_eq!(editor.selection(), Some(7..8));
        editor.select_range(0..0, false);
        assert_eq!(editor.selection(), None);
    }

    #[test]
    fn chunk_search_finds_boundary_and_overlapping_matches_once() {
        let bytes = b"00ABABA11ABA";
        let mut found = Vec::new();
        find_in_chunk(&bytes[..7], b"ABA", 0, 5, &mut found);
        find_in_chunk(&bytes[5..], b"ABA", 5, bytes.len() - 5, &mut found);
        assert_eq!(found, vec![2, 4, 9]);
        let mut capped = vec![0; MATCH_LIMIT - 1];
        find_in_chunk(b"AAAA", b"A", 0, 4, &mut capped);
        assert_eq!(capped.len(), MATCH_LIMIT);
    }

    #[test]
    fn large_navigation_preserves_integer_offsets_and_bounded_scroll() {
        #[cfg(target_pointer_width = "64")]
        {
            let len = 20usize * 1024 * 1024 * 1024;
            let offset = parse_offset("0x4FFFFFFFF", len).unwrap();
            assert_eq!(offset, len - 1);
            let rows = len.div_ceil(16);
            let base = window_start(offset / 16, rows);
            assert!(offset / 16 - base < WINDOW_ROWS);
            assert_eq!(base + WINDOW_ROWS, rows);
            assert!(parse_offset("21474836480", len).is_err());
        }
        assert!(parse_offset("0", 0).is_err());
    }

    #[test]
    fn replacement_preserves_length_and_requires_whole_hex_bytes() {
        assert_eq!(
            replacement_bytes("a4 00", false, Some(3..5), 5).unwrap(),
            (3, vec![164, 0])
        );
        assert!(replacement_bytes("A", false, Some(3..4), 5).is_err());
        assert!(replacement_bytes("AA", false, Some(3..5), 5).is_err());
        assert!(replacement_bytes("é", true, Some(3..4), 5).is_err());
        assert!(replacement_bytes("00", false, Some(5..6), 5).is_err());
        assert!(parse_hex("🙂").is_err());
    }

    #[test]
    fn navigation_history_branches_without_losing_selection_state() {
        let mut editor = HexEditor::default();
        editor.visit(40);
        editor.visit(90);
        editor.navigate_history(true);
        assert_eq!(editor.selection(), Some(40..41));
        editor.visit(60);
        assert_eq!(editor.history, vec![0, 40, 60]);
        assert_eq!(editor.selection(), Some(60..61));
    }

    #[test]
    fn signed_jumps_use_the_cursor_and_reject_overflow_without_moving_it() {
        assert_eq!(parse_jump("256", 37, 1024).unwrap(), 256);
        assert_eq!(parse_jump("0X100", 37, 1024).unwrap(), 256);
        assert_eq!(parse_jump("+256", 37, 1024).unwrap(), 293);
        assert_eq!(parse_jump(" -0x10 ", 37, 1024).unwrap(), 21);
        assert_eq!(parse_jump("+0", 37, 1024).unwrap(), 37);
        assert!(parse_jump("-38", 37, 1024).is_err());
        assert!(parse_jump("+987", 37, 1024).is_err());
        assert!(parse_jump("+1", usize::MAX - 1, usize::MAX).is_err());
        assert!(parse_jump("+2", usize::MAX - 1, usize::MAX).is_err());
        assert!(parse_jump("+1", 0, 0).is_err());
        for invalid in ["+", "--1", "+-1", "+0x", "+1.5", "x10", "-🙂"] {
            assert!(parse_jump(invalid, 100, 1024).is_err(), "{invalid}");
        }
        let mut editor = HexEditor::default();
        editor.select_range(20..38, false);
        editor.jump = "+256".into();
        editor.jump_to_input(1024);
        assert_eq!(editor.selection(), Some(293..294));
        editor.jump_to_input(1024);
        assert_eq!(editor.selection(), Some(549..550));
        let history = editor.history.clone();
        editor.jump = "-550".into();
        editor.jump_to_input(1024);
        assert_eq!(editor.selection(), Some(549..550));
        assert_eq!(editor.history, history);
        assert!(editor.status.contains("outside the file"));
    }

    #[test]
    fn back_returns_to_the_actual_cursor_and_forward_restores_the_jump() {
        let mut editor = HexEditor::default();
        editor.select_range(600..688, false);
        editor.visit(1000);
        assert_eq!(editor.history, vec![0, 687, 1000]);
        editor.navigate_history(true);
        assert_eq!(editor.selection(), Some(687..688));
        editor.navigate_history(false);
        assert_eq!(editor.selection(), Some(1000..1001));
        editor.navigate_history(false);
        assert_eq!(editor.selection(), Some(1000..1001));
        editor.navigate_history(true);
        editor.visit(800);
        assert_eq!(editor.history, vec![0, 687, 800]);
        editor.navigate_history(false);
        assert_eq!(editor.selection(), Some(800..801));
        for offset in 1000..1200 {
            editor.visit(offset);
        }
        assert_eq!(editor.history.len(), 128);
        assert_eq!(editor.history_pos, 127);
        assert_eq!(editor.history.last(), Some(&1199));
    }

    #[test]
    fn scroll_rebasing_preserves_the_visible_row_and_fractional_position() {
        let height = 21.0_f32;
        let total = WINDOW_ROWS * 20;
        for (start, local) in [(0, WINDOW_ROWS - 100), (WINDOW_ROWS * 10, 10)] {
            let y = local as f32 * height + 4.5;
            let (next, shifted) = recenter_scroll_window(start, total, y, height).unwrap();
            assert_ne!(start, next);
            assert_eq!(start + local, next + (shifted / height).floor() as usize);
            assert_eq!(shifted % height, 4.5);
            assert!(recenter_scroll_window(next, total, shifted, height).is_none());
        }
        assert!(recenter_scroll_window(0, 688, 0.0, height).is_none());
        assert!(recenter_scroll_window(0, total, 0.0, height).is_none());
        assert!(recenter_scroll_window(
            total - WINDOW_ROWS,
            total,
            (WINDOW_ROWS - 30) as f32 * height,
            height
        )
        .is_none());
        #[cfg(target_pointer_width = "64")]
        {
            let start = 20usize * 1024 * 1024 * 1024 / 16;
            let (next, shifted) = recenter_scroll_window(start, start * 2, 4.5, height).unwrap();
            assert_eq!(next + (shifted / height).floor() as usize, start);
            assert_eq!(shifted % height, 4.5);
        }
    }

    #[test]
    fn scrolling_large_file_continues_without_page_controls_or_byte_position_jumps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.data");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let source = crate::byte_store::ByteStore::from_file(Arc::new(
            crate::file_source::FileSource::open(&path).unwrap(),
        ))
        .view();
        let ctx = egui::Context::default();
        let mut editor = HexEditor::default();
        editor.pending_scroll = Some((WINDOW_ROWS - 100) as f32 * crate::theme::DARK.size.hex_row_height);
        let frame = |editor: &mut HexEditor| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 700.0),
                    )),
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        editor.ui(ui, Some(source.clone()), &[]);
                    });
                },
            )
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
        };
        let before = frame(&mut editor);
        assert!(editor.window_start > 0);
        let after = frame(&mut editor);
        let first_offset = |texts: &[String]| {
            texts
                .iter()
                .filter(|text| text.len() == 8)
                .find_map(|text| usize::from_str_radix(text, 16).ok())
                .unwrap()
        };
        assert_eq!(first_offset(&before), first_offset(&after));
        assert!(after.iter().any(|text| text == "‹"));
        assert!(after.iter().any(|text| text == "›"));
        assert!(after.iter().any(|text| text.contains("+256")));
        assert!(!after.iter().any(|text| text.contains("Earlier bytes")
            || text.contains("Later bytes")
            || text.starts_with("Window ")));
    }
}
