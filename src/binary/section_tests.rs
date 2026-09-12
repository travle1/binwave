use super::*;
use crate::cache::LoadProgress;
use std::sync::atomic::{AtomicBool, Ordering};

fn resolve(yaml: &str, bytes: &[u8]) -> anyhow::Result<ResolvedLayout> {
    Layout::parse(yaml)?.resolve(bytes.len(), |i| bytes[i], &LoadProgress::default())
}
fn doc(yaml: &str, bytes: &[u8]) -> BinaryDocument {
    BinaryDocument::new(
        Original::from_bytes(bytes).unwrap(),
        Layout::parse(yaml).unwrap(),
    )
    .unwrap()
}
fn error(yaml: &str, bytes: &[u8]) -> String {
    format!("{:#}", resolve(yaml, bytes).unwrap_err())
}
fn working(doc: &BinaryDocument) -> Vec<u8> {
    (0..doc.bytes.original.bytes().len())
        .map(|i| doc.bytes.byte(i))
        .collect()
}

#[test]
fn compact_expanded_padding_offsets_and_no_alignment() {
    let yaml = "endian: little\nsections:\n  - name: packed\n    fields:\n      - tag: u8\n      - wide: u32\n      - padding: 2\n      - temperature:\n          type: i16\n          scale: 0.01\n      - padding: 1\n";
    let bytes = [9, 4, 3, 2, 1, 0xaa, 0xbb, 0x29, 0x09, 0xcc, 0xdd];
    let doc = doc(yaml, &bytes);
    assert_eq!(doc.layout.record_size, 10);
    assert_eq!(
        doc.layout
            .fields
            .iter()
            .map(|f| f.offset)
            .collect::<Vec<_>>(),
        [0, 1, 7]
    );
    assert_eq!(doc.records, 1);
    assert_eq!(doc.text(0, 1).unwrap(), "16909060");
    assert_eq!(doc.text(0, 2).unwrap(), "23.45");
    assert_eq!(doc.structure.trailing, 10..11);
    let identity = doc.layout.identity(0, 2).unwrap();
    assert_eq!(
        identity,
        ValueIdentity {
            section: 0,
            record: 0,
            field: 2,
            byte_offset: 7,
            byte_length: 2
        }
    );
}

#[test]
fn section_types_both_endians_and_exact_scale_lexemes() {
    for endian in ["little", "big"] {
        for (kind, legacy, size, value) in [
            ("u8", "uint8", 1, "255"),
            ("i8", "int8", 1, "-128"),
            ("u16", "uint16", 2, "60000"),
            ("i16", "int16", 2, "-2345"),
            ("u32", "uint32", 4, "4294967295"),
            ("i32", "int32", 4, "-2147483648"),
            ("f32", "float32", 4, "0.1"),
            ("f64", "float64", 8, "-12.125"),
        ] {
            let mut current = doc(
                &format!("endian: {endian}\nsections:\n  - name: s\n    fields: [{{v: {kind}}}]\n"),
                &vec![0; size],
            );
            let mut old = doc(&format!("endianness: {endian}\nrecord_size: {size}\nfields: [{{name: v, offset: 0, type: {legacy}}}]"), &vec![0; size]);
            assert_eq!(
                current.edit(0, 0, value).unwrap(),
                old.edit(0, 0, value).unwrap()
            );
            assert_eq!(working(&current), working(&old));
        }
        for scale in ["0.10000000000000000001", "1e-300", "-0.01", "1e300"] {
            let mut current = doc(&format!("endian: {endian}\nsections:\n  - name: scaled\n    fields: [{{v: {{type: i16, scale: {scale}}}}}]"), &[0, 0]);
            current.edit(0, 0, scale).unwrap();
            assert_eq!(
                working(&current),
                if endian == "little" { [1, 0] } else { [0, 1] }
            );
            let actual = current.text(0, 0).unwrap();
            current.edit(0, 0, &actual).unwrap();
        }
    }
    let mut scaled = doc(
        "endian: big\nsections: [{name: s, fields: [{v: {type: i16, scale: 0.01}}]}]",
        &[0, 0],
    );
    scaled.edit(0, 0, "23.45").unwrap();
    assert_eq!(working(&scaled), [0x09, 0x29]);
    assert!(scaled.edit(0, 0, "23.451").is_err());
    assert!(scaled.edit(0, 0, "327.68").is_err());
}

