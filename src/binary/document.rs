#[cfg(test)]
use super::Layout;
#[cfg(test)]
use super::ValueIdentity;
use super::{RecordLayout, ResolvedLayout};
use crate::cache::{self, LoadProgress, SourceStamp};
use anyhow::{ensure, Context, Result};
use memmap2::Mmap;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

/// An immutable private snapshot: the source can be replaced without invalidating reads.
/// The OS pages this mapping on demand; binary size is not a RAM allocation limit.
pub struct Original {
    mapping: Option<Mmap>,
    _snapshot_file: File,
}

impl Original {
    pub fn open(path: &Path, progress: &LoadProgress) -> Result<Arc<Self>> {
        Self::open_cached(path, &cache::root(), progress)
    }

    pub fn open_cached(path: &Path, root: &Path, progress: &LoadProgress) -> Result<Arc<Self>> {
        let mut source = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let stamp = SourceStamp::read(path, &source.metadata()?)?;
        ensure!(
            stamp.len <= isize::MAX as u64,
            "file exceeds the process address-space limit of {} bytes",
            isize::MAX
        );
        progress.set_total(stamp.len);
        progress.check()?;
        let key = stamp.key("binary-snapshot", &())?;
        if let Some(directory) = cache::lookup(root, &key) {
            if let Ok(file) = File::open(directory.join("source.bin")) {
                if file.metadata()?.len() == stamp.len {
                    ensure!(
                        stamp == SourceStamp::read(path, &source.metadata()?)?,
                        "source changed while checking cache; retry"
                    );
                    let original = Self::map_private(file)?;
                    progress.hit();
                    return Ok(original);
                }
            }
        }
        let directory = cache::staging(root)?;
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("source.bin"))?;
        cache::copy_snapshot(&mut source, &mut file, progress)?;
        ensure!(
            stamp == SourceStamp::read(path, &source.metadata()?)?,
            "source changed while loading; retry after the writer has stopped"
        );
        cache::freeze(&file)?;
        let directory = cache::publish(root, &key, directory)?;
        drop(file);
        Self::map_private(File::open(directory.join("source.bin"))?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Arc<Self>> {
        Self::snapshot(bytes, bytes.len() as u64)
    }

    fn snapshot(reader: impl Read, len: u64) -> Result<Arc<Self>> {
        ensure!(len <= isize::MAX as u64, "file is too large for this process: maximum {} bytes; use a 64-bit build for large files", isize::MAX);
        let mut snapshot = tempfile::tempfile()
            .context("create private snapshot in the system temporary directory")?;
        let copied = std::io::copy(&mut reader.take(len + 1), &mut snapshot)
            .context("snapshot binary file (check free space in the temporary directory)")?;
        ensure!(copied == len, "source length changed while loading; retry");
        snapshot.flush()?;
        Self::map_private(snapshot)
    }

    fn map_private(snapshot: File) -> Result<Arc<Self>> {
        let mapping = if snapshot.metadata()?.len() == 0 {
            None
        } else {
            // SAFETY: private snapshots and published cache entries are immutable.
            // Cache publication replaces directory pointers, never mapped file contents.
            // The private file remains live as long as this mapping does.
            Some(
                unsafe { Mmap::map(&snapshot) }
                    .context("map private snapshot (address space limit)")?,
            )
        };
        Ok(Arc::new(Self {
            mapping,
            _snapshot_file: snapshot,
        }))
    }

    pub fn bytes(&self) -> &[u8] {
        self.mapping.as_deref().unwrap_or(&[])
    }
}

#[derive(Clone)]
pub struct ByteSnapshot {
    pub original: Arc<Original>,
    pub patches: BTreeMap<usize, u8>,
}

impl ByteSnapshot {
    pub fn byte(&self, offset: usize) -> u8 {
        self.patches
            .get(&offset)
            .copied()
            .unwrap_or(self.original.bytes()[offset])
    }

    pub fn field_bytes(
        &self,
        layout: &RecordLayout,
        record: usize,
        field: usize,
    ) -> Result<[u8; 8]> {
        let range = layout.field_range(record, field)?;
        ensure!(
            range.end <= self.original.bytes().len(),
            "record index is out of range"
        );
        let mut bytes = [0; 8];
        bytes[..range.len()].copy_from_slice(&self.original.bytes()[range.clone()]);
        for (&offset, &value) in self.patches.range(range.clone()) {
            bytes[offset - range.start] = value;
        }
        Ok(bytes)
    }

    pub fn value(&self, layout: &RecordLayout, record: usize, field: usize) -> Result<f64> {
        let bytes = self.field_bytes(layout, record, field)?;
        let f = &layout.fields[field];
        Ok(layout.stored_value(field, &bytes[..f.kind.size()]) * f.scale)
    }
}

#[cfg(test)]
#[derive(Clone)]
struct Edit {
    identity: ValueIdentity,
    offset: usize,
    before: Vec<u8>,
    after: Vec<u8>,
}

pub struct BinaryDocument {
    pub bytes: ByteSnapshot,
    pub layout: Arc<RecordLayout>,
    pub structure: Arc<ResolvedLayout>,
    pub section: usize,
    pub records: usize,
    #[cfg(test)]
    history: Vec<Edit>,
    #[cfg(test)]
    cursor: usize,
    saved_patches: BTreeMap<usize, u8>,
}

impl BinaryDocument {
    #[cfg(test)]
    pub fn new(original: Arc<Original>, layout: Layout) -> Result<Self> {
        let structure = layout.resolve(
            original.bytes().len(),
            |i| original.bytes()[i],
            &LoadProgress::default(),
        )?;
        Self::from_resolved(original, structure)
    }

