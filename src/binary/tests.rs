use super::*;
use std::sync::atomic::AtomicBool;

fn layout(endian: &str, kind: &str, size: usize, scale: &str) -> Layout {
    Layout::parse(&format!("endianness: {endian}\nrecord_size: {size}\nfields:\n  - name: value\n    offset: 0\n    type: {kind}\n    scale: {scale}\n")).unwrap()
}

fn document(bytes: &[u8], layout: Layout) -> BinaryDocument {
    BinaryDocument::new(Original::from_bytes(bytes).unwrap(), layout).unwrap()
}

fn working(doc: &BinaryDocument) -> Vec<u8> {
    (0..doc.bytes.original.bytes().len())
        .map(|i| doc.bytes.byte(i))
        .collect()
}

#[test]
fn signed_unsigned_and_float_decoding_both_byte_orders() {
    for endian in ["little", "big"] {
        let little = endian == "little";
        let cases = [
            ("int8", vec![0x80], -128.),
            ("uint8", vec![0xff], 255.),
            (
                "int16",
                if little {
                    (-12345i16).to_le_bytes()
                } else {
                    (-12345i16).to_be_bytes()
                }
                .to_vec(),
                -12345.,
            ),
            (
                "uint16",
                if little {
                    60000u16.to_le_bytes()
                } else {
                    60000u16.to_be_bytes()
                }
                .to_vec(),
                60000.,
            ),
            (
                "int32",
                if little {
                    i32::MIN.to_le_bytes()
                } else {
                    i32::MIN.to_be_bytes()
                }
                .to_vec(),
                i32::MIN as f64,
            ),
            (
                "uint32",
                if little {
                    u32::MAX.to_le_bytes()
                } else {
                    u32::MAX.to_be_bytes()
                }
                .to_vec(),
                u32::MAX as f64,
            ),
            (
                "float32",
                if little {
                    (-12.5f32).to_le_bytes()
                } else {
                    (-12.5f32).to_be_bytes()
                }
                .to_vec(),
                -12.5,
            ),
            (
                "float64",
                if little {
                    1.25e100f64.to_le_bytes()
                } else {
                    1.25e100f64.to_be_bytes()
                }
                .to_vec(),
                1.25e100,
            ),
        ];
        for (kind, bytes, expected) in cases {
            let doc = document(&bytes, layout(endian, kind, bytes.len(), "1"));
            assert_eq!(
                doc.bytes.value(&doc.layout, 0, 0).unwrap(),
                expected,
                "{endian} {kind}"
            );
            assert_eq!(
                doc.layout.encode(0, &expected.to_string()).unwrap(),
                bytes,
                "encode {endian} {kind}"
            );
        }
    }
}

#[test]
fn exact_decimal_scaling_and_integer_representability() {
    let mut doc = document(&[0, 0], layout("little", "int16", 2, "0.01"));
    assert_eq!(doc.edit(0, 0, "23.45").unwrap(), "23.45");
    assert_eq!(working(&doc), 2345i16.to_le_bytes());
    assert!(doc
        .edit(0, 0, "23.451")
        .unwrap_err()
        .to_string()
        .contains("value"));
    assert_eq!(working(&doc), 2345i16.to_le_bytes());
    assert_eq!(doc.edit(0, 0, "-327.68").unwrap(), "-327.68");
    assert!(doc.edit(0, 0, "327.68").is_err());
    assert!(doc.edit(0, 0, "-327.69").is_err());
    let mut negative = document(&[0, 0], layout("big", "int16", 2, "-0.01"));
    negative.edit(0, 0, "23.45").unwrap();
    assert_eq!(working(&negative), (-2345i16).to_be_bytes());
    for scale in ["0.10000000000000000001", "1e-300", "1e300"] {
        let mut doc = document(&[0], layout("little", "uint8", 1, scale));
        doc.edit(0, 0, scale).unwrap();
        assert_eq!(working(&doc), [1]);
        let displayed = doc.text(0, 0).unwrap();
        doc.edit(0, 0, &displayed).unwrap();
        assert_eq!(working(&doc), [1]);
    }
}

#[test]
fn every_integer_type_enforces_its_bounds_without_rounding() {
    for (kind, size, min, max) in [
        ("int8", 1, -128i64, 127i64),
        ("uint8", 1, 0, 255),
        ("int16", 2, -32768, 32767),
        ("uint16", 2, 0, 65535),
        ("int32", 4, i32::MIN as i64, i32::MAX as i64),
        ("uint32", 4, 0, u32::MAX as i64),
    ] {
        let mut doc = document(&vec![0; size], layout("little", kind, size, "1"));
        for value in [min, max] {
            assert_eq!(
                doc.edit(0, 0, &value.to_string()).unwrap(),
                value.to_string()
            );
        }
        for text in [
            (min - 1).to_string(),
            (max + 1).to_string(),
            "1.1".into(),
            "NaN".into(),
            "1+2".into(),
            "--1".into(),
        ] {
            assert!(doc.edit(0, 0, &text).is_err(), "{kind} accepted {text}");
        }
    }
}