#[test]
fn once_count_and_eof_preserve_gaps_and_trailing_regions() {
    let yaml = "endian: little\nsections:\n  - {name: header, bytes: 2}\n  - {name: one, at: 5, fields: [{v: u16}]}\n  - {name: many, repeat: 3, fields: [{v: u8}]}\n";
    let layout = resolve(yaml, &[0; 13]).unwrap();
    assert_eq!(
        layout
            .sections
            .iter()
            .map(|s| s.physical.clone())
            .collect::<Vec<_>>(),
        [0..2, 5..7, 7..10]
    );
    assert_eq!(
        layout
            .sections
            .iter()
            .map(|s| s.record.records)
            .collect::<Vec<_>>(),
        [0, 1, 3]
    );
    assert_eq!(layout.gaps, [2..5]);
    assert_eq!(layout.trailing, 10..13);
    let eof = "endian: big\nsections: [{name: all, repeat: eof, fields: [{v: u16}]}]";
    assert_eq!(resolve(eof, &[0; 6]).unwrap().sections[0].record.records, 3);
    assert_eq!(resolve(eof, &[]).unwrap().sections[0].record.records, 0);
    assert!(error(eof, &[0; 7]).contains("splits"));
    assert!(error(yaml, &[0; 9]).contains("many"));
}

#[test]
fn until_absolute_boundary_with_and_without_marker() {
    for marker in ["", ", marker: 'DE AD'"] {
        let yaml = format!("endian: big\nsections:\n  - {{name: samples, repeat: until next, fields: [{{v: u16}}]}}\n  - {{name: footer, at: 6{marker}, fields: [{{sum: u16}}]}}\n");
        let mut bytes = vec![0; 6];
        if !marker.is_empty() {
            // An earlier boundary match must not override the declared absolute offset.
            bytes[..2].copy_from_slice(&[0xde, 0xad]);
            bytes.extend([0xde, 0xad]);
        }
        bytes.extend([0x12, 0x34, 0x99]);
        let layout = resolve(&yaml, &bytes).unwrap();
        assert_eq!(layout.sections[0].record.records, 3);
        assert_eq!(layout.sections[1].physical.start, 6);
        assert_eq!(
            layout.sections[1].record.data_offset,
            if marker.is_empty() { 6 } else { 8 }
        );
        assert_eq!(layout.trailing.len(), 1);
        assert!(error(&yaml.replace("at: 6", "at: 5"), &bytes).contains("splits"));
        assert!(error(&yaml.replace("at: 6", "at: 100"), &bytes).contains("beyond EOF"));
        if !marker.is_empty() {
            bytes[6] = 0;
            assert!(error(&yaml, &bytes).contains("footer"));
        }
    }
}

#[test]
fn markers_are_owned_consumed_once_and_never_searched_on_ordinary_entry() {
    let yaml = "endian: big\nsections:\n  - {name: records, marker: 'DE AD', repeat: 2, fields: [{v: u16}]}\n  - {name: opaque, marker: 'BE EF', bytes: 2}\n";
    let bytes = [0xde, 0xad, 0, 1, 0, 2, 0xbe, 0xef, 0x55, 0x66, 0x77];
    let layout = resolve(yaml, &bytes).unwrap();
    assert_eq!(layout.sections[0].marker, 0..2);
    assert_eq!(layout.sections[0].physical, 0..6);
    assert_eq!(layout.sections[0].record.data_offset, 2);
    assert_eq!(layout.sections[1].marker, 6..8);
    assert_eq!(layout.sections[1].physical, 6..10);
    assert_eq!(layout.trailing, 10..11);
    let mut missing = vec![0];
    missing.extend(bytes);
    assert!(error(yaml, &missing).contains("marker missing at byte 0"));
    let at = yaml.replace("name: records,", "name: records, at: 1,");
    assert_eq!(resolve(&at, &missing).unwrap().gaps, [0..1]);
}

