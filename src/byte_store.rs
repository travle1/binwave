//! One editable byte view for every interpretation. Original data stays on disk;
//! edits are pieces over it, so inserts and deletes shift offsets without copying,
//! and background readers see immutable revisions.
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
    fn read_validated(&self, offset: usize, length: usize) -> Result<Vec<u8>> {
        ensure!(length <= READ_LIMIT, "read exceeds the 1 MiB chunk limit");
        ensure!(offset <= self.len(), "byte offset is outside the file");
        let end = offset + length.min(self.len() - offset);
        match self {
            Self::File(source) => source.read_validated_range(offset, end - offset),
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

/// A run of the edited file: bytes from the original, or bytes the user typed.
#[derive(Clone, Debug)]
enum Piece {
    Base { start: usize, len: usize },
    Added(Arc<[u8]>),
}
impl Piece {
    fn len(&self) -> usize {
        match self {
            Self::Base { len, .. } => *len,
            Self::Added(bytes) => bytes.len(),
        }
    }
    fn slice(&self, from: usize, to: usize) -> Self {
        match self {
            Self::Base { start, .. } => Self::Base { start: start + from, len: to - from },
            Self::Added(bytes) => Self::Added(bytes[from..to].into()),
        }
    }
}

/// Keep typed runs from splitting into one piece per keystroke.
const MERGE_LIMIT: usize = 64 * 1024;

/// The edited file as pieces of the original and added bytes. Offsets after an
/// insert or delete shift without copying the original.
#[derive(Clone, Debug)]
struct Pieces {
    list: Vec<Piece>,
    starts: Vec<usize>,
    len: usize,
}
impl Pieces {
    fn new(list: Vec<Piece>) -> Self {
        let mut merged: Vec<Piece> = Vec::with_capacity(list.len());
        for piece in list.into_iter().filter(|piece| piece.len() > 0) {
            match (merged.last_mut(), &piece) {
                (Some(Piece::Base { start, len }), Piece::Base { start: next, len: more })
                    if *start + *len == *next =>
                {
                    *len += more;
                }
                (Some(Piece::Added(bytes)), Piece::Added(more))
                    if bytes.len() + more.len() <= MERGE_LIMIT =>
                {
                    *bytes = [&bytes[..], &more[..]].concat().into();
                }
                _ => merged.push(piece),
            }
        }
        let mut len = 0;
        let starts = merged
            .iter()
            .map(|piece| {
                let start = len;
                len += piece.len();
                start
            })
            .collect();
        Self { list: merged, starts, len }
    }
    fn original(len: usize) -> Self {
        Self::new(vec![Piece::Base { start: 0, len }])
    }
    /// The pieces covering `offset..offset + length`, each with its logical start.
    fn covering(&self, offset: usize, length: usize) -> impl Iterator<Item = (usize, &Piece)> {
        let first = self.starts.partition_point(|&start| start <= offset).saturating_sub(1);
        let end = offset + length;
        self.starts[first..]
            .iter()
            .zip(&self.list[first..])
            .take_while(move |(&start, _)| start < end)
            .map(|(&start, piece)| (start, piece))
    }
    /// Replace `remove` bytes at `offset` with `insert`, returning the result and
    /// the pieces removed so the edit can be undone.
    fn splice(&self, offset: usize, remove: usize, insert: &[Piece]) -> (Self, Vec<Piece>) {
        let end = offset + remove;
        let (mut kept, mut removed, mut tail) = (vec![], vec![], vec![]);
        for (start, piece) in self.starts.iter().zip(&self.list) {
            let stop = start + piece.len();
            if stop <= offset {
                kept.push(piece.clone());
                continue;
            }
            if *start >= end {
                tail.push(piece.clone());
                continue;
            }
            if *start < offset {
                kept.push(piece.slice(0, offset - start));
            }
            let (from, to) = (offset.max(*start) - start, end.min(stop) - start);
            if from < to {
                removed.push(piece.slice(from, to));
            }
            if stop > end {
                tail.push(piece.slice(end.max(*start) - start, piece.len()));
            }
        }
        kept.extend(insert.iter().cloned());
        kept.extend(tail);
        (Self::new(kept), removed)
    }
    fn same_content(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        let (mut a, mut b) = (self.list.iter(), other.list.iter());
        let (mut left, mut right) = (a.next().cloned(), b.next().cloned());
        while let (Some(x), Some(y)) = (&left, &right) {
            let n = x.len().min(y.len());
            let same = match (x, y) {
                (Piece::Base { start: s, .. }, Piece::Base { start: t, .. }) => s == t,
                (Piece::Added(p), Piece::Added(q)) => p[..n] == q[..n],
                _ => false,
            };
            if !same {
                return false;
            }
            left = if n < x.len() { Some(x.slice(n, x.len())) } else { a.next().cloned() };
            right = if n < y.len() { Some(y.slice(n, y.len())) } else { b.next().cloned() };
        }
        left.is_none() && right.is_none()
    }
}

#[derive(Clone)]
pub struct ByteView {
    base: Base,
    origin: Option<Arc<Origin>>,
    document: u64,
    pieces: Arc<Pieces>,
    pub revision: u64,
}
impl ByteView {
    pub fn len(&self) -> usize {
        self.pieces.len
    }
    /// The file's length on disk, before inserts and deletes.
    pub fn original_len(&self) -> usize {
        self.base.len()
    }
    pub fn has_edits(&self) -> bool {
        !self.pieces.same_content(&Pieces::original(self.base.len()))
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn plot_cache_key(&self, options: &impl serde::Serialize) -> Option<String> {
        if self.has_edits() { return None; }
        match &self.base {
            Base::File(source) => source.cache_key("decoded-plot-v1", options).ok(),
            Base::Snapshot(_) => None,
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn plot_source_path(&self) -> Option<PathBuf> {
        match &self.base {
            Base::File(source) => source.path.canonicalize().ok(),
            Base::Snapshot(_) => None,
        }
    }
    /// The open file on disk, when there is one to replace.
    pub fn source_path(&self) -> Option<&Path> {
        self.origin.as_ref().map(|origin| origin.path.as_path())
    }
    pub fn read(&self, offset: usize, length: usize) -> Result<Vec<u8>> {
        self.read_with(offset, length, |start, len| self.base.read(start, len))
    }
    /// Use within a plot scan bracketed by `validate` calls. The final
    /// validation discards results if the source changed during the scan.
    pub(crate) fn read_validated(&self, offset: usize, length: usize) -> Result<Vec<u8>> {
        self.read_with(offset, length, |start, len| self.base.read_validated(start, len))
    }
    fn read_with(
        &self,
        offset: usize,
        length: usize,
        read: impl Fn(usize, usize) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        ensure!(length <= READ_LIMIT, "read exceeds the 1 MiB chunk limit");
        ensure!(offset <= self.len(), "byte offset is outside the file");
        let length = length.min(self.len() - offset);
        if length == 0 {
            // Keep reporting a changed source even for empty reads.
            return read(0, 0);
        }
        let mut bytes = Vec::with_capacity(length);
        for (start, piece) in self.pieces.covering(offset, length) {
            let from = offset.max(start) - start;
            let to = (offset + length).min(start + piece.len()) - start;
            match piece {
                Piece::Base { start: base, .. } => {
                    let chunk = read(base + from, to - from)?;
                    ensure!(chunk.len() == to - from, "source ended during read");
                    bytes.extend_from_slice(&chunk);
                }
                Piece::Added(added) => bytes.extend_from_slice(&added[from..to]),
            }
        }
        Ok(bytes)
    }
    /// Which bytes in `offset..offset + length` were typed or inserted.
    pub fn edited(&self, offset: usize, length: usize) -> Vec<bool> {
        let length = length.min(self.len().saturating_sub(offset));
        let mut flags = Vec::with_capacity(length);
        for (start, piece) in self.pieces.covering(offset, length) {
            let from = offset.max(start) - start;
            let to = (offset + length).min(start + piece.len()) - start;
            flags.extend(std::iter::repeat_n(matches!(piece, Piece::Added(_)), to - from));
        }
        flags
    }
    /// Overwritten bytes by offset, for views that address the original file
    /// directly. None once inserts or deletes have shifted offsets.
    pub fn overwrite_patches(&self) -> Option<BTreeMap<usize, u8>> {
        let mut patches = BTreeMap::new();
        for (start, piece) in self.pieces.starts.iter().zip(&self.pieces.list) {
            match piece {
                Piece::Base { start: base, .. } if base != start => return None,
                Piece::Base { .. } => {}
                Piece::Added(bytes) => {
                    if start + bytes.len() > self.base.len() {
                        return None;
                    }
                    for (chunk, index) in bytes.chunks(READ_LIMIT).zip((0..).step_by(READ_LIMIT)) {
                        let original = self.base.read(start + index, chunk.len()).ok()?;
                        for (i, (&value, &was)) in chunk.iter().zip(&original).enumerate() {
                            if value != was {
                                patches.insert(start + index + i, value);
                            }
                        }
                    }
                }
            }
        }
        (self.len() == self.base.len()).then_some(patches)
    }
    pub fn validate(&self) -> Result<()> {
        self.base.validate()
    }
    fn stream(&self, range: Range<usize>, output: &mut impl Write) -> Result<()> {
        let mut offset = range.start;
        while offset < range.end {
            let bytes = self.read(offset, READ_LIMIT.min(range.end - offset))?;
            ensure!(!bytes.is_empty(), "source ended during export");
            output.write_all(&bytes)?;
            offset += bytes.len();
        }
        Ok(())
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
        self.stream(range, &mut output)?;
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
    /// Write the edited file next to the original, then swap it in only once it
    /// is complete. The original stays intact if anything fails first.
    pub fn replace_source(&self) -> Result<PathBuf> {
        let origin = self.origin.as_ref().context("this data has no file to replace")?;
        let path = origin.path.clone();
        self.validate()?;
        let permissions = std::fs::metadata(&path).context("read file permissions")?.permissions();
        let parent = path.parent().context("resolve the file's folder")?;
        let mut output = tempfile::NamedTempFile::new_in(parent).context("create temporary file")?;
        self.stream(0..self.len(), &mut output)?;
        output.as_file().sync_all()?;
        self.validate()?;
        std::fs::set_permissions(output.path(), permissions).context("copy file permissions")?;
        output.persist(&path).map_err(|e| e.error).context("replace the original file")?;
        Ok(path)
    }
}

#[derive(Clone)]
struct Edit {
    offset: usize,
    removed: Vec<Piece>,
    inserted: Vec<Piece>,
    /// An overwrite reports the whole range it was asked to write.
    written: Option<Range<usize>>,
}
impl Edit {
    fn size(pieces: &[Piece]) -> usize {
        pieces.iter().map(Piece::len).sum()
    }
}
pub struct ByteStore {
    view: ByteView,
    saved: Arc<Pieces>,
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
        let pieces = Arc::new(Pieces::original(base.len()));
        Self {
            view: ByteView {
                base,
                origin,
                document,
                pieces: pieces.clone(),
                revision: document,
            },
            saved: pieces,
            history: vec![],
            cursor: 0,
        }
    }
    pub fn view(&self) -> ByteView {
        self.view.clone()
    }
    pub fn is_dirty(&self) -> bool {
        !Arc::ptr_eq(&self.view.pieces, &self.saved) && !self.view.pieces.same_content(&self.saved)
    }
    pub fn has_changes(&self) -> bool {
        self.view.has_edits()
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
            self.saved = view.pieces.clone();
        }
    }
    /// Freeze the original for legacy numeric editing without losing raw history.
    pub fn use_snapshot(&mut self, source: Arc<Original>) -> Result<()> {
        ensure!(
            source.bytes().len() == self.view.base.len(),
            "snapshot length changed"
        );
        self.view.validate()?;
        self.view.base = Base::Snapshot(source);
        Ok(())
    }
    fn apply(&mut self, offset: usize, remove: usize, insert: &[Piece]) -> Vec<Piece> {
        let (pieces, removed) = self.view.pieces.splice(offset, remove, insert);
        self.view.pieces = Arc::new(pieces);
        self.view.revision = revision();
        removed
    }
    fn record(
        &mut self,
        offset: usize,
        remove: usize,
        insert: Vec<Piece>,
        written: Option<Range<usize>>,
    ) -> Result<()> {
        self.view.validate()?;
        self.history
            .try_reserve(1)
            .context("not enough memory for edit history")?;
        let removed = self.apply(offset, remove, &insert);
        self.history.truncate(self.cursor);
        self.history.push(Edit { offset, removed, inserted: insert, written });
        self.cursor += 1;
        Ok(())
    }
    /// Overwrite bytes in place. Only the bytes that change become edits.
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
        let (Some(first), Some(last)) = (
            before.iter().zip(bytes).position(|(a, b)| a != b),
            before.iter().zip(bytes).rposition(|(a, b)| a != b),
        ) else {
            return Ok(offset..offset + bytes.len());
        };
        let mut insert = vec![];
        let mut index = first;
        while index <= last {
            let same = before[index] == bytes[index];
            let run = bytes[index..=last]
                .iter()
                .zip(&before[index..=last])
                .take_while(|(a, b)| (a == b) == same)
                .count();
            let at = offset + index;
            insert.push(if same {
                // Keep unchanged bytes as whatever they were before.
                self.view.pieces.splice(at, run, &[]).1
            } else {
                vec![Piece::Added(bytes[index..index + run].into())]
            });
            index += run;
        }
        let written = offset..offset + bytes.len();
        self.record(offset + first, last + 1 - first, insert.concat(), Some(written.clone()))?;
        Ok(written)
    }
    /// Insert bytes before `offset`, shifting every later byte.
    pub fn insert(&mut self, offset: usize, bytes: &[u8]) -> Result<Range<usize>> {
        ensure!(!bytes.is_empty(), "nothing to insert");
        ensure!(bytes.len() <= READ_LIMIT, "insert at most 1 MiB in one operation");
        ensure!(offset <= self.view.len(), "insert position is outside the file");
        self.record(offset, 0, vec![Piece::Added(bytes.into())], None)?;
        Ok(offset..offset + bytes.len())
    }
    /// Remove a range, shifting every later byte. Returns the empty range left behind.
    pub fn delete(&mut self, range: Range<usize>) -> Result<Range<usize>> {
        ensure!(!range.is_empty(), "nothing to delete");
        ensure!(range.end <= self.view.len(), "delete range is outside the file");
        self.record(range.start, range.len(), vec![], None)?;
        Ok(range.start..range.start)
    }
    /// Undo the last edit; returns the bytes it restored (empty for an insert).
    pub fn undo(&mut self) -> Result<Option<Range<usize>>> {
        if !self.can_undo() {
            return Ok(None);
        }
        self.view.validate()?;
        let edit = self.history[self.cursor - 1].clone();
        self.apply(edit.offset, Edit::size(&edit.inserted), &edit.removed);
        self.cursor -= 1;
        Ok(Some(edit.written.unwrap_or(edit.offset..edit.offset + Edit::size(&edit.removed))))
    }
    /// Redo the next edit; returns the bytes it wrote (empty for a delete).
    pub fn redo(&mut self) -> Result<Option<Range<usize>>> {
        if !self.can_redo() {
            return Ok(None);
        }
        self.view.validate()?;
        let edit = self.history[self.cursor].clone();
        self.apply(edit.offset, Edit::size(&edit.removed), &edit.inserted);
        self.cursor += 1;
        Ok(Some(edit.written.unwrap_or(edit.offset..edit.offset + Edit::size(&edit.inserted))))
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
        let pieces = format!("{:?}", store.view().pieces.list);
        let destination = dir.path().join("existing-output");
        std::fs::write(&destination, b"keep me").unwrap();
        std::fs::write(&path, b"changed input").unwrap();
        assert!(store.undo().is_err());
        assert!(store.can_undo());
        assert!(!store.can_redo());
        assert_eq!(format!("{:?}", store.view().pieces.list), pieces);
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
        assert_eq!(store.view().overwrite_patches().unwrap().len(), 1);
        assert_eq!(store.view().read(end, 1).unwrap(), [255]);
        store.undo().unwrap();
        assert!(!store.view().has_edits());
        std::fs::write(&path, b"changed").unwrap();
        assert!(store.replace(0, &[1]).is_err());
    }

    #[test]
    fn inserts_and_deletes_shift_later_bytes_and_undo_restores_them() {
        let mut store = ByteStore::from_snapshot(Original::from_bytes(b"abcdefgh").unwrap());
        assert_eq!(store.insert(2, b"XY").unwrap(), 2..4);
        assert_eq!(store.view().read(0, 16).unwrap(), b"abXYcdefgh");
        assert_eq!(store.view().edited(0, 5), [false, false, true, true, false]);
        assert_eq!(store.delete(5..8).unwrap(), 5..5);
        assert_eq!(store.view().read(0, 16).unwrap(), b"abXYcgh");
        assert_eq!(store.view().read(3, 3).unwrap(), b"Ycg", "reads cross pieces");
        assert!(store.view().overwrite_patches().is_none());
        store.replace(0, b"A").unwrap();
        assert_eq!(store.view().read(0, 16).unwrap(), b"AbXYcgh");
        assert_eq!(store.undo().unwrap(), Some(0..1));
        assert_eq!(store.undo().unwrap(), Some(5..8));
        assert_eq!(store.view().read(0, 16).unwrap(), b"abXYcdefgh");
        assert_eq!(store.undo().unwrap(), Some(2..2));
        assert_eq!(store.view().read(0, 16).unwrap(), b"abcdefgh");
        assert!(!store.is_dirty() && !store.has_changes());
        assert_eq!(store.redo().unwrap(), Some(2..4));
        assert_eq!(store.redo().unwrap(), Some(5..5));
        assert_eq!(store.view().read(0, 16).unwrap(), b"abXYcgh");
        assert!(store.insert(99, b"Z").is_err());
        assert!(store.delete(6..9).is_err());
        store.insert(7, b"!").unwrap();
        assert_eq!(store.view().read(0, 16).unwrap(), b"abXYcgh!", "append at the end");
    }

    #[test]
    fn overwrites_mark_only_changed_bytes_and_typing_stays_compact() {
        let mut store = ByteStore::from_snapshot(Original::from_bytes(&[0; 64]).unwrap());
        store.replace(4, &[1, 0, 0, 2]).unwrap();
        assert_eq!(store.view().edited(4, 4), [true, false, false, true]);
        assert_eq!(store.view().overwrite_patches().unwrap(), BTreeMap::from([(4, 1), (7, 2)]));
        for (i, byte) in (10..40).enumerate() {
            store.insert(20 + i, &[byte]).unwrap();
        }
        assert_eq!(store.view().pieces.list.len(), 7, "30 typed bytes form one piece");
        assert_eq!(store.view().read(20, 30).unwrap(), (10..40).collect::<Vec<u8>>());
        while store.can_undo() {
            store.undo().unwrap();
        }
        assert!(!store.is_dirty());
        assert_eq!(store.view().read(0, 64).unwrap(), [0; 64]);
    }

    #[test]
    fn deleting_most_of_a_large_file_copies_nothing_and_saves_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large");
        let file = std::fs::File::create(&path).unwrap();
        crate::cache::set_fixture_len(&file, 20 * 1024 * 1024 * 1024).unwrap();
        drop(file);
        let source = Arc::new(FileSource::open(&path).unwrap());
        let mut store = ByteStore::from_file(source.clone());
        store.delete(4..store.view().len() - 4).unwrap();
        store.insert(4, b"mid").unwrap();
        let view = store.view();
        assert_eq!(view.len(), 11);
        assert_eq!(view.original_len(), 20 * 1024 * 1024 * 1024);
        assert!(source.io_stats().1 < 64, "no original bytes copied");
        let copy = dir.path().join("copy");
        view.export(0..view.len(), &copy, false).unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"\0\0\0\0mid\0\0\0\0");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 20 * 1024 * 1024 * 1024);
        assert_eq!(view.replace_source().unwrap(), path.canonicalize().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"\0\0\0\0mid\0\0\0\0");
        assert!(view.replace_source().is_err(), "the replaced file no longer matches the view");
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_source_follows_links_and_keeps_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("data.bin");
        std::fs::write(&target, b"abcd").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("link.bin");
        symlink(&target, &link).unwrap();
        let mut store = ByteStore::from_file(Arc::new(FileSource::open(&link).unwrap()));
        store.insert(4, b"ef").unwrap();
        std::fs::write(dir.path().join("other"), b"x").unwrap();
        store.view().replace_source().unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"abcdef");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o640);
        let changed = ByteStore::from_file(Arc::new(FileSource::open(&target).unwrap()));
        std::fs::write(&target, b"changed!").unwrap();
        assert!(changed.view().replace_source().is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"changed!");
        assert!(ByteStore::from_snapshot(Original::from_bytes(b"x").unwrap()).view().replace_source().is_err());
    }
}
