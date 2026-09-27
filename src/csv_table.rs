//! Bounded, asynchronous CSV table pages sharing the plot's data source.
#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use crate::data::DataStore;
use anyhow::Result;
use egui::Ui;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
    Arc,
};

const PAGE_ROWS: usize = 100;
const PAGE_COLUMNS: usize = 16;
#[derive(Clone, PartialEq, Eq)]
struct Key {
    dataset: u64,
    start: usize,
    end: usize,
    columns: Vec<usize>,
}
struct Page {
    key: Key,
    rows: Vec<Vec<f64>>,
}
/// Logical CSV coordinates, independent of signal filtering and table pagination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvCellSelection {
    pub row: usize,
    /// `None` selects the row; `Some(0)` selects X; `Some(i + 1)` selects signal i.
    pub column: Option<usize>,
}
/// A whole-row range retains the original anchor when extended in either direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvRowSelection {
    pub anchor: usize,
    pub active: usize,
}
impl CsvRowSelection {
    pub fn rows(self) -> std::ops::Range<usize> {
        self.anchor.min(self.active)..self.anchor.max(self.active).saturating_add(1)
    }
}
#[derive(Debug, Default, PartialEq)]
pub struct CsvTableResponse {
    /// A cell or row clicked in this frame. External selections are not echoed.
    pub selected_cell: Option<CsvCellSelection>,
    /// Shift-click selects complete rows, including both endpoints, across pages.
    pub selected_rows: Option<CsvRowSelection>,
    /// Minimum row count needed for an explicit navigation action, emitted once.
    pub requested_rows: Option<usize>,
    /// Explicit page or row navigation, including a return to already available rows.
    pub navigation_changed: bool,
}
#[derive(Default)]
pub struct CsvTable {
    page: usize,
    column_page: usize,
    row_jump: String,
    jump_error: bool,
    selection: Option<CsvCellSelection>,
    row_selection: Option<CsvRowSelection>,
    selection_anchor: Option<usize>,
    selection_dataset: Option<u64>,
    reveal_column: bool,
    scroll_to_selection: bool,
    requested: Option<Key>,
    result: Option<Result<Page>>,
    rx: Option<Receiver<Result<Page>>>,
    cancel: Arc<AtomicBool>,
}
impl Drop for CsvTable {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl CsvTable {
    /// Reveal a linked selection without emitting navigation demand or a new selection.
    /// The next frame resolves the column page against the current visible signals.
    pub fn select(&mut self, selection: CsvCellSelection) {
        self.selection = Some(selection);
        self.row_selection = None;
        self.selection_anchor = Some(selection.row);
        self.page = selection.row / PAGE_ROWS;
        self.reveal_column = true;
        self.scroll_to_selection = true;
    }

