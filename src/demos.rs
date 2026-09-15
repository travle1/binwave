//! Small public fixtures shared by native and web. No local test files are embedded.
#[derive(Clone, Copy, Debug)]
pub enum Demo { Simple, Advanced }
impl Demo {
    pub fn bytes(self) -> &'static [u8] { match self {
        Self::Simple => include_bytes!("../samples/quickstart/simple_wave.bin"),
        Self::Advanced => include_bytes!("../samples/quickstart/advanced_wave.bin"),
    }}
    pub fn yaml(self) -> &'static str { match self {
        Self::Simple => include_str!("../samples/quickstart/simple_wave.yaml"),
        Self::Advanced => include_str!("../samples/advanced/decoder.yaml"),
    }}
    pub fn name(self) -> &'static str { match self {
        Self::Simple => "simple_wave.bin", Self::Advanced => "advanced_wave.bin",
    }}
}
