//! Bounded, read-only access to an arbitrary original file, before decoding.
use crate::cache::SourceStamp;
use anyhow::{ensure, Context, Result};
use std::fs::File;
#[cfg(not(any(unix, windows)))]
use std::io::Read;
#[cfg(any(test, not(any(unix, windows))))]
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct FileSource {
    pub path: PathBuf,
    pub len: usize,
    stamp: SourceStamp,
    file: File,
    read_calls: AtomicU64,
    bytes_read: AtomicU64,
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
            file,
            read_calls: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
        })
    }

    pub(crate) fn io_stats(&self) -> (u64, u64) {
        (self.read_calls.load(Ordering::Relaxed), self.bytes_read.load(Ordering::Relaxed))
    }

    pub(crate) fn cache_key(&self, kind: &str, options: &impl serde::Serialize) -> Result<String> {
        self.stamp.key(kind, options)
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
        let bytes = self.read_validated_range(start, length)?;
        self.validate()?;
        Ok(bytes)
    }

    /// The caller validates before and after a batch of reads. A retained file
    /// handle and positional reads keep parallel plot workers independent.
    pub(crate) fn read_validated_range(&self, start: usize, length: usize) -> Result<Vec<u8>> {
        let length = crate::file_range::bounded_length(start as u64, length, self.len as u64)
            .map_err(anyhow::Error::msg)?;
        let mut bytes = vec![0; length.min(self.len - start)];
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(&mut bytes, start as u64)?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut offset = start as u64;
            let mut remaining = bytes.as_mut_slice();
            while !remaining.is_empty() {
                let count = self.file.seek_read(remaining, offset)?;
                ensure!(count > 0, "source ended during plot read");
                offset += count as u64;
                remaining = &mut remaining[count..];
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let mut file = File::open(&self.path)?;
            file.seek(SeekFrom::Start(start as u64))?;
            file.read_exact(&mut bytes)?;
        }
        self.read_calls.fetch_add(1, Ordering::Relaxed);
        self.bytes_read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
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

    #[test]
    fn parallel_positional_reads_share_one_open_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interleaved.bin");
        let bytes: Vec<u8> = (0..8192).map(|index| (index % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let source = FileSource::open(&path).unwrap();
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let source = &source;
                let bytes = &bytes;
                scope.spawn(move || {
                    for index in 0..128 {
                        let start = (worker * 997 + index * 53) % (bytes.len() - 257);
                        assert_eq!(source.read_validated_range(start, 257).unwrap(), bytes[start..start + 257]);
                    }
                });
            }
        });
        source.validate().unwrap();
    }
}
