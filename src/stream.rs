use crate::config::{AppConfig, BinaryConfig, BinType, Endianness, FileKind};
use crate::data::Signal;
use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct BinaryReader {
    file: File,
    cfg: BinaryConfig,
    rec_size: usize,
    total_records: usize,
    time_field_idx: usize,
    time_off: usize,
    field_offsets: Vec<usize>,
    signal_field_indices: Vec<usize>, // indices into cfg.fields (excluding time)
    // Coarse index for seeking: (record_idx, time)
    coarse_index: Vec<(usize, f64)>,
}

impl BinaryReader {
    pub fn new(path: &Path, app_cfg: &AppConfig) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("Failed to open data file: {}", path.display()))?;
        let bcfg = app_cfg
            .binary
            .clone()
            .ok_or_else(|| anyhow!("binary config required for streaming"))?;

        // compute layout
        let (field_offsets, rec_size) = compute_layout(&bcfg)?;
        let meta = file.metadata()?;
        let total_records = (meta.len() as usize) / rec_size;
        if total_records == 0 {
            return Err(anyhow!("No records in file"));
        }
        // locate time field
        let mut time_field_idx = None;
        for (i, f) in bcfg.fields.iter().enumerate() {
            if f.name == bcfg.time_field { time_field_idx = Some(i); break; }
        }
        let time_field_idx = time_field_idx.ok_or_else(|| anyhow!("time field not found"))?;
        let time_off = field_offsets[time_field_idx];
        // signals included
        let mut signal_field_indices: Vec<usize> = (0..bcfg.fields.len()).filter(|&i| i != time_field_idx).collect();
        if let Some(include) = &bcfg.include_signals {
            let set: std::collections::HashSet<_> = include.iter().collect();
            signal_field_indices.retain(|i| set.contains(&bcfg.fields[*i].name));
        }

        let mut reader = Self {
            file,
            cfg: bcfg,
            rec_size,
            total_records,
            time_field_idx,
            time_off,
            field_offsets,
            signal_field_indices,
            coarse_index: Vec::new(),
        };
        reader.build_coarse_index()?;
        Ok(reader)
    }

    fn build_coarse_index(&mut self) -> Result<()> {
        let stride = self.cfg.index_stride.unwrap_or(10_000).max(1);
        self.coarse_index.clear();
        let mut i = 0usize;
        while i < self.total_records {
            let t = self.read_time_at(i)?;
            self.coarse_index.push((i, t));
            i = i.saturating_add(stride);
        }
        // ensure last
        if self.coarse_index.last().map(|(idx, _)| *idx != self.total_records - 1).unwrap_or(true) {
            let last_idx = self.total_records - 1;
            let t = self.read_time_at(last_idx)?;
            self.coarse_index.push((last_idx, t));
        }
        Ok(())
    }

    fn read_time_at(&mut self, rec_idx: usize) -> Result<f64> {
        let off = rec_idx * self.rec_size + self.time_off;
        self.file.seek(SeekFrom::Start(off as u64))?;
        read_number_as_f64(&mut self.file, &self.cfg.fields[self.time_field_idx].ty, self.cfg.endianness)
    }

    fn seek_start_idx(&self, tmin: f64) -> usize {
        // binary search in coarse index for first >= tmin
        let mut lo = 0usize;
        let mut hi = self.coarse_index.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.coarse_index[mid].1 < tmin { lo = mid + 1; } else { hi = mid; }
        }
        let anchor = if lo == 0 { 0 } else { self.coarse_index[lo - 1].0 };
        anchor
    }

    pub fn signal_names(&self) -> Vec<String> {
        self.signal_field_indices
            .iter()
            .map(|&i| self.cfg.fields[i].name.clone())
            .collect()
    }

    pub fn time_bounds(&mut self) -> Result<(f64, f64)> {
        let t0 = self.read_time_at(0)?;
        let t1 = self.read_time_at(self.total_records - 1)?;
        Ok((t0, t1))
    }

    pub fn read_window(&mut self, tmin: f64, tmax: f64, margin: f64) -> Result<Vec<Signal>> {
        let want_min = tmin - margin;
        let want_max = tmax + margin;
        let mut start_idx = self.seek_start_idx(want_min);
        // ensure we include the anchor itself even if < want_min for continuity
        let mut points: Vec<Vec<[f64;2]>> = self.signal_field_indices.iter().map(|_| Vec::with_capacity(4096)).collect();
        let mut minv = vec![f64::INFINITY; self.signal_field_indices.len()];
        let mut maxv = vec![f64::NEG_INFINITY; self.signal_field_indices.len()];

        let mut idx = start_idx;
        while idx < self.total_records {
            let base = idx * self.rec_size;
            // read time
            self.file.seek(SeekFrom::Start((base + self.time_off) as u64))?;
            let t = read_number_as_f64(&mut self.file, &self.cfg.fields[self.time_field_idx].ty, self.cfg.endianness)?;
            if t > want_max { break; }
            // read signals
            for (ci, &fi) in self.signal_field_indices.iter().enumerate() {
                let off = self.field_offsets[fi];
                self.file.seek(SeekFrom::Start((base + off) as u64))?;
                let v = read_number_as_f64(&mut self.file, &self.cfg.fields[fi].ty, self.cfg.endianness)?;
                if v.is_finite() { if v < minv[ci] { minv[ci] = v; } if v > maxv[ci] { maxv[ci] = v; } }
                points[ci].push([t, v]);
            }
            idx += 1;
        }

        let mut signals = Vec::with_capacity(self.signal_field_indices.len());
        for (ci, &fi) in self.signal_field_indices.iter().enumerate() {
            let name = self.cfg.fields[fi].name.clone();
            let mut min = minv[ci];
            let mut max = maxv[ci];
            if !min.is_finite() { min = 0.0; }
            if !max.is_finite() { max = 0.0; }
            signals.push(Signal { name, points: std::mem::take(&mut points[ci]), min, max });
        }
        Ok(signals)
    }
}

