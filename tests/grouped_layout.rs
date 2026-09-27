//! Regression coverage for grouped sensor byte layouts.
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
#[path = "../src/format.rs"]
mod format;
#[path = "../src/file_source.rs"]
mod file_source;
#[path = "../src/structure.rs"]
mod structure;

#[path = "../src/structure_plot.rs"]
mod structure_plot;

use byte_store::ByteStore;
use document::Original;
use structure::{LazyArray, Program};
use std::sync::atomic::AtomicBool;

#[test]
#[ignore = "requires BINWAVE_EXTERNAL_FILE and BINWAVE_EXTERNAL_YAML"]
fn external_definition_file_smoke() {
    let file = std::env::var("BINWAVE_EXTERNAL_FILE").unwrap();
    let yaml = std::fs::read_to_string(std::env::var("BINWAVE_EXTERNAL_YAML").unwrap()).unwrap();
    let source = file_source::FileSource::open(std::path::Path::new(&file)).unwrap();
    let store = ByteStore::from_file(std::sync::Arc::new(source));
    let view = store.view();
    let output = Program::parse(&yaml).unwrap().run(&view, &AtomicBool::new(false)).unwrap();
    assert_eq!(output.consumed, std::fs::metadata(file).unwrap().len() as usize);
}

#[test]
fn defined_fields_use_descriptor_offsets_and_steps_inside_each_block() {
    let yaml = r#"
structures:
  - name: Description
    fields:
      - {name: width, type: u8}
      - {name: offset, type: u8}
      - {name: step, type: u8}
  - name: Metadata
    fields:
      - {name: descriptions, type: {structure: Description}, repeat: 2}
    actions:
      - define:
          name: Discovered
          from: descriptions
          fields:
            type: {ref: width, cases: {'2': u16}}
            repeat: 3
            offset: {ref: offset}
            step: {ref: step}
  - name: Block
    fields:
      - {name: signals, type: {structure: Discovered}}
actions:
  - read: {name: metadata, type: {structure: Metadata}}
  - read: {name: blocks, type: {structure: Block}, length: 20, repeat: 2}
"#;
    let mut bytes = vec![2, 3, 4, 2, 14, 2];
    bytes.extend([0u8; 40]);
    for block in 0..2 {
        let base = 6 + block * 20;
        for sample in 0..3 {
            let a = 100 + block * 10 + sample;
            let b = 200 + block * 10 + sample;
            bytes[base + 3 + sample * 4..base + 5 + sample * 4]
                .copy_from_slice(&(a as u16).to_le_bytes());
            bytes[base + 14 + sample * 2..base + 16 + sample * 2]
                .copy_from_slice(&(b as u16).to_le_bytes());
        }
    }
    let source = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
    let view = source.view();
    let output = Program::parse(yaml).unwrap().run(&view, &AtomicBool::new(false)).unwrap();
    let blocks = &output.nodes[1];
    let first = blocks.children.first().unwrap();
    let fields = &first.children[0].children;
    for (field, base_value, offset, step) in [(0, 100, 3, 4), (1, 200, 14, 2)] {
        let children = &fields[field].children;
        assert_eq!(children.len(), 3);
        for sample in 0..3 {
            assert_eq!(children[sample].range, 6 + offset + sample * step..8 + offset + sample * step);
            assert_eq!(children[sample].numeric(), Some((base_value + sample) as f64));
        }
    }
}

