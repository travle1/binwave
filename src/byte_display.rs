//! Byte text conventions shared by the native and browser egui views.
pub fn ascii(byte: u8) -> char {
    if (32..127).contains(&byte) {
        byte as char
    } else {
        '·'
    }
}

/// Hex byte pairs such as `00 A4 FF` or `00A4FF`.
pub fn parse_hex(input: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err("Enter complete hexadecimal byte pairs, for example 00 A4 FF.".into());
    }
    if !cleaned.is_ascii() {
        return Err("Use hexadecimal digits 0–9 and A–F.".into());
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&cleaned[i..i + 2], 16)
                .map_err(|_| "Use hexadecimal digits 0–9 and A–F.".to_owned())
        })
        .collect()
}

/// Hex as people paste it: `74 72`, `7472`, `0x74, 0x72`.
pub fn loose_hex(input: &str) -> Result<Vec<u8>, String> {
    parse_hex(&input.replace("0x", " ").replace("0X", " ").replace([',', ';'], " "))
}

/// Bytes typed for an insert or fill: text as UTF-8, otherwise hex.
pub fn typed_bytes(input: &str, text: bool, limit: usize) -> Result<Vec<u8>, String> {
    let bytes = if text { input.as_bytes().to_vec() } else { loose_hex(input)? };
    if bytes.is_empty() {
        return Err("Enter bytes to insert.".into());
    }
    if bytes.len() > limit {
        return Err(format!("One insert is limited to {}.", crate::format::size(limit as u64)));
    }
    Ok(bytes)
}
