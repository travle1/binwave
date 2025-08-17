use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub data_path: Option<PathBuf>,
    pub format: FileKind,
    pub csv: CsvConfig,
    pub binary: Option<BinaryConfig>,
    pub view: ViewConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Csv,
    BinaryFixed,
}

impl Default for FileKind {
    fn default() -> Self { FileKind::Csv }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CsvConfig {
    pub delimiter: char,
    pub has_header: bool,
    pub time_column: String,
    pub include_signals: Option<Vec<String>>, // if None, include all non-time columns
}

impl Default for CsvConfig {
    fn default() -> Self {
        Self {
            delimiter: ',',
            has_header: true,
            time_column: "time".into(),
            include_signals: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewConfig {
    pub max_lines_in_plot: usize,
    pub target_points_per_line: usize,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            max_lines_in_plot: 2000,
            target_points_per_line: 2000,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_path: None,
            format: FileKind::Csv,
            csv: CsvConfig::default(),
            binary: None,
            view: ViewConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        let s = fs::read_to_string(p)
            .with_context(|| format!("Failed to read config file: {}", p.display()))?;
        let cfg: AppConfig = serde_json::from_str(&s)
            .with_context(|| format!("Failed to parse JSON config: {}", p.display()))?;
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BinaryConfig {
    pub endianness: Endianness,
    pub record_size: Option<usize>,
    pub fields: Vec<BinField>,
    pub time_field: String,
    pub include_signals: Option<Vec<String>>, // if None, include all non-time fields
    pub index_stride: Option<usize>, // how many records between index points (for seeking)
    pub streaming: Option<bool>,     // if true, use on-demand windowed loading
}

impl Default for BinaryConfig { fn default() -> Self { Self { endianness: Endianness::Little, record_size: None, fields: vec![], time_field: "time".into(), include_signals: None, index_stride: Some(10_000), streaming: Some(false) } } }

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endianness { Little, Big }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BinType {
    F32, F64,
    I8, U8,
    I16, U16,
    I32, U32,
    I64, U64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinField {
    pub name: String,
    pub ty: BinType,
    #[serde(default)]
    pub offset: Option<usize>,
}
