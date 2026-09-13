//! Byte text conventions shared by the native and browser egui views.
pub fn ascii(byte: u8) -> char {
    if (32..127).contains(&byte) {
        byte as char
    } else {
        '·'
    }
}
