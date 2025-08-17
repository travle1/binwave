use crate::config::{AppConfig, FileKind, BinaryConfig, BinType, Endianness};
use anyhow::{anyhow, Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Signal {
    pub name: String,
    pub points: Vec<[f64; 2]>, // [time, value]
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Default, Clone)]
pub struct DataStore {
    pub signals: Vec<Signal>,
    pub time_min: f64,
    pub time_max: f64,
    name_to_index: HashMap<String, usize>,
}

impl DataStore {
    pub fn is_empty(&self) -> bool {
        self.signals.is_empty()
    }

    pub fn signal_names(&self) -> Vec<String> {
        self.signals.iter().map(|s| s.name.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Signal> {
        self.name_to_index.get(name).and_then(|&i| self.signals.get(i))
    }
}

pub fn load_data(cfg: &AppConfig) -> Result<DataStore> {
    match cfg.format {
        FileKind::Csv => load_csv(&cfg),
        FileKind::BinaryFixed => load_binary(&cfg),
    }
}

fn load_csv(cfg: &AppConfig) -> Result<DataStore> {
    let path = cfg
        .data_path
        .as_ref()
        .ok_or_else(|| anyhow!("data_path missing in config"))?;

    let mut rdr_builder = csv::ReaderBuilder::new();
    rdr_builder.has_headers(cfg.csv.has_header);
    rdr_builder.delimiter(cfg.csv.delimiter as u8);

    // Read file into memory-mapped-like buffer to speed up parsing in sandboxed examples
    let mut file = File::open(path)
        .with_context(|| format!("Failed to open data file: {}", path.display()))?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;

    let mut rdr = rdr_builder.from_reader(&content[..]);

    let headers = if cfg.csv.has_header {
        rdr.headers()
            .with_context(|| "Failed to read CSV header")?
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    } else {
        // Without headers, synthesize names: c0, c1, ...
        let rec = rdr
            .records()
            .next()
            .transpose()
            .context("Failed to read first CSV record")?
            .ok_or_else(|| anyhow!("CSV appears empty"))?;
        let n = rec.len();
        // Put it back by rebuilding the reader
        let mut rdr2 = rdr_builder.from_reader(&content[..]);
        std::mem::swap(&mut rdr, &mut rdr2);
        (0..n).map(|i| format!("c{}", i)).collect::<Vec<_>>()
    };

    // Determine time column index
    let time_idx = headers
        .iter()
        .position(|h| h == &cfg.csv.time_column)
        .ok_or_else(|| anyhow!("Time column '{}' not found", cfg.csv.time_column))?;

    // Determine which signal columns to include
    let mut signal_cols: Vec<(usize, String)> = headers
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != time_idx)
        .map(|(i, h)| (i, h.clone()))
        .collect();
    if let Some(include) = &cfg.csv.include_signals {
        let include_set: std::collections::HashSet<_> = include.iter().collect();
        signal_cols.retain(|(_, name)| include_set.contains(name));
    }

    // Prepare temporary storage per column
    let mut time_vec: Vec<f64> = Vec::with_capacity(1024);
    let mut cols_map: BTreeMap<usize, Vec<f64>> = signal_cols
        .iter()
        .map(|(idx, _)| (*idx, Vec::with_capacity(1024)))
        .collect();

    for rec in rdr.records() {
        let rec = rec?;
        let t: f64 = rec
            .get(time_idx)
            .ok_or_else(|| anyhow!("Missing time field"))?
            .parse()
            .with_context(|| format!("Failed parsing time '{}" , rec.get(time_idx).unwrap_or("")))?;
        time_vec.push(t);
        for (idx, _) in &signal_cols {
            let val = rec
                .get(*idx)
                .unwrap_or("")
                .trim();
            if val.is_empty() {
                cols_map.get_mut(idx).unwrap().push(f64::NAN);
            } else {
                match val.parse::<f64>() {
                    Ok(v) => cols_map.get_mut(idx).unwrap().push(v),
                    Err(_) => cols_map.get_mut(idx).unwrap().push(f64::NAN),
                }
            }
        }
    }

    if time_vec.is_empty() {
        return Err(anyhow!("No data rows found"));
    }

    // Build signals in parallel
    let time_min = *time_vec.first().unwrap();
    let time_max = *time_vec.last().unwrap();

    let signals: Vec<Signal> = signal_cols
        .par_iter()
        .map(|(idx, name)| {
            let col = cols_map.get(idx).unwrap();
            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            let mut points = Vec::with_capacity(time_vec.len());
            for (t, v) in time_vec.iter().zip(col.iter()) {
                let y = *v;
                if y.is_finite() {
                    if y < min {
                        min = y;
                    }
                    if y > max {
                        max = y;
                    }
                }
                points.push([*t, y]);
            }
            if !min.is_finite() {
                min = 0.0;
            }
            if !max.is_finite() {
                max = 0.0;
            }
            Signal {
                name: name.clone(),
                points,
                min,
                max,
            }
        })
        .collect();

    let mut name_to_index = HashMap::new();
    for (i, s) in signals.iter().enumerate() {
        name_to_index.insert(s.name.clone(), i);
    }

    Ok(DataStore {
        signals,
        time_min,
        time_max,
        name_to_index,
    })
}

fn load_binary(cfg: &AppConfig) -> Result<DataStore> {
    let path = cfg
        .data_path
        .as_ref()
        .ok_or_else(|| anyhow!("data_path missing in config"))?;
    let bcfg: &BinaryConfig = cfg
        .binary
        .as_ref()
        .ok_or_else(|| anyhow!("binary config required when format is binary_fixed"))?;

    // Read file fully (could be improved to streaming if needed)
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("Failed to open data file: {}", path.display()))?;
    let mut bytes = Vec::new();
    use std::io::Read as _;
    f.read_to_end(&mut bytes)?;

