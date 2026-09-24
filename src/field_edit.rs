//! Shared decoded numeric editing rules for native and browser workspaces.
use crate::structure::{Endian, Node};
use anyhow::{Context as _, Result};

pub fn primitive_size(kind: &str) -> Option<usize> {
    Some(match kind {
        "u8" | "i8" => 1,
        "u16" | "i16" => 2,
        "u32" | "i32" | "f32" => 4,
        "u64" | "i64" | "f64" => 8,
        _ => return None,
    })
}

/// Only a complete, fixed-width numeric field can be edited as one value.
pub fn editable_value(node: &Node) -> bool {
    let Some(width) = primitive_size(&node.kind) else {
        return false;
    };
    node.value.as_ref().is_some_and(|value| value.numeric().is_some())
        && node.range.end - node.range.start == width as crate::structure::Address
}

/// Integer parsing never passes through f64, including the full u64/i64 ranges.
/// Floats use the target primitive's normal rounding and reject non-finite values.
pub fn encode_value(node: &Node, text: &str) -> Result<(Vec<u8>, String)> {
    anyhow::ensure!(
        editable_value(node),
        "Edit this field's bytes in Hex; it is not a fixed-width numeric value."
    );
    let text = text.trim();
    anyhow::ensure!(
        !text.is_empty() && text.len() <= 1024,
        "Enter a numeric value of at most 1024 characters."
    );
    macro_rules! integer {
        ($kind:ty) => {{
            let value = text.parse::<$kind>().with_context(|| {
                format!("Enter a whole number from {} to {}.", <$kind>::MIN, <$kind>::MAX)
            })?;
            let bytes = match node.endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            };
            (bytes.to_vec(), value.to_string())
        }};
    }
    macro_rules! float {
        ($kind:ty) => {{
            let value = text.parse::<$kind>().context("Enter a decimal number.")?;
            anyhow::ensure!(
                value.is_finite(),
                "Enter a finite value within the {} range.",
                node.kind
            );
            let bytes = match node.endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            };
            (bytes.to_vec(), value.to_string())
        }};
    }
    Ok(match node.kind.as_str() {
        "u8" => integer!(u8),
        "i8" => integer!(i8),
        "u16" => integer!(u16),
        "i16" => integer!(i16),
        "u32" => integer!(u32),
        "i32" => integer!(i32),
        "u64" => integer!(u64),
        "i64" => integer!(i64),
        "f32" => float!(f32),
        "f64" => float!(f64),
        _ => unreachable!("editable_value checked the primitive"),
    })
}
