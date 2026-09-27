//! Metadata for decoders stored outside the application bundle.
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    All,
    Yaml,
    Python,
    Csv,
}

impl Format {
    pub const FILTERS: [Self; 4] = [Self::All, Self::Yaml, Self::Python, Self::Csv];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Yaml => "YAML",
            Self::Python => "Python",
            Self::Csv => "CSV",
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "yaml" | "yml" => Some(Self::Yaml),
            "py" => Some(Self::Python),
            "md" | "markdown" | "csv" => Some(Self::Csv),
            _ => None,
        }
    }
}

pub fn name(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

pub fn edited_label(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let elapsed = std::time::SystemTime::now().duration_since(modified).ok()?;
    let days = elapsed.as_secs() / 86_400;
    Some(match days {
        0 => "Edited today".into(),
        1 => "Edited yesterday".into(),
        _ => format!("Edited {days} days ago"),
    })
}

pub fn yaml_structures(path: &Path) -> Vec<String> {
    if Format::from_path(path) != Some(Format::Yaml)
        || std::fs::metadata(path).map_or(true, |metadata| metadata.len() > 1024 * 1024) {
        return Vec::new();
    }
    let Ok(source) = std::fs::read_to_string(path) else { return Vec::new(); };
    let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&source) else { return Vec::new(); };
    value.get("structures").and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter().flatten().take(64)
        .filter_map(|item| item.get("name").and_then(serde_yaml_ng::Value::as_str))
        .map(str::to_owned).collect()
}

pub fn valid_new_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

pub fn duplicate_path(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let stem = path.file_stem()?.to_string_lossy();
    let extension = path.extension().and_then(|value| value.to_str()).unwrap_or_default();
    for index in 1..=999 {
        let suffix = if index == 1 { "-copy".to_owned() } else { format!("-copy-{index}") };
        let filename = if extension.is_empty() {
            format!("{stem}{suffix}")
        } else {
            format!("{stem}{suffix}.{extension}")
        };
        let candidate = parent.join(filename);
        if !candidate.exists() { return Some(candidate); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_decoder_directory() {
        assert!(valid_new_name("decoder.yaml"));
        for value in ["", ".", "..", "../other.yaml", "sub/file.yaml", "sub\\file.yaml"] {
            assert!(!valid_new_name(value));
        }
    }

    #[test]
    fn duplicate_never_reuses_an_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("decoder.yaml");
        std::fs::write(&source, "source").unwrap();
        let first = duplicate_path(&source).unwrap();
        assert_eq!(first.file_name().unwrap(), "decoder-copy.yaml");
        std::fs::write(&first, "copy").unwrap();
        let second = duplicate_path(&source).unwrap();
        assert_eq!(second.file_name().unwrap(), "decoder-copy-2.yaml");
    }

    #[test]
    fn structure_names_come_from_yaml_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decoder.yaml");
        std::fs::write(&path, "structures:\n  - name: Header\n  - name: Payload\n").unwrap();
        assert_eq!(yaml_structures(&path), ["Header", "Payload"]);
    }
}
