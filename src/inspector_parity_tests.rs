use crate::binary::Original;
use crate::byte_store::ByteStore;
use crate::structure::Program;
use std::sync::atomic::AtomicBool;

#[test]
fn interleaved_lazy_field_inspector_and_value_edit_share_record_boundaries() {
    let yaml = r#"
structures:
  - name: Short
    fields: [{name: value, type: u16}]
  - name: Long
    fields: [{name: value, type: u32}]
  - name: Record
    fields:
      - {name: tag, type: u8}
      - name: payload
        type: {choose: {ref: tag, cases: {'1': {structure: Short}, '2': {structure: Long}}}}
actions:
  - read: {name: records, type: {structure: Record}, repeat: until end}
"#;
    let mut bytes = Vec::new();
    for index in 0..300u32 {
        if index % 2 == 0 {
            bytes.push(1);
            bytes.extend_from_slice(&(index as u16).to_le_bytes());
        } else {
            bytes.push(2);
            bytes.extend_from_slice(&index.to_le_bytes());
        }
    }
    let store = ByteStore::from_snapshot(Original::from_bytes(&bytes).unwrap());
    let source = store.view();
    let output = Program::parse(yaml).unwrap().run(&source, &AtomicBool::new(false)).unwrap();
    let array = output.nodes[0].lazy.as_ref().expect("records should be lazy");
    let sample = array.bounds(221).unwrap();
    let field_range = sample.start + 1..sample.end;
    let node = crate::structure_selection::resolved_field(&output.nodes, &source, &field_range)
        .unwrap().expect("field");
    assert_eq!(node.kind, "u32");
    assert_eq!(node.numeric(), Some(221.0));
    assert_eq!(crate::structure_selection::sample_for_node(&output.nodes, &node),
        Some((221, sample.clone())));
    let info = crate::workspace_inspector::FieldInfo::from_node(
        &node, Some((221, sample.start as u64..sample.end as u64)));
    assert_eq!(info.sample_range(), Some(sample.start as u64..sample.end as u64));
    assert!(info.editable);
    assert_eq!(crate::field_edit::encode_value(&node, "54321").unwrap().0,
        54321u32.to_le_bytes());
}