#[test]
fn repeated_fields_project_three_interleaved_lanes_across_blocks() {
    let yaml = r#"
structures:
  - name: Block
    fields:
      - {name: lane_a, type: u16, offset: 0, repeat: 130, step: 6}
      - {name: lane_b, type: u16, offset: 2, repeat: 130, step: 6}
      - {name: lane_c, type: u16, offset: 4, repeat: 130, step: 6}
actions:
  - read: {name: blocks, type: {structure: Block}, length: 780, repeat: 130}
"#;
    let mut bytes = vec![0; 780 * 130];
    for block in 0..130 {
        for sample in 0..130 {
            for lane in 0..3 {
                let at = block * 780 + sample * 6 + lane * 2;
                let value = 1000 + block * 100 + sample + lane * 10;
                bytes[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes());
            }
        }
    }
    let source = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
    let view = source.view();
    let cancel = AtomicBool::new(false);
    let output = Program::parse(yaml).unwrap().run(&view, &cancel).unwrap();
    let blocks = &output.nodes[0];
    assert!(blocks.lazy.is_some());
    let first = blocks.lazy.as_ref().unwrap().page(&view, &cancel, 0, 1).unwrap();
    let lane = &first[0].children[1];
    let array = lane.lazy.as_ref().unwrap();
    assert_eq!(array.bounds(129), Some(776..778));
    assert_eq!(array.page(&view, &cancel, 129, 1).unwrap()[0].numeric(), Some(1139.0));
    let flattened = LazyArray::flattened_child(blocks, lane).unwrap();
    let flat = flattened.lazy.as_ref().unwrap();
    assert_eq!(flat.count, 16900);
    assert_eq!(flat.bounds(130), Some(782..784));
    assert_eq!(flat.page(&view, &cancel, 130, 1).unwrap()[0].numeric(), Some(1110.0));
    assert!(flat.numeric_plan().is_some());
}

#[test]
fn metadata_can_define_two_dimensional_interleaved_channels() {
    let yaml = r#"
structures:
  - name: Description
    fields:
      - {name: offset, type: u8}
  - name: Metadata
    fields:
      - {name: descriptions, type: {structure: Description}, repeat: 3}
    actions:
      - define:
          name: Discovered
          from: descriptions
          fields:
            type: {ref: offset, cases: {'0': u16, '2': u16, '4': u16}}
            offset: {ref: offset}
            repeat: 130
            step: 12
            items_per_group: 2
            item_step: 6
  - name: Block
    fields:
      - {name: signals, type: {structure: Discovered}}
actions:
  - read: {name: metadata, type: {structure: Metadata}}
  - read: {name: blocks, type: {structure: Block}, length: 1560, repeat: 130}
"#;
    let mut bytes = vec![0, 2, 4];
    bytes.resize(3 + 1560 * 130, 0);
    for block in 0..130 {
        for sample in 0..130 {
            for sensor in 0..2 {
                for lane in 0..3 {
                    let at = 3 + block * 1560 + sample * 12 + sensor * 6 + lane * 2;
                    let value = 1000 + block * 100 + sample + sensor * 10 + lane * 20;
                    bytes[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes());
                }
            }
        }
    }
    let source = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
    let view = source.view();
    let cancel = AtomicBool::new(false);
    let output = Program::parse(yaml).unwrap().run(&view, &cancel).unwrap();
    let blocks = &output.nodes[1];
    let first = blocks.lazy.as_ref().unwrap().page(&view, &cancel, 0, 1).unwrap();
    let lane = &first[0].children[0].children[1];
    let sample = lane.lazy.as_ref().unwrap().page(&view, &cancel, 0, 1).unwrap();
    let sensors = &sample[0].children[0];
    assert_eq!(sensors.children[1].numeric(), Some(1030.0));
    let flattened = LazyArray::flattened_group_sensor(blocks, lane, sensors, 1).unwrap();
    let array = flattened.lazy.as_ref().unwrap();
    assert_eq!(array.count, 130 * 130);
    assert_eq!(array.page(&view, &cancel, 130, 1).unwrap()[0].numeric(), Some(1130.0));
    assert!(array.numeric_plan().is_some());
}