#[test]
fn float32_reports_encoded_rounding_and_nonfinite_is_explicit() {
    let mut doc = document(&[0; 4], layout("little", "float32", 4, "1"));
    let actual = doc.edit(0, 0, "0.1").unwrap();
    assert_eq!(actual, "0.10000000149011612");
    assert_eq!(working(&doc), 0.1f32.to_le_bytes());
    assert!(doc.edit(0, 0, "1e40").is_err());
    assert!(doc.edit(0, 0, "1e999").is_err());
    assert_eq!(doc.edit(0, 0, "inf").unwrap(), "inf");
    assert_eq!(doc.edit(0, 0, "NaN").unwrap(), "NaN");
}

#[test]
fn exact_bytes_padding_header_undo_redo_and_saved_checkpoint() {
    let original = DEMO_BYTES;
    let mut doc = document(original, Layout::parse(DEMO_YAML).unwrap());
    assert_eq!(doc.records, 200);
    assert_eq!(doc.text(73, 1).unwrap(), "85");
    doc.edit(73, 1, "23.45").unwrap();
    let range = doc.layout.field_range(73, 1).unwrap();
    assert_eq!(range, 896..898);
    let edited = working(&doc);
    assert_eq!(&edited[range.clone()], &2345i16.to_le_bytes());
    for i in 0..original.len() {
        if !range.contains(&i) {
            assert_eq!(original[i], edited[i], "changed unrelated byte {i}");
        }
    }
    assert!(doc.is_dirty());
    assert!(doc.modified(73, 1));
    assert!(!doc.modified(73, 2));
    let saved = doc.bytes.clone();
    doc.mark_saved(&saved);
    assert!(!doc.is_dirty());
    assert_eq!(doc.undo(), doc.layout.identity(73, 1).ok());
    assert_eq!(working(&doc), original);
    assert!(doc.is_dirty());
    assert_eq!(doc.redo(), doc.layout.identity(73, 1).ok());
    assert_eq!(working(&doc), edited);
    assert!(!doc.is_dirty());
    doc.edit(73, 1, "21").unwrap();
    doc.edit(74, 2, "500").unwrap();
    while doc.can_undo() {
        doc.undo();
    }
    assert_eq!(working(&doc), original);
    while doc.can_redo() {
        doc.redo();
    }
    assert_eq!(doc.text(73, 1).unwrap(), "21");
    assert_eq!(doc.text(74, 2).unwrap(), "500");
    doc.undo();
    doc.edit(75, 1, "22").unwrap();
    assert!(!doc.can_redo());
}

#[test]
fn export_is_identical_unedited_and_never_clobbers_without_confirmation() {
    let mut doc = document(DEMO_BYTES, Layout::parse(DEMO_YAML).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("copy.bin");
    write_copy(&doc.bytes, &path, false).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), DEMO_BYTES);
    doc.edit(73, 1, "23.45").unwrap();
    assert!(write_copy(&doc.bytes, &path, false).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), DEMO_BYTES);
    write_copy(&doc.bytes, &path, true).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), working(&doc));
    doc.undo();
    write_copy(&doc.bytes, &path, true).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), DEMO_BYTES);
}

#[test]
fn snapshot_survives_source_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.bin");
    std::fs::write(&path, DEMO_BYTES).unwrap();
    let snapshot = Original::open_cached(
        &path,
        &dir.path().join("cache"),
        &crate::cache::LoadProgress::default(),
    )
    .unwrap();
    std::fs::write(&path, b"replaced").unwrap();
    assert_eq!(snapshot.bytes(), DEMO_BYTES);
}

#[test]
fn existing_nan_payloads_infinities_and_signed_zero_survive_other_edits() {
    let mut bytes = Vec::new();
    for bits in [
        0x7fc01234u32,
        0x7f800000,
        0xff800000,
        0x80000000,
        0x3f800000,
    ] {
        bytes.extend(bits.to_le_bytes());
    }
    let mut doc = document(&bytes, layout("little", "float32", 4, "1"));
    assert_eq!(doc.text(0, 0).unwrap(), "NaN");
    assert_eq!(doc.text(1, 0).unwrap(), "inf");
    assert_eq!(doc.text(2, 0).unwrap(), "-inf");
    doc.edit(4, 0, "2").unwrap();
    assert_eq!(&working(&doc)[..16], &bytes[..16]);
    let plot = build_plot(
        &doc.bytes,
        &doc.layout,
        None,
        0,
        None,
        100,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        plot.samples.iter().map(|s| s.record).collect::<Vec<_>>(),
        [3, 4]
    );
    doc.edit(0, 0, "3").unwrap();
    doc.undo();
    assert_eq!(&working(&doc)[..4], &bytes[..4]);
}

