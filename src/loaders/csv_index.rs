//! CSV data stays in the original file. Persist only seek checkpoints and plot
//! summaries; share a bounded cache of decoded blocks between all signals/views.
use crate::cache::{self, LoadProgress, SourceStamp};
use crate::config::AppConfig;
use crate::data::{DataStore, PointSeries, PointSource, Signal};
use crate::plot_index::{IndexBuilder, Sample};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const INDEX_KIND: &str = "csv-source-index-v1";
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHECKPOINTS: usize = 65536;

#[derive(Serialize, Deserialize)]
struct Metadata {
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
    reader: csv::Reader<File>,
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
    reader: Mutex<ReaderState>,
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
            stamp,
            columns: meta.columns.clone(),
            source_columns: meta.source_columns,
            block_rows: meta.block_rows,
            budget,
            reader: Mutex::new(ReaderState {
                reader: csv::ReaderBuilder::new()
                    .has_headers(false)
                    .delimiter(cfg.csv.delimiter as u8)
                    .buffer_capacity(64 * 1024)
                    .from_reader(file),
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
    });
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
    Ok(data)
}

fn validate_metadata(meta: &Metadata, stamp: &SourceStamp) -> Result<()> {
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
        meta.offsets.iter().all(|&p| p < stamp.len) && meta.offsets.windows(2).all(|w| w[0] < w[1]),
        "invalid CSV seek offsets"
    );
    Ok(())
}

pub fn load(
    cfg: &AppConfig,
    root: &Path,
    progress: &LoadProgress,
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
    progress.set_total(stamp.len);
    progress.check()?;
    if let Some(directory) = cache::lookup(root, &key) {
        let hit = (|| -> Result<DataStore> {
            let file = File::open(directory.join("index.json"))?;
            ensure!(
                file.metadata()?.len() <= MAX_INDEX_BYTES,
                "CSV index too large"
            );
            let meta: Metadata = serde_json::from_reader(BufReader::new(file))?;
            validate_metadata(&meta, &stamp)?;
            let source = SourceCsv::open(cfg, stamp.clone(), &meta)?;
            source.validate()?;
            store(source, &meta)
        })();
        if let Ok(data) = hit {
            progress.hit();
            return Ok(data);
        }
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(cfg.csv.has_header)
        .delimiter(cfg.csv.delimiter as u8)
        .buffer_capacity(1024 * 1024)
        .from_reader(file);
    let first = reader.byte_headers()?;
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
    let mut meta = Metadata {
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
    };
    let source = SourceCsv::open(cfg, stamp.clone(), &meta)?;
    let mut record = csv::ByteRecord::new();
    let mut values = vec![0.; meta.columns.len()];
    let mut block = Vec::with_capacity(block_rows * values.len());
    let mut last = Instant::now();
    let mut sent = false;
    while reader.read_byte_record(&mut record)? {
        if meta.rows % block_rows == 0 {
            progress.check()?;
        }
        if meta.rows % stride == 0 {
            meta.offsets.push(
                record
                    .position()
                    .context("CSV record position missing")?
                    .byte(),
            );
        }
        decode(
            &record,
            &meta.columns,
            meta.source_columns,
            meta.rows,
            &mut values,
        )?;
        meta.time_min = meta.time_min.min(values[0]);
        meta.time_max = meta.time_max.max(values[0]);
        for (index, &value) in meta.indexes.iter_mut().zip(&values[1..]) {
            index.push(value.is_finite().then_some(Sample {
                record: meta.rows,
                byte_offset: 0,
                point: [values[0], value],
            }));
        }
        block.extend_from_slice(&values);
        meta.rows = meta
            .rows
            .checked_add(1)
            .context("CSV row count overflows")?;
        if meta.rows % block_rows == 0 {
            source.insert(
                meta.rows - block_rows,
                std::mem::replace(&mut block, Vec::with_capacity(block_rows * values.len())),
            );
            progress.advance(reader.position().byte());
            if !sent || last.elapsed() >= Duration::from_millis(500) {
                source.validate()?;
                update(store(source.clone(), &meta)?);
                sent = true;
                last = Instant::now();
                progress.check()?;
            }
        }
    }
    ensure!(meta.rows > 0, "CSV has no data rows");
    if !block.is_empty() {
        source.insert(meta.rows / block_rows * block_rows, block);
    }
    source.validate()?;
    progress.check()?;
    let mut data = store(source, &meta)?;
    // Index persistence is an optimization: a full disk must not prevent using
    // the original data. Never create a numeric copy in this default mode.
    let saved = (|| -> Result<()> {
        let dir = cache::staging(root)?;
        let file = File::create(dir.path().join("index.json"))?;
        let mut output = BufWriter::new(cache::LimitedWriter::new(file, MAX_INDEX_BYTES));
        serde_json::to_writer(&mut output, &meta)?;
        output.flush()?;
        cache::freeze(output.get_ref().inner())?;
        drop(output);
        progress.check()?;
        cache::publish(root, &key, dir)?;
        Ok(())
    })();
    if let Err(error) = saved {
        data.storage = format!("original CSV; index kept in RAM ({error:#})");
    }
    progress.check()?;
    progress.advance(stamp.len);
    Ok(data)
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
    fn cancellation_and_late_errors_leave_usable_prefix_without_any_disk_cache() {
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
        assert!(!root.exists());
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
        assert!(prefix.is_some());
        assert!(!root.exists());
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
        let temp = if let Some(parent) = std::env::var_os("PLOTTERMAX_BENCH_CACHE_PARENT") {
            tempfile::tempdir_in(parent).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let path = if let Some(path) = std::env::var_os("PLOTTERMAX_BENCH_CSV") {
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
