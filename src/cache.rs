//! Private, disposable local caches. Publish complete entries atomically; never
//! modify a published data file in place (live read-only mappings may own it).
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, Metadata};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Arc;

#[derive(Default)]
pub struct LoadProgress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cache_hit: AtomicBool,
    pub cancelled: AtomicBool,
}

impl LoadProgress {
    pub fn check(&self) -> Result<()> {
        ensure!(!self.cancelled.load(Ordering::Relaxed), "Loading cancelled");
        Ok(())
    }
    pub fn set_total(&self, bytes: u64) {
        self.total.store(bytes, Ordering::Relaxed);
    }
    pub fn advance(&self, bytes: u64) {
        self.done.store(bytes, Ordering::Relaxed);
    }
    pub fn hit(&self) {
        self.cache_hit.store(true, Ordering::Relaxed);
        self.advance(self.total.load(Ordering::Relaxed));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStamp {
    path: Vec<u8>,
    pub len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl SourceStamp {
    pub fn read(path: &Path, metadata: &Metadata) -> Result<Self> {
        ensure!(metadata.is_file(), "select a regular local file");
        Ok(Self {
            path: path.canonicalize()?.as_os_str().as_encoded_bytes().to_vec(),
            len: metadata.len(),
            modified: metadata
                .modified()
                .context("source modification time is unavailable")?,
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        })
    }
    pub fn key(&self, kind: &str, options: &impl Serialize) -> Result<String> {
        Ok(serde_json::to_string(&(
            1,
            kind,
            cfg!(target_endian = "little"),
            self,
            options,
        ))?)
    }
}

pub fn root() -> PathBuf {
    let base = std::env::var_os("BINWAVE_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("binwave-v1")
}

fn pointer_path(root: &Path, key: &str) -> PathBuf {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hash);
    root.join(format!("{:016x}.json", hash.finish()))
}

#[derive(Serialize, Deserialize)]
struct Pointer {
    key: String,
    directory: String,
}

pub fn lookup(root: &Path, key: &str) -> Option<PathBuf> {
    let file = File::open(pointer_path(root, key)).ok()?;
    if file.metadata().ok()?.len() > 1024 * 1024 {
        return None;
    }
    let pointer: Pointer = serde_json::from_reader(file).ok()?;
    let directory = Path::new(&pointer.directory);
    if pointer.key != key
        || directory.components().count() != 1
        || !matches!(directory.components().next(), Some(Component::Normal(_)))
        || !pointer.directory.starts_with("entry-")
    {
        return None;
    }
    Some(root.join(directory))
}

pub fn staging(root: &Path) -> Result<tempfile::TempDir> {
    fs::create_dir_all(root).with_context(|| {
        format!(
            "create cache at {} (set BINWAVE_CACHE_DIR to another local disk)",
            root.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    Ok(tempfile::Builder::new().prefix("entry-").tempdir_in(root)?)
}

pub fn publish(root: &Path, key: &str, directory: tempfile::TempDir) -> Result<PathBuf> {
    publish_pointer(root, key, directory.path())?;
    Ok(directory.keep())
}

fn publish_pointer(root: &Path, key: &str, directory: &Path) -> Result<()> {
    let name = directory
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut pointer = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(
        &mut pointer,
        &Pointer {
            key: key.into(),
            directory: name,
        },
    )?;
    pointer.flush()?;
    pointer
        .persist(pointer_path(root, key))
        .map_err(|e| e.error)?;
    Ok(())
}

/// Keeps incomplete CSV chunks alive while a preview still uses their mappings.
/// Successful publication transfers cleanup responsibility to the disk cache.
#[cfg(test)]
#[derive(Debug)]
pub struct PendingDirectory {
    pub path: PathBuf,
    published: AtomicBool,
}

#[cfg(test)]
impl PendingDirectory {
    pub fn new(root: &Path) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            path: staging(root)?.keep(),
            published: AtomicBool::new(false),
        }))
    }
    pub fn publish(&self, root: &Path, key: &str) -> Result<()> {
        publish_pointer(root, key, &self.path)?;
        self.published.store(true, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
impl Drop for PendingDirectory {
    fn drop(&mut self) {
        if !self.published.load(Ordering::Relaxed) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub fn freeze(file: &File) -> Result<()> {
    file.sync_all()?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;
    Ok(())
}

pub fn copy_snapshot(source: &mut File, target: &mut File, progress: &LoadProgress) -> Result<()> {
    progress.check()?;
    // FICLONE takes a copy-on-write snapshot on supporting Linux filesystems.
    // A successful clone is independent of later writes/truncation of the source.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: FICLONE takes an integer source fd, not a pointer. Both fds are live.
        if unsafe { libc::ioctl(target.as_raw_fd(), 0x40049409, source.as_raw_fd()) } == 0 {
            progress.advance(progress.total.load(Ordering::Relaxed));
            return Ok(());
        }
    }
    let mut buffer = vec![0; 1024 * 1024];
    let mut copied = 0;
    loop {
        progress.check()?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        target
            .write_all(&buffer[..count])
            .context("copy snapshot (check cache disk space)")?;
        copied += count as u64;
        ensure!(
            copied <= progress.total.load(Ordering::Relaxed),
            "source grew while loading; retry after its writer stops"
        );
        progress.advance(copied);
    }
    ensure!(
        copied == progress.total.load(Ordering::Relaxed),
        "source length changed while loading; retry"
    );
    Ok(())
}

pub fn clear() -> Result<()> {
    let root = root();
    if !root.exists() {
        return Ok(());
    }
    // Only this dedicated application directory is removed, never the user's
    // BINWAVE_CACHE_DIR / XDG_CACHE_HOME parent. Existing mappings remain valid.
    fs::remove_dir_all(&root).with_context(|| {
        format!(
            "clear cache {} (close files if your OS keeps them locked)",
            root.display()
        )
    })
}

/// Bounds generated metadata before a write can exceed the configured budget.
pub struct LimitedWriter<W> {
    inner: W,
    remaining: u64,
}
impl<W> LimitedWriter<W> {
    pub fn new(inner: W, remaining: u64) -> Self {
        Self { inner, remaining }
    }
    pub fn inner(&self) -> &W {
        &self.inner
    }
}
impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other("cache size limit reached"));
        }
        let n = self.inner.write(bytes)?;
        self.remaining -= n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
