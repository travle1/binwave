use crate::cache::{self, LoadProgress, SourceStamp};
use crate::config::AppConfig;
use crate::data::{DataStore, MappedChunk, MappedTable, PointSeries, Signal};
use crate::plot_index::{IndexBuilder, Sample};
use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CACHE_KIND: &str = "csv-decoded-index-v2";
const CHUNK_BYTES: usize = 64 * 1024 * 1024;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Serialize, Deserialize)]
struct Metadata {
    records: usize,
    names: Vec<String>,
    indexes: Vec<IndexBuilder>,
    time_min: f64,
    time_max: f64,
    #[serde(default)]
    chunk_rows: Vec<usize>,
}

fn read_cache(directory: &Path) -> Result<DataStore> {
    let file = File::open(directory.join("metadata.json"))?;
    ensure!(
        file.metadata()?.len() <= 512 * 1024 * 1024,
        "CSV cache metadata is too large"
    );
    let meta: Metadata = serde_json::from_reader(BufReader::new(file))?;
    data_from_metadata(directory, meta)
}

fn data_from_metadata(directory: &Path, meta: Metadata) -> Result<DataStore> {
    ensure!(
        meta.names.len() == meta.indexes.len()
            && meta.time_min.is_finite()
            && meta.time_max.is_finite()
            && meta.time_min <= meta.time_max,
        "invalid CSV cache metadata"
    );
    let mut chunks = Vec::new();
    if meta.chunk_rows.is_empty() {
        // Existing complete v1 caches remain usable without reparsing CSV.
        chunks.push(MappedChunk::open(
            File::open(directory.join("values.bin"))?,
            meta.records,
            meta.names.len() + 1,
        )?);
    } else {
        ensure!(
            meta.chunk_rows.len() <= 1_000_000,
            "CSV cache has too many chunks"
        );
        for (i, &rows) in meta.chunk_rows.iter().enumerate() {
            chunks.push(MappedChunk::open(
                File::open(directory.join(chunk_name(i)))?,
                rows,
                meta.names.len() + 1,
            )?);
        }
    }
    let table = MappedTable::from_chunks(chunks, None)?;
    ensure!(table.rows == meta.records, "CSV chunk row count mismatch");
    data_from_parts(
        table,
        &meta.names,
        &meta.indexes,
        meta.time_min,
        meta.time_max,
    )
}

