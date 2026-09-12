pub mod csv;
pub mod csv_index;

use crate::cache::LoadProgress;
use crate::config::AppConfig;
use crate::data::DataStore;
use anyhow::Result;
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub enum FileKind {
    Csv,
    Markdown,
    Yaml,
    Binary,
}

pub fn file_kind(path: &Path) -> FileKind {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "csv" => FileKind::Csv,
        "md" | "markdown" => FileKind::Markdown,
        "yaml" | "yml" => FileKind::Yaml,
        _ => FileKind::Binary,
    }
}

pub fn load_data(
    cfg: &AppConfig,
    progress: &LoadProgress,
    update: impl FnMut(DataStore),
) -> Result<DataStore> {
    csv::load_csv_progressive(cfg, &crate::cache::root(), progress, update)
}
