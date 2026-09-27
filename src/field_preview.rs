//! Small, asynchronous value previews for the definition builder.
//! A changing form replaces the pending request, never queues work per keystroke.
use crate::byte_store::ByteView;
use crate::structure::Endian;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};

const MAX_FIELDS: usize = 64;
const MAX_BYTES: usize = 64 * 1024;
const PREFIX_BYTES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewField {
    pub name: String,
    pub kind: String,
    pub offset: usize,
    pub length: usize,
    pub endian: Endian,
}

struct Request {
    id: u64,
    source: ByteView,
    fields: Vec<PreviewField>,
}
struct Job {
    id: u64,
    cancel: Arc<AtomicBool>,
    receiver: Receiver<Vec<String>>,
}

#[derive(Default)]
pub struct FieldPreview {
    next_id: u64,
    desired: Option<Request>,
    job: Option<Job>,
    completed: Option<u64>,
    values: Vec<String>,
}
impl FieldPreview {
    /// Clear displayed values immediately. The existing worker exits between reads.
    pub fn reset(&mut self) {
        self.desired = None;
        self.completed = None;
        self.values.clear();
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Call once per frame before drawing value cells. This performs no file reads.
    /// Only the latest field list/revision can populate those cells.
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        source: Option<ByteView>,
        mut fields: Vec<PreviewField>,
    ) {
        fields.truncate(MAX_FIELDS);
        match source {
            Some(source) if !fields.is_empty() => {
                let unchanged = self.desired.as_ref().is_some_and(|request| {
                    request.source.revision == source.revision && request.fields == fields
                });
                if !unchanged {
                    self.next_id = self.next_id.wrapping_add(1);
                    self.values = vec!["Reading…".into(); fields.len()];
                    self.desired = Some(Request {
                        id: self.next_id,
                        source,
                        fields,
                    });
                    self.completed = None;
                    if let Some(job) = &self.job {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
            _ => self.reset(),
        }
        self.poll();
        if self.job.is_none() {
            if let Some(request) = &self.desired {
                if self.completed != Some(request.id) {
                    let id = request.id;
                    let source = request.source.clone();
                    let fields = request.fields.clone();
                    let cancel = Arc::new(AtomicBool::new(false));
                    let worker_cancel = cancel.clone();
                    let (sender, receiver) = mpsc::sync_channel(1);
                    let ctx = ctx.clone();
                    std::thread::spawn(move || {
                        let values = preview(&source, &fields, &worker_cancel);
                        let _ = sender.send(values);
                        ctx.request_repaint();
                    });
                    self.job = Some(Job {
                        id,
                        cancel,
                        receiver,
                    });
                }
            }
        }
    }

    pub fn value(&self, index: usize) -> &str {
        if index >= MAX_FIELDS {
            return "Preview limited to 64 fields";
        }
        self.values.get(index).map(String::as_str).unwrap_or("—")
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let result = match job.receiver.try_recv() {
            Ok(values) => Some(values),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        let id = job.id;
        self.job = None;
        if let Some(request) = self.desired.as_ref().filter(|r| r.id == id) {
            self.values = result.unwrap_or_else(|| {
                vec!["Error: preview worker stopped".into(); request.fields.len()]
            });
            self.completed = Some(id);
        }
    }
}
impl Drop for FieldPreview {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn preview(source: &ByteView, fields: &[PreviewField], cancel: &AtomicBool) -> Vec<String> {
    let fields = &fields[..fields.len().min(MAX_FIELDS)];
    if cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    if let Err(error) = source.validate() {
        return vec![format!("Error: {error:#}"); fields.len()];
    }
    let mut remaining = MAX_BYTES;
    let mut values = Vec::with_capacity(fields.len());
    for field in fields {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        values.push(
            preview_one(source, field, &mut remaining)
                .unwrap_or_else(|error| format!("Error: {error:#}")),
        );
    }
    if !cancel.load(Ordering::Relaxed) {
        if let Err(error) = source.validate() {
            return vec![format!("Error: {error:#}"); fields.len()];
        }
    }
    values
}

fn preview_one(
    source: &ByteView,
    field: &PreviewField,
    remaining: &mut usize,
) -> anyhow::Result<String> {
    use anyhow::{ensure, Context};
    let end = field
        .offset
        .checked_add(field.length)
        .context("byte range overflow")?;
    ensure!(end <= source.len(), "field extends beyond the file");
    let width = match field.kind.as_str() {
        "u8" | "i8" => Some(1),
        "u16" | "i16" => Some(2),
        "u32" | "i32" | "f32" => Some(4),
        "u64" | "i64" | "f64" => Some(8),
        "bytes" | "string" | "utf8" | "cstring" => None,
        _ => return Ok("Open structure to preview fields".into()),
    };
    let wanted = if let Some(width) = width {
        ensure!(field.length >= width, "{} needs {width} bytes", field.kind);
        width
    } else {
        field.length.min(PREFIX_BYTES)
    };
    ensure!(wanted <= *remaining, "preview read limit reached");
    *remaining -= wanted;
    let bytes = source.read(field.offset, wanted)?;
    ensure!(bytes.len() == wanted, "source ended while reading field");
    if width.is_some() {
        let mut padded = [0u8; 8];
        let raw = match field.endian {
            Endian::Little => {
                padded[..wanted].copy_from_slice(&bytes);
                u64::from_le_bytes(padded)
            }
            Endian::Big => {
                padded[8 - wanted..].copy_from_slice(&bytes);
                u64::from_be_bytes(padded)
            }
        };
        let value = match field.kind.as_str() {
            "u8" | "u16" | "u32" | "u64" => raw.to_string(),
            "i8" => (raw as i8).to_string(),
            "i16" => (raw as i16).to_string(),
            "i32" => (raw as i32).to_string(),
            "i64" => (raw as i64).to_string(),
            "f32" => (f32::from_bits(raw as u32) as f64).to_string(),
            "f64" => f64::from_bits(raw).to_string(),
            _ => unreachable!(),
        };
        return Ok(if field.length > wanted {
            format!("{value} (first item)")
        } else {
            value
        });
    }
    let (value, terminated) = match field.kind.as_str() {
        "bytes" => (
            bytes
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" "),
            false,
        ),
        _ => {
            let terminator = (field.kind == "cstring")
                .then(|| bytes.iter().position(|b| *b == 0))
                .flatten();
            let text = String::from_utf8_lossy(&bytes[..terminator.unwrap_or(bytes.len())]);
            (format!("{text:?}"), terminator.is_some())
        }
    };
    Ok(if field.length > bytes.len() && !terminated {
        format!("{value} … ({})", crate::format::exact_bytes(field.length as u64))
    } else if value.is_empty() {
        "(empty)".into()
    } else {
        value
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{binary::Original, byte_store::ByteStore};
    use std::time::{Duration, Instant};

    fn field(kind: &str, offset: usize, length: usize, endian: Endian) -> PreviewField {
        PreviewField {
            name: "value".into(),
            kind: kind.into(),
            offset,
            length,
            endian,
        }
    }
    fn source(bytes: &[u8]) -> ByteStore {
        ByteStore::from_snapshot(Original::from_bytes(bytes).unwrap())
    }
    fn settle(preview: &mut FieldPreview, source: ByteView, fields: Vec<PreviewField>) {
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            preview.update(&ctx, Some(source.clone()), fields.clone());
            if preview.job.is_none() {
                return;
            }
            assert!(Instant::now() < deadline, "preview did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn integers_remain_exact_and_endian_matches_the_decoder() {
        let mut bytes = u64::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(&i64::MIN.to_be_bytes());
        bytes.extend_from_slice(&(-12_i16).to_le_bytes());
        bytes.extend_from_slice(&(1.25_f32).to_be_bytes());
        let fields = vec![
            field("u64", 0, 8, Endian::Little),
            field("i64", 8, 8, Endian::Big),
            field("i16", 16, 2, Endian::Little),
            field("f32", 18, 4, Endian::Big),
        ];
        let values = preview(&source(&bytes).view(), &fields, &AtomicBool::new(false));
        assert_eq!(
            values,
            [
                "18446744073709551615",
                "-9223372036854775808",
                "-12",
                "1.25"
            ]
        );
    }

    #[test]
    fn edited_bytes_clear_old_values_before_new_preview_finishes() {
        let mut store = source(&[1, 2]);
        let fields = vec![field("u8", 0, 1, Endian::Little)];
        let mut preview = FieldPreview::default();
        settle(&mut preview, store.view(), fields.clone());
        assert_eq!(preview.value(0), "1");
        store.replace(0, &[9]).unwrap();
        preview.update(
            &egui::Context::default(),
            Some(store.view()),
            fields.clone(),
        );
        assert_eq!(preview.value(0), "Reading…");
        settle(&mut preview, store.view(), fields);
        assert_eq!(preview.value(0), "9");
    }

    #[test]
    fn rapid_form_changes_coalesce_and_discard_the_old_worker() {
        let store = source(&[10, 20, 30]);
        let mut preview = FieldPreview::default();
        let ctx = egui::Context::default();
        preview.update(
            &ctx,
            Some(store.view()),
            vec![field("u8", 0, 1, Endian::Little)],
        );
        let old = preview.job.take().unwrap();
        old.cancel.store(true, Ordering::Relaxed);
        // Hold a deterministic old completion until the newer request is installed.
        let (sender, receiver) = mpsc::sync_channel(1);
        preview.job = Some(Job {
            id: old.id,
            cancel: old.cancel.clone(),
            receiver,
        });
        for offset in [1, 2] {
            preview.update(
                &ctx,
                Some(store.view()),
                vec![field("u8", offset, 1, Endian::Little)],
            );
            assert_eq!(preview.value(0), "Reading…");
            assert_eq!(preview.job.as_ref().unwrap().id, old.id);
        }
        sender.send(vec!["10".into()]).unwrap();
        let latest = vec![field("u8", 2, 1, Endian::Little)];
        settle(&mut preview, store.view(), latest);
        assert_eq!(preview.value(0), "30");
        preview.reset();
        assert_eq!(preview.value(0), "—");
    }

    #[test]
    fn changed_disk_sources_report_an_error_instead_of_fresh_looking_values() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), [5, 6]).unwrap();
        let original = crate::file_source::FileSource::open(file.path()).unwrap();
        let store = ByteStore::from_file(Arc::new(original));
        std::fs::write(file.path(), [9, 10, 11]).unwrap();
        let mut preview = FieldPreview::default();
        settle(
            &mut preview,
            store.view(),
            vec![field("u8", 0, 1, Endian::Little)],
        );
        assert!(preview.value(0).contains("Source file changed"));
        preview.update(&egui::Context::default(), None, vec![]);
        assert_eq!(preview.value(0), "—");
    }

    #[test]
    fn large_raw_fields_read_only_a_prefix_and_ranges_report_errors() {
        let store = source(&vec![0x41; 100_000]);
        let mut remaining = MAX_BYTES;
        let value = preview_one(
            &store.view(),
            &field("string", 0, 100_000, Endian::Little),
            &mut remaining,
        )
        .unwrap();
        assert_eq!(remaining, MAX_BYTES - 32);
        assert!(value.contains("100,000 bytes"));
        assert!(value.starts_with(&format!("\"{}\"", "A".repeat(32))));
        let invalid = vec![
            field("u64", usize::MAX, 8, Endian::Little),
            field("u16", 99_999, 2, Endian::Little),
        ];
        let values = preview(&store.view(), &invalid, &AtomicBool::new(false));
        assert!(values[0].contains("overflow"));
        assert!(values[1].contains("beyond the file"));
        let many = vec![field("bytes", 0, 100_000, Endian::Little); 100];
        assert_eq!(
            preview(&store.view(), &many, &AtomicBool::new(false)).len(),
            MAX_FIELDS
        );
    }
}
