//! Deterministic waveforms in a generic, descriptor-driven tagged container.
//! PWAV is a synthetic fixture format defined by this generator and its decoders.
//! Build: rustc --edition=2021 -O samples/generators/generate_waveform_stress.rs -o /tmp/generate_waveform_stress
//! Binary: /tmp/generate_waveform_stress --output samples/data/waveform_demo_2gb.bin --size-gb 2
//! CSV: /tmp/generate_waveform_stress --csv --output samples/data/waveform_demo_2gb.csv --size-gb 2
use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const DATA_OFFSET: u64 = 4096;
const RECORD_BYTES: u64 = 64;
const TRAILER_BYTES: u64 = 64;
const BUFFER_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_BYTES: u64 = 40_000_000_000;
const PATTERN_RECORDS: usize = 384;
const CHANNELS: [(&str, u32, u32); 12] = [
    ("sine", 3, 8),
    ("triangle", 3, 8),
    ("ramp", 3, 8),
    ("chirp", 3, 8),
    ("square", 4, 4),
    ("pulse", 4, 4),
    ("staircase", 4, 4),
    ("damped_sine", 4, 4),
    ("zigzag", 6, 4),
    ("alternating", 6, 4),
    ("counter_ramp", 5, 4),
    ("bit_pattern", 5, 4),
];
fn data_offset(reordered: bool) -> u64 {
    if reordered {
        8192
    } else {
        DATA_OFFSET
    }
}
fn section(bytes: &mut [u8], id: u32, payload_bytes: u64) {
    bytes[0..4].copy_from_slice(&id.to_le_bytes());
    bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
    bytes[8..16].copy_from_slice(&payload_bytes.to_le_bytes());
}
fn total_bytes(count: u64, reordered: bool) -> Result<u64> {
    count
        .checked_mul(RECORD_BYTES)
        .and_then(|n| n.checked_add(data_offset(reordered) + TRAILER_BYTES))
        .ok_or_else(|| "Fixture size overflows u64".into())
}
fn header(count: u64, reordered: bool) -> Result<Vec<u8>> {
    let payload_start = data_offset(reordered) as usize;
    let sample_header = payload_start - 16;
    let mut bytes = vec![0u8; payload_start];
    bytes[0..4].copy_from_slice(b"PWAV");
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    bytes[8..16].copy_from_slice(&total_bytes(count, reordered)?.to_le_bytes());
    bytes[16..24].copy_from_slice(&count.to_le_bytes());
    bytes[24..28].copy_from_slice(&4u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&(CHANNELS.len() as u32).to_le_bytes());
    section(&mut bytes[32..48], 1, (CHANNELS.len() * 24) as u64);
    for n in 0..CHANNELS.len() {
        let position = if reordered { CHANNELS.len() - 1 - n } else { n };
        let (name, code, width) = CHANNELS[position];
        let at = 48 + n * 24;
        bytes[at..at + name.len()].copy_from_slice(name.as_bytes());
        bytes[at + 16..at + 20].copy_from_slice(&code.to_le_bytes());
        bytes[at + 20..at + 24].copy_from_slice(&width.to_le_bytes());
    }
    let unknown_at = 48 + CHANNELS.len() * 24;
    section(
        &mut bytes[unknown_at..unknown_at + 16],
        0x7fff_ffff,
        (sample_header - unknown_at - 16) as u64,
    );
    bytes[unknown_at + 16..sample_header].fill(0xA5);
    section(
        &mut bytes[sample_header..payload_start],
        2,
        count * RECORD_BYTES,
    );
    Ok(bytes)
}
fn footer(count: u64, reordered: bool) -> [u8; TRAILER_BYTES as usize] {
    let mut bytes = [0u8; TRAILER_BYTES as usize];
    section(&mut bytes[..16], 3, 48);
    bytes[16..24].copy_from_slice(b"PWAV-END");
    for (slot, value) in bytes[24..].chunks_exact_mut(8).zip([
        count,
        count * RECORD_BYTES,
        data_offset(reordered),
        count - 1,
        0,
    ]) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}
