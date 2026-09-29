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

/// When something happened, for lists of recent things: `Just now`, `12 min ago`,
/// `3 h ago`, `Yesterday`, `4 days ago`, then the date (`12 Sep 2026`, UTC).
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop only.
pub fn ago(then_unix_secs: u64, now_unix_secs: u64) -> String {
    if then_unix_secs == 0 {
        return "Earlier".into();
    }
    let elapsed = now_unix_secs.saturating_sub(then_unix_secs);
    let days = now_unix_secs / 86_400 - (then_unix_secs / 86_400).min(now_unix_secs / 86_400);
    match (elapsed, days) {
        (0..60, _) => "Just now".into(),
        (60..3600, _) => format!("{} min ago", elapsed / 60),
        (_, 0) => format!("{} h ago", elapsed / 3600),
        (_, 1) => "Yesterday".into(),
        (_, 2..=6) => format!("{days} days ago"),
        _ => {
            // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
            let z = (then_unix_secs / 86_400) as i64 + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let day = doy - (153 * mp + 2) / 5 + 1;
            let month = if mp < 10 { mp + 3 } else { mp - 9 };
            let year = yoe + era * 400 + i64::from(month <= 2);
            const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            format!("{day} {} {year}", MONTHS[(month - 1) as usize])
        }
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

    #[test]
    fn ago_is_relative_for_a_week_then_a_date() {
        let now = 1_792_000_000; // 2026-10-14, 17:46 UTC
        assert_eq!(super::ago(now - 30, now), "Just now");
        assert_eq!(super::ago(now - 12 * 60, now), "12 min ago");
        assert_eq!(super::ago(now - 3 * 3600, now), "3 h ago");
        assert_eq!(super::ago(now - 20 * 3600, now), "Yesterday");
        assert_eq!(super::ago(now - 4 * 86_400, now), "4 days ago");
        assert_eq!(super::ago(1_789_000_000, now), "10 Sep 2026");
        assert_eq!(super::ago(0, now), "Earlier");
    }
}

