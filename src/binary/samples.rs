use super::{ByteSnapshot, Original, RecordLayout, ValueIdentity};
pub use crate::plot_index::Sample;
use crate::plot_index::SummaryIndex;
use anyhow::{ensure, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// A rendered series retains its section and both field identities even when
/// duplicate X coordinates or downsampling remove other records from the view.
#[derive(Clone, Debug)]
pub struct PlotData {
    data: crate::plot_index::PlotData,
    section: usize,
    x: Option<usize>,
    y: usize,
}
impl std::ops::Deref for PlotData {
    type Target = crate::plot_index::PlotData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}
impl std::ops::DerefMut for PlotData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}
impl PlotData {
    pub fn identities(
        &self,
        sample: &Sample,
        layout: &RecordLayout,
    ) -> Result<(Option<ValueIdentity>, ValueIdentity)> {
        ensure!(
            self.section == layout.section,
            "plot belongs to a different section"
        );
        ensure!(
            sample.byte_offset == layout.record_offset(sample.record)?,
            "plot source offset no longer matches the layout"
        );
        Ok((
            self.x
                .map(|x| layout.identity(sample.record, x))
                .transpose()?,
            layout.identity(sample.record, self.y)?,
        ))
    }
}

impl Sample {
    pub fn read(
        bytes: &ByteSnapshot,
        layout: &RecordLayout,
        record: usize,
        x: Option<usize>,
        y: usize,
    ) -> Result<Option<Self>> {
        let x = match x {
            Some(f) => bytes.value(layout, record, f)?,
            None => record as f64,
        };
        let y = bytes.value(layout, record, y)?;
        Ok((x.is_finite() && y.is_finite()).then_some(Self {
            record,
            byte_offset: layout.record_offset(record)?,
            point: [x, y],
        }))
    }
}

type ViewKey = (Option<[u64; 2]>, usize);
struct CachedSeries {
    original: Arc<Original>,
    layout: Arc<RecordLayout>,
    x: Option<usize>,
    y: usize,
    patches: BTreeMap<usize, u8>,
    index: SummaryIndex,
    views: VecDeque<(ViewKey, PlotData)>,
}

#[derive(Default)]
pub struct PlotCache {
    entries: VecDeque<CachedSeries>,
}

impl PlotCache {
    pub fn query(
        &mut self,
        bytes: &ByteSnapshot,
        layout: &Arc<RecordLayout>,
        x: Option<usize>,
        y: usize,
        view: Option<[f64; 2]>,
        target: usize,
        cancel: &AtomicBool,
    ) -> Result<PlotData> {
        ensure!(!cancel.load(Ordering::Relaxed), "plot cancelled");
        ensure!(
            y < layout.fields.len() && x.is_none_or(|f| f < layout.fields.len()),
            "plot field is out of range"
        );
        let position = self.entries.iter().position(|e| {
            Arc::ptr_eq(&e.original, &bytes.original)
                && Arc::ptr_eq(&e.layout, layout)
                && e.x == x
                && e.y == y
        });
        let read = |record| Sample::read(bytes, layout, record, x, y);
        if let Some(position) = position {
            let entry = self.entries.remove(position).unwrap();
            self.entries.push_back(entry);
        } else {
            let index = SummaryIndex::build(layout.records, &read, cancel)?;
            if self.entries.len() == 4 {
                self.entries.pop_front();
            }
            self.entries.push_back(CachedSeries {
                original: bytes.original.clone(),
                layout: layout.clone(),
                x,
                y,
                patches: bytes.patches.clone(),
                index,
                views: VecDeque::new(),
            });
        }
        let entry = self.entries.back_mut().unwrap();
        if entry.patches != bytes.patches {
            let mut changed_leaves = BTreeSet::new();
            for &offset in entry.patches.keys().chain(bytes.patches.keys()) {
                if entry.patches.get(&offset) == bytes.patches.get(&offset)
                    || offset < layout.data_offset
                    || offset >= layout.data_offset + layout.records * layout.record_size
                {
                    continue;
                }
                let relative = (offset - layout.data_offset) % layout.record_size;
                if [Some(y), x].into_iter().flatten().any(|f| {
                    (layout.fields[f].offset
                        ..layout.fields[f].offset + layout.fields[f].kind.size())
                        .contains(&relative)
                }) {
                    let record = (offset - layout.data_offset) / layout.record_size;
                    changed_leaves.insert((entry.index.leaf_for_record(record), record));
                }
            }
            if !changed_leaves.is_empty() {
                entry.views.clear();
            }
            let mut previous_leaf = None;
            for (leaf, record) in changed_leaves {
                if previous_leaf != Some(leaf) {
                    entry.index.update(record, &read, cancel)?;
                    previous_leaf = Some(leaf);
                }
            }
            entry.patches.clone_from(&bytes.patches);
        }
        let key = (view.map(|v| v.map(f64::to_bits)), target);
        if let Some((_, result)) = entry.views.iter().find(|(k, _)| *k == key) {
            return Ok(result.clone());
        }
        let result = PlotData {
            data: entry.index.query(view, target, &read, cancel)?,
            section: layout.section,
            x,
            y,
        };
        if entry.views.len() == 8 {
            entry.views.pop_front();
        }
        entry.views.push_back((key, result.clone()));
        Ok(result)
    }
}

#[cfg(test)]
pub fn build_plot(
    bytes: &ByteSnapshot,
    layout: &RecordLayout,
    x: Option<usize>,
    y: usize,
    view: Option<[f64; 2]>,
    target: usize,
    cancel: &AtomicBool,
) -> Result<PlotData> {
    let read = |record| Sample::read(bytes, layout, record, x, y);
    let data =
        SummaryIndex::build(layout.records, &read, cancel)?.query(view, target, &read, cancel)?;
    Ok(PlotData {
        data,
        section: layout.section,
        x,
        y,
    })
}
