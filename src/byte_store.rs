//! One editable byte view for every interpretation. Original data stays on disk;
//! edits and undo groups are sparse, and background readers see immutable revisions.
use crate::binary::Original;
use crate::file_source::FileSource;
use anyhow::{ensure, Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

static REVISION: AtomicU64 = AtomicU64::new(1);
pub const READ_LIMIT: usize = 1024 * 1024;
fn revision() -> u64 {
    REVISION.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
enum Base {
    File(Arc<FileSource>),
    Snapshot(Arc<Original>),
}
impl Base {
    fn len(&self) -> usize {
        match self {
            Self::File(s) => s.len,
            Self::Snapshot(s) => s.bytes().len(),
        }
    }
    fn read(&self, offset: usize, length: usize) -> Result<Vec<u8>> {
        ensure!(length <= READ_LIMIT, "read exceeds the 1 MiB chunk limit");
        ensure!(offset <= self.len(), "byte offset is outside the file");
        let end = offset + length.min(self.len() - offset);
        match self {
            Self::File(source) => source.read_range(offset, end - offset),
            Self::Snapshot(source) => Ok(source.bytes()[offset..end].to_vec()),
        }
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::File(source) => source.validate(),
            Self::Snapshot(_) => Ok(()),
        }
    }
}

/// The original file identity remains protected after switching to a private
/// snapshot. A snapshot is a read optimization, not permission to replace input.
#[derive(Clone)]
struct Origin {
    path: PathBuf,
    #[cfg(unix)]
    identity: Option<(u64, u64)>,
}
impl Origin {
    fn from_file(source: &FileSource) -> Self {
        Self {
            path: source
                .path
                .canonicalize()
                .unwrap_or_else(|_| source.path.clone()),
            #[cfg(unix)]
            identity: std::fs::metadata(&source.path).ok().map(|metadata| {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            }),
        }
    }
    fn check_destination(&self, path: &Path) -> Result<()> {
        let canonical = if path.exists() {
            path.canonicalize().context("resolve output file")?
        } else {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            parent
                .canonicalize()
                .context("resolve output directory")?
                .join(path.file_name().context("choose an output filename")?)
        };
        ensure!(canonical != self.path, "Choose a different output file: Save edited copy and Extract cannot replace the open source file.");
        if path.exists() && self.path.exists() {
            ensure!(!same_file::is_same_file(&self.path, path).context("compare output with source")?,
                "Choose a different output file: this destination is a link to the open source file.");
        }
        #[cfg(unix)]
        if let (Some(identity), Ok(metadata)) = (self.identity, std::fs::metadata(path)) {
            use std::os::unix::fs::MetadataExt;
            ensure!(identity != (metadata.dev(), metadata.ino()), "Choose a different output file: this destination is a link to the open source file.");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ByteView {
    base: Base,
    origin: Option<Arc<Origin>>,
    document: u64,
    pub patches: Arc<BTreeMap<usize, u8>>,
    pub revision: u64,
}
impl ByteView {
    pub fn len(&self) -> usize {
        self.base.len()
    }
    pub fn read_original(&self, offset: usize, length: usize) -> Result<Vec<u8>> {
        self.base.read(offset, length)
    }
    pub fn read(&self, offset: usize, length: usize) -> Result<Vec<u8>> {
        let mut bytes = self.read_original(offset, length)?;
        for (&position, &value) in self.patches.range(offset..offset + bytes.len()) {
            bytes[position - offset] = value;
        }
        Ok(bytes)
    }
    pub fn validate(&self) -> Result<()> {
        self.base.validate()
    }
    /// Stream an exact selected range, preserving the destination on errors.
    pub fn export(&self, range: Range<usize>, path: &Path, overwrite: bool) -> Result<()> {
        ensure!(
            range.start <= range.end && range.end <= self.len(),
            "export range is outside the file"
        );
        self.validate()?;
        if let Some(origin) = &self.origin {
            origin.check_destination(path)?;
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut output = tempfile::NamedTempFile::new_in(parent).context("create output file")?;
        let mut offset = range.start;
        while offset < range.end {
            let bytes = self.read(offset, READ_LIMIT.min(range.end - offset))?;
            ensure!(!bytes.is_empty(), "source ended during export");
            output.write_all(&bytes)?;
            offset += bytes.len();
        }
        self.validate()?;
        output.as_file().sync_all()?;
        if let Some(origin) = &self.origin {
            origin.check_destination(path)?;
        }
        if overwrite {
            output.persist(path).map_err(|e| e.error)?;
        } else {
            output
                .persist_noclobber(path)
                .map_err(|e| e.error)
                .context("destination already exists; confirm overwrite or choose another file")?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Edit {
    offset: usize,
    before: Vec<u8>,
    after: Vec<u8>,
}
pub struct ByteStore {
    view: ByteView,
    saved: Arc<BTreeMap<usize, u8>>,
    history: Vec<Edit>,
    cursor: usize,
}
impl ByteStore {
    pub fn from_file(source: Arc<FileSource>) -> Self {
        Self::new(Base::File(source))
    }
    pub fn from_snapshot(source: Arc<Original>) -> Self {
        Self::new(Base::Snapshot(source))
    }
    fn new(base: Base) -> Self {
        let origin = match &base {
            Base::File(source) => Some(Arc::new(Origin::from_file(source))),
            Base::Snapshot(_) => None,
        };
        let document = revision();
        Self {
            view: ByteView {
                base,
                origin,
                document,
                patches: Arc::new(BTreeMap::new()),
                revision: document,
            },
            saved: Arc::new(BTreeMap::new()),
            history: vec![],
            cursor: 0,
        }
    }
    pub fn view(&self) -> ByteView {
        self.view.clone()
    }
    pub fn is_dirty(&self) -> bool {
        self.view.patches != self.saved
    }
    pub fn has_changes(&self) -> bool {
        !self.view.patches.is_empty()
    }
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }
    pub fn can_redo(&self) -> bool {
        self.cursor < self.history.len()
    }
    pub fn mark_saved(&mut self, view: &ByteView) {
        // A save worker can finish after the user has opened a different file.
        if self.view.document == view.document {
            self.saved = view.patches.clone();
        }
    }
    /// Freeze the original for legacy numeric editing without losing raw history.
    pub fn use_snapshot(&mut self, source: Arc<Original>) -> Result<()> {
        ensure!(
            source.bytes().len() == self.view.len(),
            "snapshot length changed"
        );
        self.view.validate()?;
        self.view.base = Base::Snapshot(source);
        Ok(())
    }
    fn patch(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        let original = self.view.read_original(offset, bytes.len())?;
        ensure!(
            original.len() == bytes.len(),
            "replacement extends beyond the file"
        );
        let patches = Arc::make_mut(&mut self.view.patches);
        for (i, &value) in bytes.iter().enumerate() {
            if value == original[i] {
                patches.remove(&(offset + i));
            } else {
                patches.insert(offset + i, value);
            }
        }
        self.view.revision = revision();
        Ok(())
    }
    pub fn replace(&mut self, offset: usize, bytes: &[u8]) -> Result<Range<usize>> {
        ensure!(!bytes.is_empty(), "replacement is empty");
        ensure!(
            bytes.len() <= READ_LIMIT,
            "replace at most 1 MiB in one operation"
        );
        ensure!(
            offset
                .checked_add(bytes.len())
                .is_some_and(|end| end <= self.view.len()),
            "replacement extends beyond the file"
        );
        let before = self.view.read(offset, bytes.len())?;
        if before != bytes {
            self.history
                .try_reserve(1)
                .context("not enough memory for edit history")?;
            self.patch(offset, bytes)?;
            self.history.truncate(self.cursor);
            self.history.push(Edit {
                offset,
                before,
                after: bytes.to_vec(),
            });
            self.cursor += 1;
        }
        Ok(offset..offset + bytes.len())
    }
    pub fn undo(&mut self) -> Result<Option<Range<usize>>> {
        if !self.can_undo() {
            return Ok(None);
        }
        let edit = self.history[self.cursor - 1].clone();
        self.patch(edit.offset, &edit.before)?;
        self.cursor -= 1;
        Ok(Some(edit.offset..edit.offset + edit.before.len()))
    }
    pub fn redo(&mut self) -> Result<Option<Range<usize>>> {
        if !self.can_redo() {
            return Ok(None);
        }
        let edit = self.history[self.cursor].clone();
        self.patch(edit.offset, &edit.after)?;
        self.cursor += 1;
        Ok(Some(edit.offset..edit.offset + edit.after.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exports_preserve_the_input_and_aliases_even_after_freezing_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.dat");
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir).unwrap();
        std::fs::write(&source_path, b"original").unwrap();
        let mut aliases = vec![source_path.clone(), subdir.join("../source.dat")];
        let hard = dir.path().join("hard.dat");
        std::fs::hard_link(&source_path, &hard).unwrap();
        aliases.push(hard);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let sym = dir.path().join("symlink.dat");
            symlink(&source_path, &sym).unwrap();
            aliases.push(sym);
            let directory_alias = dir.path().join("alias");
            symlink(&subdir, &directory_alias).unwrap();
            aliases.push(directory_alias.join("../source.dat"));
        }
        let mut store = ByteStore::from_file(Arc::new(FileSource::open(&source_path).unwrap()));
        store.replace(0, b"EDIT").unwrap();
        for path in &aliases {
            let error = store.view().export(0..8, path, true).unwrap_err();
            assert!(
                error.to_string().contains("different output file"),
                "{error}"
            );
        }
        store
            .use_snapshot(Original::from_bytes(b"original").unwrap())
            .unwrap();
        for path in &aliases {
            assert!(store.view().export(0..8, path, true).is_err());
        }
        assert_eq!(std::fs::read(&source_path).unwrap(), b"original");
        let output = dir.path().join("edited.dat");
        store.view().export(0..8, &output, false).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"EDITinal");
        std::fs::remove_file(&source_path).unwrap();
        assert!(store.view().export(0..8, &source_path, true).is_err());
    }

    #[test]
    fn saved_views_belong_to_one_document_and_undo_branches_stay_consistent() {
        let mut store = ByteStore::from_snapshot(Original::from_bytes(b"abcd").unwrap());
        store.replace(0, b"A").unwrap();
        let saving = store.view();
        store.replace(1, b"B").unwrap();
        store.mark_saved(&saving);
        assert!(store.is_dirty(), "edits made during export remain unsaved");
        store.undo().unwrap();
        assert!(!store.is_dirty());
        let mut other = ByteStore::from_snapshot(Original::from_bytes(b"abcd").unwrap());
        other.replace(2, b"C").unwrap();
        store.mark_saved(&other.view());
        assert!(
            !store.is_dirty(),
            "a late save from another document is ignored"
        );
        store.replace(0, b"A").unwrap();
        assert!(store.can_redo(), "a no-op replacement preserves redo");
        store.replace(2, b"Z").unwrap();
        assert!(!store.can_redo(), "a new edit branches the undo history");
        store.undo().unwrap();
        assert!(!store.is_dirty());
        store.redo().unwrap();
        assert_eq!(store.view().read(0, 4).unwrap(), b"AbZd");
    }

    #[test]
    fn empty_export_is_valid_and_source_changes_do_not_mutate_history_or_destination() {
        let dir = tempfile::tempdir().unwrap();
        let mut empty = ByteStore::from_snapshot(Original::from_bytes(&[]).unwrap());
        assert!(empty.view().read(0, 0).unwrap().is_empty());
        let empty_path = dir.path().join("empty");
        empty.view().export(0..0, &empty_path, false).unwrap();
        assert_eq!(std::fs::metadata(&empty_path).unwrap().len(), 0);
        assert!(empty.replace(0, &[1]).is_err());
        assert!(!empty.is_dirty());
        let path = dir.path().join("source");
        std::fs::write(&path, b"original").unwrap();
        let mut store = ByteStore::from_file(Arc::new(FileSource::open(&path).unwrap()));
        store.replace(0, b"E").unwrap();
        let patches = store.view().patches;
        let destination = dir.path().join("existing-output");
        std::fs::write(&destination, b"keep me").unwrap();
        std::fs::write(&path, b"changed input").unwrap();
        assert!(store.undo().is_err());
        assert!(store.can_undo());
        assert!(!store.can_redo());
        assert_eq!(store.view().patches, patches);
        assert!(store.view().export(0..8, &destination, true).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep me");
    }

    #[test]
    fn revisions_history_export_and_saved_checkpoints_share_exact_bytes() {
        let original = Original::from_bytes(b"abcdefgh").unwrap();
        let mut store = ByteStore::from_snapshot(original);
        let before = store.view();
        store.replace(2, b"XYZ").unwrap();
        let edited = store.view();
        assert_ne!(before.revision, edited.revision);
        assert_eq!(before.read(0, 8).unwrap(), b"abcdefgh");
        assert_eq!(edited.read(0, 8).unwrap(), b"abXYZfgh");
        store.mark_saved(&edited);
        assert!(!store.is_dirty());
        store.undo().unwrap();
        assert!(store.is_dirty());
        assert_eq!(store.view().read(0, 8).unwrap(), b"abcdefgh");
        store.redo().unwrap();
        assert!(!store.is_dirty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selection");
        edited.export(2..5, &path, false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"XYZ");
        assert!(before.export(0..8, &path, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"XYZ");
        assert!(store.replace(7, b"XX").is_err());
        assert_eq!(store.view().read(0, 8).unwrap(), b"abXYZfgh");
    }
    #[test]
    fn large_files_edit_without_snapshotting_and_changed_sources_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large");
        let file = std::fs::File::create(&path).unwrap();
        crate::cache::set_fixture_len(&file, 20 * 1024 * 1024 * 1024).unwrap();
        drop(file);
        let mut store = ByteStore::from_file(Arc::new(FileSource::open(&path).unwrap()));
        let end = store.view().len() - 1;
        store.replace(end, &[255]).unwrap();
        assert_eq!(store.view().patches.len(), 1);
        assert_eq!(store.view().read(end, 1).unwrap(), [255]);
        store.undo().unwrap();
        assert!(store.view().patches.is_empty());
        std::fs::write(&path, b"changed").unwrap();
        assert!(store.replace(0, &[1]).is_err());
    }
}
