//! The large automated fixture is deliberately sparse. The user-facing generator
//! writes every record; its output is checked separately by the ignored test.
#![allow(dead_code)]
#![cfg(not(target_arch = "wasm32"))]
#[path = "../src/binary/document.rs"]
mod document;
#[path = "../src/binary/layout.rs"]
mod layout;
#[path = "../src/binary/numeric.rs"]
mod numeric;
#[path = "../src/binary/sections.rs"]
mod sections;
use layout::{RecordLayout, ValueIdentity};
use sections::{Layout, ResolvedLayout};
mod binary {
    pub use crate::document::Original;
}
#[path = "../src/byte_store.rs"]
mod byte_store;
#[path = "../src/cache.rs"]
mod cache;
#[path = "../src/file_range.rs"]
mod file_range;
#[path = "../src/file_source.rs"]
mod file_source;
#[path = "../src/structure.rs"]
mod structure;
#[path = "../src/structure_plot.rs"]
mod structure_plot;

use crate::byte_store::{ByteStore, ByteView};
use crate::file_source::FileSource;
use crate::structure::{Node, ParseOutput, Program, Value};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Instant;

const YAML: &str = include_str!("../samples/decoders/yaml/waveform.yaml");
const DATA_OFFSET: u64 = 4096;
const STRIDE: u64 = 64;
const FOOTER_SIZE: u64 = 64;
const DESCRIPTORS: [(&str, u32, u32); 12] = [
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

fn values(index: u64) -> [Value; 12] {
    use std::f64::consts::TAU;
    let p = (index % 128) as f64 / 128.0;
    let bits = (index % 16) as u32;
    [
        Value::Float(4.0 * (TAU * (index % 64) as f64 / 64.0).sin()),
        Value::Float(10.0 + 4.0 * (1.0 - 4.0 * ((index % 96) as f64 / 96.0 - 0.5).abs())),
        Value::Float(20.0 + 8.0 * ((index % 128) as f64 / 127.0 - 0.5)),
        Value::Float(30.0 + 4.0 * (TAU * (p + 3.0 * p * p)).sin()),
        Value::Float((40.0_f32 + if index % 64 < 32 { 3.0 } else { -3.0 }) as f64),
        Value::Float((50.0_f32 + if index % 96 < 8 { 6.0 } else { 0.0 }) as f64),
        Value::Float((60.0_f32 + ((index % 128) / 16) as f32) as f64),
        Value::Float((70.0 + 4.0 * (-3.0 * p).exp() * (TAU * 4.0 * p).sin()) as f32 as f64),
        Value::Signed((80 + ((index % 32) as i32 - 16).abs() / 2) as i64),
        Value::Signed((100i32 + if (index / 8) % 2 == 0 { 3 } else { -3 }) as i64),
        Value::Unsigned((110 + bits) as u64),
        Value::Unsigned((130 + (bits ^ (bits >> 1))) as u64),
    ]
}

fn record(index: u64, order: &[usize]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(STRIDE as usize);
    let expected = values(index);
    for &position in order {
        let (_, _, width) = DESCRIPTORS[position];
        let value = expected[position].clone();
        match value {
            Value::Unsigned(value) => {
                bytes.extend_from_slice(&value.to_le_bytes()[..width as usize])
            }
            Value::Signed(value) => bytes.extend_from_slice(&value.to_le_bytes()[..width as usize]),
            Value::Float(value) if width == 4 => {
                bytes.extend_from_slice(&(value as f32).to_le_bytes())
            }
            Value::Float(value) => bytes.extend_from_slice(&value.to_le_bytes()),
            _ => unreachable!(),
        }
    }
    assert_eq!(bytes.len(), STRIDE as usize);
    bytes
}

fn write_section(bytes: &mut [u8], offset: usize, id: u32, length: u64) {
    bytes[offset..offset + 4].copy_from_slice(&id.to_le_bytes());
    bytes[offset + 8..offset + 16].copy_from_slice(&length.to_le_bytes());
}

/// Independent byte encoder for the documented fixture, kept outside the generator.
fn header(count: u64, data_offset: u64, order: &[usize]) -> Vec<u8> {
    let mut bytes = vec![0; data_offset as usize];
    bytes[0..4].copy_from_slice(b"PWAV");
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    bytes[8..16].copy_from_slice(&(data_offset + count * STRIDE + FOOTER_SIZE).to_le_bytes());
    bytes[16..24].copy_from_slice(&count.to_le_bytes());
    bytes[24..28].copy_from_slice(&4u32.to_le_bytes());
    bytes[28..32].copy_from_slice(&(DESCRIPTORS.len() as u32).to_le_bytes());
    let description_bytes = DESCRIPTORS.len() * 24;
    write_section(&mut bytes, 32, 1, description_bytes as u64);
    for (index, &position) in order.iter().enumerate() {
        let (name, format, width) = DESCRIPTORS[position];
        let offset = 48 + index * 24;
        bytes[offset..offset + name.len()].copy_from_slice(name.as_bytes());
        bytes[offset + 16..offset + 20].copy_from_slice(&format.to_le_bytes());
        bytes[offset + 20..offset + 24].copy_from_slice(&width.to_le_bytes());
    }
    let unknown_header = 48 + description_bytes;
    let unknown_payload = unknown_header + 16;
    write_section(
        &mut bytes,
        unknown_header,
        0x7FFF_FFFF,
        data_offset - unknown_payload as u64 - 16,
    );
    bytes[unknown_payload..data_offset as usize - 16].fill(0xA5);
    write_section(&mut bytes, data_offset as usize - 16, 2, count * STRIDE);
    bytes
}

fn footer(count: u64, data_offset: u64) -> Vec<u8> {
    let mut bytes = vec![0; FOOTER_SIZE as usize];
    write_section(&mut bytes, 0, 3, 48);
    bytes[16..24].copy_from_slice(b"PWAV-END");
    for (index, value) in [count, count * STRIDE, data_offset, count - 1, 0]
        .into_iter()
        .enumerate()
    {
        bytes[24 + index * 8..32 + index * 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn page_starts(count: usize, data_offset: u64) -> Vec<usize> {
    let mut starts = BTreeSet::new();
    for index in [0, 128, 1_000_003, count / 2, count.saturating_sub(16)] {
        if index < count {
            starts.insert(index);
        }
    }
    if let Ok(boundary) = usize::try_from((1u64 << 32).saturating_sub(data_offset) / STRIDE) {
        if boundary < count {
            starts.insert(boundary.saturating_sub(1));
        }
    }
    starts.into_iter().collect()
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
}
impl Fixture {
    fn sparse(count: u64) -> Self {
        Self::with_layout(
            count,
            DATA_OFFSET,
            &(0..DESCRIPTORS.len()).collect::<Vec<_>>(),
        )
    }
    fn with_layout(count: u64, data_offset: u64, order: &[usize]) -> Self {
        assert!(count > 0);
        assert!(data_offset >= 384);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("parser-fixture.bin");
        let mut file = File::create(&path).unwrap();
        crate::cache::set_fixture_len(&file, data_offset + count * STRIDE + FOOTER_SIZE)
            .unwrap();
        file.write_all(&header(count, data_offset, order)).unwrap();
        let mut indices = BTreeSet::new();
        if count <= 128 {
            indices.extend(0..count);
        } else {
            for start in page_starts(count as usize, data_offset) {
                indices.extend(start as u64..(start as u64 + 16).min(count));
            }
        }
        for index in indices {
            file.seek(SeekFrom::Start(data_offset + index * STRIDE))
                .unwrap();
            file.write_all(&record(index, order)).unwrap();
        }
        file.seek(SeekFrom::Start(data_offset + count * STRIDE))
            .unwrap();
        file.write_all(&footer(count, data_offset)).unwrap();
        Self {
            _directory: directory,
            path,
        }
    }
    fn patch(&self, offset: u64, bytes: &[u8]) {
        let mut file = OpenOptions::new().write(true).open(&self.path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(bytes).unwrap();
    }
    fn view(&self) -> ByteView {
        view(&self.path)
    }
}

fn view(path: &Path) -> ByteView {
    ByteStore::from_file(Arc::new(FileSource::open(path).unwrap())).view()
}
fn child<'a>(node: &'a Node, name: &str) -> &'a Node {
    node.children.iter().find(|node| node.name == name).unwrap()
}
fn unsigned(node: &Node) -> u64 {
    match node.value.as_ref().unwrap() {
        Value::Unsigned(value) => *value,
        value => panic!("expected exact unsigned integer, got {value:?}"),
    }
}
fn records(output: &ParseOutput) -> &Node {
    child(child(&output.nodes[1].children[2], "payload"), "records")
}
fn node_count(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|node| 1 + node_count(&node.children))
        .sum()
}
fn assert_record(node: &Node, index: u64, data_offset: u64) {
    assert_eq!(node.range.start as u64, data_offset + index * STRIDE);
    assert_eq!(node.range.len(), STRIDE as usize);
    assert_eq!(node.children.len(), DESCRIPTORS.len());
    for ((name, _, _), expected) in DESCRIPTORS.iter().zip(values(index)) {
        assert_eq!(
            child(node, name).value.as_ref(),
            Some(&expected),
            "record {index}, field {name}"
        );
    }
}
fn assert_container(output: &ParseOutput, file_size: usize) -> u64 {
    assert_eq!(output.consumed, file_size);
    assert_eq!(output.nodes.len(), 2);
    let file_header = &output.nodes[0];
    let count = unsigned(child(file_header, "sample_count"));
    assert_eq!(unsigned(child(file_header, "file_bytes")), file_size as u64);
    let data_offset = records(output).range.start as u64;
    assert_eq!(data_offset + count * STRIDE + FOOTER_SIZE, file_size as u64);
    assert_eq!(output.nodes[1].children.len(), 4);
    let unknown = child(&output.nodes[1].children[1], "payload");
    assert_eq!(unknown.kind, "bytes");
    assert_eq!(unknown.range.end as u64, data_offset - 16);
    assert_eq!(unknown.value, Some(Value::Bytes(vec![0xA5; 64])));
    let sample_definition = output
        .definitions
        .iter()
        .find(|definition| definition.name == "Sample")
        .unwrap();
    assert_eq!(
        sample_definition.origin,
        Some(48..48 + DESCRIPTORS.len() * 24)
    );
    assert_eq!(sample_definition.fields.len(), DESCRIPTORS.len());
    let file_footer = child(&output.nodes[1].children[3], "payload");
    for (name, expected) in [
        ("sample_count", count),
        ("payload_bytes", count * STRIDE),
        ("sample_offset", data_offset),
        ("final_index", count - 1),
        ("generator_seed", 0),
    ] {
        assert_eq!(unsigned(child(file_footer, name)), expected);
    }
    assert_eq!(file_footer.range.end, file_size);
    count
}

#[test]
fn waveform_fixture_decodes_descriptions_unknown_sections_and_waveforms() {
    let fixture = Fixture::sparse(32);
    let source = fixture.view();
    let output = Program::parse(YAML)
        .unwrap()
        .run(&source, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(assert_container(&output, source.len()), 32);
    let samples = records(&output);
    assert!(samples.lazy.is_none());
    assert_eq!(samples.children.len(), 32);
    for (index, sample) in samples.children.iter().enumerate() {
        assert_record(sample, index as u64, samples.range.start as u64);
    }
}

#[test]
fn waveform_fixture_discovers_reordered_fields_and_shifted_payload_with_same_yaml() {
    let order: Vec<usize> = (0..DESCRIPTORS.len()).rev().collect();
    let fixture = Fixture::with_layout(8, 8192, &order);
    let source = fixture.view();
    let output = Program::parse(YAML)
        .unwrap()
        .run(&source, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(assert_container(&output, source.len()), 8);
    let samples = records(&output);
    assert_eq!(samples.range.start, 8192);
    assert_eq!(samples.children[0].children[0].name, "bit_pattern");
    assert_eq!(samples.children[0].children[11].name, "sine");
    for (index, sample) in samples.children.iter().enumerate() {
        assert_record(sample, index as u64, samples.range.start as u64);
    }
}

#[test]
fn waveform_fixture_header_descriptors_change_later_decoding_without_yaml_changes() {
    let fixture = Fixture::sparse(1);
    // The same eight ramp bytes now decode as u64 because the file says so.
    fixture.patch(48 + 2 * 24 + 16, &1u32.to_le_bytes());
    let output = Program::parse(YAML)
        .unwrap()
        .run(&fixture.view(), &AtomicBool::new(false))
        .unwrap();
    let ramp = child(&records(&output).children[0], "ramp");
    assert_eq!(ramp.kind, "u64");
    let Value::Float(expected) = values(0)[2] else {
        unreachable!()
    };
    assert_eq!(ramp.value, Some(Value::Unsigned(expected.to_bits())));
}

#[test]
fn waveform_fixture_rejects_incomplete_records_and_out_of_file_sections() {
    let fixture = Fixture::sparse(256);
    fixture.patch(4088, &(256u64 * STRIDE - 1).to_le_bytes());
    let program = Program::parse(YAML).unwrap();
    let error = format!(
        "{:#}",
        program
            .run(&fixture.view(), &AtomicBool::new(false))
            .unwrap_err()
    );
    assert!(error.contains("trailing bytes"), "{error}");
    fixture.patch(4088, &(256u64 * STRIDE + FOOTER_SIZE + 1).to_le_bytes());
    assert!(program
        .run(&fixture.view(), &AtomicBool::new(false))
        .is_err());
}

#[test]
#[cfg(target_pointer_width = "64")]
fn waveform_fixture_40gb_array_is_lazy_and_pages_beyond_four_gib() {
    let file_size = 40_000_000_000u64;
    let count = (file_size - DATA_OFFSET - FOOTER_SIZE) / STRIDE;
    let fixture = Fixture::sparse(count);
    verify_large_file(&fixture.path);
}

fn verify_large_file(path: &Path) {
    let source = view(path);
    let started = Instant::now();
    let output = Program::parse(YAML)
        .unwrap()
        .run(&source, &AtomicBool::new(false))
        .unwrap();
    let apply_elapsed = started.elapsed();
    let count = assert_container(&output, source.len());
    let samples = records(&output);
    let lazy = samples
        .lazy
        .as_ref()
        .expect("large sample collection must stay deferred");
    assert_eq!(lazy.count as u64, count);
    assert_eq!(lazy.stride as u64, STRIDE);
    let data_offset = samples.range.start as u64;
    assert_eq!(samples.range.end as u64, source.len() as u64 - FOOTER_SIZE);
    assert!(samples.children.is_empty());
    assert!(node_count(&output.nodes) < 128);
    let mut checked = 0;
    for start in page_starts(lazy.count, data_offset) {
        let page = lazy
            .page(&source, &AtomicBool::new(false), start, 16)
            .unwrap();
        for (relative, sample) in page.iter().enumerate() {
            assert_record(sample, (start + relative) as u64, data_offset);
        }
        checked += page.len();
    }
    assert!(lazy
        .page(&source, &AtomicBool::new(false), lazy.count + 1, 1)
        .is_err());
    println!(
        "{}: {} bytes; {count} records; {} initial nodes; apply {:.3}s; verified {checked} records from bounded pages across the file in {:.3}s total",
        path.display(), source.len(), node_count(&output.nodes), apply_elapsed.as_secs_f64(), started.elapsed().as_secs_f64()
    );
}

#[test]
#[ignore = "requires generated fixture; set BINWAVE_WAVEFORM_FILE to choose its path"]
#[cfg(target_pointer_width = "64")]
fn verify_generated_waveform_stress_file() {
    let path = std::env::var_os("BINWAVE_WAVEFORM_FILE").or_else(|| std::env::var_os("PLOTTERMAX_WAVEFORM_FILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/data/waveform_stress_40gb.bin")
        });
    assert!(
        path.is_file(),
        "generate the fixture first: {}",
        path.display()
    );
    verify_large_file(&path);
}

/// Exercises the same asynchronous plot reader as the UI against actual generated
/// bytes. The default run intentionally avoids requiring a 40 GB local artifact.
#[test]
#[ignore = "requires generated fixture; set BINWAVE_WAVEFORM_FILE to choose its path"]
#[cfg(target_pointer_width = "64")]
fn verify_generated_waveform_plot_ranges() {
    use structure_plot::{PlotQuality, PlotReader};
    let path = std::env::var_os("BINWAVE_WAVEFORM_FILE").or_else(|| std::env::var_os("PLOTTERMAX_WAVEFORM_FILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/data/waveform_stress_40gb.bin")
        });
    let source = view(&path);
    let output = Program::parse(YAML)
        .unwrap()
        .run(&source, &AtomicBool::new(false))
        .unwrap();
    let node = records(&output);
    let count = node
        .lazy
        .as_ref()
        .map_or(node.children.len(), |array| array.count);
    assert!(count > 4096);
    let context = egui::Context::default();
    let mut reader = PlotReader::default();
    let descriptor_bytes = source.read(48, DESCRIPTORS.len() * 24).unwrap();
    let mut offsets = std::collections::BTreeMap::new();
    let mut field_offset = 0;
    for descriptor in descriptor_bytes.chunks_exact(24) {
        let name = String::from_utf8(
            descriptor[..16]
                .iter()
                .copied()
                .take_while(|byte| *byte != 0)
                .collect(),
        )
        .unwrap();
        offsets.insert(name, field_offset);
        field_offset += u32::from_le_bytes(descriptor[20..24].try_into().unwrap()) as usize;
    }
    let ranges = [
        0..4096,
        count / 2..count / 2 + 4096,
        count - 4096..count,
        0..count,
    ];
    for range in ranges {
        let started = Instant::now();
        reader.request(
            &context,
            source.clone(),
            node.clone(),
            range.clone(),
            1000,
            false,
        );
        while reader.is_loading() {
            reader.tick(&context);
            assert!(
                started.elapsed().as_secs() < 60,
                "plot range worker timed out: {range:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(reader.error().is_none(), "{:?}", reader.error());
        let data = reader.data().expect("plot should have data");
        assert!(data.complete);
        assert_eq!(data.range, range);
        assert_eq!(data.series.len(), DESCRIPTORS.len());
        assert!(data.series.values().map(Vec::len).sum::<usize>() < 100_000);
        if count > 65_536 && range.len() == count {
            assert_eq!(data.quality, PlotQuality::SampledPreview);
            assert!(data.scanned < count);
        }
        let mut checked = 0;
        for (position, (name, _, width)) in DESCRIPTORS.iter().enumerate() {
            let series = &data.series[*name];
            assert!(!series.is_empty());
            assert_eq!(series.first().unwrap().index, range.start);
            assert_eq!(series.last().unwrap().index, range.end - 1);
            for point in series {
                assert!(range.contains(&point.index));
                let expected = values(point.index as u64)[position].numeric().unwrap();
                assert_eq!(point.value, expected, "sample {}, {name}", point.index);
                let offset = node.range.start + point.index * STRIDE as usize + offsets[*name];
                assert_eq!(point.bytes, offset..offset + *width as usize);
                assert_eq!(point.path, format!("{}[{}].{name}", node.path, point.index));
                checked += 1;
            }
        }
        println!(
            "{} plot {:?}: {:?}, {} scanned, {} exact field values and byte ranges checked, {:.3}s",
            path.display(),
            range,
            data.quality,
            data.scanned,
            checked,
            started.elapsed().as_secs_f64()
        );
    }
}
