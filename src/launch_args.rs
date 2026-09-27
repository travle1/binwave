//! Command-line inputs for the native workspace.

use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LaunchArgs {
    pub data: Option<PathBuf>,
    pub decoder: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Help,
    Launch(LaunchArgs),
}

pub const USAGE: &str = "Usage: binwave [FILE] [--decoder DECODER]
       binwave --file FILE [--decoder DECODER]
       binwave --decoder DECODER

Options:
  -f, --file FILE       Open a data file
  -d, --decoder FILE    Open YAML, Python, or Markdown CSV configuration
  -h, --help            Show this help
";

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Parsed, String> {
    let mut args = args.into_iter();
    let mut launch = LaunchArgs::default();
    let mut positional = false;
    while let Some(arg) = args.next() {
        if !positional && arg == "--" {
            positional = true;
            continue;
        }
        if !positional && (arg == "-h" || arg == "--help") {
            return Ok(Parsed::Help);
        }
        if !positional && (arg == "-f" || arg == "--file") {
            let value = args.next().filter(|value| !value.is_empty() && value != "--decoder"
                && value != "-d" && value != "--file" && value != "-f")
                .ok_or("--file requires a path")?;
            if launch.data.replace(PathBuf::from(value)).is_some() {
                return Err("data file was specified more than once".into());
            }
        } else if !positional && (arg == "-d" || arg == "--decoder") {
            let value = args.next().filter(|value| !value.is_empty() && value != "--file"
                && value != "-f" && value != "--decoder" && value != "-d")
                .ok_or("--decoder requires a path")?;
            if launch.decoder.replace(PathBuf::from(value)).is_some() {
                return Err("decoder was specified more than once".into());
            }
        } else if !positional && arg.to_string_lossy().starts_with('-') {
            return Err(format!("unknown option: {}", arg.to_string_lossy()));
        } else if launch.data.replace(PathBuf::from(arg)).is_some() {
            return Err("data file was specified more than once".into());
        }
    }
    for (label, path) in [("data file", &launch.data), ("decoder", &launch.decoder)] {
        if let Some(path) = path {
            if !path.is_file() {
                return Err(format!("{label} is not a file: {}", path.display()));
            }
        }
    }
    if let Some(path) = &launch.decoder {
        if crate::decoder_library::Format::from_path(path).is_none()
            || path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("csv")) {
            return Err(format!("unsupported decoder type: {}", path.display()));
        }
    }
    Ok(Parsed::Launch(launch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn accepts_positional_and_named_paths() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("bytes.bin");
        let decoder = directory.path().join("definition.yaml");
        std::fs::write(&data, b"bytes").unwrap();
        std::fs::write(&decoder, b"structures: []").unwrap();
        let data_text = data.to_string_lossy().into_owned();
        let decoder_text = decoder.to_string_lossy().into_owned();
        assert_eq!(parse(args(&[&data_text, "--decoder", &decoder_text])).unwrap(),
            Parsed::Launch(LaunchArgs { data: Some(data.clone()), decoder: Some(decoder.clone()) }));
        assert_eq!(parse(args(&["--file", &data_text, "-d", &decoder_text])).unwrap(),
            Parsed::Launch(LaunchArgs { data: Some(data), decoder: Some(decoder) }));
    }

    #[test]
    fn accepts_decoder_only_and_rejects_bad_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let decoder = directory.path().join("definition.py");
        std::fs::write(&decoder, b"print('ok')").unwrap();
        let decoder_text = decoder.to_string_lossy().into_owned();
        assert_eq!(parse(args(&["--decoder", &decoder_text])).unwrap(),
            Parsed::Launch(LaunchArgs { data: None, decoder: Some(decoder) }));
        assert!(matches!(parse(args(&["--help"])), Ok(Parsed::Help)));
        for invalid in [args(&["--file"]), args(&["--unknown"]), args(&["missing.bin"])] {
            assert!(parse(invalid).is_err());
        }
    }
}
