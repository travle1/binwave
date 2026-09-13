//! CSV data stays in the original file. Persist only seek checkpoints and plot
//! summaries; share a bounded cache of decoded blocks between all signals/views.
use crate::cache::{self, LoadProgress, SourceStamp};
use crate::config::AppConfig;
use crate::data::{CsvExtent, CsvRow, CsvRows, DataStore, PointSeries, PointSource, Signal};
use crate::plot_index::{IndexBuilder, Sample};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock, Weak,
};
use std::time::{Duration, Instant};

const INDEX_KIND: &str = "csv-source-index-v2";
/// Stop interactive work between records after this much newly indexed input.
/// One final record (at most MAX_RECORD_BYTES) and 64 KiB read-ahead can follow.
pub const REQUEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHECKPOINTS: usize = 65536;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Metadata {
    next_offset: u64,
    complete: bool,
    rows: usize,
    names: Vec<String>,
    columns: Vec<usize>,
    source_columns: usize,
    block_rows: usize,
    stride: usize,
    offsets: Vec<u64>,
    indexes: Vec<IndexBuilder>,
    time_min: f64,
    time_max: f64,
}

struct Block {
    values: Vec<f64>,
    touched: u64,
}
struct ReaderState {
    reader: csv::Reader<RecordLimitedFile>,
    next_row: usize,
    blocks: HashMap<usize, Block>,
    used: usize,
    clock: u64,
}

struct SourceCsv {
    path: PathBuf,
    stamp: SourceStamp,
    columns: Vec<usize>,
    source_columns: usize,
    block_rows: usize,
    budget: usize,
    delimiter: u8,
    reader: Mutex<ReaderState>,
    resume: Mutex<Option<Metadata>>,
}
impl std::fmt::Debug for SourceCsv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceCsv")
            .field("path", &self.path)
            .field("ram_budget", &self.budget)
            .finish()
    }
}

impl SourceCsv {
    fn open(cfg: &AppConfig, stamp: SourceStamp, meta: &Metadata) -> Result<Arc<Self>> {
        let path = cfg.data_path.as_ref().context("CSV path missing")?.clone();
        let file = File::open(&path)?;
        ensure!(
            stamp == SourceStamp::read(&path, &file.metadata()?)?,
            "CSV changed while opening; reload it"
        );
        let budget = cfg
            .csv
            .ram_cache_mib
            .clamp(1, 4096)
            .checked_mul(1024 * 1024)
            .context("CSV RAM cache budget exceeds the platform limit")?;
        let block_bytes = meta
            .block_rows
            .checked_mul(meta.columns.len())
            .and_then(|n| n.checked_mul(8))
            .context("CSV block size overflows")?;
        ensure!(
            block_bytes <= budget,
            "CSV block exceeds RAM cache budget; increase RAM cache size"
        );
        Ok(Arc::new(Self {
            path,
            stamp: stamp.clone(),
            columns: meta.columns.clone(),
            source_columns: meta.source_columns,
            block_rows: meta.block_rows,
            budget,
            delimiter: cfg.csv.delimiter as u8,
            resume: Mutex::new(None),
            reader: Mutex::new(ReaderState {
                reader: resume_reader(file, cfg, &stamp, meta, 0)?,
                next_row: usize::MAX,
                blocks: HashMap::new(),
                used: 0,
                clock: 0,
            }),
        }))
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.stamp == SourceStamp::read(&self.path, &std::fs::metadata(&self.path)?)?,
            "CSV source changed; reload it before continuing"
        );
        Ok(())
    }

    fn insert(&self, start: usize, values: Vec<f64>) {
        let mut state = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        Self::insert_locked(&mut state, self.budget, start, values);
    }

    fn insert_locked(state: &mut ReaderState, budget: usize, start: usize, values: Vec<f64>) {
        if let Some(old) = state.blocks.remove(&start) {
            state.used -= old.values.capacity() * 8;
        }
        let bytes = values.capacity() * 8;
        if bytes > budget {
            return;
        }
        while state.used + bytes > budget {
            let oldest = *state
                .blocks
                .iter()
                .min_by_key(|(_, b)| b.touched)
                .unwrap()
                .0;
            state.used -= state.blocks.remove(&oldest).unwrap().values.capacity() * 8;
        }
        state.clock = state.clock.wrapping_add(1);
        let touched = state.clock;
        state.used += bytes;
        state.blocks.insert(start, Block { values, touched });
    }

    fn point(
        &self,
        row: usize,
        column: usize,
        rows: usize,
        stride: usize,
        offsets: &[u64],
        cancel: &AtomicBool,
    ) -> Result<[f64; 2]> {
        ensure!(
            row < rows && column < self.columns.len(),
            "row/column outside loaded CSV portion"
        );
        let start = row / self.block_rows * self.block_rows;
        let relative = (row - start) * self.columns.len();
        let mut state = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let touched = state.clock;
        if let Some(block) = state.blocks.get_mut(&start) {
            if relative + column < block.values.len() {
                block.touched = touched;
                return Ok([block.values[relative], block.values[relative + column]]);
            }
        }
        self.validate()?;
        let checkpoint = start / stride;
        let checkpoint_row = checkpoint * stride;
        if state.next_row < checkpoint_row || state.next_row > start {
            let mut position = csv::Position::new();
            position.set_byte(
                *offsets
                    .get(checkpoint)
                    .context("missing CSV seek checkpoint")?,
            );
            position.set_record(checkpoint_row as u64);
            state.reader.seek(position)?;
            state.next_row = checkpoint_row;
        }
        let end = start.saturating_add(self.block_rows).min(rows);
        let mut values = Vec::with_capacity((end - start) * self.columns.len());
        let mut record = csv::ByteRecord::new();
        let mut decoded = vec![0.; self.columns.len()];
        while state.next_row < end {
            if state.next_row % 4096 == 0 {
                ensure!(!cancel.load(Ordering::Relaxed), "plot cancelled");
            }
            let position = state.reader.position().byte();
            state.reader.get_mut().record_start = position;
            ensure!(
                state.reader.read_byte_record(&mut record)?,
                "CSV ended before indexed row; reload it"
            );
            if state.next_row >= start {
                decode(
                    &record,
                    &self.columns,
                    self.source_columns,
                    state.next_row,
                    &mut decoded,
                )?;
                values.extend_from_slice(&decoded);
            }
            state.next_row += 1;
        }
        self.validate()?;
        let point = [values[relative], values[relative + column]];
        Self::insert_locked(&mut state, self.budget, start, values);
        Ok(point)
    }
}

#[derive(Debug)]
struct SourceTable {
    source: Arc<SourceCsv>,
    rows: usize,
    stride: usize,
    offsets: Arc<Vec<u64>>,
    indexed_bytes: u64,
}
impl PointSource for SourceTable {
    fn point(&self, row: usize, column: usize, cancel: &AtomicBool) -> Result<[f64; 2]> {
        self.source
            .point(row, column, self.rows, self.stride, &self.offsets, cancel)
    }
    fn validate(&self) -> Result<()> {
        self.source.validate()
    }
}

impl SourceTable {
    fn seek(state: &mut ReaderState, row: usize, offset: u64) -> Result<()> {
        let mut position = csv::Position::new();
        position.set_byte(offset).set_record(row as u64);
        state.reader.seek(position)?;
        state.next_row = row;
        Ok(())
    }

    fn read_next(&self, state: &mut ReaderState, record: &mut csv::ByteRecord) -> Result<()> {
        let position = state.reader.position().byte();
        state.reader.get_mut().record_start = position;
        ensure!(
            state.reader.read_byte_record(record)?,
            "CSV ended before indexed row; reload it"
        );
        state.next_row += 1;
        Ok(())
    }