#[test]
fn both_sensor_layouts_preserve_samples_across_blocks() {
    for layout in ["interleaved", "contiguous", "default"] {
        let layout_field = if layout == "default" { String::new() } else { format!("layout: {layout}") };
        let (groups, items) = if layout == "interleaved" { (4096, 3) } else { (3, 4096) };
        let yaml = format!(r#"
structures:
  - name: Descriptor
    fields:
      - {{name: width, type: u8}}
  - name: Block
    fields:
      - {{name: header, type: u16}}
      - {{name: payload, type: {{structure: Generated}}}}
actions:
  - read: {{name: descriptors, type: {{structure: Descriptor}}, repeat: 1}}
  - define:
      name: Generated
      from: descriptors
      fields:
        type: {{ref: width, cases: {{'4': u32}}}}
        repeat: {groups}
        items_per_group: {items}
        {layout_field}
  - read: {{name: blocks, type: {{structure: Block}}, repeat: 129}}
"#);
        let value = |block: u32, sensor: u32, sample: u32| block * 100_000 + sensor * 10_000 + sample;
        let mut bytes = vec![4];
        for block in 0..129 {
            bytes.extend(0xABCD_u16.to_le_bytes());
            for group in 0..groups {
                for item in 0..items {
                    let (sensor, sample) = if layout == "interleaved" { (item, group) } else { (group, item) };
                    bytes.extend(value(block, sensor, sample).to_le_bytes());
                }
            }
        }
        let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
        let view = store.view();
        let cancel = AtomicBool::new(false);
        let program = Program::parse(&yaml).unwrap();
        // The visual editor's YAML round trip must retain the layout.
        let encoded = serde_yaml_ng::to_string(&program).unwrap();
        let output = Program::parse(&encoded).unwrap().run(&view, &cancel).unwrap();
        let blocks = &output.nodes[1];
        let first = blocks.lazy.as_ref().unwrap().page(&view, &cancel, 0, 1).unwrap();
        let channel = &first[0].children[1].children[0];
        for sensor in 0..3 {
            let series = if layout != "interleaved" {
                LazyArray::flattened_child(blocks, &channel.children[sensor]).unwrap()
            } else {
                let group = channel.lazy.as_ref().unwrap().page(&view, &cancel, 0, 1).unwrap();
                LazyArray::flattened_group_sensor(blocks, channel, &group[0].children[0], sensor).unwrap()
            };
            let array = series.lazy.as_ref().unwrap();
            assert_eq!(array.count, 129 * 4096);
            for index in [0, 4095, 4096, 129 * 4096 - 1] {
                let sample = array.page(&view, &cancel, index, 1).unwrap();
                assert_eq!(sample[0].numeric(), Some(value((index / 4096) as u32, sensor as u32, (index % 4096) as u32) as f64), "{layout} sensor {sensor} sample {index}");
            }
        }
    }
}

#[test]
fn contiguous_series_labels_preserve_the_defined_channel_names() {
    let yaml = r#"
structures:
  - name: Descriptor
    fields:
      - {name: label, type: string, length: 4}
  - name: Block
    fields:
      - {name: payload, type: {structure: Channels}}
actions:
  - read: {name: descriptors, type: {structure: Descriptor}, repeat: 2}
  - define:
      name: Channels
      from: descriptors
      fields:
        name: {ref: label}
        type: {ref: label, otherwise: u16}
        repeat: 1
        items_per_group: 4096
  - read: {name: blocks, type: {structure: Block}, repeat: 129}
"#;
    let mut bytes = b"TempLoad".to_vec();
    bytes.resize(8 + 129 * 2 * 4096 * 2, 0);
    let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
    let view = store.view();
    let cancel = AtomicBool::new(false);
    let output = Program::parse(yaml).unwrap().run(&view, &cancel).unwrap();
    let blocks = &output.nodes[1];
    let first = blocks.lazy.as_ref().unwrap().page(&view, &cancel, 0, 1).unwrap();
    let channels = &first[0].children[0].children;
    let temp = LazyArray::flattened_child(blocks, &channels[0].children[0]).unwrap();
    let load = LazyArray::flattened_child(blocks, &channels[1].children[0]).unwrap();
    assert_eq!(temp.name, "payload.Temp.sensor_0 across blocks");
    assert_eq!(load.name, "payload.Load.sensor_0 across blocks");
    assert_eq!(temp.path, "blocks.payload.Temp.sensor_0");
    assert_eq!(load.path, "blocks.payload.Load.sensor_0");
    assert_eq!(temp.lazy.as_ref().unwrap().count, 129 * 4096);
    assert_eq!(load.lazy.as_ref().unwrap().count, 129 * 4096);
}

#[test]
fn interleaved_sensors_stay_inside_separate_channel_regions_in_live_plot() {
    let yaml = r#"
structures:
  - name: Descriptor
    fields:
      - {name: label, type: string, length: 4}
      - {name: width, type: u8}
  - name: Block
    fields:
      - {name: header, type: u32}
      - {name: samples, type: {structure: Channels}}
actions:
  - read: {name: descriptors, type: {structure: Descriptor}, repeat: 2}
  - define:
      name: Channels
      from: descriptors
      fields:
        name: {ref: label}
        type: {ref: width, cases: {'2': u16, '4': u32}}
        length: {ref: width}
        repeat: 4096
        items_per_group: 3
        layout: interleaved
  - read: {name: blocks, type: {structure: Block}, repeat: 129}
"#;
    let expected = |channel: usize, block: usize, sensor: usize, sample: usize| {
        (if channel == 0 { 1000 } else { 100_000 }) + block * 100 + sensor * 10 + sample % 10
    };
    let mut bytes = b"LowA\x02High\x04".to_vec();
    for block in 0..129 {
        bytes.extend(0xdeadbeef_u32.to_le_bytes());
        for channel in 0..2 {
            for sample in 0..4096 {
                for sensor in 0..3 {
                    let value = expected(channel, block, sensor, sample) as u32;
                    if channel == 0 { bytes.extend((value as u16).to_le_bytes()); }
                    else { bytes.extend(value.to_le_bytes()); }
                }
            }
        }
    }
    let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
    let view = store.view();
    let cancel = AtomicBool::new(false);
    let output = Program::parse(yaml).unwrap().run(&view, &cancel).unwrap();
    let blocks = &output.nodes[1];
    let first = blocks.lazy.as_ref().unwrap().page(&view, &cancel, 0, 1).unwrap();
    let ctx = egui::Context::default();
    // Reuse the live reader when switching channels to exercise RAM cache identity.
    let mut reader = structure_plot::PlotReader::default();
    for channel in [0, 1, 0] {
        let group = &first[0].children[1].children[channel];
        let item = group.lazy.as_ref().unwrap().page(&view, &cancel, 0, 1).unwrap();
        let sensors: Vec<_> = (0..3).map(|sensor|
            LazyArray::flattened_group_sensor(blocks, group, &item[0].children[0], sensor).unwrap()).collect();
        let plans: Vec<_> = sensors.iter().map(|node| node.lazy.as_ref().unwrap().numeric_plan().unwrap()).collect();
        let combined = structure::NumericPlan::combine(&plans).unwrap();
        for start in [0, 4094, 8190] {
            reader.request_group(&ctx, view.clone(), sensors[0].clone(), combined.clone(), start..start + 4, 800, false);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                reader.tick(&ctx);
                if reader.data().is_some_and(|data| data.complete && data.range == (start..start + 4)) { break; }
                assert!(std::time::Instant::now() < deadline, "plot timeout: {:?}", reader.error());
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            for sensor in 0..3 {
                let array = sensors[sensor].lazy.as_ref().unwrap();
                let decoded = array.page(&view, &cancel, start, 4).unwrap();
                let name = format!("{} / value", sensors[sensor].path);
                let plotted = &reader.data().unwrap().series[&name];
                assert_eq!(plotted.len(), 4);
                for offset in 0..4 {
                    let index = start + offset;
                    let value = expected(channel, index / 4096, sensor, index % 4096) as f64;
                    assert_eq!(decoded[offset].numeric(), Some(value));
                    assert_eq!(plotted[offset].value, value);
                    assert_eq!(plotted[offset].bytes, array.bounds(index).unwrap());
                }
            }
        }
    }
}
