#[cfg(test)]
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

pub const CSV_INITIAL_ROWS: usize = 8192;

/// Demand a CSV prefix without creating a complete decoded copy. A request may
/// stop before `target_rows` at its byte budget; inspect `csv_extent` before
/// requesting another bounded continuation.
pub fn load_csv_rows(
    cfg: &AppConfig,
    root: &Path,
    progress: &LoadProgress,
    target_rows: usize,
    update: impl FnMut(DataStore),
) -> Result<DataStore> {
    csv_index::load_rows(cfg, root, progress, target_rows, update)
}
