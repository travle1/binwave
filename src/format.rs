//! Formatting for numbers shown to people: counts, sizes, offsets and ranges.
//! Values used by decoders, file names or machine-readable output keep their raw form.

/// Unsigned sizes, counts and offsets. `usize` and `u64` differ between the native
/// and browser builds, so the helpers accept either without casts at call sites.
pub trait Unsigned: Copy {
    fn get(self) -> u64;
}
impl Unsigned for u64 {
    fn get(self) -> u64 { self }
}
impl Unsigned for usize {
    fn get(self) -> u64 { self as u64 }
}
impl Unsigned for u32 {
    fn get(self) -> u64 { self.into() }
}

/// Decimal number with thousands separators: `31,249,935`.
pub fn count(value: impl std::fmt::Display) -> String {
    let text = value.to_string();
    let (sign, rest) = text.strip_prefix('-').map_or(("", text.as_str()), |rest| ("-", rest));
    let (digits, fraction) = rest.split_once('.').map_or((rest, None), |(whole, part)| (whole, Some(part)));
    let mut grouped = String::from(sign);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 { grouped.push(','); }
        grouped.push(digit);
    }
    if let Some(fraction) = fraction {
        grouped.push('.');
        grouped.push_str(fraction);
    }
    grouped
}

/// A count and its noun with the right plural: `1 byte`, `4,096 items`.
pub fn plural(value: impl Unsigned, singular: &str, plural: &str) -> String {
    let value = value.get();
    format!("{} {}", count(value), if value == 1 { singular } else { plural })
}

/// Human-readable size: `512 B`, `1.69 KiB`, `61.99 GiB`. Pair with [`exact_bytes`] on hover.
pub fn size(bytes: impl Unsigned) -> String {
    let bytes = bytes.get();
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    format!("{} {}", count(format!("{value:.2}")), UNITS[unit])
}

/// Exact size for tooltips: `1 byte`, `1,765,376 bytes`.
pub fn exact_bytes(bytes: impl Unsigned) -> String {
    plural(bytes, "byte", "bytes")
}

/// Standalone hex offset, 8 digits like the Hex view: `0x00000030`.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Only native tables show offsets.
pub fn offset(value: impl Unsigned) -> String {
    format!("0x{:08X}", value.get())
}

/// Inclusive byte range with its size, from an end-exclusive range: `0x60–0x6F · 16 B`.
pub fn byte_range(start: impl Unsigned, end: impl Unsigned) -> String {
    let (start, end) = (start.get(), end.get());
    match end.saturating_sub(start) {
        0 => format!("0x{start:X} · 0 B"),
        1 => format!("0x{start:X} · 1 B"),
        len => format!("0x{start:X}–0x{:X} · {}", end - 1, size(len)),
    }
}

/// Float with up to seven significant digits, switching to exponent form outside
/// `0.0001..10,000,000`.
pub fn compact_float(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0. {
        return "0".into();
    }
    let magnitude = value.abs();
    if !(0.0001..10_000_000.).contains(&magnitude) {
        return format!("{value:.6e}");
    }
    let decimals = (6. - magnitude.log10().floor()).max(0.) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

/// Float for a narrow cell: `29.5`, `2.69e20`. Pair with the full value on hover.
pub fn short_float(value: f64) -> String {
    let magnitude = value.abs();
    if !value.is_finite() || value == 0. || (0.0001..10_000_000.).contains(&magnitude) {
        return compact_float(value);
    }
    let text = format!("{value:.2e}");
    match text.split_once('e') {
        Some((mantissa, exponent)) if mantissa.contains('.') =>
            format!("{}e{exponent}", mantissa.trim_end_matches('0').trim_end_matches('.')),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_group_thousands_and_keep_signs_and_fractions() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(31_249_935), "31,249,935");
        assert_eq!(count(-1_234_567), "-1,234,567");
        assert_eq!(count("63476.20"), "63,476.20");
    }

    #[test]
    fn plurals_match_the_count() {
        assert_eq!(plural(1u64, "byte", "bytes"), "1 byte");
        assert_eq!(plural(0u64, "item", "items"), "0 items");
        assert_eq!(plural(4096u64, "item", "items"), "4,096 items");
    }

    #[test]
    fn sizes_are_readable_with_exact_bytes_available() {
        assert_eq!(size(0u64), "0 B");
        assert_eq!(size(1023u64), "1023 B");
        assert_eq!(size(1730u64), "1.69 KiB");
        assert_eq!(size(66_559_653_489u64), "61.99 GiB");
        assert_eq!(size(4 * 1024u64.pow(4)), "4,096.00 GiB");
        assert_eq!(exact_bytes(1u64), "1 byte");
        assert_eq!(exact_bytes(1_765_376u64), "1,765,376 bytes");
    }

    #[test]
    fn offsets_and_ranges_follow_the_hex_view() {
        assert_eq!(offset(0x30u64), "0x00000030");
        assert_eq!(offset(0x1_0000_0000u64), "0x100000000");
        assert_eq!(byte_range(0x60u64, 0x70u64), "0x60–0x6F · 16 B");
        assert_eq!(byte_range(0x6Bu64, 0x6Cu64), "0x6B · 1 B");
        assert_eq!(byte_range(0x6Bu64, 0x6Bu64), "0x6B · 0 B");
    }
}