fn encode_record(out: &mut [u8], index: u64, reordered: bool) {
    use std::f64::consts::TAU;
    let p = (index % 128) as f64 / 128.0;
    let sine = 4.0 * (TAU * (index % 64) as f64 / 64.0).sin();
    let triangle = 10.0 + 4.0 * (1.0 - 4.0 * ((index % 96) as f64 / 96.0 - 0.5).abs());
    let ramp = 20.0 + 8.0 * ((index % 128) as f64 / 127.0 - 0.5);
    let chirp = 30.0 + 4.0 * (TAU * (p + 3.0 * p * p)).sin();
    let square = 40.0_f32 + if index % 64 < 32 { 3.0 } else { -3.0 };
    let pulse = 50.0_f32 + if index % 96 < 8 { 6.0 } else { 0.0 };
    let staircase = 60.0_f32 + ((index % 128) / 16) as f32;
    let damped_sine = (70.0 + 4.0 * (-3.0 * p).exp() * (TAU * 4.0 * p).sin()) as f32;
    let zigzag = 80 + ((index % 32) as i32 - 16).abs() / 2;
    let alternating = 100i32 + if (index / 8) % 2 == 0 { 3 } else { -3 };
    let counter_ramp = 110 + (index % 16) as u32;
    let bits = (index % 16) as u32;
    let bit_pattern = 130 + (bits ^ (bits >> 1));
    for (slot, value) in out[..32]
        .chunks_exact_mut(8)
        .zip([sine, triangle, ramp, chirp])
    {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    for (slot, value) in
        out[32..48]
            .chunks_exact_mut(4)
            .zip([square, pulse, staircase, damped_sine])
    {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    out[48..52].copy_from_slice(&zigzag.to_le_bytes());
    out[52..56].copy_from_slice(&alternating.to_le_bytes());
    out[56..60].copy_from_slice(&counter_ramp.to_le_bytes());
    out[60..64].copy_from_slice(&bit_pattern.to_le_bytes());
    if reordered {
        let original: [u8; RECORD_BYTES as usize] = out.try_into().unwrap();
        let mut source_end = original.len();
        let mut target = 0;
        for (_, _, width) in CHANNELS.iter().rev() {
            let width = *width as usize;
            out[target..target + width].copy_from_slice(&original[source_end - width..source_end]);
            source_end -= width;
            target += width;
        }
    }
}
fn fill_records(buffer: &mut [u8], first: u64, reordered: bool) {
    let seed_len = buffer.len().min(PATTERN_RECORDS * RECORD_BYTES as usize);
    for (index, record) in buffer[..seed_len]
        .chunks_exact_mut(RECORD_BYTES as usize)
        .enumerate()
    {
        encode_record(record, first + index as u64, reordered);
    }
    let mut filled = seed_len;
    while filled < buffer.len() {
        let copy = filled.min(buffer.len() - filled);
        buffer.copy_within(0..copy, filled);
        filled += copy;
    }
}

fn check_size(requested: u64, reordered: bool) -> Result<u64> {
    let count = requested
        .checked_sub(data_offset(reordered) + TRAILER_BYTES)
        .map(|n| n / RECORD_BYTES)
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            format!(
                "Need at least {} bytes for a header, one record, and footer",
                data_offset(reordered) + RECORD_BYTES + TRAILER_BYTES
            )
        })?;
    total_bytes(count, reordered)?;
    Ok(count)
}
fn probes(count: u64, reordered: bool) -> BTreeSet<u64> {
    let mut result: BTreeSet<_> = [
        0,
        1,
        127,
        128,
        511,
        512,
        524287,
        524288,
        1_000_003,
        (4 * 1024 * 1024 * 1024u64 - data_offset(reordered)) / RECORD_BYTES,
        (4 * 1024 * 1024 * 1024u64 - data_offset(reordered)) / RECORD_BYTES + 1,
        count / 2,
        count - 1,
    ]
    .into_iter()
    .filter(|n| *n < count)
    .collect();
    // Deterministic points across the complete extent, including after 32 GiB.
    for n in 0..64u64 {
        result.insert(((count as u128 - 1) * n as u128 / 63) as u64);
    }
    result
}
fn verify_file(file: &mut File, count: u64, reordered: bool) -> Result<()> {
    let actual = file.metadata()?.len();
    if actual != total_bytes(count, reordered)? {
        return Err(format!("Length mismatch: {actual}").into());
    }
    let mut head = vec![0; data_offset(reordered) as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut head)?;
    if head != header(count, reordered)? {
        return Err("Header/descriptors/section lengths differ from the recipe".into());
    }
    let mut tail = [0; TRAILER_BYTES as usize];
    file.seek(SeekFrom::Start(actual - TRAILER_BYTES))?;
    file.read_exact(&mut tail)?;
    if tail != footer(count, reordered) {
        return Err("Footer mismatch".into());
    }
    let checks = probes(count, reordered);
    for &index in &checks {
        let mut actual = [0; RECORD_BYTES as usize];
        let mut expected = actual;
        encode_record(&mut expected, index, reordered);
        file.seek(SeekFrom::Start(
            data_offset(reordered) + index * RECORD_BYTES,
        ))?;
        file.read_exact(&mut actual)?;
        if actual != expected {
            return Err(format!("Sample {index} does not match its expected bytes").into());
        }
    }
    println!("Verified header, footer, and {} sampled records across the file (not a full-file checksum).",checks.len());
    Ok(())
}
fn inspect(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let mut first = [0; 64];
    file.read_exact(&mut first)?;
    if &first[..4] != b"PWAV" {
        return Err("This is not a Binwave PWAV test fixture".into());
    }
    let count = u64::from_le_bytes(first[16..24].try_into()?);
    if count == 0 {
        return Err("Fixture has no samples".into());
    }
    let first_name = first[48..64].split(|byte| *byte == 0).next().unwrap();
    let reordered = first_name == CHANNELS.last().unwrap().0.as_bytes();
    verify_file(&mut file, count, reordered)?;
    println!(
        "{}: {} bytes; {count} records of {RECORD_BYTES} bytes",
        path.display(),
        total_bytes(count, reordered)?
    );
    for index in [0, count / 2, count - 1] {
        println!(
            "sample {index}: offset {} (0x{:X}), waveform phase {} / {PATTERN_RECORDS}",
            data_offset(reordered) + index * RECORD_BYTES,
            data_offset(reordered) + index * RECORD_BYTES,
            index % PATTERN_RECORDS as u64
        );
    }
    Ok(())
}
struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn generate(path: &Path, requested: u64, no_compression: bool, reordered: bool) -> Result<()> {
    let count = check_size(requested, reordered)?;
    let total = total_bytes(count, reordered)?;
    if fs::symlink_metadata(path).is_ok() {
        return Err(format!("Refusing to overwrite {}", path.display()).into());
    }
    let partial = path.with_file_name(format!(
        "{}.partial",
        path.file_name()
            .ok_or("Output needs a filename")?
            .to_string_lossy()
    ));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let guard = Partial(partial.clone());
    if no_compression {
        let status = std::process::Command::new("chattr")
            .arg("+m")
            .arg(&partial)
            .status()?;
        if !status.success() {
            return Err("Could not disable compression on the new fixture".into());
        }
    }
    println!(
        "Writing {} bytes ({:.3} GB / {:.3} GiB), {count} records, fully populated; 32 MiB buffer.",
        total,
        total as f64 / 1e9,
        total as f64 / 1073741824.
    );
    println!("Temporary file: {}", partial.display());
    let start = Instant::now();
    let mut progress = Instant::now();
    file.write_all(&header(count, reordered)?)?;
    let mut buffer = vec![0u8; BUFFER_BYTES];
    let mut first = 0u64;
    while first < count {
        let records = (count - first).min((BUFFER_BYTES / RECORD_BYTES as usize) as u64);
        let bytes = &mut buffer[..records as usize * RECORD_BYTES as usize];
        fill_records(bytes, first, reordered);
        file.write_all(bytes)?;
        first += records;
        if progress.elapsed() >= Duration::from_secs(2) || first == count {
            let done = data_offset(reordered) + first * RECORD_BYTES;
            let elapsed = start.elapsed().as_secs_f64();
            println!(
                "{:.2}/{:.2} GB ({:.1}%) · {:.0} MB/s · {:.0}s elapsed",
                done as f64 / 1e9,
                total as f64 / 1e9,
                100. * done as f64 / total as f64,
                done as f64 / 1e6 / elapsed,
                elapsed
            );
            io::stdout().flush()?;
            progress = Instant::now();
        }
    }
    file.write_all(&footer(count, reordered))?;
    println!("Flushing file to disk…");
    io::stdout().flush()?;
    file.sync_all()?;
    verify_file(&mut file, count, reordered)?;
    drop(file);
    // Publish atomically without ever replacing an existing destination.
    fs::hard_link(&partial, path)?;
    drop(guard);
    println!(
        "Ready: {} ({total} bytes), {:.1}s total",
        path.display(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn csv_header() -> String {
    let mut header = String::from("time");
    for (name, _, _) in CHANNELS {
        header.push(',');
        header.push_str(name);
    }
    header.push('\n');
    header
}

fn csv_values(index: u64) -> String {
    let mut record = [0; RECORD_BYTES as usize];
    encode_record(&mut record, index, false);
    let mut offset = 0;
    let mut values = String::new();
    for (_, code, width) in CHANNELS {
        let end = offset + width as usize;
        let bytes = &record[offset..end];
        // CSV is read as f64. Widen f32 before formatting so both views display
        // exactly the same value, including the binary record's f32 rounding.
        let value = match code {
            3 => f64::from_le_bytes(bytes.try_into().unwrap()).to_string(),
            4 => (f32::from_le_bytes(bytes.try_into().unwrap()) as f64).to_string(),
            5 => u32::from_le_bytes(bytes.try_into().unwrap()).to_string(),
            6 => i32::from_le_bytes(bytes.try_into().unwrap()).to_string(),
            _ => unreachable!("unsupported waveform channel type"),
        };
        values.push(',');
        values.push_str(&value);
        offset = end;
    }
    values.push('\n');
    values
}

/// Every row has a unique sample index followed by the common waveform period.
/// A short blank-line tail fills the requested size without a partial CSV row.
fn write_csv(output: &mut impl Write, requested: u64) -> Result<(u64, u64)> {
    let header = csv_header();
    let period: Vec<_> = (0..PATTERN_RECORDS as u64).map(csv_values).collect();
    let minimum = (header.len() + 1 + period[0].len()) as u64;
    if requested < minimum {
        return Err(
            format!("CSV needs at least {minimum} bytes for a header and one sample").into(),
        );
    }
    output.write_all(header.as_bytes())?;
    let mut written = header.len() as u64;
    let mut count = 0u64;
    let started = Instant::now();
    let mut progress = Instant::now();
    loop {
        let index = count.to_string();
        let values = &period[(count % PATTERN_RECORDS as u64) as usize];
        let row_bytes = (index.len() + values.len()) as u64;
        if row_bytes > requested - written {
            break;
        }
        output.write_all(index.as_bytes())?;
        output.write_all(values.as_bytes())?;
        count += 1;
        written += row_bytes;
        if count % 65536 == 0 && progress.elapsed() >= Duration::from_secs(2) {
            println!(
                "{:.2}/{:.2} GB ({:.1}%) · {count} CSV samples · {:.0}s elapsed",
                written as f64 / 1e9,
                requested as f64 / 1e9,
                100. * written as f64 / requested as f64,
                started.elapsed().as_secs_f64()
            );
            io::stdout().flush()?;
            progress = Instant::now();
        }
    }
    let blank_bytes = requested - written;
    output.write_all(&vec![b'\n'; blank_bytes as usize])?;
    Ok((count, blank_bytes))
}

fn generate_csv(path: &Path, requested: u64) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(format!("Refusing to overwrite {}", path.display()).into());
    }
    let partial = path.with_file_name(format!(
        "{}.partial",
        path.file_name()
            .ok_or("Output needs a filename")?
            .to_string_lossy()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let guard = Partial(partial.clone());
    let mut writer = BufWriter::with_capacity(BUFFER_BYTES, file);
    println!("Writing {requested} bytes of CSV; 32 MiB buffer, 12 waveform channels.");
    let started = Instant::now();
    let (count, blank_bytes) = write_csv(&mut writer, requested)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    if writer.get_ref().metadata()?.len() != requested {
        return Err("CSV output length does not match the requested size".into());
    }
    drop(writer);
    fs::hard_link(&partial, path)?;
    drop(guard);
    println!(
        "Ready: {} ({requested} bytes), {count} samples, {blank_bytes} trailing blank bytes, {:.1}s total",
        path.display(), started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let mut output = None;
    let mut size = DEFAULT_BYTES;
    let mut verify = None;
    let mut no_compression = false;
    let mut reordered = false;
    let mut csv = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-compression" => no_compression = true,
            "--reordered" => reordered = true,
            "--csv" => csv = true,
            "--output" => output = Some(PathBuf::from(args.next().ok_or("--output needs a path")?)),
            "--size-bytes" => size = args.next().ok_or("--size-bytes needs a number")?.parse()?,
            "--size-gb" => {
                size = args
                    .next()
                    .ok_or("--size-gb needs a whole number")?
                    .parse::<u64>()?
                    .checked_mul(1_000_000_000)
                    .ok_or("Size overflow")?
            }
            "--verify" => verify = Some(PathBuf::from(args.next().ok_or("--verify needs a path")?)),
            "--help" | "-h" => {
                println!("Generate deterministic waveforms as a PWAV tagged container or CSV.\n--csv               Write CSV: sample index (time) plus the same 12 channels\n--output PATH       New output file (never overwritten)\n--size-gb N         Decimal GB; default 40\n--size-bytes N      Exact CSV bytes; binary rounds down to complete records\n--reordered         Reverse descriptors and fields; move sample payload to byte 8192\n--no-compression    Disable compression on the new file (Linux chattr +m)\n--verify PATH       Verify metadata and distributed record probes\nBinary decoders: samples/decoders/python/waveform.py or samples/decoders/yaml/waveform.yaml.\nCSV opens directly; demo settings: samples/configs/waveform_demo_2gb.md.\nVerify the pair: python3 samples/generators/verify_waveform_demo.py --help");
                return Ok(());
            }
            _ => return Err(format!("Unknown argument {arg}").into()),
        }
    }
    if csv && (reordered || no_compression || verify.is_some()) {
        return Err(
            "--csv cannot be combined with --reordered, --no-compression, or --verify".into(),
        );
    }
    if let Some(path) = verify {
        inspect(&path)
    } else {
        let output = output.unwrap_or_else(|| {
            PathBuf::from(if csv {
                "samples/data/waveform_stress_40gb.csv"
            } else {
                "samples/data/waveform_stress_40gb.bin"
            })
        });
        if csv {
            generate_csv(&output, size)
        } else {
            generate(&output, size, no_compression, reordered)
        }
    }
}