#[test]
fn marker_only_uses_first_record_boundary_ignoring_inside_and_crossing_records() {
    let yaml = "endian: little\nsections:\n  - {name: samples, repeat: until next, fields: [{v: u32}]}\n  - {name: footer, marker: 'DE AD', fields: [{v: u16}]}\n";
    // An internal marker at 1 and one spanning boundaries at 7 are ignored.
    let bytes = [
        0, 0xde, 0xad, 0, 0, 0, 0, 0xde, 0xad, 0, 0, 0, 0xde, 0xad, 1, 0, 0xde, 0xad,
    ];
    let layout = resolve(yaml, &bytes).unwrap();
    assert_eq!(layout.sections[0].record.records, 3);
    assert_eq!(layout.sections[1].marker, 12..14);
    assert_eq!(layout.trailing, 16..18);
    // Ordinary data sharing marker bytes at a record boundary intentionally wins.
    let first = resolve(yaml, &[0xde, 0xad, 1, 0, 0, 0, 0, 0]).unwrap();
    assert_eq!(first.sections[0].record.records, 0);
    assert_eq!(first.trailing, 4..8);
    assert!(error(yaml, &[0; 8]).contains("missing before EOF"));
    assert!(error(yaml, &[0; 7]).contains("partial record"));
    assert!(error(yaml, &[0xde]).contains("partial record"));
}

#[test]
fn marker_may_be_longer_than_a_record_and_is_not_repeated() {
    let yaml = "endian: little\nsections:\n  - {name: samples, repeat: until next, fields: [{v: u8}]}\n  - {name: footer, marker: 'DE AD BE EF', repeat: 2, fields: [{v: u8}]}\n";
    let resolved = resolve(yaml, &[1, 2, 0xde, 0xad, 0xbe, 0xef, 7, 8]).unwrap();
    assert_eq!(resolved.sections[0].record.records, 2);
    assert_eq!(resolved.sections[1].record.records, 2);
    assert_eq!(resolved.sections[1].record.data_offset, 6);
}

#[test]
fn invalid_declarations_are_rejected_with_section_or_field_context() {
    let cases = [
        ("{name: s, bytes: 1, fields: [{v: u8}]}", "exactly one"),
        ("{name: s}", "exactly one"),
        ("{name: s, bytes: 1, repeat: 2}", "repeat"),
        ("{name: s, bytes: -1}", "bytes"),
        ("{name: s, fields: []}", "nonzero"),
        ("{name: s, fields: [{padding: 0}]}", "nonzero"),
        ("{name: s, fields: [{padding: u8}]}", "reserved"),
        ("{name: s, fields: [{v: 4}]}", "field 'v'"),
        ("{name: s, fields: [{v: u64}]}", "unsupported type"),
        ("{name: s, fields: [{v: uint32}]}", "unsupported type"),
        (
            "{name: s, fields: [{v: u8}, {v: u16}]}",
            "duplicate field name",
        ),
        ("{name: s, fields: [{v: u8, other: u8}]}", "exactly one"),
        ("{name: s, fields: [{v: u8, v: u16}]}", "duplicate"),
        (
            "{name: s, fields: [{v: {type: u8, offset: 0}}]}",
            "unknown field",
        ),
        ("{name: s, fields: [{v: {type: u8, scale: 0}}]}", "scale"),
        ("{name: s, fields: [{v: {type: u8, scale: .nan}}]}", "scale"),
        (
            "{name: s, fields: [{v: {type: u8, scale: 1e999}}]}",
            "scale",
        ),
        (
            "{name: s, fields: [{v: {type: u8, scale: 1e-999}}]}",
            "scale",
        ),
        (
            "{name: s, fields: [{v: {type: u8, scale: '1+2'}}]}",
            "scale",
        ),
        (
            "{name: s, fields: [{v: u8}], record_size: 1}",
            "unknown field",
        ),
        ("{name: s, fields: [{v: u8}], repeat: 0}", "repeat"),
        (
            "{name: s, fields: [{v: u8}], repeat: -1}",
            "positive integer",
        ),
        (
            "{name: s, fields: [{v: u8}], repeat: 1.5}",
            "positive integer",
        ),
        ("{name: s, fields: [{v: u8}], repeat: '2'}", "repeat"),
        (
            "{name: s, fields: [{v: u8}], repeat: until next}",
            "following section",
        ),
        ("{name: s, bytes: 1, at: -1}", "at"),
        ("{name: s, bytes: null}", "bytes"),
        ("{name: s, bytes: 1, at: null}", "at"),
        ("{name: s, bytes: 1, marker: ''}", "marker cannot be empty"),
        (
            "{name: s, bytes: 1, marker: 'DEADBEEF'}",
            "malformed marker",
        ),
        ("{name: s, bytes: 1, marker: 'GG'}", "malformed marker"),
        ("{name: s, bytes: 1, marker: 'D E'}", "malformed marker"),
        ("{name: s, bytes: 1, surprise: true}", "unknown field"),
        ("{name: s, name: t, bytes: 1}", "duplicate field"),
        ("{name: s, bytes: 1, bytes: 2}", "duplicate field"),
        ("{name: '', bytes: 1}", "empty"),
        ("{name: s, fields: [{'': u8}]}", "empty"),
    ];
    for (section, wanted) in cases {
        let err = error(&format!("endian: little\nsections: [{section}]"), &[0; 16]);
        assert!(
            err.contains(wanted),
            "{section}: expected {wanted}, got {err}"
        );
        assert!(err.contains("section") || err.contains("field"), "{err}");
    }
}