    // Compute field offsets and record size
    let (field_offsets, record_size) = compute_layout(bcfg)?;
    if bytes.len() < record_size { return Err(anyhow!("File too small for one record")); }
    let nrec = bytes.len() / record_size;
    if nrec == 0 { return Err(anyhow!("No records found")); }

    // Identify time field index
    let mut time_idx: Option<usize> = None;
    for (i, f) in bcfg.fields.iter().enumerate() {
        if f.name == bcfg.time_field { time_idx = Some(i); break; }
    }
    let time_idx = time_idx.ok_or_else(|| anyhow!("Time field '{}' not found", bcfg.time_field))?;

    // Determine which fields are signals (exclude time)
    let mut signal_fields: Vec<usize> = (0..bcfg.fields.len()).filter(|&i| i != time_idx).collect();
    if let Some(include) = &bcfg.include_signals {
        let include_set: std::collections::HashSet<_> = include.iter().collect();
        signal_fields.retain(|&i| include_set.contains(&bcfg.fields[i].name));
    }

    // Prepare storage
    let mut time_vec: Vec<f64> = Vec::with_capacity(nrec);
    let mut cols: Vec<Vec<f64>> = signal_fields.iter().map(|_| Vec::with_capacity(nrec)).collect();

    // Parse records
    let mut base = 0usize;
    for _ in 0..nrec {
        // time
        let tf = &bcfg.fields[time_idx];
        let toff = field_offsets[time_idx];
        let tval = read_number_as_f64(&bytes, base + toff, &tf.ty, bcfg.endianness)?;
        time_vec.push(tval);
        // signals
        for (ci, &fi) in signal_fields.iter().enumerate() {
            let ff = &bcfg.fields[fi];
            let off = field_offsets[fi];
            let v = read_number_as_f64(&bytes, base + off, &ff.ty, bcfg.endianness)?;
            cols[ci].push(v);
        }
        base += record_size;
    }

    // Build signals
    let time_min = *time_vec.first().unwrap();
    let time_max = *time_vec.last().unwrap();

    let mut signals: Vec<Signal> = Vec::with_capacity(signal_fields.len());
    for (ci, &fi) in signal_fields.iter().enumerate() {
        let name = bcfg.fields[fi].name.clone();
        let col = &cols[ci];
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut points = Vec::with_capacity(nrec);
        for (t, v) in time_vec.iter().zip(col.iter()) {
            let y = *v;
            if y.is_finite() { if y < min { min = y; } if y > max { max = y; } }
            points.push([*t, y]);
        }
        if !min.is_finite() { min = 0.0; }
        if !max.is_finite() { max = 0.0; }
        signals.push(Signal { name, points, min, max });
    }

    let mut name_to_index = HashMap::new();
    for (i, s) in signals.iter().enumerate() { name_to_index.insert(s.name.clone(), i); }

    Ok(DataStore { signals, time_min, time_max, name_to_index })
}

fn compute_layout(cfg: &BinaryConfig) -> Result<(Vec<usize>, usize)> {
    // Determine offsets: if any field supplies offset, use those; otherwise pack tightly.
    let mut offsets = Vec::with_capacity(cfg.fields.len());
    let mut cursor = 0usize;
    let mut max_end = 0usize;
    for f in &cfg.fields {
        let sz = bin_type_size(&f.ty);
        let off = match f.offset { Some(o) => o, None => { let o = cursor; cursor += sz; o } };
        offsets.push(off);
        max_end = max_end.max(off + sz);
    }
    let rec_size = cfg.record_size.unwrap_or(max_end);
    if rec_size < max_end { return Err(anyhow!("record_size {} smaller than fields end {}", rec_size, max_end)); }
    Ok((offsets, rec_size))
}

fn bin_type_size(t: &BinType) -> usize {
    match t {
        BinType::F32 => 4,
        BinType::F64 => 8,
        BinType::I8 | BinType::U8 => 1,
        BinType::I16 | BinType::U16 => 2,
        BinType::I32 | BinType::U32 => 4,
        BinType::I64 | BinType::U64 => 8,
    }
}

