//! Bounded, read-only access to an arbitrary original file, before decoding.
use crate::cache::SourceStamp;
use anyhow::{ensure, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct FileSource {
    pub path: PathBuf,
    pub len: usize,
    stamp: SourceStamp,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let stamp = SourceStamp::read(path, &file.metadata()?)?;
        let len = usize::try_from(stamp.len).context("file exceeds platform address range")?;
        Ok(Self {
            path: path.to_owned(),
            len,
            stamp,
        })
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.stamp == SourceStamp::read(&self.path, &std::fs::metadata(&self.path)?)?,
            "Source file changed; reopen it to refresh the workspace."
        );
        Ok(())
    }

    #[cfg(test)]
    pub fn read(&self, start: usize, length: usize) -> Result<Vec<u8>> {
        ensure!(
            length <= 4096,
            "raw preview is limited to 4096 bytes per request"
        );
        self.read_range(start, length)
    }

    pub fn read_range(&self, start: usize, length: usize) -> Result<Vec<u8>> {
        let length = crate::file_range::bounded_length(start as u64, length, self.len as u64)
            .map_err(anyhow::Error::msg)?;
        self.validate()?;
        let mut file = File::open(&self.path)?;
        ensure!(
            self.stamp == SourceStamp::read(&self.path, &file.metadata()?)?,
            "Source file changed; reopen it."
        );
        file.seek(SeekFrom::Start(start as u64))?;
        let mut bytes = vec![0; length.min(self.len - start)];
        file.read_exact(&mut bytes)?;
        self.validate()?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn any_extension_empty_file_and_sparse_large_file_use_bounded_reads() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "data.csv",
            "data.raw",
            "layout.yaml",
            "config.md",
            "no-extension",
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"TEST\x00\x01").unwrap();
            let source = FileSource::open(&path).unwrap();
            assert_eq!(source.read(0, 256).unwrap(), b"TEST\x00\x01");
            assert!(source.read(0, 4097).is_err());
        }
        let path = dir.path().join("empty");
        File::create(&path).unwrap();
        let source = FileSource::open(&path).unwrap();
        assert!(source.read(0, 256).unwrap().is_empty());
        #[cfg(target_pointer_width = "64")]
        {
            let path = dir.path().join("large.csv");
            let mut file = File::create(&path).unwrap();
            let size = 20 * 1024 * 1024 * 1024u64;
            crate::cache::set_fixture_len(&file, size).unwrap();
            file.seek(SeekFrom::Start(size - 4)).unwrap();
            file.write_all(b"TAIL").unwrap();
            drop(file);
            let source = FileSource::open(&path).unwrap();
            assert_eq!(source.len, size as usize);
            assert_eq!(source.read(size as usize - 4, 256).unwrap(), b"TAIL");
        }
    }

    #[test]
    fn changed_or_replaced_source_requires_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, b"before").unwrap();
        let source = FileSource::open(&path).unwrap();
        std::fs::write(&path, b"after").unwrap();
        assert!(source.read(0, 256).is_err());
    }
}