fn data_from_parts(
    table: Arc<MappedTable>,
    names: &[String],
    indexes: &[IndexBuilder],
    time_min: f64,
    time_max: f64,
) -> Result<DataStore> {
    let records = table.rows;
    let signals = names
        .iter()
        .zip(indexes)
        .enumerate()
        .map(|(i, (name, index))| {
            let index = index.finish()?;
            ensure!(
                index.records() == table.rows,
                "CSV index row count mismatch"
            );
            Ok(Signal {
                name: name.clone(),
                points: PointSeries::new(table.clone(), i + 1, index),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(DataStore::from_signals(
        signals, time_min, time_max, records,
    ))
}

fn chunk_name(index: usize) -> String {
    format!("part-{index:06}.bin")
}

fn seal_chunk(
    output: &mut Option<BufWriter<File>>,
    directory: &Path,
    chunks: &mut Vec<Arc<MappedChunk>>,
    rows: &mut usize,
    columns: usize,
) -> Result<()> {
    if *rows == 0 {
        return Ok(());
    }
    let mut writer = output.take().context("CSV chunk writer missing")?;
    writer.flush()?;
    cache::freeze(writer.get_ref())?;
    drop(writer);
    chunks.push(MappedChunk::open(
        File::open(directory.join(chunk_name(chunks.len())))?,
        *rows,
        columns,
    )?);
    *rows = 0;
    Ok(())
}

#[cfg(test)]
pub fn load_csv(cfg: &AppConfig, root: &Path, progress: &LoadProgress) -> Result<DataStore> {
    load_csv_progressive(cfg, root, progress, |_| {})
}

/// Default storage reads the original CSV with a compact seek/summary index.
/// A full numeric copy is explicit opt-in and shares a total decoded-data budget.
pub fn load_csv_progressive(
    cfg: &AppConfig,
    root: &Path,
    progress: &LoadProgress,
    mut update: impl FnMut(DataStore),
) -> Result<DataStore> {
    if !cfg.csv.decoded_disk_cache {
        return super::csv_index::load(cfg, root, progress, update);
    }
    static DECODED_WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = DECODED_WRITER.lock().unwrap_or_else(|e| e.into_inner());
    progress.check()?;
    let decoded_root = root.join("csv-decoded");
    let limit = cfg.csv.decoded_cache_limit_mib.min(1_048_576) as u64 * 1024 * 1024;
    let used = decoded_bytes(&decoded_root)?;
    let available = limit.saturating_sub(used);
    let path = cfg
        .data_path
        .as_ref()
        .context("data_path missing in config")?;
    // Every valid CSV row needs at least one source byte per column. This
    // conservative bound avoids starting a copy that cannot fit the quota.
    let bound = path
        .metadata()?
        .len()
        .checked_add(1)
        .and_then(|n| n.checked_mul(8))
        .unwrap_or(u64::MAX);
    let mut published_rows = 0;
    let result = load_csv_decoded(
        cfg,
        &decoded_root,
        progress,
        if bound <= available { available } else { 0 },
        |data| {
            published_rows = data.rows;
            update(data);
        },
    );
    drop(guard);
    match result {
        Ok(data) => Ok(data),
        Err(err) if err.downcast_ref::<CacheLimit>().is_some() => {
            let mut data = super::csv_index::load(cfg, root, progress, |data| {
                if data.rows > published_rows {
                    update(data);
                }
            })?;
            data.storage
                .push_str(" (decoded cache skipped: size limit)");
            Ok(data)
        }
        Err(err) => Err(err),
    }
}

#[derive(Debug)]
struct CacheLimit;
impl std::fmt::Display for CacheLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("decoded cache size limit reached")
    }
}
impl std::error::Error for CacheLimit {}

fn decoded_bytes(root: &Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let bytes = if kind.is_dir() {
            decoded_bytes(&entry.path())?
        } else if kind.is_file() && entry.path().extension().is_some_and(|s| s == "bin") {
            entry.metadata()?.len()
        } else {
            0
        };
        total = total.checked_add(bytes).context("cache size overflows")?;
    }
    Ok(total)
}

fn load_csv_decoded(
    cfg: &AppConfig,
    root: &Path,
    progress: &LoadProgress,
    budget: u64,
    mut update: impl FnMut(DataStore),
) -> Result<DataStore> {
    let path = cfg
        .data_path
        .as_ref()
        .ok_or_else(|| anyhow!("data_path missing in config"))?;
    ensure!(
        cfg.csv.delimiter.is_ascii(),
        "CSV delimiter must be an ASCII character"
    );
    let mut source = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let stamp = SourceStamp::read(path, &source.metadata()?)?;
    progress.set_total(stamp.len);
    progress.check()?;
    let key = stamp.key(CACHE_KIND, &cfg.csv.parse_options())?;
    for candidate in [
        &key,
        &stamp.key("csv-decoded-index-v1", &cfg.csv.parse_options())?,
    ] {
        if let Some(directory) = cache::lookup(root, candidate) {
            if let Ok(data) = read_cache(&directory) {
                ensure!(
                    stamp == SourceStamp::read(path, &source.metadata()?)?,
                    "CSV changed while checking cache; retry"
                );
                progress.hit();
                return Ok(data);
            }
        }
    }

    if budget == 0 {
        return Err(CacheLimit.into());
    }

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(cfg.csv.has_header)
        .delimiter(cfg.csv.delimiter as u8)
        .buffer_capacity(1024 * 1024)
        .from_reader(&mut source);
    // The parser keeps its first record when has_headers is false. Validate the
    // schema immediately, before reading the rest of a multi-gigabyte file.
    let first = reader
        .byte_headers()
        .context("read CSV header / first record")?;
    let headers: Vec<String> = if cfg.csv.has_header {
        first
            .iter()
            .map(|v| String::from_utf8(v.to_vec()).context("CSV column names must be UTF-8"))
            .collect::<Result<_>>()?
    } else {
        (0..first.len()).map(|i| format!("c{i}")).collect()
    };
    let time_idx = headers.iter().position(|name| name == &cfg.csv.time_column).ok_or_else(|| anyhow!(
        "Time column '{}' not found. For a headerless CSV, turn off 'Has header row' and set Time column to c0, then Reload CSV.", cfg.csv.time_column))?;
    let signals: Vec<_> = headers
        .iter()
        .enumerate()
        .filter(|(i, name)| {
            *i != time_idx
                && cfg
                    .csv
                    .include_signals
                    .as_ref()
                    .is_none_or(|include| include.contains(name))
        })
        .collect();
    let columns = signals
        .len()
        .checked_add(1)
        .context("CSV column count overflows")?;
    let record_upper_bound = usize::try_from(stamp.len / headers.len().max(1) as u64 + 1)
        .context("CSV is too large for this process")?;
    let mut indexes: Vec<_> = signals
        .iter()
        .map(|_| IndexBuilder::new(record_upper_bound))
        .collect();
    let directory = cache::PendingDirectory::new(root)?;
    let names: Vec<String> = signals.iter().map(|(_, name)| (*name).clone()).collect();
    let mut chunks = Vec::new();
    let mut chunk_records = 0usize;
    let mut output: Option<BufWriter<File>> = None;
    let bytes_per_row = columns.checked_mul(8).context("CSV row size overflows")?;
    let check_every = (1024 * 1024 / bytes_per_row).clamp(1, 8192);
    let mut last_preview = Instant::now();
    let mut sent_preview = false;
    let mut record = csv::ByteRecord::new();
    let mut values = vec![0.; columns];
    let mut records = 0usize;
    let mut time_min = f64::INFINITY;
    let mut time_max = f64::NEG_INFINITY;
    while reader.read_byte_record(&mut record)? {
        if records % check_every == 0 {
            progress.check()?;
            progress.advance(reader.position().byte());
        }
        let time = std::str::from_utf8(record.get(time_idx).context("missing CSV time field")?)?
            .parse::<f64>()
            .with_context(|| format!("invalid time in CSV row {}", records + 1))?;
        ensure!(
            time.is_finite(),
            "non-finite time in CSV row {}",
            records + 1
        );
        time_min = time_min.min(time);
        time_max = time_max.max(time);
        values[0] = time;
        for (i, (column, _)) in signals.iter().enumerate() {
            let value = std::str::from_utf8(record.get(*column).unwrap_or_default())
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(f64::NAN);
            values[i + 1] = value;
            indexes[i].push(value.is_finite().then_some(Sample {
                record: records,
                byte_offset: 0,
                point: [time, value],
            }));
        }
        let next_bytes = (records as u64)
            .checked_add(1)
            .and_then(|n| n.checked_mul(bytes_per_row as u64))
            .context("decoded size overflows")?;
        if next_bytes > budget {
            return Err(CacheLimit.into());
        }
        if output.is_none() {
            output = Some(BufWriter::with_capacity(
                1024 * 1024,
                File::create(directory.path.join(chunk_name(chunks.len())))?,
            ));
        }
        output
            .as_mut()
            .unwrap()
            .write_all(bytemuck::cast_slice(&values))
            .context("write CSV cache (check free cache disk space)")?;
        records = records.checked_add(1).context("CSV row count overflows")?;
        chunk_records += 1;
        ensure!(
            records
                .checked_mul(columns)
                .and_then(|n| n.checked_mul(8))
                .is_some_and(|n| n <= isize::MAX as usize),
            "decoded CSV exceeds the process address-space limit"
        );
        if records % check_every == 0 {
            progress.check()?;
            let preview_due = !sent_preview || last_preview.elapsed() >= PREVIEW_INTERVAL;
            if preview_due || chunk_records * bytes_per_row >= CHUNK_BYTES {
                seal_chunk(
                    &mut output,
                    &directory.path,
                    &mut chunks,
                    &mut chunk_records,
                    columns,
                )?;
            }
            if preview_due {
                progress.advance(reader.position().byte());
                let table = MappedTable::from_chunks(chunks.clone(), Some(directory.clone()))?;
                update(data_from_parts(
                    table, &names, &indexes, time_min, time_max,
                )?);
                sent_preview = true;
                last_preview = Instant::now();
                progress.check()?;
            }
        }
    }
    ensure!(records > 0, "CSV has no data rows");
    drop(reader);
    ensure!(
        stamp == SourceStamp::read(path, &source.metadata()?)?,
        "CSV changed while loading; retry after its writer stops"
    );
    progress.check()?;
    seal_chunk(
        &mut output,
        &directory.path,
        &mut chunks,
        &mut chunk_records,
        columns,
    )?;
    let meta = Metadata {
        records,
        names,
        indexes,
        time_min,
        time_max,
        chunk_rows: chunks.iter().map(|chunk| chunk.rows).collect(),
    };
    let table = MappedTable::from_chunks(chunks, Some(directory.clone()))?;
    let data = data_from_parts(table, &meta.names, &meta.indexes, time_min, time_max)?;
    let mut file = BufWriter::new(cache::LimitedWriter::new(
        File::create(directory.path.join("metadata.json"))?,
        64 * 1024 * 1024,
    ));
    serde_json::to_writer(&mut file, &meta)?;
    file.flush()?;
    cache::freeze(file.get_ref().inner())?;
    drop(file);
    progress.check()?;
    directory.publish(root, &key)?;
    progress.advance(stamp.len);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercise the opt-in decoded backend independently from policy dispatch.
    fn load_csv(cfg: &AppConfig, root: &Path, progress: &LoadProgress) -> Result<DataStore> {
        super::load_csv_decoded(cfg, root, progress, u64::MAX, |_| {})
    }
    fn load_csv_progressive(
        cfg: &AppConfig,
        root: &Path,
        progress: &LoadProgress,
        update: impl FnMut(DataStore),
    ) -> Result<DataStore> {
        super::load_csv_decoded(cfg, root, progress, u64::MAX, update)
    }
    use std::sync::atomic::Ordering;

    fn config(path: &Path, header: bool) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.data_path = Some(path.into());
        cfg.csv.has_header = header;
        cfg.csv.time_column = if header { "time" } else { "c0" }.into();
        cfg
    }

    fn generated_csv(path: &Path, rows: usize) {
        let mut output = BufWriter::new(File::create(path).unwrap());
        for row in 0..rows {
            // Duplicate, unsorted X crosses chunk boundaries.
            writeln!(output, "{},{row},{}", row % 256, row * 2).unwrap();
        }
        output.flush().unwrap();
    }

    #[test]
    fn decoded_write_budget_stops_before_overflow_and_keeps_only_live_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        let root = dir.path().join("cache");
        generated_csv(&path, 20_000);
        let cfg = config(&path, false);
        let budget = 8192 * 3 * 8;
        let mut first = None;
        let error =
            super::load_csv_decoded(&cfg, &root, &LoadProgress::default(), budget, |data| {
                first = Some(data)
            })
            .unwrap_err();
        assert!(error.downcast_ref::<CacheLimit>().is_some());
        assert_eq!(decoded_bytes(&root).unwrap(), budget);
        assert!(std::fs::read_dir(&root).unwrap().all(|e| {
            e.unwrap()
                .path()
                .extension()
                .is_none_or(|ext| ext != "json")
        }));
        let first = first.unwrap();
        assert_eq!(first.rows, 8192);
        assert_eq!(first.signals[0].points.point(8191), [255., 8191.]);
        drop(first);
        assert_eq!(decoded_bytes(&root).unwrap(), 0);
    }

    #[test]
    fn first_rows_are_queryable_before_eof_and_snapshots_remain_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        let root = dir.path().join("cache");
        generated_csv(&path, 25_000);
        let cfg = config(&path, false);
        let progress = LoadProgress::default();
        let mut first = None;
        let final_data = load_csv_progressive(&cfg, &root, &progress, |data| {
            if first.is_none() {
                assert_eq!(data.rows, 8192);
                assert!(
                    progress.done.load(Ordering::Relaxed) < progress.total.load(Ordering::Relaxed)
                );
                assert_eq!(data.signals[0].points.point(8191), [255., 8191.]);
                let zoom = data.signals[0].points.for_range(10., 12., 25_000).unwrap();
                assert_eq!(zoom.len(), 96);
                first = Some(data);
            }
        })
        .unwrap();
        let first = first.unwrap();
        assert_eq!(first.rows, 8192);
        assert_eq!(first.signals[0].points.point(8191), [255., 8191.]);
        assert_eq!(final_data.rows, 25_000);
        for row in [0, 8191, 8192, 24_999] {
            assert_eq!(
                final_data.signals[0].points.point(row),
                [(row % 256) as f64, row as f64]
            );
        }
        let expected: Vec<_> = (0..25_000)
            .filter(|r| (10..=12).contains(&(r % 256)))
            .map(|r| [(r % 256) as f64, r as f64])
            .collect();
        assert_eq!(
            final_data.signals[0]
                .points
                .for_range(10., 12., 25_000)
                .unwrap(),
            expected
        );
        let warm_progress = LoadProgress::default();
        let warm = load_csv_progressive(&cfg, &root, &warm_progress, |_| {
            panic!("complete cache must load directly")
        })
        .unwrap();
        assert!(warm_progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(warm.signals[0].points.point(8192), [0., 8192.]);
        assert_eq!(
            warm.signals[0].points.for_range(10., 12., 25_000).unwrap(),
            expected
        );
    }

    #[test]
    fn cancel_retains_usable_rows_without_publishing_or_leaking_partial_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        let root = dir.path().join("cache");
        generated_csv(&path, 25_000);
        let cfg = config(&path, false);
        let progress = LoadProgress::default();
        let mut preview = None;
        let result = load_csv_progressive(&cfg, &root, &progress, |data| {
            preview = Some(data);
            progress.cancelled.store(true, Ordering::Relaxed);
        });
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        let preview = preview.unwrap();
        assert_eq!(preview.rows, 8192);
        assert!(!preview.signals[0]
            .points
            .for_range(0., 255., 100)
            .unwrap()
            .is_empty());
        let key = SourceStamp::read(&path, &path.metadata().unwrap())
            .unwrap()
            .key(CACHE_KIND, &cfg.csv.parse_options())
            .unwrap();
        assert!(cache::lookup(&root, &key).is_none());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "preview retains its incomplete directory"
        );
        drop(preview);
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "last preview releases unfinished chunks"
        );
        let next = LoadProgress::default();
        assert_eq!(load_csv(&cfg, &root, &next).unwrap().rows, 25_000);
        assert!(!next.cache_hit.load(Ordering::Relaxed));
    }

    #[test]
    fn error_after_first_rows_keeps_prefix_usable_but_never_publishes_complete_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        let root = dir.path().join("cache");
        generated_csv(&path, 9000);
        let mut source = File::options().append(true).open(&path).unwrap();
        writeln!(source, "bad-time,1,2").unwrap();
        drop(source);
        let cfg = config(&path, false);
        let mut first = None;
        let result = load_csv_progressive(&cfg, &root, &LoadProgress::default(), |data| {
            first = Some(data)
        });
        assert!(format!("{:#}", result.unwrap_err()).contains("row 9001"));
        let first = first.unwrap();
        assert_eq!(first.signals[0].points.point(8191), [255., 8191.]);
        let key = SourceStamp::read(&path, &path.metadata().unwrap())
            .unwrap()
            .key(CACHE_KIND, &cfg.csv.parse_options())
            .unwrap();
        assert!(cache::lookup(&root, &key).is_none());
        drop(first);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn existing_single_file_cache_is_reused_without_reparsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        let root = dir.path().join("cache");
        std::fs::write(&path, "0,11\n1,22\n").unwrap();
        let cfg = config(&path, false);
        let staging = cache::staging(&root).unwrap();
        let mut values = File::create(staging.path().join("values.bin")).unwrap();
        values
            .write_all(bytemuck::cast_slice(&[0f64, 11., 1., 22.]))
            .unwrap();
        cache::freeze(&values).unwrap();
        drop(values);
        let mut index = IndexBuilder::new(2);
        index.push(Some(Sample {
            record: 0,
            byte_offset: 0,
            point: [0., 11.],
        }));
        index.push(Some(Sample {
            record: 1,
            byte_offset: 0,
            point: [1., 22.],
        }));
        let old = serde_json::json!({"records": 2, "names": ["c1"], "indexes": [index], "time_min": 0., "time_max": 1.});
        std::fs::write(
            staging.path().join("metadata.json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let key = SourceStamp::read(&path, &path.metadata().unwrap())
            .unwrap()
            .key("csv-decoded-index-v1", &cfg.csv.parse_options())
            .unwrap();
        cache::publish(&root, &key, staging).unwrap();
        let progress = LoadProgress::default();
        let data = load_csv_progressive(&cfg, &root, &progress, |_| {
            panic!("warm cache should not parse rows")
        })
        .unwrap();
        assert!(progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(data.rows, 2);
        assert_eq!(data.signals[0].points.point(1), [1., 22.]);
    }

    #[test]
    fn cold_then_warm_csv_cache_preserves_headerless_row_zero_and_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        let root = dir.path().join("cache");
        std::fs::write(&path, "0,12,50\n2,99,51\n1,23,52\n2,24,53\n").unwrap();
        let cfg = config(&path, false);
        let progress = LoadProgress::default();
        let cold = load_csv(&cfg, &root, &progress).unwrap();
        assert!(!progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(cold.rows, 4);
        assert_eq!(cold.signals[0].points.point(0), [0., 12.]);
        assert_eq!(
            cold.signals[0].points.for_range(0., 2., 100).unwrap(),
            vec![[0., 12.], [2., 99.], [1., 23.], [2., 24.]]
        );
        drop(cold);
        let progress = LoadProgress::default();
        let warm = load_csv(&cfg, &root, &progress).unwrap();
        assert!(progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(warm.signals[0].points.point(1), [2., 99.]);
        // Same byte length still invalidates using timestamp/file identity.
        std::fs::write(&path, "0,12,50\n2,88,51\n1,23,52\n2,24,53\n").unwrap();
        let progress = LoadProgress::default();
        let changed = load_csv(&cfg, &root, &progress).unwrap();
        assert!(!progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(changed.signals[0].points.point(1), [2., 88.]);
        assert_eq!(
            warm.signals[0].points.point(1),
            [2., 99.],
            "live cache mappings must stay immutable"
        );
        let mut filtered = cfg;
        filtered.csv.include_signals = Some(vec!["c2".into()]);
        let progress = LoadProgress::default();
        let data = load_csv(&filtered, &root, &progress).unwrap();
        assert!(!progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(data.signals.len(), 1);
        assert_eq!(data.signals[0].points.point(0), [0., 50.]);
    }

    #[test]
    fn header_errors_fail_before_cache_creation_and_cancel_preserves_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        std::fs::write(&path, "0,1\n1,2\n").unwrap();
        let root = dir.path().join("cache");
        let err = load_csv(&config(&path, true), &root, &LoadProgress::default()).unwrap_err();
        assert!(err.to_string().contains("Has header row"));
        assert!(!root.exists());
        let progress = LoadProgress::default();
        progress.cancelled.store(true, Ordering::Relaxed);
        assert!(load_csv(&config(&path, false), &root, &progress)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(!root.exists());
    }

    #[test]
    fn csv_cache_handles_quoted_headers_nan_and_changed_delimiter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        let root = dir.path().join("cache");
        std::fs::write(&path, "time;\"sensor;A\"\n0;NaN\n1;inf\n2;1.25\n").unwrap();
        let mut cfg = config(&path, true);
        cfg.csv.delimiter = ';';
        let data = load_csv(&cfg, &root, &LoadProgress::default()).unwrap();
        assert_eq!(data.signals[0].name, "sensor;A");
        assert!(data.signals[0].points.point(0)[1].is_nan());
        assert!(data.signals[0].points.point(1)[1].is_infinite());
        assert_eq!(
            data.signals[0].points.for_range(0., 2., 100).unwrap(),
            vec![[2., 1.25]]
        );
        cfg.csv.delimiter = ',';
        assert!(load_csv(&cfg, &root, &LoadProgress::default()).is_err());
    }

    #[test]
    fn malformed_cached_index_is_rebuilt_without_changing_live_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.csv");
        let root = dir.path().join("cache");
        std::fs::write(&path, "0,11\n1,22\n").unwrap();
        let cfg = config(&path, false);
        let old = load_csv(&cfg, &root, &LoadProgress::default()).unwrap();
        let key = SourceStamp::read(&path, &path.metadata().unwrap())
            .unwrap()
            .key(CACHE_KIND, &cfg.csv.parse_options())
            .unwrap();
        let entry = cache::lookup(&root, &key).unwrap();
        let metadata_path = entry.join("metadata.json");
        let mut metadata: serde_json::Value =
            serde_json::from_reader(File::open(&metadata_path).unwrap()).unwrap();
        let index = &mut metadata["indexes"][0];
        index["records"] = serde_json::json!(usize::MAX);
        index["leaf_size"] = serde_json::json!(1usize << (usize::BITS - 1));
        let leaf = index["leaves"][0].clone();
        index["leaves"] = serde_json::json!([leaf.clone(), leaf]);
        let mut replacement = tempfile::NamedTempFile::new_in(&entry).unwrap();
        serde_json::to_writer(&mut replacement, &metadata).unwrap();
        replacement.persist(&metadata_path).unwrap();
        let progress = LoadProgress::default();
        let rebuilt = load_csv(&cfg, &root, &progress).unwrap();
        assert!(!progress.cache_hit.load(Ordering::Relaxed));
        assert_eq!(rebuilt.rows, 2);
        assert_eq!(rebuilt.signals[0].points.point(1), [1., 22.]);
        assert_eq!(old.signals[0].points.point(1), [1., 22.]);
    }

    /// Reproducible local benchmark; run with `cargo test --release benchmark_csv_cache -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn benchmark_csv_cache() {
        let dir = if let Some(parent) = std::env::var_os("BINWAVE_BENCH_CACHE_PARENT").or_else(|| std::env::var_os("PLOTTERMAX_BENCH_CACHE_PARENT")) {
            tempfile::tempdir_in(parent).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let path = if let Some(path) = std::env::var_os("BINWAVE_BENCH_CSV").or_else(|| std::env::var_os("PLOTTERMAX_BENCH_CSV")) {
            path.into()
        } else {
            let path = dir.path().join("benchmark.csv");
            let mut writer = BufWriter::new(File::create(&path).unwrap());
            for row in 0..1_000_000 {
                writeln!(
                    writer,
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
            writer.flush().unwrap();
            path
        };
        let cfg = config(&path, false);
        let root = dir.path().join("cache");
        let loading = std::sync::Arc::new(LoadProgress::default());
        let reporting = loading.clone();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let reporter = std::thread::spawn(move || {
            while rx.recv_timeout(std::time::Duration::from_secs(10))
                == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                println!(
                    "CSV cold load: {:.1} / {:.1} MiB",
                    reporting.done.load(Ordering::Relaxed) as f64 / 1048576.,
                    reporting.total.load(Ordering::Relaxed) as f64 / 1048576.
                );
            }
        });
        let now = std::time::Instant::now();
        let mut first_ready = None;
        let cold = load_csv_progressive(&cfg, &root, &loading, |data| {
            if first_ready.is_none() {
                first_ready = Some(now.elapsed());
                println!(
                    "First usable CSV rows: {} after {:?}",
                    data.rows,
                    first_ready.unwrap()
                );
            }
        })
        .unwrap();
        let cold_time = now.elapsed();
        drop(tx);
        reporter.join().unwrap();
        drop(cold);
        let progress = LoadProgress::default();
        let now = std::time::Instant::now();
        let warm = load_csv(&cfg, &root, &progress).unwrap();
        let warm_time = now.elapsed();
        assert!(progress.cache_hit.load(Ordering::Relaxed));
        let now = std::time::Instant::now();
        let points = warm.signals[0]
            .points
            .for_range(warm.time_min, warm.time_max, 2000)
            .unwrap();
        let first_view = now.elapsed();
        let now = std::time::Instant::now();
        assert_eq!(
            points,
            warm.signals[0]
                .points
                .for_range(warm.time_min, warm.time_max, 2000)
                .unwrap()
        );
        println!("CSV benchmark: {} bytes, {} rows, {} signals; cold={cold_time:?}, warm={warm_time:?}, first_view={first_view:?}, cached_view={:?}", path.metadata().unwrap().len(), warm.rows, warm.signals.len(), now.elapsed());
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(peak) = status.lines().find(|s| s.starts_with("VmHWM:")) {
                println!("Benchmark peak process RSS: {peak}");
            }
        }
    }

    /// Measures time to the first usable chunk without parsing the entire source.
    #[test]
    #[ignore]
    fn benchmark_csv_first_rows() {
        let dir = if let Some(parent) = std::env::var_os("BINWAVE_BENCH_CACHE_PARENT").or_else(|| std::env::var_os("PLOTTERMAX_BENCH_CACHE_PARENT")) {
            tempfile::tempdir_in(parent).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let path = if let Some(path) = std::env::var_os("BINWAVE_BENCH_CSV").or_else(|| std::env::var_os("PLOTTERMAX_BENCH_CSV")) {
            path.into()
        } else {
            let path = dir.path().join("benchmark.csv");
            generated_csv(&path, 100_000);
            path
        };
        let cfg = config(&path, false);
        let progress = LoadProgress::default();
        let start = Instant::now();
        let mut first = None;
        let mut first_ready = Duration::ZERO;
        let mut plot_ready = Duration::ZERO;
        let result = load_csv_progressive(&cfg, &dir.path().join("cache"), &progress, |data| {
            first_ready = start.elapsed();
            let query = Instant::now();
            let points = data.signals[0]
                .points
                .for_range(data.time_min, data.time_max, 2000)
                .unwrap();
            plot_ready = query.elapsed();
            assert!(!points.is_empty());
            first = Some(data);
            progress.cancelled.store(true, Ordering::Relaxed);
        });
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        let first = first.unwrap();
        assert!(progress.done.load(Ordering::Relaxed) < progress.total.load(Ordering::Relaxed));
        println!("Progressive CSV: {} byte source, {} rows / {} signals usable after {first_ready:?}; first plot {plot_ready:?}; stopped after {} source bytes; loaded portion remains queryable",
            path.metadata().unwrap().len(), first.rows, first.signals.len(), progress.done.load(Ordering::Relaxed));
        assert!(!first.signals[0]
            .points
            .for_range(first.time_min, first.time_max, 2000)
            .unwrap()
            .is_empty());
    }
}
