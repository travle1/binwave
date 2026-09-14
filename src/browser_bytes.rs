//! Synchronous bounded reads are available only inside the decoding Web Worker.
use anyhow::{ensure, Result};
use wasm_bindgen::prelude::*;
pub const READ_LIMIT: u64 = 1024 * 1024;
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = binwaveDecoder, catch, js_name = read)]
    fn read_bytes(offset: &str, length: u32) -> Result<js_sys::Uint8Array, JsValue>;
}
#[derive(Clone)]
pub struct ByteView {
    pub revision: u64,
    pub length: u64,
}
impl ByteView {
    pub fn len(&self) -> u64 {
        self.length
    }
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
    pub fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        ensure!(length <= READ_LIMIT, "read exceeds 1 MiB limit");
        ensure!(offset <= self.length, "offset outside file");
        let length = length.min(self.length - offset);
        let bytes = read_bytes(&offset.to_string(), length as u32)
            .map_err(|e| anyhow::anyhow!("Browser file read failed: {e:?}"))?;
        ensure!(
            u64::from(bytes.length()) == length,
            "source ended during read"
        );
        Ok(bytes.to_vec())
    }
}
