//! Byte-to-sample selection mapping shared by native and browser views.
use crate::structure::{Address, Node};
use std::ops::Range;

/// Map only a selection contained by this collection. Endpoints are exclusive.
pub(crate) fn sample_indices_for_bytes(node: &Node, bytes: &Range<Address>) -> Option<Range<Address>> {
    if bytes.is_empty() || bytes.start < node.range.start || bytes.end > node.range.end {
        return None;
    }
    if let Some(array) = &node.lazy {
        if array.stride == 0 || array.count == 0 {
            return None;
        }
        let first = (bytes.start - node.range.start) / array.stride;
        let end = (bytes.end - node.range.start)
            .div_ceil(array.stride)
            .min(array.count);
        return (first < end).then_some(first..end);
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
        let start = node
            .range
            .start
            .checked_add(samples.start.checked_mul(array.stride)?)?;
        let end = node
            .range
            .start
            .checked_add(samples.end.checked_mul(array.stride)?)?;
        return (start < end && end <= node.range.end).then_some(start..end);
    }
    Some(
        node.children.get(usize::try_from(samples.start).ok()?)?.range.start
            ..node.children.get(usize::try_from(samples.end - 1).ok()?)?.range.end,
    )
}