    fn describe(
        &self,
        index: usize,
        record: &csv::ByteRecord,
        end: u64,
        cancel: &AtomicBool,
    ) -> Result<CsvRow> {
        let start = record
            .position()
            .context("CSV record position missing")?
            .byte();
        ensure!(
            end >= start && end - start <= MAX_RECORD_BYTES,
            "CSV record exceeds 1 MiB"
        );
        let mut file = File::open(&self.source.path)?;
        ensure!(
            self.source.stamp == SourceStamp::read(&self.source.path, &file.metadata()?)?,
            "CSV source changed; reload it before continuing"
        );
        file.seek(SeekFrom::Start(start))?;
        let mut raw = vec![0; usize::try_from(end - start).context("CSV record is too large")?];
        for chunk in raw.chunks_mut(READ_BUFFER_BYTES) {
            ensure!(!cancel.load(Ordering::Relaxed), "CSV row query cancelled");
            file.read_exact(chunk)?;
        }
        // csv returns a CR-terminated record before consuming its optional LF.
        // Include both original newline bytes without parsing the following row.
        if raw.last() == Some(&b'\r') && end < self.source.stamp.len {
            let mut next = [0];
            file.read_exact(&mut next)?;
            if next[0] == b'\n' {
                raw.push(b'\n');
            }
        }
        let start = usize::try_from(start).context("CSV source exceeds the platform byte limit")?;
        let (range, source_fields) = raw_fields(&raw, start, self.source.delimiter, cancel)?;
        ensure!(
            source_fields.len() == record.len(),
            "CSV source field boundaries changed; reload it"
        );
        let fields = self
            .source
            .columns
            .iter()
            .map(|&column| {
                source_fields
                    .get(column)
                    .cloned()
                    .context("CSV source column missing")
            })
            .collect::<Result<Vec<_>>>()?;
        let mut values = vec![0.; self.source.columns.len()];
        decode(
            record,
            &self.source.columns,
            self.source.source_columns,
            index,
            &mut values,
        )?;
        Ok(CsvRow {
            index,
            range,
            fields,
            values,
        })
    }
}

impl CsvRows for SourceTable {
    fn row(&self, index: usize, cancel: &AtomicBool) -> Result<CsvRow> {
        ensure!(!cancel.load(Ordering::Relaxed), "CSV row query cancelled");
        self.source.validate()?;
        ensure!(index < self.rows, "row is outside the indexed CSV portion");
        let result = (|| {
            let checkpoint = index / self.stride;
            let checkpoint_row = checkpoint * self.stride;
            let mut state = self.source.reader.lock().unwrap_or_else(|e| e.into_inner());
            if state.next_row < checkpoint_row || state.next_row > index {
                Self::seek(&mut state, checkpoint_row, self.offsets[checkpoint])?;
            }
            let mut record = csv::ByteRecord::new();
            while state.next_row <= index {
                ensure!(!cancel.load(Ordering::Relaxed), "CSV row query cancelled");
                self.read_next(&mut state, &mut record)?;
            }
            self.describe(index, &record, state.reader.position().byte(), cancel)
        })();
        self.source.validate()?;
        ensure!(!cancel.load(Ordering::Relaxed), "CSV row query cancelled");
        result
    }

    fn find_byte(&self, offset: usize, cancel: &AtomicBool) -> Result<Option<CsvRow>> {
        ensure!(!cancel.load(Ordering::Relaxed), "CSV row query cancelled");
        self.source.validate()?;
        let result = (|| {
            let offset =
                u64::try_from(offset).context("CSV byte offset exceeds the platform limit")?;
            // Equality can be the LF belonging to the last indexed CRLF row.
            if offset > self.indexed_bytes
                || offset >= self.source.stamp.len
                || self.offsets.first().is_none_or(|&first| offset < first)
            {
                return Ok(None);
            }
            // A checkpoint may sit on the LF after the preceding row's CR.
            // Start one checkpoint earlier at equality so that LF finds its row.
            let checkpoint = self
                .offsets
                .partition_point(|&start| start < offset)
                .saturating_sub(1);
            let mut state = self.source.reader.lock().unwrap_or_else(|e| e.into_inner());
            Self::seek(
                &mut state,
                checkpoint * self.stride,
                self.offsets[checkpoint],
            )?;
            let mut record = csv::ByteRecord::new();
            while state.next_row < self.rows {
                ensure!(!cancel.load(Ordering::Relaxed), "CSV row query cancelled");
                let index = state.next_row;
                self.read_next(&mut state, &mut record)?;
                let end = state.reader.position().byte();
                if offset <= end {
                    let row = self.describe(index, &record, end, cancel)?;
                    if row.range.contains(&(offset as usize)) {
                        return Ok(Some(row));
                    }
                    if offset < row.range.start as u64 {
                        return Ok(None);
                    }
                }
            }
            Ok(None)
        })();
        self.source.validate()?;
        ensure!(!cancel.load(Ordering::Relaxed), "CSV row query cancelled");
        result
    }
}

/// Split the already parsed, bounded original record using csv's default
/// quoting rules: a quote only opens at field start, doubled quotes escape,
/// and text after a closing quote continues as an unquoted field. Retain raw
/// quotes/escapes and exclude delimiters/terminators from the field ranges.
fn raw_fields(
    raw: &[u8],
    base: usize,
    delimiter: u8,
    cancel: &AtomicBool,
) -> Result<(std::ops::Range<usize>, Vec<std::ops::Range<usize>>)> {
    #[derive(Clone, Copy)]
    enum State {
        Start,
        Plain,
        Quoted,
        AfterQuote,
    }
    let mut start = if base == 0 && raw.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    // csv's record position precedes skipped empty physical records and may
    // precede the LF left over from the previous CRLF terminator.
    while raw.get(start).is_some_and(|b| matches!(b, b'\r' | b'\n')) {
        start += 1;
    }
    ensure!(start < raw.len(), "CSV record has no source fields");
    let mut fields = Vec::new();
    let mut field = start;
    let mut state = State::Start;
    for (i, &byte) in raw.iter().enumerate().skip(start) {
        if i % 4096 == 0 {
            ensure!(!cancel.load(Ordering::Relaxed), "CSV row query cancelled");
        }
        match state {
            State::Quoted => {
                if byte == b'"' {
                    state = State::AfterQuote;
                }
            }
            State::Start | State::AfterQuote if byte == b'"' => state = State::Quoted,
            _ if byte == delimiter => {
                fields.push(base + field..base + i);
                field = i + 1;
                state = State::Start;
            }
            _ if matches!(byte, b'\r' | b'\n') => {
                fields.push(base + field..base + i);
                let end = i + 1 + usize::from(byte == b'\r' && raw.get(i + 1) == Some(&b'\n'));
                return Ok((base + start..base + end, fields));
            }
            _ => state = State::Plain,
        }
    }
    fields.push(base + field..base + raw.len());
    Ok((base + start..base + raw.len(), fields))
}

fn decode(
    record: &csv::ByteRecord,
    columns: &[usize],
    expected: usize,
    row: usize,
    values: &mut [f64],
) -> Result<()> {
    ensure!(
        record.len() == expected,
        "CSV row {} has an unexpected number of columns",
        row + 1
    );
    let time = std::str::from_utf8(record.get(columns[0]).context("missing time column")?)?
        .parse::<f64>()
        .with_context(|| format!("invalid time in CSV row {}", row + 1))?;
    ensure!(time.is_finite(), "non-finite time in CSV row {}", row + 1);
    values[0] = time;
    for (out, &column) in values[1..].iter_mut().zip(&columns[1..]) {
        *out = std::str::from_utf8(record.get(column).unwrap_or_default())
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(f64::NAN);
    }
    Ok(())
}