    /// Acknowledge a linked range without losing its anchor or emitting a click.
    pub fn select_rows(&mut self, selection: CsvRowSelection) {
        self.selection = Some(CsvCellSelection {
            row: selection.active,
            column: None,
        });
        self.row_selection = Some(selection);
        self.selection_anchor = Some(selection.anchor);
        self.page = selection.active / PAGE_ROWS;
        self.reveal_column = false;
        self.scroll_to_selection = true;
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.row_selection = None;
        self.selection_anchor = None;
        self.reveal_column = false;
        self.scroll_to_selection = false;
    }

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        data: Option<&DataStore>,
        dataset: u64,
        visible: &[bool],
        complete: bool,
        reading: bool,
    ) -> CsvTableResponse {
        let mut response = CsvTableResponse::default();
        if self
            .selection_dataset
            .is_some_and(|previous| previous != dataset)
        {
            self.clear_selection();
            self.page = 0;
        }
        self.selection_dataset = Some(dataset);
        let Some(data) = data else {
            ui.label("CSV rows will appear here as they become available.");
            return response;
        };
        if complete && data.rows == 0 {
            self.page = 0;
            ui.label("No rows.");
            return response;
        }
        let columns: Vec<_> = (0..data.signals.len())
            .filter(|&i| visible.get(i).copied().unwrap_or(true))
            .collect();
        if self.reveal_column {
            if let Some(column) = self.selection.and_then(|selection| selection.column) {
                if let Some(signal) = column.checked_sub(1) {
                    if let Some(position) = columns.iter().position(|&column| column == signal) {
                        self.column_page = position / PAGE_COLUMNS;
                    }
                }
            }
            self.reveal_column = false;
        }
        let pages = data.rows.div_ceil(PAGE_ROWS);
        let column_pages = columns.len().div_ceil(PAGE_COLUMNS);
        if complete {
            self.page = self.page.min(pages.saturating_sub(1));
        }
        self.column_page = self.column_page.min(column_pages.saturating_sub(1));
        let mut navigate = false;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(self.page > 0, egui::Button::new("Previous rows"))
                .clicked()
            {
                self.page -= 1;
                navigate = true;
            }
            let mut page = self.page.saturating_add(1);
            let max_pages = if complete {
                pages.max(1)
            } else {
                usize::MAX.div_ceil(PAGE_ROWS)
            };
            if ui
                .add(
                    egui::DragValue::new(&mut page)
                        .range(1..=max_pages)
                        .prefix("Page "),
                )
                .changed()
            {
                self.page = page.saturating_sub(1);
                navigate = true;
            }
            if complete {
                ui.label(format!("/ {pages}"));
            }
            if ui
                .add_enabled(
                    !complete || self.page.saturating_add(1) < pages,
                    egui::Button::new("Next rows"),
                )
                .clicked()
            {
                self.page = self.page.saturating_add(1).min(max_pages - 1);
                navigate = true;
            }
            let jump = ui.add(
                egui::TextEdit::singleline(&mut self.row_jump)
                    .desired_width(90.)
                    .hint_text("Row number"),
            );
            if ui.button("Go to row").clicked()
                || (jump.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            {
                if let Ok(row) = self.row_jump.trim().parse::<usize>() {
                    self.page = row / PAGE_ROWS;
                    if complete {
                        self.page = self.page.min(pages.saturating_sub(1));
                    }
                    self.jump_error = false;
                    navigate = true;
                } else {
                    self.jump_error = true;
                }
            }
            if column_pages > 1 {
                let mut page = self.column_page + 1;
                if ui
                    .add(
                        egui::DragValue::new(&mut page)
                            .range(1..=column_pages)
                            .prefix("Columns page "),
                    )
                    .changed()
                {
                    self.column_page = page - 1;
                    self.scroll_to_selection = false;
                }
                ui.label(format!("/ {column_pages}"));
            }
        });
        if self.jump_error {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                "Enter a row number starting at 0.",
            );
        }
        response.navigation_changed = navigate;
        if navigate {
            self.scroll_to_selection = false;
        }
        let page_end = self.page.saturating_add(1).saturating_mul(PAGE_ROWS);
        let awaiting_rows = !complete && page_end > data.rows;
        if navigate && awaiting_rows {
            response.requested_rows = Some(page_end);
        }
        ui.horizontal_wrapped(|ui| {
            if complete {
                ui.small(crate::format::plural(data.rows as u64, "row", "rows"));
            } else {
                ui.small(format!("{} available", crate::format::plural(data.rows as u64, "row", "rows")));
            }
            if awaiting_rows {
                if reading || response.requested_rows.is_some() {
                    ui.spinner();
                    ui.small("Finding row…");
                } else if ui.button("Read requested rows").clicked() {
                    response.requested_rows = Some(page_end);
                }
            }
        });
        let start = self.page.saturating_mul(PAGE_ROWS);
        if start >= data.rows {
            return response;
        }
        if columns.is_empty() {
            ui.label("Select signals in Signal to show columns.");
            return response;
        }
        ui.small(
            "Read-only CSV values · click to link with Hex and Signal; Shift-click selects rows.",
        );
        if let Some(selection) = self.row_selection {
            ui.small(format!("{} selected", crate::format::plural(selection.rows().len() as u64, "row", "rows")));
        }
        let key = Key {
            dataset,
            start,
            end: page_end.min(data.rows),
            columns: columns
                .into_iter()
                .skip(self.column_page * PAGE_COLUMNS)
                .take(PAGE_COLUMNS)
                .collect(),
        };
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.result = Some(result);
                    self.rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result = Some(Err(anyhow::anyhow!("Table worker stopped unexpectedly")));
                    self.rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.requested.as_ref() != Some(&key) {
            // Coalesce fast navigation: let the one bounded query finish before starting another.
            if self.rx.is_none() {
                let (tx, rx) = mpsc::channel();
                self.rx = Some(rx);
                self.result = None;
                self.requested = Some(key.clone());
                self.cancel = Arc::new(AtomicBool::new(false));
                let cancel = self.cancel.clone();
                let data = data.clone();
                let key = key.clone();
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let _ = tx.send(read_page(&data, key, &cancel));
                    ctx.request_repaint();
                });
            }
        }
        if self.rx.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
        if self.requested.as_ref() != Some(&key) {
            ui.spinner();
            return response;
        }
        let Some(result) = &self.result else {
            ui.spinner();
            return response;
        };
        let page = match result {
            Ok(page) => page,
            Err(error) => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("{error:#}"));
                return response;
            }
        };
        if page.key != key {
            return response;
        }
        let selection = self.selection;
        let selected_rows = self.row_selection.map(|selection| selection.rows());
        let highlighted_rows = selected_rows
            .clone()
            .or_else(|| selection.map(|selection| selection.row..selection.row.saturating_add(1)));
        let mut clicked = None;
        let mut reveal_response = None;
        egui::ScrollArea::both()
            .id_salt("csv_table_scroll")
            .show(ui, |ui| {
                egui::Grid::new("csv_table")
                    .with_row_color(move |row, style| {
                        if row > 0
                            && highlighted_rows
                                .as_ref()
                                .is_some_and(|rows| rows.contains(&(key.start + row - 1)))
                        {
                            Some(style.visuals.selection.bg_fill.gamma_multiply(0.35))
                        } else if row % 2 == 1 {
                            Some(style.visuals.faint_bg_color)
                        } else {
                            None
                        }
                    })
                    .show(ui, |ui| {
                        ui.strong("Row");
                        ui.strong("Time");
                        for &column in &key.columns {
                            ui.strong(&data.signals[column].name);
                        }
                        ui.end_row();
                        for (i, row) in page.rows.iter().enumerate() {
                            let row_selection = CsvCellSelection {
                                row: key.start + i,
                                column: None,
                            };
                            let row_response = ui.selectable_label(
                                selected_rows
                                    .as_ref()
                                    .is_some_and(|rows| rows.contains(&row_selection.row))
                                    || selection == Some(row_selection),
                                row_selection.row.to_string(),
                            );
                            if row_response.clicked() {
                                clicked = Some(row_selection);
                            }
                            if self.scroll_to_selection
                                && selection
                                    .is_some_and(|selection| selection.row == row_selection.row)
                            {
                                reveal_response = Some(row_response);
                            }
                            for (value_index, value) in row.iter().enumerate() {
                                let cell = CsvCellSelection {
                                    row: row_selection.row,
                                    column: Some(if value_index == 0 {
                                        0
                                    } else {
                                        key.columns[value_index - 1] + 1
                                    }),
                                };
                                let cell_response = ui.selectable_label(
                                    selection == Some(cell),
                                    egui::RichText::new(value.to_string()).monospace(),
                                );
                                if cell_response.clicked() {
                                    clicked = Some(cell);
                                }
                                if self.scroll_to_selection && selection == Some(cell) {
                                    reveal_response = Some(cell_response);
                                }
                            }
                            ui.end_row();
                        }
                        if let Some(reveal) = reveal_response.take() {
                            reveal.scroll_to_me(Some(egui::Align::Center));
                            self.scroll_to_selection = false;
                        }
                    });
            });
        if let Some(selection) = clicked {
            if ui.input(|input| input.modifiers.shift) {
                let rows = CsvRowSelection {
                    anchor: self.selection_anchor.unwrap_or(selection.row),
                    active: selection.row,
                };
                self.select_rows(rows);
                response.selected_rows = Some(rows);
            } else {
                self.select(selection);
                response.selected_cell = Some(selection);
            }
            self.scroll_to_selection = false;
        }
        response
    }
}
fn read_page(data: &DataStore, key: Key, cancel: &AtomicBool) -> Result<Page> {
    let mut rows = vec![Vec::with_capacity(key.columns.len() + 1); key.end - key.start];
    for (i, &column) in key.columns.iter().enumerate() {
        let points = data.signals[column]
            .points
            .read_rows(key.start..key.end, cancel)?;
        for (row, point) in rows.iter_mut().zip(points) {
            if i == 0 {
                row.push(point[0]);
            }
            row.push(point[1]);
        }
    }
    Ok(Page { key, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{PointSeries, PointSource, Signal};
    use crate::plot_index::{IndexBuilder, Sample};
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug, Default)]
    struct Rows {
        reads: AtomicUsize,
    }
    impl PointSource for Rows {
        fn point(&self, record: usize, column: usize, _cancel: &AtomicBool) -> Result<[f64; 2]> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok([record as f64, (record * column) as f64])
        }
    }
    fn data() -> (DataStore, Arc<Rows>) {
        let source = Arc::new(Rows::default());
        let signals = (1..=20)
            .map(|column| {
                let mut index = IndexBuilder::new(250);
                for record in 0..250 {
                    index.push(Some(Sample {
                        record,
                        byte_offset: 0,
                        point: [record as f64, (record * column) as f64],
                    }));
                }
                Signal {
                    name: format!("column{column}"),
                    points: PointSeries::new(source.clone(), column, index.finish().unwrap()),
                }
            })
            .collect();
        (DataStore::from_signals(signals, 0., 249., 250), source)
    }
    #[test]
    fn table_page_preserves_row_and_column_identity_and_bounds_source_reads() {
        let (data, source) = data();
        let page = read_page(
            &data,
            Key {
                dataset: 1,
                start: 200,
                end: 250,
                columns: vec![19, 0],
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(page.rows.len(), 50);
        assert_eq!(page.rows[0], [200., 4000., 200.]);
        assert_eq!(page.rows[49], [249., 4980., 249.]);
        assert_eq!(source.reads.load(Ordering::Relaxed), 100);
    }
    #[test]
    fn returning_to_an_inflight_page_keeps_its_query_usable() {
        let (data, _) = data();
        let ctx = egui::Context::default();
        let mut table = CsvTable::default();
        let (tx, rx) = mpsc::channel();
        let original_key = Key {
            dataset: 1,
            start: 0,
            end: 100,
            columns: (0..16).collect(),
        };
        table.requested = Some(original_key.clone());
        table.rx = Some(rx);
        let render = |table: &mut CsvTable| {
            let _ = ctx.run(Default::default(), |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    table.ui(ui, Some(&data), 1, &[true; 20], true, false);
                });
            });
        };
        table.page = 1;
        render(&mut table);
        table.page = 0;
        render(&mut table);
        assert!(!table.cancel.load(Ordering::Relaxed));
        tx.send(read_page(&data, original_key, &table.cancel))
            .unwrap();
        render(&mut table);
        assert!(table.result.as_ref().unwrap().is_ok());
        assert!(table.rx.is_none());
    }
    fn frame(
        ctx: &egui::Context,
        table: &mut CsvTable,
        data: &DataStore,
        complete: bool,
        reading: bool,
        events: Vec<egui::Event>,
    ) -> (CsvTableResponse, Vec<(String, egui::Pos2)>) {
        frame_with_visible(ctx, table, data, &[true; 20], (complete, reading), events)
    }

    fn frame_with_visible(
        ctx: &egui::Context,
        table: &mut CsvTable,
        data: &DataStore,
        visible: &[bool],
        (complete, reading): (bool, bool),
        events: Vec<egui::Event>,
    ) -> (CsvTableResponse, Vec<(String, egui::Pos2)>) {
        let mut response = CsvTableResponse::default();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200., 600.),
                )),
                events: std::iter::once(egui::Event::ModifiersChanged(
                    events.iter().rev().find_map(|event| match event {
                        egui::Event::PointerButton { modifiers, .. } => Some(*modifiers),
                        _ => None,
                    }).unwrap_or_default(),
                )).chain(events).collect(),
                ..Default::default()
            },
            |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    response = table.ui(ui, Some(data), 1, visible, complete, reading);
                });
            },
        );
        fn texts(shape: &egui::epaint::Shape, output: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::epaint::Shape::Text(text) => output.push((
                    text.galley.text().to_owned(),
                    text.pos + text.galley.size() * 0.5,
                )),
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        texts(shape, output);
                    }
                }
                _ => {}
            }
        }
        let mut labels = Vec::new();
        for shape in output.shapes {
            texts(&shape.shape, &mut labels);
        }
        (response, labels)
    }

    fn click(
        ctx: &egui::Context,
        table: &mut CsvTable,
        data: &DataStore,
        complete: bool,
        label: &str,
    ) -> CsvTableResponse {
        let (_, labels) = frame(ctx, table, data, complete, false, vec![]);
        let position = labels
            .iter()
            .find_map(|(text, position)| (text == label).then_some(*position))
            .unwrap_or_else(|| panic!("missing control {label:?}: {labels:?}"));
        let pointer = |pressed| egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let (response, _) = frame(
            ctx,
            table,
            data,
            complete,
            false,
            vec![egui::Event::PointerMoved(position), pointer(true)],
        );
        assert_eq!(response.requested_rows, None);
        frame(ctx, table, data, complete, false, vec![pointer(false)]).0
    }

    #[test]
    fn incomplete_next_requests_once_and_never_claims_a_last_page() {
        let (mut data, _) = data();
        data.rows = 100;
        let ctx = egui::Context::default();
        let mut table = CsvTable::default();
        let (idle, labels) = frame(&ctx, &mut table, &data, false, false, vec![]);
        assert_eq!(idle.requested_rows, None);
        assert!(labels.iter().any(|(text, _)| text == "100 rows available"));
        assert!(!labels.iter().any(|(text, _)| text == "/ 1"));
        let response = click(&ctx, &mut table, &data, false, "Next rows");
        assert_eq!(response.requested_rows, Some(200));
        assert_eq!(table.page, 1);
        for reading in [true, true, false, false] {
            let (response, labels) = frame(&ctx, &mut table, &data, false, reading, vec![]);
            assert_eq!(response.requested_rows, None);
            assert_eq!(table.page, 1);
            assert!(labels.iter().any(|(text, _)| text
                == if reading {
                    "Finding row…"
                } else {
                    "Read requested rows"
                }));
        }
    }

    #[test]
    fn row_jump_keeps_destination_until_arrival_and_retries_only_explicitly() {
        let (mut data, _) = data();
        data.rows = 100;
        let ctx = egui::Context::default();
        let mut table = CsvTable::default();
        table.row_jump = "225".into();
        let response = click(&ctx, &mut table, &data, false, "Go to row");
        assert_eq!(response.requested_rows, Some(300));
        assert_eq!(table.page, 2);
        let (idle, _) = frame(&ctx, &mut table, &data, false, false, vec![]);
        assert_eq!(
            idle.requested_rows, None,
            "cancellation must not restart reading"
        );
        let retry = click(&ctx, &mut table, &data, false, "Read requested rows");
        assert_eq!(retry.requested_rows, Some(300));
        data.rows = 250;
        let (arrival, _) = frame(&ctx, &mut table, &data, false, true, vec![]);
        assert_eq!(arrival.requested_rows, None);
        assert_eq!(table.page, 2, "a partial page must not discard the target");
        let (eof, labels) = frame(&ctx, &mut table, &data, true, false, vec![]);
        assert_eq!(eof.requested_rows, None);
        assert_eq!(table.page, 2);
        assert!(labels.iter().any(|(text, _)| text == "/ 3"));
        assert!(!labels.iter().any(|(text, _)| text == "Read requested rows"));
    }

    #[test]
    fn incomplete_jump_past_eof_clamps_only_when_eof_is_known() {
        let (mut data, _) = data();
        data.rows = 100;
        let ctx = egui::Context::default();
        let mut table = CsvTable::default();
        table.row_jump = "1000".into();
        assert_eq!(
            click(&ctx, &mut table, &data, false, "Go to row").requested_rows,
            Some(1100)
        );
        assert_eq!(table.page, 10);
        data.rows = 150;
        let (response, _) = frame(&ctx, &mut table, &data, false, true, vec![]);
        assert_eq!(response.requested_rows, None);
        assert_eq!(table.page, 10);
        let (response, _) = frame(&ctx, &mut table, &data, true, false, vec![]);
        assert_eq!(response.requested_rows, None);
        assert_eq!(table.page, 1);
    }

    #[test]
    fn completed_table_navigation_never_requests_more_rows() {
        let (data, _) = data();
        let ctx = egui::Context::default();
        let mut table = CsvTable::default();
        for expected_page in [1, 2, 2] {
            let response = click(&ctx, &mut table, &data, true, "Next rows");
            assert_eq!(response.requested_rows, None);
            assert_eq!(table.page, expected_page);
        }
        assert_eq!(
            click(&ctx, &mut table, &data, true, "Previous rows").requested_rows,
            None
        );
        assert_eq!(table.page, 1);
        table.row_jump = "99999".into();
        assert_eq!(
            click(&ctx, &mut table, &data, true, "Go to row").requested_rows,
            None
        );
        assert_eq!(table.page, 2);
    }

    #[test]
    fn incomplete_empty_prefix_can_request_rows_without_an_empty_page_query() {
        let (mut data, _) = data();
        data.rows = 0;
        let ctx = egui::Context::default();
        let mut table = CsvTable::default();
        let (response, _) = frame(&ctx, &mut table, &data, false, false, vec![]);
        assert_eq!(response.requested_rows, None);
        assert!(table.requested.is_none());
        assert_eq!(
            click(&ctx, &mut table, &data, false, "Read requested rows").requested_rows,
            Some(100)
        );
        assert!(table.requested.is_none());
        let (response, labels) = frame(&ctx, &mut table, &data, true, false, vec![]);
        assert_eq!(response.requested_rows, None);
        assert!(labels.iter().any(|(text, _)| text == "No rows."));
    }
    #[test]
    fn returning_to_available_rows_while_reading_signals_navigation_without_more_demand() {
        let (data, _) = data();
        let ctx = egui::Context::default();
        let mut table = CsvTable::default();
        table.page = 2;
        let (idle, labels) = frame(&ctx, &mut table, &data, false, true, vec![]);
        assert!(!idle.navigation_changed);
        assert_eq!(idle.requested_rows, None);
        let position = labels
            .iter()
            .find_map(|(text, position)| (text == "Previous rows").then_some(*position))
            .unwrap();
        for pressed in [true, false] {
            let (response, _) = frame(
                &ctx,
                &mut table,
                &data,
                false,
                true,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            assert_eq!(response.navigation_changed, !pressed);
            assert_eq!(response.requested_rows, None);
        }
        assert_eq!(table.page, 1);
        let (idle, _) = frame(&ctx, &mut table, &data, false, true, vec![]);
        assert!(
            !idle.navigation_changed,
            "navigation must be emitted only once"
        );
    }

    fn cached_page(data: &DataStore, start: usize, columns: Vec<usize>) -> CsvTable {
        let key = Key {
            dataset: 1,
            start,
            end: (start + PAGE_ROWS).min(data.rows),
            columns,
        };
        let mut table = CsvTable::default();
        table.page = start / PAGE_ROWS;
        table.requested = Some(key.clone());
        table.result = Some(read_page(data, key, &AtomicBool::new(false)));
        table
    }

    fn click_table_label(
        ctx: &egui::Context,
        table: &mut CsvTable,
        data: &DataStore,
        visible: &[bool],
        label: &str,
        occurrence: usize,
    ) -> CsvTableResponse {
        click_table_label_with_shift(ctx, table, data, visible, label, occurrence, false)
    }

    fn click_table_label_with_shift(
        ctx: &egui::Context,
        table: &mut CsvTable,
        data: &DataStore,
        visible: &[bool],
        label: &str,
        occurrence: usize,
        shift: bool,
    ) -> CsvTableResponse {
        // Allow the grid to settle its column widths before choosing a hit target.
        frame_with_visible(ctx, table, data, visible, (true, false), vec![]);
        let (_, labels) = frame_with_visible(ctx, table, data, visible, (true, false), vec![]);
        let position = labels
            .iter()
            .filter(|(text, _)| text == label)
            .nth(occurrence)
            .map(|(_, position)| *position)
            .unwrap_or_else(|| panic!("missing cell {label:?} #{occurrence}: {labels:?}"));
        let pointer = |pressed| egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers {
                shift,
                ..Default::default()
            },
        };
        let (pressed, _) = frame_with_visible(
            ctx,
            table,
            data,
            visible,
            (true, false),
            vec![egui::Event::PointerMoved(position), pointer(true)],
        );
        assert_eq!(pressed, CsvTableResponse::default());
        frame_with_visible(
            ctx,
            table,
            data,
            visible,
            (true, false),
            vec![pointer(false)],
        )
        .0
    }

    #[test]
    fn clicks_keep_logical_row_and_cell_identity_through_filtering_and_paging() {
        let (data, source) = data();
        let ctx = egui::Context::default();
        let mut visible = [true; 20];
        for column in [0, 4, 8] {
            visible[column] = false;
        }
        // The seventeenth visible signal is the twentieth logical CSV column.
        let mut table = cached_page(&data, 200, vec![19]);
        table.column_page = 1;
        let source_reads = source.reads.load(Ordering::Relaxed);
        for (label, occurrence, column) in
            [("200", 0, None), ("200", 1, Some(0)), ("4000", 0, Some(20))]
        {
            let response = click_table_label(&ctx, &mut table, &data, &visible, label, occurrence);
            let expected = CsvCellSelection { row: 200, column };
            assert_eq!(response.selected_cell, Some(expected));
            assert_eq!(table.selection, Some(expected));
            assert!(!response.navigation_changed);
            assert_eq!(response.requested_rows, None);
            let (idle, _) =
                frame_with_visible(&ctx, &mut table, &data, &visible, (true, false), vec![]);
            assert_eq!(idle, CsvTableResponse::default());
        }
        assert_eq!(source.reads.load(Ordering::Relaxed), source_reads);
    }

    #[test]
    fn shift_click_extends_complete_rows_across_pages_without_moving_the_anchor() {
        let (data, source) = data();
        let ctx = egui::Context::default();
        let visible = [false; 19].into_iter().chain([true]).collect::<Vec<_>>();
        let mut table = cached_page(&data, 200, vec![19]);
        let click = click_table_label(&ctx, &mut table, &data, &visible, "200", 0);
        assert_eq!(
            click.selected_cell,
            Some(CsvCellSelection {
                row: 200,
                column: None
            })
        );
        let reads = source.reads.load(Ordering::Relaxed);
        for active in [205, 210, 203] {
            let click = click_table_label_with_shift(
                &ctx,
                &mut table,
                &data,
                &visible,
                &active.to_string(),
                0,
                true,
            );
            let selected = CsvRowSelection {
                anchor: 200,
                active,
            };
            assert_eq!(click.selected_cell, None);
            assert_eq!(click.selected_rows, Some(selected));
            assert_eq!(selected.rows(), 200..active + 1);
            // The application resolves source bytes asynchronously, then acknowledges
            // the same range. That round trip must preserve the original anchor.
            table.select_rows(selected);
        }
        assert_eq!(source.reads.load(Ordering::Relaxed), reads);
        let mut first_page = cached_page(&data, 0, vec![19]);
        table.page = 0;
        table.requested = first_page.requested.take();
        table.result = first_page.result.take();
        table.scroll_to_selection = false;
        // The viewport keeps its scroll position when pages change. Row 8 is
        // visible after the earlier selections; row 5 is clipped above the grid.
        let click = click_table_label_with_shift(&ctx, &mut table, &data, &visible, "8", 0, true);
        assert_eq!(
            click.selected_rows,
            Some(CsvRowSelection {
                anchor: 200,
                active: 8
            })
        );
        assert_eq!(table.row_selection.unwrap().rows(), 8..201);
        let (_, labels) =
            frame_with_visible(&ctx, &mut table, &data, &visible, (true, false), vec![]);
        assert!(labels.iter().any(|(text, _)| text == "193 rows selected"));
    }

    #[test]
    fn shift_cell_selects_whole_rows_and_plain_cell_resets_the_anchor() {
        let (data, _) = data();
        let ctx = egui::Context::default();
        let visible = [false; 19].into_iter().chain([true]).collect::<Vec<_>>();
        let mut table = cached_page(&data, 200, vec![19]);
        click_table_label(&ctx, &mut table, &data, &visible, "4000", 0);
        assert_eq!(table.selection_anchor, Some(200));
        let extended =
            click_table_label_with_shift(&ctx, &mut table, &data, &visible, "4100", 0, true);
        assert_eq!(
            extended.selected_rows,
            Some(CsvRowSelection {
                anchor: 200,
                active: 205
            })
        );
        assert_eq!(
            table.selection,
            Some(CsvCellSelection {
                row: 205,
                column: None
            })
        );
        let clicked = click_table_label(&ctx, &mut table, &data, &visible, "4060", 0);
        assert_eq!(
            clicked.selected_cell,
            Some(CsvCellSelection {
                row: 203,
                column: Some(20)
            })
        );
        assert_eq!(table.row_selection, None);
        assert_eq!(table.selection_anchor, Some(203));
        let extended =
            click_table_label_with_shift(&ctx, &mut table, &data, &visible, "4000", 0, true);
        assert_eq!(
            extended.selected_rows,
            Some(CsvRowSelection {
                anchor: 203,
                active: 200
            })
        );
    }

    #[test]
    fn external_selection_clear_and_new_dataset_reset_range_anchors() {
        let (data, _) = data();
        let ctx = egui::Context::default();
        let visible = [false; 19].into_iter().chain([true]).collect::<Vec<_>>();
        let mut table = cached_page(&data, 200, vec![19]);
        table.select_rows(CsvRowSelection {
            anchor: 5,
            active: 205,
        });
        table.select(CsvCellSelection {
            row: 207,
            column: Some(20),
        });
        let extended =
            click_table_label_with_shift(&ctx, &mut table, &data, &visible, "200", 0, true);
        assert_eq!(
            extended.selected_rows,
            Some(CsvRowSelection {
                anchor: 207,
                active: 200
            })
        );
        table.clear_selection();
        let extended =
            click_table_label_with_shift(&ctx, &mut table, &data, &visible, "202", 0, true);
        assert_eq!(
            extended.selected_rows,
            Some(CsvRowSelection {
                anchor: 202,
                active: 202
            })
        );
        let _ = ctx.run(Default::default(), |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(
                    table.ui(ui, Some(&data), 2, &visible, true, false),
                    CsvTableResponse::default(),
                );
            });
        });
        assert_eq!(table.row_selection, None);
        assert_eq!(table.selection_anchor, None);
        assert_eq!(table.selection, None);
        assert_eq!(table.page, 0);
    }
    #[test]
    fn nonfinite_x_does_not_prevent_cell_or_row_selection() {
        let (data, _) = data();
        let ctx = egui::Context::default();
        let visible = [false; 19].into_iter().chain([true]).collect::<Vec<_>>();
        let mut table = cached_page(&data, 200, vec![19]);
        table.result.as_mut().unwrap().as_mut().unwrap().rows[0][0] = f64::NAN;
        for (label, column) in [("NaN", Some(0)), ("200", None)] {
            let response = click_table_label(&ctx, &mut table, &data, &visible, label, 0);
            assert_eq!(
                response.selected_cell,
                Some(CsvCellSelection { row: 200, column })
            );
            assert_eq!(response.requested_rows, None);
            assert!(!response.navigation_changed);
        }
    }

    #[test]
    fn linked_selection_reveals_filtered_pages_without_echo_or_demand() {
        let (data, source) = data();
        let ctx = egui::Context::default();
        let mut visible = [true; 20];
        for column in [0, 4, 8] {
            visible[column] = false;
        }
        let mut table = cached_page(&data, 200, vec![19]);
        table.page = 0;
        let selection = CsvCellSelection {
            row: 249,
            column: Some(20),
        };
        table.select(selection);
        assert_eq!(table.page, 2);
        let source_reads = source.reads.load(Ordering::Relaxed);
        for _ in 0..3 {
            let (response, _) =
                frame_with_visible(&ctx, &mut table, &data, &visible, (true, false), vec![]);
            assert_eq!(response, CsvTableResponse::default());
            assert_eq!(table.page, 2);
            assert_eq!(table.column_page, 1);
            assert_eq!(table.selection, Some(selection));
            assert!(!table.reveal_column);
            assert!(!table.scroll_to_selection);
        }
        assert_eq!(source.reads.load(Ordering::Relaxed), source_reads);
        table.clear_selection();
        assert_eq!(table.selection, None);
        assert!(!table.reveal_column);
        assert!(!table.scroll_to_selection);
        assert_eq!(table.page, 2);
        assert_eq!(table.column_page, 1);
    }

    #[test]
    fn linked_selection_preserves_demand_driven_loading_and_hidden_columns() {
        let (mut data, _) = data();
        data.rows = 100;
        let ctx = egui::Context::default();
        let mut table = CsvTable::default();
        let mut visible = [true; 20];
        visible[19] = false;
        let selection = CsvCellSelection {
            row: 225,
            column: Some(20),
        };
        table.select(selection);
        for reading in [false, true, false] {
            let (response, _) =
                frame_with_visible(&ctx, &mut table, &data, &visible, (false, reading), vec![]);
            assert_eq!(response, CsvTableResponse::default());
            assert_eq!(table.page, 2);
            assert_eq!(table.column_page, 0);
            assert_eq!(table.selection, Some(selection));
            assert!(table.requested.is_none());
        }
    }
}