#[test]
fn duplicate_unsorted_x_preserves_sample_identity_and_spike() {
    let doc = document(DEMO_BYTES, Layout::parse(DEMO_YAML).unwrap());
    let full = build_plot(
        &doc.bytes,
        &doc.layout,
        Some(0),
        1,
        None,
        1000,
        &AtomicBool::new(false),
    )
    .unwrap();
    let duplicate: Vec<_> = full
        .samples
        .iter()
        .filter(|s| s.point[0] == 1700000070.)
        .map(|s| s.record)
        .collect();
    assert_eq!(duplicate, [70, 72, 73]);
    assert!(full
        .samples
        .windows(2)
        .any(|s| s[0].point[0] > s[1].point[0]));
    for sample in &full.samples {
        assert_eq!(sample.byte_offset, 16 + sample.record * 12);
    }
    let reduced = build_plot(
        &doc.bytes,
        &doc.layout,
        Some(0),
        1,
        Some([1700000060., 1700000080.]),
        8,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(reduced.samples.len() <= 8);
    let spike = reduced.samples.iter().find(|s| s.record == 73).unwrap();
    assert_eq!(spike.point[1], 85.);
    assert_eq!(spike.byte_offset, 892);
    assert!(reduced
        .samples
        .iter()
        .all(|s| (1700000060. ..=1700000080.).contains(&s.point[0])));
    let min = full
        .samples
        .iter()
        .filter(|s| (1700000060. ..=1700000080.).contains(&s.point[0]))
        .map(|s| s.point[1])
        .fold(f64::INFINITY, f64::min);
    assert!(reduced.samples.iter().any(|s| s.point[1] == min));
}

#[test]
fn invalid_layouts_are_actionable_and_incomplete_records_rejected() {
    let valid = "endianness: little\nrecord_size: 8\nfields:\n  - {name: a, offset: 0, type: uint32}\n  - {name: b, offset: 4, type: uint16}\n";
    let cases = [
        (valid.replace("name: b", "name: a"), "duplicate field name"),
        (valid.replace("offset: 4", "offset: 2"), "overlaps"),
        (valid.replace("offset: 4", "offset: 7"), "exceed"),
        (valid.replace("offset: 4", "offset: -1"), "offset"),
        (valid.replace("uint32", "uint64"), "type"),
        (valid.replace("little", "middle"), "endianness"),
        (
            valid.replace("record_size: 8", "record_size: 0"),
            "record_size",
        ),
        (format!("{valid}typo: true\n"), "unknown field"),
        (
            valid.replace("type: uint16", "type: uint16, expression: x"),
            "unknown field",
        ),
        (
            valid.replace("type: uint16", "type: uint16, scale: 0"),
            "scale",
        ),
        (
            valid.replace("type: uint16", "type: uint16, scale: .nan"),
            "scale",
        ),
        (
            valid.replace("type: uint16", "type: uint16, scale: 1e999"),
            "scale",
        ),
        (
            valid.replace("type: uint16", "type: uint16, scale: 1e-999"),
            "scale",
        ),
        (valid.replace("name: a", "name: !execute a"), "tags"),
        (format!("{valid}record_size: 10\n"), "duplicate"),
        (
            valid.replace("offset: 4", &format!("offset: {}", usize::MAX)),
            "overflows",
        ),
        (format!("data_offset: {}\n{valid}", usize::MAX), "overflows"),
    ];
    for (yaml, expected) in cases {
        let err = format!("{:#}", Layout::parse(&yaml).unwrap_err());
        assert!(err.contains(expected), "wanted {expected:?}, got {err:?}");
    }
    let spec = Layout::parse(valid).unwrap();
    let resolve = |n| spec.resolve(n, |_| 0, &crate::cache::LoadProgress::default());
    assert_eq!(resolve(16).unwrap().sections[0].record.records, 2);
    assert!(format!("{:#}", resolve(17).unwrap_err()).contains("trailing bytes"));
    assert_eq!(resolve(0).unwrap().sections[0].record.records, 0);
    assert!(Layout::parse(DEMO_YAML)
        .unwrap()
        .resolve(3, |_| 0, &crate::cache::LoadProgress::default())
        .is_err());
    assert!(resolve(16).unwrap().sections[0]
        .record
        .record_offset(usize::MAX)
        .is_err());
}

#[test]
fn layout_changes_preserve_edits_and_failure_is_transactional() {
    let mut doc = document(DEMO_BYTES, Layout::parse(DEMO_YAML).unwrap());
    doc.edit(73, 1, "23.45").unwrap();
    let edited = working(&doc);
    assert!(doc
        .apply_layout(layout("little", "uint16", 7, "1"))
        .is_err());
    assert_eq!(working(&doc), edited);
    assert!(doc.can_undo());
    doc.apply_layout(Layout::parse(&DEMO_YAML.replace("temperature", "sensor")).unwrap())
        .unwrap();
    assert_eq!(working(&doc), edited);
    assert!(doc.is_dirty());
    assert!(!doc.can_undo());
}

#[test]
fn schema_resource_limits_and_plot_cancellation_are_explicit() {
    let oversized = format!("{}{}", DEMO_YAML, "#".repeat(MAX_LAYOUT_BYTES));
    assert!(Layout::parse(&oversized)
        .unwrap_err()
        .to_string()
        .contains("64 KiB"));
    let mut many = "endianness: little\nrecord_size: 257\nfields:\n".to_owned();
    for i in 0..257 {
        many.push_str(&format!("  - {{name: f{i}, offset: {i}, type: uint8}}\n"));
    }
    assert!(Layout::parse(&many)
        .unwrap_err()
        .to_string()
        .contains("256-field"));
    let doc = document(DEMO_BYTES, Layout::parse(DEMO_YAML).unwrap());
    let result = build_plot(
        &doc.bytes,
        &doc.layout,
        None,
        1,
        None,
        4096,
        &AtomicBool::new(true),
    );
    assert!(result.err().unwrap().to_string().contains("cancelled"));
}

#[test]
fn existing_csv_loader_still_loads_the_repository_sample() {
    let config = crate::config::AppConfig {
        data_path: Some(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("samples/data/sample.csv"),
        ),
        ..Default::default()
    };
    let cache = tempfile::tempdir().unwrap();
    let data = crate::loaders::csv::load_csv(
        &config,
        cache.path(),
        &crate::cache::LoadProgress::default(),
    )
    .unwrap();
    assert_eq!(data.signals.len(), 3);
    assert_eq!(data.signals[0].name, "signal_a");
    assert_eq!(data.signals[0].points.point(0), [0., 0.]);
    assert!(data.time_max > data.time_min);
}

#[test]
fn binary_snapshot_cache_hits_invalidate_and_keep_old_snapshots_immutable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.bin");
    let root = dir.path().join("cache");
    std::fs::write(&path, DEMO_BYTES).unwrap();
    let cold_progress = crate::cache::LoadProgress::default();
    let old = Original::open_cached(&path, &root, &cold_progress).unwrap();
    assert!(!cold_progress
        .cache_hit
        .load(std::sync::atomic::Ordering::Relaxed));
    let warm_progress = crate::cache::LoadProgress::default();
    let warm = Original::open_cached(&path, &root, &warm_progress).unwrap();
    assert!(warm_progress
        .cache_hit
        .load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(warm.bytes(), DEMO_BYTES);
    let mut changed = DEMO_BYTES.to_vec();
    changed[896..898].copy_from_slice(&2345i16.to_le_bytes());
    std::fs::write(&path, &changed).unwrap();
    let next = crate::cache::LoadProgress::default();
    let new = Original::open_cached(&path, &root, &next).unwrap();
    assert!(!next.cache_hit.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(new.bytes(), changed);
    assert_eq!(old.bytes(), DEMO_BYTES);
    assert_eq!(warm.bytes(), DEMO_BYTES);
}

#[test]
fn binary_plot_cache_invalidates_on_edit_undo_redo_and_axis_change() {
    let mut doc = document(DEMO_BYTES, Layout::parse(DEMO_YAML).unwrap());
    let mut cache = PlotCache::default();
    let cancel = AtomicBool::new(false);
    let query = |cache: &mut PlotCache, doc: &BinaryDocument| {
        cache
            .query(&doc.bytes, &doc.layout, Some(0), 1, None, 4096, &cancel)
            .unwrap()
    };
    let before = query(&mut cache, &doc);
    assert_eq!(query(&mut cache, &doc).samples, before.samples);
    doc.edit(73, 1, "23.45").unwrap();
    let edited = query(&mut cache, &doc);
    assert!(edited
        .samples
        .iter()
        .any(|s| s.record == 73 && s.point[1] == 23.45));
    doc.undo();
    assert_eq!(query(&mut cache, &doc).samples, before.samples);
    doc.redo();
    assert_eq!(query(&mut cache, &doc).samples, edited.samples);
    let pressure = cache
        .query(&doc.bytes, &doc.layout, None, 2, None, 4096, &cancel)
        .unwrap();
    assert_ne!(pressure.samples, edited.samples);
    assert_eq!(query(&mut cache, &doc).samples, edited.samples);
}
