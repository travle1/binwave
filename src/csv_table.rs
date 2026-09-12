//! Bounded, asynchronous CSV table pages sharing the plot's data source.
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
#[derive(Default)]
pub struct CsvTable {
    page: usize,
    column_page: usize,
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
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        data: Option<&DataStore>,
        dataset: u64,
        visible: &[bool],
    ) -> Option<f64> {
        let Some(data) = data else {
            ui.label("CSV rows will appear here as they become available.");
            return None;
        };
        if data.rows == 0 {
            ui.label("No rows.");
            return None;
        }
        let columns: Vec<_> = (0..data.signals.len())
            .filter(|&i| visible.get(i).copied().unwrap_or(true))
            .collect();
        if columns.is_empty() {
            ui.label("Select signals in Inspector to show columns.");
            return None;
        }
        let pages = data.rows.div_ceil(PAGE_ROWS);
        let column_pages = columns.len().div_ceil(PAGE_COLUMNS);
        self.page = self.page.min(pages - 1);
        self.column_page = self.column_page.min(column_pages - 1);
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(self.page > 0, egui::Button::new("Previous rows"))
                .clicked()
            {
                self.page -= 1;
            }
            let mut page = self.page + 1;
            if ui
                .add(
                    egui::DragValue::new(&mut page)
                        .clamp_range(1..=pages)
                        .prefix("Page "),
                )
                .changed()
            {
                self.page = page - 1;
            }
            ui.label(format!("/ {pages}"));
            if ui
                .add_enabled(self.page + 1 < pages, egui::Button::new("Next rows"))
                .clicked()
            {
                self.page += 1;
            }
            if column_pages > 1 {
                let mut page = self.column_page + 1;
                if ui
                    .add(
                        egui::DragValue::new(&mut page)
                            .clamp_range(1..=column_pages)
                            .prefix("Columns page "),
                    )
                    .changed()
                {
                    self.column_page = page - 1;
                }
                ui.label(format!("/ {column_pages}"));
            }
        });
        ui.small("Read-only CSV values · click a row number to center the plot.");
        let key = Key {
            dataset,
            start: self.page * PAGE_ROWS,
            end: ((self.page + 1) * PAGE_ROWS).min(data.rows),
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
            return None;
        }
        let Some(result) = &self.result else {
            ui.spinner();
            return None;
        };
        let page = match result {
            Ok(page) => page,
            Err(error) => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("{error:#}"));
                return None;
            }
        };
        if page.key != key {
            return None;
        }
        let mut selected = None;
        egui::ScrollArea::both()
            .id_source("csv_table_scroll")
            .show(ui, |ui| {
                egui::Grid::new("csv_table").striped(true).show(ui, |ui| {
                    ui.strong("Row");
                    ui.strong("Time");
                    for &column in &key.columns {
                        ui.strong(&data.signals[column].name);
                    }
                    ui.end_row();
                    for (i, row) in page.rows.iter().enumerate() {
                        if ui
                            .selectable_label(false, (key.start + i).to_string())
                            .clicked()
                            && row[0].is_finite()
                        {
                            selected = Some(row[0]);
                        }
                        for value in row {
                            ui.monospace(value.to_string());
                        }
                        ui.end_row();
                    }
                });
            });
        selected
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
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    table.ui(ui, Some(&data), 1, &[true; 20]);
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
}