    pub fn from_resolved(original: Arc<Original>, structure: ResolvedLayout) -> Result<Self> {
        let section = structure
            .sections
            .iter()
            .position(|s| !s.record.fields.is_empty())
            .unwrap_or(0);
        let layout = structure.sections[section].record.clone();
        Ok(Self {
            bytes: ByteSnapshot {
                original,
                patches: BTreeMap::new(),
            },
            records: layout.records,
            layout,
            section,
            structure: Arc::new(structure),
            #[cfg(test)]
            history: Vec::new(),
            #[cfg(test)]
            cursor: 0,
            saved_patches: BTreeMap::new(),
        })
    }

    /// Validate against working bytes before replacing any active layout/history.
    #[cfg(test)]
    pub fn apply_layout(&mut self, layout: Layout) -> Result<()> {
        let structure = layout.resolve(
            self.bytes.original.bytes().len(),
            |i| self.bytes.byte(i),
            &LoadProgress::default(),
        )?;
        self.apply_resolved(structure);
        Ok(())
    }

    pub fn apply_resolved(&mut self, structure: ResolvedLayout) {
        let section = structure
            .sections
            .iter()
            .position(|s| !s.record.fields.is_empty())
            .unwrap_or(0);
        self.structure = Arc::new(structure);
        self.select_section(section)
            .expect("validated nonempty sections");
        #[cfg(test)]
        {
            self.history.clear();
            self.cursor = 0;
        }
    }

    pub fn select_section(&mut self, section: usize) -> Result<()> {
        let entry = self
            .structure
            .sections
            .get(section)
            .context("section index is out of range")?;
        self.layout = entry.record.clone();
        self.section = section;
        self.records = self.layout.records;
        Ok(())
    }

    pub fn text(&self, record: usize, field: usize) -> Result<String> {
        ensure!(record < self.records, "record index is out of range");
        let bytes = self.bytes.field_bytes(&self.layout, record, field)?;
        Ok(self
            .layout
            .display_text(field, &bytes[..self.layout.fields[field].kind.size()]))
    }

    pub fn modified(&self, record: usize, field: usize) -> bool {
        self.layout
            .field_range(record, field)
            .is_ok_and(|range| self.bytes.patches.range(range).next().is_some())
    }

    pub fn is_dirty(&self) -> bool {
        self.bytes.patches != self.saved_patches
    }
    pub fn mark_saved(&mut self, snapshot: &ByteSnapshot) {
        self.saved_patches.clone_from(&snapshot.patches);
    }
    #[cfg(test)]
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }
    #[cfg(test)]
    pub fn can_redo(&self) -> bool {
        self.cursor < self.history.len()
    }

    #[cfg(test)]
    fn patch(&mut self, offset: usize, bytes: &[u8]) {
        for (i, &byte) in bytes.iter().enumerate() {
            if byte == self.bytes.original.bytes()[offset + i] {
                self.bytes.patches.remove(&(offset + i));
            } else {
                self.bytes.patches.insert(offset + i, byte);
            }
        }
    }

    #[cfg(test)]
    pub fn edit(&mut self, record: usize, field: usize, text: &str) -> Result<String> {
        ensure!(record < self.records, "record index is out of range");
        let range = self.layout.field_range(record, field)?;
        let after = self.layout.encode(field, text)?;
        let before: Vec<_> = range.clone().map(|i| self.bytes.byte(i)).collect();
        if before != after {
            self.history
                .try_reserve(1)
                .context("not enough memory for edit history")?;
            self.history.truncate(self.cursor);
            self.patch(range.start, &after);
            self.history.push(Edit {
                identity: self.layout.identity(record, field)?,
                offset: range.start,
                before,
                after,
            });
            self.cursor += 1;
        }
        self.text(record, field)
    }

    #[cfg(test)]
    pub fn undo(&mut self) -> Option<ValueIdentity> {
        if !self.can_undo() {
            return None;
        }
        self.cursor -= 1;
        let edit = self.history[self.cursor].clone();
        self.patch(edit.offset, &edit.before);
        self.select_section(edit.identity.section)
            .expect("history section remains valid");
        Some(edit.identity)
    }

    #[cfg(test)]
    pub fn redo(&mut self) -> Option<ValueIdentity> {
        if !self.can_redo() {
            return None;
        }
        let edit = self.history[self.cursor].clone();
        self.patch(edit.offset, &edit.after);
        self.cursor += 1;
        self.select_section(edit.identity.section)
            .expect("history section remains valid");
        Some(edit.identity)
    }
}

/// Streams exact bytes to a sibling temporary file, then atomically publishes it.
/// `overwrite` must only be true after explicit user confirmation.
pub fn write_copy(snapshot: &ByteSnapshot, path: &Path, overwrite: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut output =
        tempfile::NamedTempFile::new_in(parent).context("create export temporary file")?;
    const CHUNK: usize = 1024 * 1024;
    let mut buffer = vec![0u8; CHUNK];
    for (i, chunk) in snapshot.original.bytes().chunks(CHUNK).enumerate() {
        buffer[..chunk.len()].copy_from_slice(chunk);
        let start = i * CHUNK;
        for (&offset, &byte) in snapshot.patches.range(start..start + chunk.len()) {
            buffer[offset - start] = byte;
        }
        output
            .write_all(&buffer[..chunk.len()])
            .context("write edited copy")?;
    }
    output.as_file().sync_all().context("flush edited copy")?;
    if overwrite {
        output.persist(path).map_err(|e| e.error)?;
    } else {
        output.persist_noclobber(path).map_err(|e| e.error).context("destination already exists or cannot be created; choose another name or confirm overwrite")?;
    }
    Ok(())
}
