//! Section-based binary inspection. No UI dependencies; uncompressed field offsets
//! refer to physical source bytes. Legacy fixed-record YAML is normalized here.
mod document;
mod layout;
mod numeric;
mod samples;
mod sections;

pub use document::{write_copy, BinaryDocument, ByteSnapshot, Original};
#[cfg(test)]
pub use layout::MAX_LAYOUT_BYTES;
pub use layout::{RecordLayout, ValueIdentity};
#[cfg(test)]
pub use samples::build_plot;
pub use samples::{PlotCache, PlotData, Sample};
pub use sections::{Layout, ResolvedLayout};

pub const DEMO_YAML: &str = include_str!("../../samples/decoders/yaml/sensors.yaml");
pub const DEMO_BYTES: &[u8] = include_bytes!("../../samples/data/sensors.bin");

#[cfg(test)]
mod section_tests;
#[cfg(test)]
mod tests;