#[test]
fn schema_mixing_tags_compression_and_invalid_top_levels_fail_explicitly() {
    for yaml in [
        "endian: little\nsections: []\nendianness: little",
        "endian: little\nrecord_size: 1\nfields: []",
        "sections: []\ndata_offset: 0",
    ] {
        assert!(error(yaml, &[]).contains("cannot mix"));
    }
    for yaml in [
        "endian: little\nsections: [{name: s, bytes: 1, compression: zlib}]",
        "endian: little\nsections: [{name: s, bytes: 1, compression: null}]",
        "endian: little\nsections: []\ncompression: gzip",
        "endianness: little\nrecord_size: 1\nfields: []\ncompression: gzip",
    ] {
        assert!(error(yaml, &[]).contains("compressed sections are not supported yet"));
    }
    for yaml in [
        "sections: [{name: s, bytes: 1}]",
        "endian: middle\nsections: [{name: s, bytes: 1}]",
        "endian: little\nsections: []",
        "endian: little\nsections: [{name: s, bytes: 1}, {name: s, bytes: 1}]",
        "endian: little\nsections: [{name: s, bytes: 1}]\ntyop: true",
        "endian: little\nendian: big\nsections: []",
        "!execute {endian: little, sections: [{name: s, bytes: 1}]}",
        "endian: little\nsections: [{name: !execute s, bytes: 1}]",
        "endian: little\nsections: [{name: s, fields: [{v: !execute u8}]}]",
    ] {
        assert!(resolve(yaml, &[0; 10]).is_err(), "accepted {yaml}");
    }
    let err = error("endian: little\nsections:\n - name: s\n   fields:\n    - temperature: {type: i16, offset: 2}\n", &[]);
    assert!(
        err.contains("line 5") && err.contains("temperature"),
        "{err}"
    );
}

#[test]
fn bad_boundaries_overlap_missing_next_and_arithmetic_overflow() {
    for tail in ["{name: b, at: 1, bytes: 1}", "{name: b, at: 99, bytes: 1}"] {
        assert!(resolve(
            &format!("endian: little\nsections: [{{name: a, bytes: 2}}, {tail}]"),
            &[0; 6]
        )
        .is_err());
    }
    assert!(error("endian: little\nsections: [{name: a, repeat: until next, fields: [{v: u8}]}, {name: b, bytes: 1}]", &[0; 6]).contains("must declare at or marker"));
    assert!(error("endian: little\nsections: [{name: a, repeat: eof, fields: [{v: u8}]}, {name: b, bytes: 1}]", &[0; 6]).contains("must be final"));
    for section in [
        format!(
            "{{name: s, fields: [{{padding: {}}}, {{v: u8}}]}}",
            usize::MAX
        ),
        format!("{{name: s, repeat: {}, fields: [{{v: u16}}]}}", usize::MAX),
        format!("{{name: s, at: {}, bytes: 1}}", usize::MAX),
        format!(
            "{{name: s, at: {}, marker: '00', fields: [{{v: u8}}]}}",
            usize::MAX
        ),
    ] {
        assert!(
            error(&format!("endian: little\nsections: [{section}]"), &[]).contains("overflows")
        );
    }
    let progress = LoadProgress::default();
    progress.cancelled.store(true, Ordering::Relaxed);
    assert!(Layout::parse(DEMO_YAML)
        .unwrap()
        .resolve(DEMO_BYTES.len(), |i| DEMO_BYTES[i], &progress)
        .is_err());
}