fn store(source: Arc<SourceCsv>, meta: &Metadata) -> Result<DataStore> {
    let table = Arc::new(SourceTable {
        source,
        rows: meta.rows,
        stride: meta.stride,
        offsets: Arc::new(meta.offsets.clone()),
        indexed_bytes: meta.next_offset,
    });
    let source_bytes = table.source.stamp.len;
    let signals = meta
        .names
        .iter()
        .zip(&meta.indexes)
        .enumerate()
        .map(|(i, (name, index))| {
            let index = index.finish()?;
            ensure!(index.records() == meta.rows, "CSV index row count mismatch");
            Ok(Signal {
                name: name.clone(),
                points: PointSeries::new(table.clone(), i + 1, index),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut data = DataStore::from_signals(signals, meta.time_min, meta.time_max, meta.rows);
    data.storage = "original CSV + compact index".into();
    data.csv_rows = Some(table);
    data.csv_extent = Some(CsvExtent {
        complete: meta.complete,
        indexed_bytes: meta.next_offset,
        source_bytes,
    });
    Ok(data)
}

fn validate_metadata(meta: &Metadata, stamp: &SourceStamp) -> Result<()> {
    ensure!(
        meta.rows > 0
            && meta.rows <= isize::MAX as usize
            && meta.next_offset <= stamp.len
            && meta.next_offset > 0
            && (!meta.complete || meta.next_offset == stamp.len),
        "invalid CSV indexed extent"
    );
    ensure!(
        meta.rows > 0
            && meta.names.len() == meta.indexes.len()
            && meta.columns.len() == meta.names.len() + 1,
        "invalid CSV index columns"
    );
    ensure!(
        meta.time_min.is_finite() && meta.time_max.is_finite() && meta.time_min <= meta.time_max,
        "invalid CSV index bounds"
    );
    ensure!(
        meta.block_rows > 0
            && meta.block_rows <= 8192
            && meta.stride >= meta.block_rows
            && meta.stride % meta.block_rows == 0,
        "invalid CSV checkpoint stride"
    );
    ensure!(
        meta.offsets.len() == meta.rows.div_ceil(meta.stride)
            && meta.offsets.len() <= MAX_CHECKPOINTS + 1,
        "invalid CSV checkpoint count"
    );
    ensure!(
        meta.columns.iter().all(|&c| c < meta.source_columns),
        "invalid CSV source column"
    );
    ensure!(
        meta.offsets.iter().all(|&p| p < meta.next_offset)
            && meta.offsets.windows(2).all(|w| w[0] < w[1]),
        "invalid CSV seek offsets"
    );
    for index in &meta.indexes {
        ensure!(
            index.finish()?.records() == meta.rows,
            "CSV index row count mismatch"
        );
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static READ_RANGES: std::cell::RefCell<Option<Vec<std::ops::Range<u64>>>> = const { std::cell::RefCell::new(None) };
}

/// Source input never reports an artificial EOF at a record-size limit. An
/// oversized header or record is an actionable error, not a truncated CSV row.
struct RecordLimitedFile {
    file: File,
    position: u64,
    len: u64,
    record_start: u64,
}
impl RecordLimitedFile {
    fn new(file: File, len: u64) -> Self {
        Self {
            file,
            position: 0,
            len,
            record_start: 0,
        }
    }
}
impl Read for RecordLimitedFile {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position == self.len {
            return Ok(0);
        }
        let remaining = self
            .record_start
            .saturating_add(MAX_RECORD_BYTES)
            .saturating_sub(self.position);
        if remaining == 0 {
            return Err(io::Error::other(
                "CSV record exceeds 1 MiB; shorten the record before continuing",
            ));
        }
        let allowed = output.len().min(remaining as usize);
        let read = self.file.read(&mut output[..allowed])?;
        #[cfg(test)]
        if read > 0 {
            READ_RANGES.with(|reads| {
                if let Some(reads) = reads.borrow_mut().as_mut() {
                    reads.push(self.position..self.position + read as u64);
                }
            });
        }
        self.position += read as u64;
        Ok(read)
    }
}
impl Seek for RecordLimitedFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let position = self.file.seek(position)?;
        self.position = position;
        self.record_start = position;
        Ok(position)
    }
}

fn reader(file: File, cfg: &AppConfig, len: u64, headers: bool) -> csv::Reader<RecordLimitedFile> {
    csv::ReaderBuilder::new()
        .has_headers(headers)
        // Check field counts ourselves, including after seeking. The first row
        // at a checkpoint must not become a new implicit schema.
        .flexible(true)
        .delimiter(cfg.csv.delimiter as u8)
        .buffer_capacity(READ_BUFFER_BYTES)
        .from_reader(RecordLimitedFile::new(file, len))
}

fn resume_reader(
    file: File,
    cfg: &AppConfig,
    stamp: &SourceStamp,
    meta: &Metadata,
    offset: u64,
) -> Result<csv::Reader<RecordLimitedFile>> {
    let mut reader = reader(file, cfg, stamp.len, false);
    // csv::Reader::seek otherwise reads the header at byte zero first. The
    // already validated metadata supplies that schema, so no prefix read is needed.
    reader.set_byte_headers(csv::ByteRecord::new());
    let mut position = csv::Position::new();
    position.set_byte(offset).set_record(meta.rows as u64);
    reader.seek_raw(SeekFrom::Start(offset), position)?;
    Ok(reader)
}

type ResumeRegistry = Mutex<HashMap<String, Weak<SourceCsv>>>;
fn registry() -> &'static ResumeRegistry {
    static REGISTRY: OnceLock<ResumeRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}
fn remember(key: String, source: &Arc<SourceCsv>) {
    let incoming = source
        .resume
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(extent_order);
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    registry.retain(|_, source| source.strong_count() > 0);
    if let Some(existing) = registry.get(&key).and_then(Weak::upgrade) {
        let previous = existing
            .resume
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(extent_order);
        if previous > incoming {
            return;
        }
    }
    registry.insert(key, Arc::downgrade(source));
}

fn extent_order(meta: &Metadata) -> (usize, u64, bool) {
    (meta.rows, meta.next_offset, meta.complete)
}

fn retain_resume(source: &SourceCsv, meta: &Metadata) {
    let mut current = source.resume.lock().unwrap_or_else(|e| e.into_inner());
    if current
        .as_ref()
        .is_none_or(|old| extent_order(meta) > extent_order(old))
    {
        *current = Some(meta.clone());
    }
}

/// Serialize publication, including the length comparison. A cancelled older
/// worker can finish after a newer request; it must not roll the index back.
fn publish_metadata(
    root: &Path,
    key: &str,
    stamp: &SourceStamp,
    meta: &Metadata,
) -> Result<Metadata> {
    static WRITER: Mutex<()> = Mutex::new(());
    let _guard = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let old = cache::lookup(root, key);
    if let Some(old) = old.as_ref() {
        if let Ok(existing) = read_metadata(old, stamp) {
            if extent_order(&existing) >= extent_order(meta) {
                return Ok(existing);
            }
        }
    }
    let dir = cache::staging(root)?;
    let file = File::create(dir.path().join("index.json"))?;
    let mut output = BufWriter::new(cache::LimitedWriter::new(file, MAX_INDEX_BYTES));
    serde_json::to_writer(&mut output, meta)?;
    output.flush()?;
    cache::freeze(output.get_ref().inner())?;
    drop(output);
    let published = cache::publish(root, key, dir)?;
    // Metadata is deserialized, not mapped: retire the old snapshot once the
    // pointer has atomically moved, without accumulating an entry for every page.
    if let Some(old) = old.filter(|old| old != &published) {
        let _ = std::fs::remove_dir_all(old);
    }
    Ok(meta.clone())
}

fn read_metadata(directory: &Path, stamp: &SourceStamp) -> Result<Metadata> {
    let file = File::open(directory.join("index.json"))?;
    ensure!(
        file.metadata()?.len() <= MAX_INDEX_BYTES,
        "CSV index too large"
    );
    let meta: Metadata = serde_json::from_reader(BufReader::new(file))?;
    validate_metadata(&meta, stamp)?;
    Ok(meta)
}

fn retain_prefix(
    source: &Arc<SourceCsv>,
    meta: &Metadata,
    root: &Path,
    key: &str,
    memory_key: String,
) -> Result<DataStore> {
    source.validate()?;
    validate_metadata(meta, &source.stamp)?;
    let mut data = store(source.clone(), meta)?;
    // The prefix itself remains valid if a later row fails or the scan is
    // cancelled. Persistence is bounded metadata work and does not parse more CSV.
    match publish_metadata(root, key, &source.stamp, meta) {
        Ok(saved) => retain_resume(source, &saved),
        Err(error) => {
            data.storage = format!("original CSV; index kept in RAM ({error:#})");
            retain_resume(source, meta);
        }
    }
    remember(memory_key, source);
    Ok(data)
}

/// Explicit full indexing. Shares and extends the same prefix cache as bounded
/// row requests, but continues until true EOF unless cancelled or invalid.
pub fn load(
    cfg: &AppConfig,
    root: &Path,
    progress: &LoadProgress,
    update: impl FnMut(DataStore),
) -> Result<DataStore> {
    load_request(cfg, root, progress, None, update)
}

/// Index only the requested prefix, pausing at the interactive byte budget even
/// if the requested row is farther away. Existing longer prefixes are reused.
pub fn load_rows(
    cfg: &AppConfig,
    root: &Path,
    progress: &LoadProgress,
    target_rows: usize,
    update: impl FnMut(DataStore),
) -> Result<DataStore> {
    ensure!(
        target_rows <= isize::MAX as usize,
        "CSV row target exceeds the platform limit"
    );
    load_request(cfg, root, progress, Some(target_rows.max(1)), update)
}

fn initial_metadata(
    reader: &mut csv::Reader<RecordLimitedFile>,
    cfg: &AppConfig,
    stamp: &SourceStamp,
) -> Result<Metadata> {
    let first = reader
        .byte_headers()
        .context("read CSV header (maximum record size is 1 MiB)")?;
    let headers: Vec<String> = if cfg.csv.has_header {
        first
            .iter()
            .map(|v| String::from_utf8(v.to_vec()).context("CSV column names must be UTF-8"))
            .collect::<Result<_>>()?
    } else {
        (0..first.len()).map(|i| format!("c{i}")).collect()
    };
    let time = headers.iter().position(|v| v == &cfg.csv.time_column).with_context(||
        format!("Time column '{}' not found. For a headerless CSV turn off 'Has header row' and use c0, then Reload CSV.", cfg.csv.time_column))?;
    let mut columns = vec![time];
    let mut names = Vec::new();
    for (i, name) in headers.iter().enumerate() {
        if i != time
            && cfg
                .csv
                .include_signals
                .as_ref()
                .is_none_or(|s| s.contains(name))
        {
            columns.push(i);
            names.push(name.clone());
        }
    }
    let row_bytes = columns
        .len()
        .checked_mul(8)
        .context("CSV row size overflows")?;
    ensure!(
        row_bytes <= 1024 * 1024,
        "CSV selected row exceeds 1 MiB; select fewer signals"
    );
    let block_rows = (1024 * 1024 / row_bytes).clamp(1, 8192);
    let upper =
        usize::try_from(stamp.len / headers.len().max(1) as u64 + 1).context("CSV too large")?;
    ensure!(
        upper <= isize::MAX as usize,
        "CSV exceeds the platform row limit"
    );
    let stride = upper
        .div_ceil(MAX_CHECKPOINTS)
        .max(block_rows)
        .div_ceil(block_rows)
        .checked_mul(block_rows)
        .context("CSV checkpoint stride overflows")?;
    let indexes = names
        .iter()
        .map(|_| IndexBuilder::with_max_leaves(upper, (32768 / names.len().max(1)).max(1)))
        .collect();
    Ok(Metadata {
        next_offset: if cfg.csv.has_header {
            reader.position().byte()
        } else {
            0
        },
        complete: false,
        rows: 0,
        names,
        columns,
        source_columns: headers.len(),
        block_rows,
        stride,
        offsets: Vec::new(),
        indexes,
        time_min: f64::INFINITY,
        time_max: f64::NEG_INFINITY,
    })
}

fn load_request(
    cfg: &AppConfig,
    root: &Path,
    progress: &LoadProgress,
    target_rows: Option<usize>,
    mut update: impl FnMut(DataStore),
) -> Result<DataStore> {
    let path = cfg
        .data_path
        .as_ref()
        .context("data_path missing in config")?;
    ensure!(
        cfg.csv.delimiter.is_ascii(),
        "CSV delimiter must be an ASCII character"
    );
    let file = File::open(path)?;
    let stamp = SourceStamp::read(path, &file.metadata()?)?;
    let key = stamp.key(INDEX_KIND, &cfg.csv.parse_options())?;
    let memory_key = serde_json::to_string(&(
        root.as_os_str().as_encoded_bytes(),
        &key,
        cfg.csv.ram_cache_mib,
    ))?;
    progress.set_total(stamp.len);
    progress.check()?;
    let existing = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&memory_key)
        .and_then(Weak::upgrade);
    let mut source = None;
    let mut cached = None;
    if let Some(existing) = existing {
        let meta = existing
            .resume
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(meta) = meta {
            if validate_metadata(&meta, &stamp).is_ok() && existing.validate().is_ok() {
                source = Some(existing);
                cached = Some(meta);
            }
        }
    }
    if cached.is_none() {
        if let Some(directory) = cache::lookup(root, &key) {
            cached = read_metadata(&directory, &stamp).ok();
        }
    }
    if let Some(meta) = cached.as_ref() {
        if meta.complete || target_rows.is_some_and(|target| meta.rows >= target) {
            let source = match source {
                Some(source) => source,
                None => SourceCsv::open(cfg, stamp.clone(), meta)?,
            };
            source.validate()?;
            let data = store(source.clone(), meta)?;
            retain_resume(&source, meta);
            remember(memory_key, &source);
            progress.cache_hit.store(true, Ordering::Relaxed);
            progress.advance(meta.next_offset);
            return Ok(data);
        }
    }
    let (mut reader, mut meta) = if let Some(meta) = cached {
        let reader = resume_reader(file, cfg, &stamp, &meta, meta.next_offset)?;
        (reader, meta)
    } else {
        let mut reader = reader(file, cfg, stamp.len, cfg.csv.has_header);
        let meta = initial_metadata(&mut reader, cfg, &stamp)?;
        (reader, meta)
    };
    let source = match source {
        Some(source) => source,
        None => SourceCsv::open(cfg, stamp.clone(), &meta)?,
    };
    // Count the cold header toward the same request budget.
    let initial_offset = if meta.rows == 0 { 0 } else { meta.next_offset };
    progress.advance(initial_offset);
    let mut record = csv::ByteRecord::new();
    let mut values = vec![0.; meta.columns.len()];
    let mut block_start = meta.rows;
    let mut block = Vec::with_capacity(meta.block_rows * values.len());
    let mut last = Instant::now();
    let mut sent = false;
    let initial_rows = meta.rows;
    let result = (|| -> Result<()> {
        while target_rows.is_none_or(|target| {
            meta.rows < target && meta.next_offset.saturating_sub(initial_offset) < REQUEST_BYTES
        }) {
            progress.check()?;
            // Headerless byte_headers() caches the first record. Preserve its start
            // for the first replay as well as for the actual source reader.
            reader.get_mut().record_start = meta.next_offset;
            if !reader
                .read_byte_record(&mut record)
                .with_context(|| format!("read CSV row {}", meta.rows + 1))?
            {
                meta.complete = true;
                meta.next_offset = stamp.len;
                break;
            }
            decode(
                &record,
                &meta.columns,
                meta.source_columns,
                meta.rows,
                &mut values,
            )?;
            if meta.rows % meta.stride == 0 {
                meta.offsets.push(
                    record
                        .position()
                        .context("CSV record position missing")?
                        .byte(),
                );
            }
            meta.time_min = meta.time_min.min(values[0]);
            meta.time_max = meta.time_max.max(values[0]);
            for (index, &value) in meta.indexes.iter_mut().zip(&values[1..]) {
                index.push(value.is_finite().then_some(Sample {
                    record: meta.rows,
                    byte_offset: 0,
                    point: [values[0], value],
                }));
            }
            // A resumed partial block does not contain its earlier values. Leave it
            // to the bounded point reader instead of inserting a misaligned cache.
            if block_start % meta.block_rows == 0 {
                block.extend_from_slice(&values);
            }
            meta.rows = meta
                .rows
                .checked_add(1)
                .context("CSV row count overflows")?;
            meta.next_offset = reader.position().byte();
            meta.complete = meta.next_offset == stamp.len;
            progress.advance(meta.next_offset);
            if meta.rows % meta.block_rows == 0 {
                if !block.is_empty() {
                    source.insert(
                        block_start,
                        std::mem::replace(
                            &mut block,
                            Vec::with_capacity(meta.block_rows * values.len()),
                        ),
                    );
                }
                block_start = meta.rows;
                if !sent || last.elapsed() >= Duration::from_millis(500) {
                    source.validate()?;
                    update(store(source.clone(), &meta)?);
                    sent = true;
                    last = Instant::now();
                    progress.check()?;
                }
            }
            if meta.complete {
                break;
            }
        }
        ensure!(meta.rows > 0, "CSV has no data rows");
        source.validate()?;
        progress.check()?;
        Ok(())
    })();
    if !block.is_empty() {
        source.insert(block_start, block);
    }
    if let Err(error) = result {
        if meta.rows > initial_rows {
            if let Ok(prefix) = retain_prefix(&source, &meta, root, &key, memory_key) {
                update(prefix);
            }
        }
        return Err(error);
    }
    retain_prefix(&source, &meta, root, &key, memory_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(path: &Path) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.data_path = Some(path.into());
        cfg.csv.ram_cache_mib = 1;
        cfg.csv.include_signals = Some(vec!["value".into()]);
        cfg
    }

    fn fixture(path: &Path, rows: usize) {
        let mut writer = BufWriter::new(File::create(path).unwrap());
        writeln!(writer, "note,time,value").unwrap();
        for row in 0..rows {
            // CSV record boundaries are not physical newline boundaries.
            writeln!(
                writer,
                "\"quoted, multiline\nrow \"\"{row}\"\"\",{},{}",
                row % 256,
                row * 3
            )
            .unwrap();
        }
        writer.flush().unwrap();
    }

    fn metadata(cfg: &AppConfig, root: &Path) -> (SourceStamp, Metadata, PathBuf) {
        let path = cfg.data_path.as_ref().unwrap();
        let stamp = SourceStamp::read(path, &path.metadata().unwrap()).unwrap();
        let key = stamp.key(INDEX_KIND, &cfg.csv.parse_options()).unwrap();
        let directory = cache::lookup(root, &key).unwrap();
        let meta =
            serde_json::from_reader(File::open(directory.join("index.json")).unwrap()).unwrap();
        (stamp, meta, directory)
    }

    fn capture_reads() {
        READ_RANGES.with(|reads| *reads.borrow_mut() = Some(Vec::new()));
    }
    fn captured_reads() -> Vec<std::ops::Range<u64>> {
        READ_RANGES.with(|reads| reads.borrow_mut().take().unwrap())
    }

    #[test]
    fn row_provenance_preserves_raw_quotes_empty_fields_and_crlf() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.csv");
        let root = temp.path().join("cache");
        let header = b"note,b,time,a\r\n";
        let first = b"\"quoted, \"\"word\"\"\nnext\",,3,\"4\"\r\n";
        let second = b"plain\"quote,5,1,6\n";
        let bytes = [header.as_slice(), first.as_slice(), second.as_slice()].concat();
        std::fs::write(&path, &bytes).unwrap();
        let mut cfg = config(&path);
        cfg.csv.include_signals = Some(vec!["a".into(), "note".into(), "b".into()]);
        let data = load_rows(&cfg, &root, &LoadProgress::default(), 1, |_| {}).unwrap();
        assert_eq!(
            data.signals
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["note", "b", "a"]
        );
        let rows = data.csv_rows.as_ref().unwrap();
        let cancel = AtomicBool::new(false);
        let row = rows.row(0, &cancel).unwrap();
        assert_eq!(row.index, 0);
        assert_eq!(row.range, header.len()..header.len() + first.len());
        assert_eq!(&bytes[row.fields[0].clone()], b"3");
        assert_eq!(
            &bytes[row.fields[1].clone()],
            b"\"quoted, \"\"word\"\"\nnext\""
        );
        assert!(row.fields[2].is_empty());
        assert_eq!(&bytes[row.fields[3].clone()], b"\"4\"");
        assert_eq!(row.values[0], 3.);
        assert!(row.values[1].is_nan() && row.values[2].is_nan());
        assert_eq!(row.values[3], 4.);
        for offset in 0..header.len() {
            assert!(rows.find_byte(offset, &cancel).unwrap().is_none());
        }
        // Every original byte of this row, including both CRLF bytes, maps
        // back to row zero. Separators and terminators identify no cell.
        for offset in row.range.clone() {
            assert_eq!(rows.find_byte(offset, &cancel).unwrap().unwrap().index, 0);
        }
        for offset in [row.fields[1].end, row.range.end - 2, row.range.end - 1] {
            assert!(!row.fields.iter().any(|field| field.contains(&offset)));
        }
        assert!(rows.row(1, &cancel).is_err());
        assert!(rows.find_byte(row.range.end, &cancel).unwrap().is_none());
        assert!(rows.find_byte(bytes.len(), &cancel).unwrap().is_none());
        let extended = load_rows(&cfg, &root, &LoadProgress::default(), 2, |_| {}).unwrap();
        let next = extended.csv_rows.as_ref().unwrap().row(1, &cancel).unwrap();
        assert_eq!(&bytes[next.fields[1].clone()], b"plain\"quote");
        assert_eq!(next.range, row.range.end..bytes.len());
    }

    #[test]
    fn provenance_handles_headerless_bom_custom_delimiters_blank_lines_and_eof() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.csv");
        let bytes = b"\xef\xbb\xbf\r\n0;\"5;6\";7\r\n\r\n\n1;;\"9\"";
        std::fs::write(&path, bytes).unwrap();
        let mut cfg = config(&path);
        cfg.csv.has_header = false;
        cfg.csv.time_column = "c0".into();
        cfg.csv.delimiter = ';';
        cfg.csv.include_signals = None;
        let data = load(
            &cfg,
            &temp.path().join("cache"),
            &LoadProgress::default(),
            |_| {},
        )
        .unwrap();
        let rows = data.csv_rows.as_ref().unwrap();
        let cancel = AtomicBool::new(false);
        let first = rows.row(0, &cancel).unwrap();
        let second = rows.row(1, &cancel).unwrap();
        assert_eq!(&bytes[first.range.clone()], b"0;\"5;6\";7\r\n");
        assert_eq!(&bytes[first.fields[1].clone()], b"\"5;6\"");
        assert_eq!(&bytes[second.range.clone()], b"1;;\"9\"");
        assert!(second.fields[1].is_empty());
        assert_eq!(second.values[2], 9.);
        for offset in (0..first.range.start).chain(first.range.end..second.range.start) {
            assert!(
                rows.find_byte(offset, &cancel).unwrap().is_none(),
                "offset {offset}"
            );
        }
        assert_eq!(
            rows.find_byte(second.range.end - 1, &cancel)
                .unwrap()
                .unwrap()
                .index,
            1
        );
    }

    #[test]
    fn provenance_seeks_late_checkpoints_and_keeps_old_prefixes_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.csv");
        let root = temp.path().join("cache");
        fixture(&path, 40_000);
        let cfg = config(&path);
        let prefix = load_rows(&cfg, &root, &LoadProgress::default(), 17, |_| {}).unwrap();
        let full = load(&cfg, &root, &LoadProgress::default(), |_| {}).unwrap();
        let (_, meta, _) = metadata(&cfg, &root);
        let rows = full.csv_rows.as_ref().unwrap();
        let cancel = AtomicBool::new(false);
        capture_reads();
        let row = rows.row(30_000, &cancel).unwrap();
        let reads = captured_reads();
        let checkpoint = meta.offsets[30_000 / meta.stride];
        assert!(!reads.is_empty());
        assert!(reads.iter().all(|range| range.start >= checkpoint));
        assert_eq!(row.values, [48., 90_000.]);
        capture_reads();
        let found = rows
            .find_byte(row.fields[1].start, &cancel)
            .unwrap()
            .unwrap();
        let reads = captured_reads();
        assert!(!reads.is_empty());
        assert!(reads.iter().all(|range| range.start >= checkpoint));
        assert_eq!(found.index, row.index);
        assert_eq!(found.fields, row.fields);
        assert_eq!(found.range, row.range);
        capture_reads();
        let old = prefix.csv_rows.as_ref().unwrap();
        assert!(old.find_byte(row.range.start, &cancel).unwrap().is_none());
        assert!(old.row(17, &cancel).is_err());
        assert!(captured_reads().is_empty());
        assert_eq!(old.row(16, &cancel).unwrap().values, [16., 48.]);
        assert_eq!(metadata(&cfg, &root).1.offsets, meta.offsets);
    }

    #[test]
    fn crlf_at_checkpoint_boundary_belongs_to_the_preceding_row() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.csv");
        let root = temp.path().join("cache");
        let mut output = BufWriter::new(File::create(&path).unwrap());
        write!(output, "time,value\r\n").unwrap();
        for row in 0..9000 {
            write!(output, "{row},{row}\r\n").unwrap();
        }
        output.flush().unwrap();
        let cfg = config(&path);
        let data = load(&cfg, &root, &LoadProgress::default(), |_| {}).unwrap();
        let (_, meta, _) = metadata(&cfg, &root);
        let rows = data.csv_rows.as_ref().unwrap();
        let cancel = AtomicBool::new(false);
        let previous = rows.row(meta.stride - 1, &cancel).unwrap();
        let next = rows.row(meta.stride, &cancel).unwrap();
        assert_eq!(previous.range.end, next.range.start);
        assert_eq!(
            rows.find_byte(previous.range.end - 1, &cancel)
                .unwrap()
                .unwrap()
                .index,
            meta.stride - 1
        );
        assert_eq!(
            rows.find_byte(next.range.start, &cancel)
                .unwrap()
                .unwrap()
                .index,
            meta.stride
        );
    }

    #[test]
    fn row_provenance_is_cancellable_and_rejects_source_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.csv");
        fixture(&path, 30);
        let cfg = config(&path);
        let data = load_rows(
            &cfg,
            &temp.path().join("cache"),
            &LoadProgress::default(),
            17,
            |_| {},
        )
        .unwrap();
        let rows = data.csv_rows.as_ref().unwrap();
        let cancelled = AtomicBool::new(true);
        capture_reads();
        assert!(rows
            .row(3, &cancelled)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(rows
            .find_byte(100, &cancelled)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(captured_reads().is_empty());
        let cancel = AtomicBool::new(false);
        let old = rows.row(3, &cancel).unwrap();
        fixture(&path, 31);
        assert!(rows
            .row(3, &cancel)
            .unwrap_err()
            .to_string()
            .contains("changed"));
        assert!(rows
            .find_byte(old.range.start, &cancel)
            .unwrap_err()
            .to_string()
            .contains("changed"));
    }

    #[test]
    fn csv_plot_samples_retain_row_identity_for_duplicate_unsorted_x() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.csv");
        std::fs::write(&path, b"time,value\n2,20\n1,10\n2,21\n0,0\n").unwrap();
        let cfg = config(&path);
        let data = load(
            &cfg,
            &temp.path().join("cache"),
            &LoadProgress::default(),
            |_| {},
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let points = &data.signals[0].points;
        let expected = [
            (0, [2., 20.]),
            (1, [1., 10.]),
            (2, [2., 21.]),
            (3, [0., 0.]),
        ];
        for _ in 0..2 {
            let samples = points
                .samples_for_range_cancel(0., 2., 100, &cancel)
                .unwrap();
            assert_eq!(
                samples
                    .iter()
                    .map(|sample| (sample.record, sample.point))
                    .collect::<Vec<_>>(),
                expected
            );
            for sample in samples {
                assert_eq!(
                    data.csv_rows
                        .as_ref()
                        .unwrap()
                        .row(sample.record, &cancel)
                        .unwrap()
                        .values,
                    sample.point
                );
            }
        }
        let repeated = points
            .samples_for_range_cancel(2., 2., 100, &cancel)
            .unwrap();
        assert_eq!(
            repeated
                .iter()
                .map(|sample| sample.record)
                .collect::<Vec<_>>(),
            [0, 2]
        );
    }

    #[test]
    fn demand_open_stops_before_invalid_tail_and_preserves_truthful_extent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 20);
        writeln!(File::options().append(true).open(&path).unwrap(), "x,bad,1").unwrap();
        let cfg = config(&path);
        let prefix = load_rows(&cfg, &root, &LoadProgress::default(), 10, |_| {}).unwrap();
        assert_eq!(prefix.rows, 10);
        let extent = prefix.csv_extent.unwrap();
        assert!(!extent.complete);
        assert!(extent.indexed_bytes < extent.source_bytes);
        assert_eq!(prefix.signals[0].points.point(9), [9., 27.]);
        let later = load_rows(&cfg, &root, &LoadProgress::default(), 100, |_| {}).unwrap_err();
        assert!(format!("{later:#}").contains("invalid time in CSV row 21"));
        assert_eq!(metadata(&cfg, &root).1.rows, 20);
        let warm_progress = LoadProgress::default();
        let warm = load_rows(&cfg, &root, &warm_progress, 10, |_| {
            panic!("cached prefix updated")
        })
        .unwrap();
        assert_eq!(warm.rows, 20);
        assert!(!warm.csv_extent.unwrap().complete);
        assert!(warm_progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(
            warm_progress.done.load(Ordering::Relaxed),
            warm.csv_extent.unwrap().indexed_bytes
        );
    }

    #[test]
    fn demand_reopen_resumes_at_record_boundary_without_rereading_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("quoted.csv");
        let root = temp.path().join("cache");
        fixture(&path, 250);
        let cfg = config(&path);
        let first = load_rows(&cfg, &root, &LoadProgress::default(), 13, |_| {}).unwrap();
        let next = first.csv_extent.unwrap().indexed_bytes;
        drop(first); // Only the persisted metadata survives reopening.
        capture_reads();
        let second = load_rows(&cfg, &root, &LoadProgress::default(), 37, |_| {}).unwrap();
        let reads = captured_reads();
        assert!(!reads.is_empty());
        assert!(
            reads.iter().all(|range| range.start >= next),
            "prefix was reread: {reads:?}"
        );
        assert_eq!(second.rows, 37);
        assert_eq!(second.signals[0].points.point(0), [0., 0.]);
        assert_eq!(second.signals[0].points.point(13), [13., 39.]);
        assert_eq!(second.signals[0].points.point(36), [36., 108.]);
        let (_, meta, _) = metadata(&cfg, &root);
        assert_eq!(meta.rows, 37);
        assert_eq!(meta.offsets.len(), 1);
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter(|entry| entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("entry-"))
                .count(),
            1
        );
    }

    #[test]
    fn demand_byte_budget_and_record_limit_bound_large_text_work() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("wide.csv");
        let root = temp.path().join("cache");
        let mut writer = BufWriter::new(File::create(&path).unwrap());
        writeln!(writer, "note,time,value").unwrap();
        let note = "x".repeat(512 * 1024);
        for row in 0..40 {
            writeln!(writer, "\"{note}\",{row},{row}").unwrap();
        }
        writer.flush().unwrap();
        drop(writer);
        let cfg = config(&path);
        capture_reads();
        let first = load_rows(&cfg, &root, &LoadProgress::default(), 40, |_| {}).unwrap();
        let reads = captured_reads();
        assert!(first.rows > 0 && first.rows < 40);
        assert!(!first.csv_extent.unwrap().complete);
        assert!(
            reads
                .iter()
                .map(|range| range.end - range.start)
                .sum::<u64>()
                <= REQUEST_BYTES + MAX_RECORD_BYTES + READ_BUFFER_BYTES as u64
        );
        let second = load_rows(&cfg, &root, &LoadProgress::default(), 40, |_| {}).unwrap();
        assert!(second.rows > first.rows);
        // Oversized quoted records fail instead of treating the byte limit as EOF.
        let huge = temp.path().join("oversized.csv");
        let mut writer = BufWriter::new(File::create(&huge).unwrap());
        writeln!(writer, "note,time,value\nsmall,0,0").unwrap();
        writeln!(
            writer,
            "\"{}\",1,1",
            "x".repeat(MAX_RECORD_BYTES as usize + 10)
        )
        .unwrap();
        writer.flush().unwrap();
        let cfg = config(&huge);
        let prefix = load_rows(&cfg, &root, &LoadProgress::default(), 1, |_| {}).unwrap();
        assert_eq!(prefix.rows, 1);
        capture_reads();
        let error = load_rows(&cfg, &root, &LoadProgress::default(), 2, |_| {}).unwrap_err();
        let reads = captured_reads();
        assert!(format!("{error:#}").contains("CSV record exceeds 1 MiB"));
        assert!(
            reads
                .iter()
                .map(|range| range.end - range.start)
                .sum::<u64>()
                <= MAX_RECORD_BYTES
        );
        assert_eq!(metadata(&cfg, &root).1.rows, 1);
    }

    #[test]
    fn explicit_full_scan_extends_prefix_and_complete_cache_needs_no_source_reads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 1000);
        let cfg = config(&path);
        let prefix = load_rows(&cfg, &root, &LoadProgress::default(), 17, |_| {}).unwrap();
        let next = prefix.csv_extent.unwrap().indexed_bytes;
        drop(prefix);
        capture_reads();
        let complete = load(&cfg, &root, &LoadProgress::default(), |_| {}).unwrap();
        assert!(captured_reads().iter().all(|range| range.start >= next));
        assert_eq!(complete.rows, 1000);
        let extent = complete.csv_extent.unwrap();
        assert!(extent.complete);
        assert_eq!(extent.indexed_bytes, path.metadata().unwrap().len());
        drop(complete);
        capture_reads();
        let progress = LoadProgress::default();
        let cached = load_rows(&cfg, &root, &progress, 10, |_| {
            panic!("complete cache scanned")
        })
        .unwrap();
        assert!(captured_reads().is_empty());
        assert_eq!(cached.rows, 1000);
        assert!(progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(cached.signals[0].points.point(999), [231., 2997.]);
    }

    #[test]
    fn partial_cache_invalidates_source_options_and_corrupt_resume_offsets() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 100);
        let cfg = config(&path);
        drop(load_rows(&cfg, &root, &LoadProgress::default(), 17, |_| {}).unwrap());
        let (_, mut meta, directory) = metadata(&cfg, &root);
        meta.next_offset = path.metadata().unwrap().len() + 1;
        let mut replacement = tempfile::NamedTempFile::new_in(&directory).unwrap();
        serde_json::to_writer(&mut replacement, &meta).unwrap();
        replacement.persist(directory.join("index.json")).unwrap();
        let progress = LoadProgress::default();
        let rebuilt = load_rows(&cfg, &root, &progress, 7, |_| {}).unwrap();
        assert!(!progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(rebuilt.rows, 7);
        let mut filtered = cfg.clone();
        filtered.csv.include_signals = Some(Vec::new());
        let selected = load_rows(&filtered, &root, &LoadProgress::default(), 9, |_| {}).unwrap();
        assert_eq!(selected.rows, 9);
        assert!(selected.signals.is_empty());
        fixture(&path, 3);
        let changed = load_rows(&cfg, &root, &LoadProgress::default(), 17, |_| {}).unwrap();
        assert_eq!(changed.rows, 3);
        assert!(changed.csv_extent.unwrap().complete);
        assert!(rebuilt.signals[0].points.for_range(0., 5., 10).is_err());
    }

    #[test]
    fn cancelled_extension_and_unwritable_cache_keep_the_existing_prefix_usable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 20_000);
        let cfg = config(&path);
        let prefix = load_rows(&cfg, &root, &LoadProgress::default(), 17, |_| {}).unwrap();
        let progress = LoadProgress::default();
        let result = load_rows(&cfg, &root, &progress, 20_000, |_| {
            progress.cancelled.store(true, Ordering::Relaxed)
        });
        assert!(result.is_err());
        assert_eq!(metadata(&cfg, &root).1.rows, 8192);
        assert_eq!(prefix.signals[0].points.point(16), [16., 48.]);
        let unavailable = temp.path().join("not-a-directory");
        std::fs::write(&unavailable, b"file").unwrap();
        let first = load_rows(&cfg, &unavailable, &LoadProgress::default(), 17, |_| {}).unwrap();
        assert!(first.storage.contains("index kept in RAM"));
        capture_reads();
        let second = load_rows(&cfg, &unavailable, &LoadProgress::default(), 37, |_| {}).unwrap();
        assert!(captured_reads()
            .iter()
            .all(|range| range.start >= first.csv_extent.unwrap().indexed_bytes));
        assert_eq!(second.rows, 37);
        assert_eq!(second.signals[0].points.point(36), [36., 108.]);
    }

    #[test]
    fn demanded_headerless_rows_and_exact_eof_are_not_skipped_or_invented() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("headerless.csv");
        let root = temp.path().join("cache");
        std::fs::write(&path, b"0,5\n1,6\n2,7").unwrap();
        let mut cfg = config(&path);
        cfg.csv.has_header = false;
        cfg.csv.time_column = "c0".into();
        cfg.csv.include_signals = None;
        let first = load_rows(&cfg, &root, &LoadProgress::default(), 0, |_| {}).unwrap();
        assert_eq!(first.rows, 1);
        assert_eq!(first.signals[0].points.point(0), [0., 5.]);
        drop(first);
        let second = load_rows(&cfg, &root, &LoadProgress::default(), 3, |_| {}).unwrap();
        assert_eq!(second.rows, 3);
        assert_eq!(second.signals[0].points.point(2), [2., 7.]);
        assert!(second.csv_extent.unwrap().complete);
        assert!(load_rows(&cfg, &root, &LoadProgress::default(), usize::MAX, |_| {}).is_err());
    }

    #[test]
    fn older_cancelled_request_cannot_replace_a_newer_longer_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 20_000);
        let cfg = config(&path);
        let progress = LoadProgress::default();
        let mut advanced = false;
        let mut latest = None;
        let result = load_rows(&cfg, &root, &progress, 20_000, |_| {
            if !advanced {
                advanced = true;
                // The older worker is paused at its callback while a replacement
                // finishes. It then cancels and tries to preserve a shorter prefix.
                latest =
                    Some(load_rows(&cfg, &root, &LoadProgress::default(), 16_000, |_| {}).unwrap());
                progress.cancelled.store(true, Ordering::Relaxed);
            }
        });
        assert!(result.is_err());
        assert_eq!(latest.as_ref().unwrap().rows, 16_000);
        assert_eq!(metadata(&cfg, &root).1.rows, 16_000);
        let next = load_rows(&cfg, &root, &LoadProgress::default(), 1, |_| {
            panic!("longer prefix was lost")
        })
        .unwrap();
        assert_eq!(next.rows, 16_000);
        assert_eq!(next.signals[0].points.point(15_999), [127., 47_997.]);
    }

    #[test]
    fn malformed_checkpoint_row_preserves_only_the_valid_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 8192);
        writeln!(File::options().append(true).open(&path).unwrap(), "x,bad,1").unwrap();
        let cfg = config(&path);
        let mut prefix = None;
        let error = load(&cfg, &root, &LoadProgress::default(), |data| {
            prefix = Some(data)
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("row 8193"));
        let (stamp, meta, _) = metadata(&cfg, &root);
        validate_metadata(&meta, &stamp).unwrap();
        assert_eq!(meta.rows, 8192);
        assert_eq!(meta.offsets.len(), 1);
        assert!(!meta.complete);
        assert_eq!(prefix.unwrap().rows, 8192);
    }

    #[test]
    fn default_streams_without_numeric_disk_copy_and_warm_index_seeks_multiline_records() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("quoted.csv");
        let root = temp.path().join("cache");
        fixture(&path, 40_000);
        let cfg = config(&path);
        assert!(!cfg.csv.decoded_disk_cache);
        let progress = LoadProgress::default();
        let mut first = None;
        let data = super::super::csv::load_csv_progressive(&cfg, &root, &progress, |data| {
            assert!(
                !root.exists(),
                "partial default load must not write decoded data"
            );
            assert_eq!(data.signals[0].points.point(8191), [255., 24573.]);
            first = Some(data);
        })
        .unwrap();
        assert_eq!(data.rows, 40_000);
        assert_eq!(first.unwrap().rows, 8192);
        let (_, _, directory) = metadata(&cfg, &root);
        let files: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, ["index.json"]);
        assert!(
            directory.join("index.json").metadata().unwrap().len()
                < path.metadata().unwrap().len() / 5
        );
        let warm_progress = LoadProgress::default();
        let warm = load(&cfg, &root, &warm_progress, |_| {
            panic!("warm index should not scan source")
        })
        .unwrap();
        assert!(warm_progress.cache_hit.load(Ordering::Relaxed));
        for row in [8192, 16383, 16384, 39999, 0, 12345] {
            assert_eq!(
                warm.signals[0].points.point(row),
                [(row % 256) as f64, (row * 3) as f64]
            );
        }
        let expected: Vec<_> = (0..40_000)
            .filter(|r| (10..=11).contains(&(r % 256)))
            .map(|r| [(r % 256) as f64, (r * 3) as f64])
            .collect();
        assert_eq!(
            warm.signals[0].points.for_range(10., 11., 40_000).unwrap(),
            expected
        );
    }

    #[test]
    fn decoded_blocks_stay_under_ram_budget_and_evicted_rows_can_be_read_again() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.csv");
        let root = temp.path().join("cache");
        fixture(&path, 150_000);
        let cfg = config(&path);
        load(&cfg, &root, &LoadProgress::default(), |_| {}).unwrap();
        let (stamp, meta, _) = metadata(&cfg, &root);
        let source = SourceCsv::open(&cfg, stamp, &meta).unwrap();
        let cancel = AtomicBool::new(false);
        for row in (0..meta.rows).step_by(meta.block_rows) {
            assert_eq!(
                source
                    .point(row, 1, meta.rows, meta.stride, &meta.offsets, &cancel)
                    .unwrap(),
                [(row % 256) as f64, (row * 3) as f64]
            );
            assert!(source.reader.lock().unwrap().used <= 1024 * 1024);
        }
        assert!(!source.reader.lock().unwrap().blocks.contains_key(&0));
        assert_eq!(
            source
                .point(0, 1, meta.rows, meta.stride, &meta.offsets, &cancel)
                .unwrap(),
            [0., 0.]
        );
        assert!(source.reader.lock().unwrap().used <= 1024 * 1024);
    }

    #[test]
    fn cancellation_and_late_errors_preserve_the_latest_valid_prefix_cache() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 25_000);
        let cfg = config(&path);
        let progress = LoadProgress::default();
        let mut first = None;
        assert!(load(&cfg, &root, &progress, |data| {
            first = Some(data);
            progress.cancelled.store(true, Ordering::Relaxed);
        })
        .is_err());
        assert_eq!(metadata(&cfg, &root).1.rows, 8192);
        let first = first.unwrap();
        assert_eq!(first.rows, 8192);
        assert!(!first.signals[0]
            .points
            .for_range(0., 255., 100)
            .unwrap()
            .is_empty());
        // A malformed later row is never advertised as a complete file.
        let mut writer = File::options().append(true).open(&path).unwrap();
        writeln!(writer, "x,bad,1").unwrap();
        drop(writer);
        let mut prefix = None;
        let result = load(&cfg, &root, &LoadProgress::default(), |data| {
            prefix = Some(data)
        });
        assert!(format!("{:#}", result.unwrap_err()).contains("row 25001"));
        assert_eq!(prefix.unwrap().rows, 25_000);
        let (_, meta, _) = metadata(&cfg, &root);
        assert_eq!(meta.rows, 25_000);
        assert!(!meta.complete);
        // Cached render results also validate the original source before reuse.
        assert!(first.signals[0]
            .points
            .for_range(0., 255., 100)
            .unwrap_err()
            .to_string()
            .contains("changed"));
    }

    #[test]
    fn invalid_index_rebuilds_and_unwritable_cache_does_not_block_original_data() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 20);
        let cfg = config(&path);
        load(&cfg, &root, &LoadProgress::default(), |_| {}).unwrap();
        let (_, mut meta, directory) = metadata(&cfg, &root);
        meta.stride = 0;
        let mut replacement = tempfile::NamedTempFile::new_in(&directory).unwrap();
        serde_json::to_writer(&mut replacement, &meta).unwrap();
        replacement.persist(directory.join("index.json")).unwrap();
        let progress = LoadProgress::default();
        let data = load(&cfg, &root, &progress, |_| {}).unwrap();
        assert!(!progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(data.rows, 20);
        let not_a_directory = temp.path().join("unavailable-cache");
        std::fs::write(&not_a_directory, "file").unwrap();
        let data = load(&cfg, &not_a_directory, &LoadProgress::default(), |_| {}).unwrap();
        assert!(data.storage.contains("index kept in RAM"));
        assert_eq!(data.signals[0].points.point(19), [19., 57.]);
    }

    #[test]
    fn optional_full_cache_has_total_limit_and_defaults_back_to_original() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rows.csv");
        let root = temp.path().join("cache");
        fixture(&path, 20);
        let mut cfg = config(&path);
        cfg.csv.decoded_disk_cache = true;
        cfg.csv.decoded_cache_limit_mib = 1;
        let first =
            super::super::csv::load_csv_progressive(&cfg, &root, &LoadProgress::default(), |_| {})
                .unwrap();
        assert_eq!(first.storage, "decoded disk cache");
        let values = root.join("csv-decoded").join("occupied.bin");
        let filler = File::create(&values).unwrap();
        filler.set_len(1024 * 1024 - 20 * 2 * 8).unwrap();
        drop(filler);
        fixture(&path, 30); // Different source version requires a new entry.
        let next =
            super::super::csv::load_csv_progressive(&cfg, &root, &LoadProgress::default(), |_| {})
                .unwrap();
        assert!(next.storage.contains("decoded cache skipped"));
        assert_eq!(next.rows, 30);
        assert_eq!(values.metadata().unwrap().len(), 1024 * 1024 - 320);
        cfg.csv.decoded_disk_cache = false;
        let direct =
            super::super::csv::load_csv_progressive(&cfg, &root, &LoadProgress::default(), |_| {})
                .unwrap();
        assert_eq!(direct.storage, "original CSV + compact index");
    }

    #[test]
    #[ignore]
    fn benchmark_csv_source_index() {
        let temp = if let Some(parent) = std::env::var_os("BINWAVE_BENCH_CACHE_PARENT").or_else(|| std::env::var_os("PLOTTERMAX_BENCH_CACHE_PARENT")) {
            tempfile::tempdir_in(parent).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let path = if let Some(path) = std::env::var_os("BINWAVE_BENCH_CSV").or_else(|| std::env::var_os("PLOTTERMAX_BENCH_CSV")) {
            PathBuf::from(path)
        } else {
            let path = temp.path().join("benchmark.csv");
            let mut file = BufWriter::new(File::create(&path).unwrap());
            for row in 0..1_000_000 {
                writeln!(
                    file,
                    "{row},{},{},{},{},{},{},{},{}",
                    row % 10000,
                    row % 8000,
                    row % 6000,
                    row % 4000,
                    row % 3000,
                    row % 2000,
                    row % 1000,
                    row % 500
                )
                .unwrap();
            }
            file.flush().unwrap();
            path
        };
        let mut cfg = AppConfig::default();
        cfg.data_path = Some(path.clone());
        cfg.csv.has_header = false;
        cfg.csv.time_column = "c0".into();
        let root = temp.path().join("cache");
        let progress = LoadProgress::default();
        let start = Instant::now();
        let mut first = false;
        let mut last_report = Instant::now();
        let data = load(&cfg, &root, &progress, |data| {
            if !first {
                let ready = start.elapsed();
                let query = Instant::now();
                assert!(!data.signals[0].points.for_range(data.time_min, data.time_max, 2000).unwrap().is_empty());
                println!("Original CSV: first {} rows ready in {ready:?}; first query {:?}; no disk data created: {}", data.rows, query.elapsed(), !root.exists());
                first = true;
            }
            if last_report.elapsed() >= Duration::from_secs(10) {
                println!("Indexed {} rows; {:.1}/{:.1} MiB read", data.rows, progress.done.load(Ordering::Relaxed) as f64 / 1048576., progress.total.load(Ordering::Relaxed) as f64 / 1048576.);
                last_report = Instant::now();
            }
        }).unwrap();
        let cold = start.elapsed();
        let rows = data.rows;
        let (_, _, directory) = metadata(&cfg, &root);
        let disk = directory.join("index.json").metadata().unwrap().len();
        drop(data);
        let start = Instant::now();
        let warm_progress = LoadProgress::default();
        let data = load(&cfg, &root, &warm_progress, |_| panic!("warm scan")).unwrap();
        let warm = start.elapsed();
        let start = Instant::now();
        let points = data.signals[0].points.for_range(100., 110., 2000).unwrap();
        assert_eq!(points.len(), 11);
        println!("CSV source index benchmark: {} source bytes, {rows} rows, {disk} index bytes, indexing={cold:?}, warm={warm:?}, uncached zoom={:?}", path.metadata().unwrap().len(), start.elapsed());
        assert!(warm_progress.cache_hit.load(Ordering::Relaxed));
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(peak) = status.lines().find(|s| s.starts_with("VmHWM:")) {
                println!("Peak process RSS: {peak}");
            }
        }
    }
}
