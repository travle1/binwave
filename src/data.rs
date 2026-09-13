use crate::plot_index::{Sample, SummaryIndex};
#[cfg(test)]
use anyhow::Context;
use anyhow::{ensure, Result};
#[cfg(test)]
use memmap2::Mmap;
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::fs::File;
use std::sync::{atomic::AtomicBool, Arc, Mutex};

/// A sealed native-f64 CSV chunk. A mapped chunk is never appended to or modified.
#[cfg(test)]
#[derive(Debug)]
pub struct MappedChunk {
    map: Mmap,
    pub rows: usize,
    columns: usize,
}

#[cfg(test)]
impl MappedChunk {
    pub fn open(file: File, rows: usize, columns: usize) -> Result<Arc<Self>> {
        let len = rows
            .checked_mul(columns)
            .and_then(|n| n.checked_mul(8))
            .context("decoded CSV size overflows")?;
        ensure!(
            len > 0 && len <= isize::MAX as usize && file.metadata()?.len() == len as u64,
            "invalid or unsupported CSV cache size"
        );
        // SAFETY: this private file is complete, closed for writing, and frozen.
        // Subsequent rows go in another file; existing mapped bytes never change.
        let map = unsafe { Mmap::map(&file) }.context("map decoded CSV cache")?;
        bytemuck::try_cast_slice::<u8, f64>(&map)
            .map_err(|_| anyhow::anyhow!("invalid CSV cache alignment"))?;
        Ok(Arc::new(Self { map, rows, columns }))
    }
}

/// Immutable view of all completed chunks, sharing mappings with newer views.
/// X is stored once per row. Only small chunk/index metadata is copied for updates.
#[cfg(test)]
#[derive(Debug)]
pub struct MappedTable {
    chunks: Vec<(usize, Arc<MappedChunk>)>,
    pub rows: usize,
    // Drop mappings before releasing the last incomplete-directory lease.
    _pending: Option<Arc<crate::cache::PendingDirectory>>,
}

#[cfg(test)]
impl MappedTable {
    pub fn from_chunks(
        chunks: Vec<Arc<MappedChunk>>,
        pending: Option<Arc<crate::cache::PendingDirectory>>,
    ) -> Result<Arc<Self>> {
        ensure!(!chunks.is_empty(), "CSV cache has no completed chunks");
        let columns = chunks[0].columns;
        let mut rows = 0usize;
        let mut entries = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            ensure!(chunk.columns == columns, "CSV chunk column count mismatch");
            let start = rows;
            rows = rows
                .checked_add(chunk.rows)
                .context("CSV row count overflows")?;
            entries.push((start, chunk));
        }
        Ok(Arc::new(Self {
            chunks: entries,
            rows,
            _pending: pending,
        }))
    }
    fn point(&self, record: usize, column: usize) -> [f64; 2] {
        assert!(record < self.rows);
        let index = self.chunks.partition_point(|(start, _)| *start <= record) - 1;
        let (start, chunk) = &self.chunks[index];
        let row = (record - start) * chunk.columns;
        let values: &[f64] = bytemuck::cast_slice(&chunk.map);
        [values[row], values[row + column]]
    }
}

type ViewCache = VecDeque<((u64, u64, usize), Vec<Sample>)>;

