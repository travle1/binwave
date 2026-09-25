//! Byte-to-sample selection mapping shared by native and browser views.
use crate::structure::{Address, Node};
use std::ops::Range;

/// Map only a selection contained by this collection. Endpoints are exclusive.
pub(crate) fn sample_indices_for_bytes(node: &Node, bytes: &Range<Address>) -> Option<Range<Address>> {
    if bytes.is_empty() || bytes.start < node.range.start || bytes.end > node.range.end {
        return None;
    }
    if let Some(array) = &node.lazy {
        return array.indices_for_bytes(bytes);
    }
    let first = node
        .children
        .iter()
        .position(|row| row.range.start < bytes.end && bytes.start < row.range.end)?;
    let last = node
        .children
        .iter()
        .rposition(|row| row.range.start < bytes.end && bytes.start < row.range.end)?;
    Some(first as Address..last as Address + 1)
}


pub(crate) fn sample_bytes(node: &Node, samples: Range<Address>) -> Option<Range<Address>> {
    if samples.is_empty() || samples.end > node.lazy.as_ref().map_or(node.children.len() as Address, |a| a.count) {
        return None;
    }
    if let Some(array) = &node.lazy {
        let start = array.bounds(samples.start)?.start;
        let end = array.bounds(samples.end - 1)?.end;
        return (start < end && end <= node.range.end).then_some(start..end);
    }
    Some(
        node.children.get(usize::try_from(samples.start).ok()?)?.range.start
            ..node.children.get(usize::try_from(samples.end - 1).ok()?)?.range.end,
    )
}

/// Find the most specific decoded field containing a selected byte range.
pub(crate) fn covering<'a>(nodes: &'a [Node], range: &Range<Address>) -> Option<&'a Node> {
    nodes
        .iter()
        .filter(|node| {
            node.range.start <= range.start && node.range.end >= range.end && !node.range.is_empty()
        })
        .filter_map(|node| covering(&node.children, range).or(Some(node)))
        .min_by_key(|node| node.range.end - node.range.start)
}

pub(crate) fn sample_for_node(nodes: &[Node], field: &Node) -> Option<(Address, Range<Address>)> {
    let bracket = field.path.rfind('[')?;
    let suffix = &field.path[bracket + 1..];
    let end = suffix.find(']')?;
    let index = suffix[..end].parse::<Address>().ok()?;
    let collection_path = &field.path[..bracket];
    fn find<'a>(nodes: &'a [Node], path: &str) -> Option<&'a Node> {
        nodes.iter().find_map(|node| {
            if node.path == path { Some(node) } else { find(&node.children, path) }
        })
    }
    let collection = find(nodes, collection_path)?;
    let range = sample_bytes(collection, index..index.checked_add(1)?)?;
    Some((index, range))
}

/// Resolve one selected field, paging only its containing lazy records.
#[cfg(any(test, target_arch = "wasm32"))]
pub(crate) fn resolved_field(
    nodes: &[Node],
    source: &crate::byte_store::ByteView,
    range: &Range<Address>,
) -> anyhow::Result<Option<Node>> {
    let mut field = covering(nodes, range).cloned();
    for _ in 0..64 {
        let Some(current) = field.as_ref() else { break; };
        let Some(array) = current.lazy.as_ref() else { break; };
        let Some(indices) = sample_indices_for_bytes(current, range) else { break; };
        if indices.end - indices.start != 1 { break; }
        let rows = array.page(source, &std::sync::atomic::AtomicBool::new(false), indices.start, 1)?;
        let Some(next) = covering(&rows, range) else { break; };
        field = Some(next.clone());
    }
    Ok(field)
}