#[test]
fn all_opaque_or_padding_only_layouts_are_inspectable() {
    let opaque = doc(
        "endian: little\nsections: [{name: header, bytes: 2}]",
        &[1, 2, 3],
    );
    assert!(opaque.layout.fields.is_empty());
    assert_eq!(opaque.structure.trailing, 2..3);
    let padding = doc(
        "endian: little\nsections: [{name: reserved, repeat: eof, fields: [{padding: 2}]}]",
        &[1, 2, 3, 4],
    );
    assert_eq!(padding.records, 2);
    assert!(padding.layout.fields.is_empty());
    let marker_only = resolve("endian: little\nsections: [{name: start, bytes: 0}, {name: marker, marker: 'DE AD', bytes: 0}, {name: data, fields: [{padding: 0}, {v: u8}]}]", &[0xde, 0xad, 7]).unwrap();
    assert_eq!(marker_only.sections[0].physical, 0..0);
    assert_eq!(marker_only.sections[1].physical, 0..2);
    assert_eq!(marker_only.sections[2].record.record_size, 1);
    assert_eq!(marker_only.sections[2].record.fields[0].offset, 0);
}

#[test]
fn demo_export_precise_edits_cross_section_history_and_plot_identity() {
    let mut doc = doc(DEMO_YAML, DEMO_BYTES);
    assert_eq!(doc.structure.sections.len(), 3);
    assert_eq!(doc.section, 1);
    assert_eq!(doc.structure.sections[0].physical, 0..16);
    assert_eq!(doc.structure.sections[1].record.record_size, 12);
    assert_eq!(doc.structure.sections[1].record.records, 200);
    assert_eq!(doc.structure.sections[2].marker, 2416..2420);
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("roundtrip.bin");
    write_copy(&doc.bytes, &path, false).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), DEMO_BYTES);
    let mut cache = PlotCache::default();
    let cancel = AtomicBool::new(false);
    let plot = cache
        .query(&doc.bytes, &doc.layout, Some(0), 1, None, 8, &cancel)
        .unwrap();
    let spike = plot.samples.iter().find(|s| s.record == 73).unwrap();
    let (x, y) = plot.identities(spike, &doc.layout).unwrap();
    assert_eq!(x.unwrap().byte_offset, 892);
    assert_eq!(
        y,
        ValueIdentity {
            section: 1,
            record: 73,
            field: 1,
            byte_offset: 896,
            byte_length: 2
        }
    );
    doc.edit(73, 1, "23.45").unwrap();
    let mut expected = DEMO_BYTES.to_vec();
    expected[896..898].copy_from_slice(&2345i16.to_le_bytes());
    assert_eq!(working(&doc), expected); // Includes marker/padding/header/footer.
    doc.select_section(2).unwrap();
    assert_eq!(doc.text(0, 0).unwrap(), "305419896"); // No checksum repair.
    assert!(plot.identities(spike, &doc.layout).is_err());
    let footer_identity = doc.layout.identity(0, 0).unwrap();
    assert_eq!(footer_identity.byte_offset, 2420);
    doc.edit(0, 0, "42").unwrap();
    expected[2420..2424].copy_from_slice(&42u32.to_le_bytes());
    assert_eq!(working(&doc), expected);
    write_copy(&doc.bytes, &path, true).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), expected);
    assert_eq!(doc.undo(), Some(footer_identity));
    assert_eq!(doc.undo(), Some(y));
    assert_eq!(doc.section, 1);
    assert_eq!(working(&doc), DEMO_BYTES);
    assert_eq!(doc.redo(), Some(y));
    assert_eq!(doc.redo(), Some(footer_identity));
    assert_eq!(doc.section, 2);
    doc.select_section(1).unwrap();
    // Footer patches must not address a non-existent leaf in the sample index.
    let refreshed = cache
        .query(&doc.bytes, &doc.layout, Some(0), 1, None, 4096, &cancel)
        .unwrap();
    assert_eq!(
        refreshed
            .samples
            .iter()
            .find(|s| s.record == 73)
            .unwrap()
            .point[1],
        23.45
    );
}