pub trait PointSource: std::fmt::Debug + Send + Sync {
    fn point(&self, record: usize, column: usize, cancel: &AtomicBool) -> Result<[f64; 2]>;
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl PointSource for MappedTable {
    fn point(&self, record: usize, column: usize, _cancel: &AtomicBool) -> Result<[f64; 2]> {
        ensure!(record < self.rows, "row is outside the loaded portion");
        Ok(self.point(record, column))
    }
}

#[derive(Debug, Clone)]
pub struct PointSeries {
    table: Arc<dyn PointSource>,
    column: usize,
    index: Arc<SummaryIndex>,
    views: Arc<Mutex<ViewCache>>,
}

impl PointSeries {
    pub fn new(table: Arc<dyn PointSource>, column: usize, index: SummaryIndex) -> Self {
        Self {
            table,
            column,
            index: Arc::new(index),
            views: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
    pub fn read_rows(
        &self,
        rows: std::ops::Range<usize>,
        cancel: &AtomicBool,
    ) -> Result<Vec<[f64; 2]>> {
        ensure!(rows.len() <= 100, "table query exceeds page limit");
        self.table.validate()?;
        let mut points = Vec::with_capacity(rows.len());
        for record in rows {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Relaxed),
                "Table query cancelled"
            );
            points.push(self.table.point(record, self.column, cancel)?);
        }
        self.table.validate()?;
        Ok(points)
    }
    #[cfg(test)]
    pub fn point(&self, record: usize) -> [f64; 2] {
        self.table
            .point(record, self.column, &AtomicBool::new(false))
            .unwrap()
    }
    #[cfg(test)]
    pub fn for_range(&self, min: f64, max: f64, target: usize) -> Result<Vec<[f64; 2]>> {
        self.for_range_cancel(min, max, target, &AtomicBool::new(false))
    }
    #[cfg(test)]
    pub fn for_range_cancel(
        &self,
        min: f64,
        max: f64,
        target: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<[f64; 2]>> {
        Ok(self
            .samples_for_range_cancel(min, max, target, cancel)?
            .into_iter()
            .map(|sample| sample.point)
            .collect())
    }
    /// Keep source row identities attached to plotted samples, including when
    /// X values repeat or arrive out of order.
    pub fn samples_for_range_cancel(
        &self,
        min: f64,
        max: f64,
        target: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<Sample>> {
        self.table.validate()?;
        if target == 0 {
            return Ok(Vec::new());
        }
        let key = (min.to_bits(), max.to_bits(), target);
        let mut cache = self.views.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, points)) = cache.iter().find(|(k, _)| *k == key) {
            return Ok(points.clone());
        }
        let plot = self.index.query(
            Some([min, max]),
            target.max(2),
            &|record| {
                let point = self.table.point(record, self.column, cancel)?;
                Ok(point.iter().all(|v| v.is_finite()).then_some(Sample {
                    record,
                    byte_offset: 0,
                    point,
                }))
            },
            cancel,
        )?;
        self.table.validate()?;
        let points = plot.samples;
        if cache.len() == 4 {
            cache.pop_front();
        }
        cache.push_back((key, points.clone()));
        Ok(points)
    }
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub name: String,
    pub points: PointSeries,
}

/// How much of an original CSV has been indexed. Bounds and row counts in the
/// containing store describe only this prefix until `complete` becomes true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvExtent {
    pub complete: bool,
    pub indexed_bytes: u64,
    pub source_bytes: u64,
}

/// Exact original-source bytes for one indexed CSV data row. Field ranges
/// exclude delimiters and include any raw quotes/escapes. Empty fields have
/// empty ranges. Both fields and values are ordered X, then `DataStore.signals`.
#[derive(Debug, Clone)]
pub struct CsvRow {
    pub index: usize,
    pub range: std::ops::Range<usize>,
    pub fields: Vec<std::ops::Range<usize>>,
    pub values: Vec<f64>,
}

pub trait CsvRows: std::fmt::Debug + Send + Sync {
    fn row(&self, index: usize, cancel: &AtomicBool) -> Result<CsvRow>;
    /// Header bytes, blank records and bytes outside this indexed snapshot do
    /// not identify a data row. Separators/newlines identify only their row.
    fn find_byte(&self, offset: usize, cancel: &AtomicBool) -> Result<Option<CsvRow>>;
}

#[derive(Debug, Default, Clone)]
pub struct DataStore {
    pub signals: Vec<Signal>,
    pub time_min: f64,
    pub time_max: f64,
    pub rows: usize,
    pub storage: String,
    pub csv_extent: Option<CsvExtent>,
    pub csv_rows: Option<Arc<dyn CsvRows>>,
    name_to_index: HashMap<String, usize>,
}

impl DataStore {
    pub fn from_signals(signals: Vec<Signal>, time_min: f64, time_max: f64, rows: usize) -> Self {
        let name_to_index = signals
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.clone(), i))
            .collect();
        Self {
            signals,
            time_min,
            time_max,
            rows,
            storage: "decoded disk cache".into(),
            csv_extent: None,
            csv_rows: None,
            name_to_index,
        }
    }
    pub fn get(&self, name: &str) -> Option<&Signal> {
        self.name_to_index
            .get(name)
            .and_then(|&i| self.signals.get(i))
    }
}