fn read_number_as_f64(buf: &[u8], off: usize, ty: &BinType, end: Endianness) -> Result<f64> {
    use Endianness::*;
    use std::convert::TryInto;
    let f = match ty {
        BinType::F32 => {
            let b: [u8; 4] = buf.get(off..off+4).ok_or_else(|| anyhow!("out of bounds"))?.try_into().unwrap();
            let v = match end { Little => f32::from_le_bytes(b), Big => f32::from_be_bytes(b) };
            v as f64
        }
        BinType::F64 => {
            let b: [u8; 8] = buf.get(off..off+8).ok_or_else(|| anyhow!("out of bounds"))?.try_into().unwrap();
            let v = match end { Little => f64::from_le_bytes(b), Big => f64::from_be_bytes(b) };
            v
        }
        BinType::I8 => { let v = *buf.get(off).ok_or_else(|| anyhow!("out of bounds"))? as i8; v as f64 }
        BinType::U8 => { let v = *buf.get(off).ok_or_else(|| anyhow!("out of bounds"))? as u8; v as f64 }
        BinType::I16 => {
            let b: [u8; 2] = buf.get(off..off+2).ok_or_else(|| anyhow!("out of bounds"))?.try_into().unwrap();
            let v = match end { Little => i16::from_le_bytes(b), Big => i16::from_be_bytes(b) };
            v as f64
        }
        BinType::U16 => {
            let b: [u8; 2] = buf.get(off..off+2).ok_or_else(|| anyhow!("out of bounds"))?.try_into().unwrap();
            let v = match end { Little => u16::from_le_bytes(b), Big => u16::from_be_bytes(b) };
            v as f64
        }
        BinType::I32 => {
            let b: [u8; 4] = buf.get(off..off+4).ok_or_else(|| anyhow!("out of bounds"))?.try_into().unwrap();
            let v = match end { Little => i32::from_le_bytes(b), Big => i32::from_be_bytes(b) };
            v as f64
        }
        BinType::U32 => {
            let b: [u8; 4] = buf.get(off..off+4).ok_or_else(|| anyhow!("out of bounds"))?.try_into().unwrap();
            let v = match end { Little => u32::from_le_bytes(b), Big => u32::from_be_bytes(b) };
            v as f64
        }
        BinType::I64 => {
            let b: [u8; 8] = buf.get(off..off+8).ok_or_else(|| anyhow!("out of bounds"))?.try_into().unwrap();
            let v = match end { Little => i64::from_le_bytes(b), Big => i64::from_be_bytes(b) };
            v as f64
        }
        BinType::U64 => {
            let b: [u8; 8] = buf.get(off..off+8).ok_or_else(|| anyhow!("out of bounds"))?.try_into().unwrap();
            let v = match end { Little => u64::from_le_bytes(b), Big => u64::from_be_bytes(b) };
            v as f64
        }
    };
    Ok(f)
}

#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub t_min: f64,
    pub t_max: f64,
}

impl Bounds {
    pub fn clamp_time(&self, t: f64) -> f64 {
        t.clamp(self.t_min, self.t_max)
    }
}

/// Decimate a series to at most target points, preserving extremes within time buckets
pub fn decimate_minmax(points: &[[f64; 2]], tmin: f64, tmax: f64, target: usize) -> Vec<[f64; 2]> {
    if points.is_empty() || target == 0 {
        return Vec::new();
    }
    if tmax <= tmin {
        return points.to_vec();
    }
    // Binary search start/end indices
    let start = match points.binary_search_by(|p| p[0].partial_cmp(&tmin).unwrap_or(std::cmp::Ordering::Less)) {
        Ok(i) | Err(i) => i,
    };
    let end = match points.binary_search_by(|p| p[0].partial_cmp(&tmax).unwrap_or(std::cmp::Ordering::Less)) {
        Ok(i) | Err(i) => i.min(points.len()),
    };
    let slice = &points[start..end];
    if slice.len() <= target {
        return slice.to_vec();
    }

    let buckets = target;
    let dt = (tmax - tmin) / buckets as f64;
    let mut out = Vec::with_capacity(buckets * 2);
    let mut i = 0;
    for b in 0..buckets {
        let bmin = tmin + b as f64 * dt;
        let bmax = if b + 1 == buckets { tmax } else { tmin + (b as f64 + 1.0) * dt };
        let mut local_min = (f64::INFINITY, f64::INFINITY);
        let mut local_max = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        while i < slice.len() && slice[i][0] < bmax {
            let t = slice[i][0];
            let v = slice[i][1];
            if v.is_finite() {
                if v < local_min.1 {
                    local_min = (t, v);
                }
                if v > local_max.1 {
                    local_max = (t, v);
                }
            }
            i += 1;
        }
        let have_min = local_min.1.is_finite();
        let have_max = local_max.1.is_finite();
        if have_min && have_max {
            if local_min.0 <= local_max.0 {
                out.push([local_min.0, local_min.1]);
                out.push([local_max.0, local_max.1]);
            } else {
                out.push([local_max.0, local_max.1]);
                out.push([local_min.0, local_min.1]);
            }
        } else if have_min {
            out.push([local_min.0, local_min.1]);
        } else if have_max {
            out.push([local_max.0, local_max.1]);
        } else {
            // No finite values in bucket; insert a gap by skipping
        }
    }
    out
}