#[test]
fn gaps_markers_padding_and_tail_survive_edit_export_undo() {
    let yaml = "endian: big\nsections:\n  - {name: header, bytes: 2}\n  - {name: data, at: 5, marker: 'AB CD', fields: [{v: u16}, {padding: 1}]}\n";
    let bytes = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0xab, 0xcd, 0, 1, 0x66, 0x77, 0x88,
    ];
    let mut doc = doc(yaml, &bytes);
    doc.edit(0, 0, "65535").unwrap();
    let mut expected = bytes;
    expected[7..9].copy_from_slice(&[255, 255]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edited.bin");
    write_copy(&doc.bytes, &path, false).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), expected);
    doc.undo();
    assert_eq!(working(&doc), bytes);
}

#[test]
fn failed_resolution_keeps_active_layout_selection_and_history() {
    let mut doc = doc(DEMO_YAML, DEMO_BYTES);
    doc.edit(73, 1, "23.45").unwrap();
    let old_layout = doc.layout.clone();
    let old_structure = doc.structure.clone();
    let old_bytes = working(&doc);
    let invalid = Layout::parse(&DEMO_YAML.replace("DE AD BE EF", "DE AD BE 00")).unwrap();
    assert!(doc.apply_layout(invalid).is_err());
    assert!(std::sync::Arc::ptr_eq(&old_layout, &doc.layout));
    assert!(std::sync::Arc::ptr_eq(&old_structure, &doc.structure));
    assert_eq!(doc.section, 1);
    assert!(doc.can_undo());
    assert_eq!(working(&doc), old_bytes);
}

#[test]
fn fixed_boundaries_do_not_scan_payload_and_marker_scans_can_cancel() {
    let yaml = "endian: little\nsections: [{name: s, repeat: 1000000000, fields: [{v: u8}]}]";
    let layout = Layout::parse(yaml)
        .unwrap()
        .resolve(
            1_000_000_001,
            |_| panic!("fixed boundaries must not read payload bytes"),
            &LoadProgress::default(),
        )
        .unwrap();
    assert_eq!(layout.sections[0].record.records, 1_000_000_000);
    assert_eq!(layout.trailing, 1_000_000_000..1_000_000_001);
    let yaml = "endian: little\nsections: [{name: s, repeat: until next, fields: [{v: u8}]}, {name: end, marker: 'FF', bytes: 1}]";
    let progress = LoadProgress::default();
    let reads = std::cell::Cell::new(0);
    let result = Layout::parse(yaml).unwrap().resolve(
        100_000,
        |_| {
            reads.set(reads.get() + 1);
            if reads.get() == 10 {
                progress.cancelled.store(true, Ordering::Relaxed);
            }
            0
        },
        &progress,
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("cancelled"));
    assert!(reads.get() <= 4097);
}

#[test]
fn legacy_offsets_padding_and_eof_rules_keep_their_meaning() {
    let yaml = "endianness: big\ndata_offset: 3\nrecord_size: 4\nfields: [{name: old, offset: 1, type: uint16, scale: 0.1}]";
    let doc = doc(yaml, &[9, 9, 9, 8, 0, 123, 7]);
    assert!(doc.structure.legacy);
    assert_eq!(doc.records, 1);
    assert_eq!(doc.layout.field_range(0, 0).unwrap(), 4..6);
    assert_eq!(doc.text(0, 0).unwrap(), "12.3");
    assert_eq!(doc.structure.gaps, [0..3]);
    assert!(error(yaml, &[0; 8]).contains("trailing bytes"));
}