fn compute_layout(cfg: &BinaryConfig) -> Result<(Vec<usize>, usize)> {
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

fn read_number_as_f64(file: &mut File, ty: &BinType, end: Endianness) -> Result<f64> {
    use Endianness::*;
    match ty {
        BinType::F32 => {
            let mut b = [0u8; 4];
            file.read_exact(&mut b)?;
            let v = match end { Little => f32::from_le_bytes(b), Big => f32::from_be_bytes(b) };
            Ok(v as f64)
        }
        BinType::F64 => {
            let mut b = [0u8; 8];
            file.read_exact(&mut b)?;
            let v = match end { Little => f64::from_le_bytes(b), Big => f64::from_be_bytes(b) };
            Ok(v)
        }
        BinType::I8 => { let mut b = [0u8;1]; file.read_exact(&mut b)?; Ok((b[0] as i8) as f64) }
        BinType::U8 => { let mut b = [0u8;1]; file.read_exact(&mut b)?; Ok((b[0] as u8) as f64) }
        BinType::I16 => { let mut b=[0u8;2]; file.read_exact(&mut b)?; let v=match end{Little=>i16::from_le_bytes(b),Big=>i16::from_be_bytes(b)}; Ok(v as f64) }
        BinType::U16 => { let mut b=[0u8;2]; file.read_exact(&mut b)?; let v=match end{Little=>u16::from_le_bytes(b),Big=>u16::from_be_bytes(b)}; Ok(v as f64) }
        BinType::I32 => { let mut b=[0u8;4]; file.read_exact(&mut b)?; let v=match end{Little=>i32::from_le_bytes(b),Big=>i32::from_be_bytes(b)}; Ok(v as f64) }
        BinType::U32 => { let mut b=[0u8;4]; file.read_exact(&mut b)?; let v=match end{Little=>u32::from_le_bytes(b),Big=>u32::from_be_bytes(b)}; Ok(v as f64) }
        BinType::I64 => { let mut b=[0u8;8]; file.read_exact(&mut b)?; let v=match end{Little=>i64::from_le_bytes(b),Big=>i64::from_be_bytes(b)}; Ok(v as f64) }
        BinType::U64 => { let mut b=[0u8;8]; file.read_exact(&mut b)?; let v=match end{Little=>u64::from_le_bytes(b),Big=>u64::from_be_bytes(b)}; Ok(v as f64) }
    }
}

pub struct StreamingState {
    reader: BinaryReader,
    // cache of current window
    window_tmin: f64,
    window_tmax: f64,
    margin: f64,
    window_signals: Vec<Signal>,
    t0: f64,
    t1: f64,
}

impl StreamingState {
    pub fn new(path: &Path, cfg: &AppConfig) -> Result<Self> {
        let mut reader = BinaryReader::new(path, cfg)?;
        let (t0, t1) = reader.time_bounds()?;
        Ok(Self { reader, window_tmin: t0, window_tmax: t0, margin: 0.0, window_signals: Vec::new(), t0, t1 })
    }

    pub fn signal_names(&self) -> Vec<String> { self.reader.signal_names() }

    pub fn time_bounds(&mut self) -> Result<(f64,f64)> { self.reader.time_bounds() }

    pub fn cached_time_bounds(&self) -> (f64,f64) { (self.t0, self.t1) }

    pub fn ensure_window(&mut self, tmin: f64, tmax: f64) -> Result<()> {
        let range = tmax - tmin;
        let margin = range * 0.5;
        let need_reload = self.window_signals.is_empty()
            || tmin < self.window_tmin + margin * 0.25
            || tmax > self.window_tmax - margin * 0.25;
        if need_reload {
            let signals = self.reader.read_window(tmin, tmax, margin)?;
            self.window_tmin = tmin - margin;
            self.window_tmax = tmax + margin;
            self.margin = margin;
            self.window_signals = signals;
        }
        Ok(())
    }

    pub fn window_signals(&self) -> &Vec<Signal> { &self.window_signals }
}
