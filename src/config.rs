use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub data_path: Option<PathBuf>,
    pub csv: CsvConfig,
    pub view: ViewConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CsvConfig {
    pub delimiter: char,
    pub has_header: bool,
    pub time_column: String,
    pub include_signals: Option<Vec<String>>, // if None, include all non-time columns
    pub decoded_disk_cache: bool,
    pub ram_cache_mib: usize,
    pub decoded_cache_limit_mib: usize,
}

impl Default for CsvConfig {
    fn default() -> Self {
        Self {
            delimiter: ',',
            has_header: true,
            time_column: "time".into(),
            include_signals: None,
            decoded_disk_cache: false,
            ram_cache_mib: 64,
            decoded_cache_limit_mib: 1024,
        }
    }
}

impl CsvConfig {
    // Keep storage preferences out of source-format keys, including old caches.
    pub fn parse_options(&self) -> impl Serialize + '_ {
        #[derive(Serialize)]
        struct Options<'a> {
            delimiter: char,
            has_header: bool,
            time_column: &'a str,
            include_signals: &'a Option<Vec<String>>,
        }
        Options {
            delimiter: self.delimiter,
            has_header: self.has_header,
            time_column: &self.time_column,
            include_signals: &self.include_signals,
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
            csv: CsvConfig::default(),
            view: ViewConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        let s = fs::read_to_string(p)
            .with_context(|| format!("Failed to read config file: {}", p.display()))?;
        // Markdown-only: extract first fenced ```json code block
        let json_str = extract_json_from_markdown(&s).ok_or_else(|| {
            anyhow::anyhow!(
                "Only Markdown configs are supported. Provide a fenced ```json code block in {}",
                p.display()
            )
        })?;
        let cfg: AppConfig = serde_json::from_str(json_str)
            .with_context(|| format!("Failed to parse JSON inside Markdown: {}", p.display()))?;
        Ok(cfg)
    }
}
fn extract_json_from_markdown(md: &str) -> Option<&str> {
    // Robustly locate the first fenced block with language starting with "json"
    let mut offset = 0usize;
    let mut lines = md.split_inclusive('\n');
    let mut start_idx: Option<usize> = None;
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if start_idx.is_none() {
            if trimmed.starts_with("```") {
                // capture language token following ``` up to whitespace or end-of-line
                let after = trimmed.trim_start_matches('`');
                let lang = after.split_whitespace().next().unwrap_or("");
                if lang.to_lowercase().starts_with("json") {
                    start_idx = Some(offset + line.len()); // start after this line's newline
                }
            }
        } else {
            if trimmed.starts_with("```") {
                let start = start_idx.unwrap();
                let end = offset; // start of this closing fence line
                if start <= end && end <= md.len() {
                    return Some(&md[start..end]);
                } else {
                    return None;
                }
            }
        }
        offset += line.len();
    }
    None
}

// All binary schemas removed to focus on CSV-based config
