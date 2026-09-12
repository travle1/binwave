//! Bounded min/max hierarchy over source record ranges. Queries skip invisible
//! ranges and answer fully covered ranges from summaries without decoding again.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_LEAVES: usize = 32768;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub record: usize,
    pub byte_offset: usize,
    pub point: [f64; 2],
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    count: usize,
    x: [f64; 2],
    low: Option<Sample>,
    high: Option<Sample>,
}

impl Summary {
    fn add(&mut self, sample: Sample) {
        if self.count == 0 {
            self.x = [sample.point[0]; 2];
        }
        self.count += 1;
        self.x[0] = self.x[0].min(sample.point[0]);
        self.x[1] = self.x[1].max(sample.point[0]);
        if self.low.is_none_or(|s| sample.point[1] < s.point[1]) {
            self.low = Some(sample);
        }
        if self.high.is_none_or(|s| sample.point[1] > s.point[1]) {
            self.high = Some(sample);
        }
    }
    fn merge(a: Self, b: Self) -> Self {
        if a.count == 0 {
            return b;
        }
        if b.count == 0 {
            return a;
        }
        Self {
            count: a.count + b.count,
            x: [a.x[0].min(b.x[0]), a.x[1].max(b.x[1])],
            low: if a.low.unwrap().point[1] <= b.low.unwrap().point[1] {
                a.low
            } else {
                b.low
            },
            high: if a.high.unwrap().point[1] >= b.high.unwrap().point[1] {
                a.high
            } else {
                b.high
            },
        }
    }
    fn intersects(self, [min, max]: [f64; 2]) -> bool {
        self.count > 0 && self.x[1] >= min && self.x[0] <= max
    }
    fn contained(self, [min, max]: [f64; 2]) -> bool {
        self.count > 0 && self.x[0] >= min && self.x[1] <= max
    }
}

#[derive(Clone, Debug)]
pub struct PlotData {
    pub samples: Vec<Sample>,
    pub full_x: [f64; 2],
    pub full_y: [f64; 2],
    pub finite_records: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexBuilder {
    leaf_size: usize,
    records: usize,
    leaves: Vec<Summary>,
}

impl IndexBuilder {
    pub fn new(record_upper_bound: usize) -> Self {
        Self::with_max_leaves(record_upper_bound, MAX_LEAVES)
    }

    pub fn with_max_leaves(record_upper_bound: usize, max_leaves: usize) -> Self {
        Self {
            leaf_size: record_upper_bound
                .div_ceil(max_leaves.clamp(1, MAX_LEAVES))
                .max(256)
                .next_power_of_two(),
            records: 0,
            leaves: Vec::new(),
        }
    }
    pub fn push(&mut self, sample: Option<Sample>) {
        if self.records % self.leaf_size == 0 {
            self.leaves.push(Summary::default());
        }
        if let Some(sample) = sample {
            self.leaves.last_mut().unwrap().add(sample);
        }
        self.records += 1;
    }
    pub fn finish(&self) -> Result<SummaryIndex> {
        ensure!(
            self.leaf_size >= 256 && self.leaf_size.is_power_of_two(),
            "invalid cached index block size"
        );
        ensure!(
            self.leaves.len() <= MAX_LEAVES
                && self.leaves.len() == self.records.div_ceil(self.leaf_size),
            "invalid cached index size"
        );
        let base = self.leaves.len().max(1).next_power_of_two();
        ensure!(
            self.records <= isize::MAX as usize && base.checked_mul(self.leaf_size).is_some(),
            "cached index range overflows"
        );
        for (i, leaf) in self.leaves.iter().enumerate() {
            let start = i * self.leaf_size;
            let end = (start + self.leaf_size).min(self.records);
            ensure!(leaf.count <= end - start, "invalid cached sample count");
            ensure!(
                (leaf.count == 0) == leaf.low.is_none()
                    && leaf.low.is_some() == leaf.high.is_some(),
                "invalid cached extrema"
            );
            if let (Some(low), Some(high)) = (leaf.low, leaf.high) {
                ensure!(low.point[1] <= high.point[1], "invalid cached Y bounds");
                ensure!(
                    leaf.x.iter().all(|x| x.is_finite()) && leaf.x[0] <= leaf.x[1],
                    "invalid cached X bounds"
                );
                for sample in [low, high] {
                    ensure!(
                        (start..end).contains(&sample.record)
                            && sample.point.iter().all(|v| v.is_finite())
                            && (leaf.x[0]..=leaf.x[1]).contains(&sample.point[0]),
                        "invalid cached sample identity"
                    );
                }
            }
        }
        let mut nodes = vec![Summary::default(); base * 2];
        nodes[base..base + self.leaves.len()].copy_from_slice(&self.leaves);
        for node in (1..base).rev() {
            nodes[node] = Summary::merge(nodes[node * 2], nodes[node * 2 + 1]);
        }
        Ok(SummaryIndex {
            leaf_size: self.leaf_size,
            records: self.records,
            nodes,
            base,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn sample(record: usize) -> Sample {
        Sample {
            record,
            byte_offset: 16 + record * 12,
            point: [
                record as f64,
                if record == 12345 {
                    1e6
                } else {
                    (record % 97) as f64
                },
            ],
        }
    }

    #[test]
    fn full_and_aligned_views_use_summaries_without_source_reads() {
        let cancelled = AtomicBool::new(false);
        let index = SummaryIndex::build(65536, &|r| Ok(Some(sample(r))), &cancelled).unwrap();
        let reads = Cell::new(0);
        let read = |r| {
            reads.set(reads.get() + 1);
            Ok(Some(sample(r)))
        };
        let full = index.query(None, 512, &read, &cancelled).unwrap();
        assert!(full.samples.len() <= 512);
        assert!(full
            .samples
            .iter()
            .any(|s| s.record == 12345 && s.point[1] == 1e6));
        assert_eq!(reads.get(), 0, "full view must not rescan the original");
        let zoom = index
            .query(Some([8192., 16383.]), 128, &read, &cancelled)
            .unwrap();
        assert!(zoom.samples.iter().any(|s| s.record == 12345));
        assert_eq!(reads.get(), 0, "aligned zoom should use cached summaries");
        let exact = index
            .query(Some([12340., 12350.]), 100, &read, &cancelled)
            .unwrap();
        assert_eq!(
            exact.samples.iter().map(|s| s.record).collect::<Vec<_>>(),
            (12340..=12350).collect::<Vec<_>>()
        );
        assert!(
            reads.get() <= 512,
            "small zoom must read only boundary blocks"
        );
    }

    #[test]
    fn unsorted_duplicate_x_queries_preserve_identity_and_visible_extrema() {
        let read = |record| {
            let x = (record * 97 % 317) as f64;
            Ok((record % 31 != 0).then_some(Sample {
                record,
                byte_offset: record * 8,
                point: [x, (record * 197 % 1009) as f64],
            }))
        };
        let cancel = AtomicBool::new(false);
        let index = SummaryIndex::build(4096, &read, &cancel).unwrap();
        for target in [2, 3, 4, 8, 32, 128] {
            let data = index
                .query(Some([100., 150.]), target, &read, &cancel)
                .unwrap();
            assert!(
                data.samples.len() <= target,
                "target {target}, got {}",
                data.samples.len()
            );
            let visible: Vec<_> = (0..4096)
                .filter_map(|r| read(r).unwrap())
                .filter(|s| (100. ..=150.).contains(&s.point[0]))
                .collect();
            let low = visible
                .iter()
                .map(|s| s.point[1])
                .fold(f64::INFINITY, f64::min);
            let high = visible
                .iter()
                .map(|s| s.point[1])
                .fold(f64::NEG_INFINITY, f64::max);
            assert!(data.samples.iter().any(|s| s.point[1] == low));
            assert!(data.samples.iter().any(|s| s.point[1] == high));
            for sample in data.samples {
                assert_eq!(sample, read(sample.record).unwrap().unwrap());
            }
        }
    }

    #[test]
    fn editing_rebuilds_only_one_leaf_and_updates_bounds() {
        let cancel = AtomicBool::new(false);
        let mut index = SummaryIndex::build(65536, &|r| Ok(Some(sample(r))), &cancel).unwrap();
        let reads = Cell::new(0);
        let read = |r| {
            reads.set(reads.get() + 1);
            let mut point = sample(r);
            if r == 12345 {
                point.point[1] = -1e6;
            }
            Ok(Some(point))
        };
        index.update(12345, &read, &cancel).unwrap();
        assert_eq!(reads.get(), 256);
        let data = index.query(None, 512, &read, &cancel).unwrap();
        assert_eq!(data.full_y, [-1e6, 96.]);
        assert!(data
            .samples
            .iter()
            .any(|s| s.record == 12345 && s.point[1] == -1e6));
        assert_eq!(reads.get(), 256);
    }
}

#[derive(Clone, Debug)]
pub struct SummaryIndex {
    leaf_size: usize,
    records: usize,
    nodes: Vec<Summary>,
    base: usize,
}

impl SummaryIndex {
    pub fn build(
        records: usize,
        read: &impl Fn(usize) -> Result<Option<Sample>>,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut builder = IndexBuilder::new(records);
        for record in 0..records {
            if record % 4096 == 0 {
                ensure!(!cancel.load(Ordering::Relaxed), "plot cancelled");
            }
            builder.push(read(record)?);
        }
        builder.finish()
    }

    /// Rebuild only the leaf containing an edited record, then its ancestors.
    pub fn update(
        &mut self,
        record: usize,
        read: &impl Fn(usize) -> Result<Option<Sample>>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        ensure!(record < self.records, "index record out of range");
        let leaf = record / self.leaf_size;
        let mut summary = Summary::default();
        for record in leaf * self.leaf_size..((leaf + 1) * self.leaf_size).min(self.records) {
            if record % 4096 == 0 {
                ensure!(!cancel.load(Ordering::Relaxed), "plot cancelled");
            }
            if let Some(sample) = read(record)? {
                summary.add(sample);
            }
        }
        let mut node = self.base + leaf;
        self.nodes[node] = summary;
        while node > 1 {
            node /= 2;
            self.nodes[node] = Summary::merge(self.nodes[node * 2], self.nodes[node * 2 + 1]);
        }
        Ok(())
    }

    pub fn leaf_for_record(&self, record: usize) -> usize {
        record / self.leaf_size
    }
    pub fn records(&self) -> usize {
        self.records
    }

    pub fn query(
        &self,
        view: Option<[f64; 2]>,
        target: usize,
        read: &impl Fn(usize) -> Result<Option<Sample>>,
        cancel: &AtomicBool,
    ) -> Result<PlotData> {
        ensure!(target >= 2, "plot target must be at least two samples");
        let root = self.nodes[1];
        let full_x = if root.count > 0 { root.x } else { [0., 1.] };
        let full_y = if let (Some(low), Some(high)) = (root.low, root.high) {
            [low.point[1], high.point[1]]
        } else {
            [0., 1.]
        };
        let view = view.unwrap_or(full_x);
        ensure!(
            view.iter().all(|v| v.is_finite()) && view[0] <= view[1],
            "invalid plot view"
        );
        let mut first = self.records;
        let mut last = 0;
        for (i, leaf) in self.nodes[self.base..].iter().enumerate() {
            if leaf.intersects(view) {
                first = first.min(i * self.leaf_size);
                last = ((i + 1) * self.leaf_size).min(self.records);
            }
        }
        let mut output = PlotData {
            samples: Vec::new(),
            full_x,
            full_y,
            finite_records: root.count,
        };
        if first >= last {
            return Ok(output);
        }
        // Power-of-two, leaf-aligned buckets never require decoding just to split
        // a cached leaf. Only partially visible leaves need source reads.
        let bucket_count = (target / 2).saturating_sub(1).max(1);
        let width = (last - first)
            .div_ceil(bucket_count)
            .div_ceil(self.leaf_size)
            .max(1)
            .next_power_of_two()
            * self.leaf_size;
        let base_bucket = first / width;
        let mut buckets = vec![Summary::default(); last.div_ceil(width) - base_bucket];
        let mut visible = 0;
        let mut stack = vec![(1, 0, self.base * self.leaf_size)];
        while let Some((node, start, end)) = stack.pop() {
            ensure!(!cancel.load(Ordering::Relaxed), "plot cancelled");
            let summary = self.nodes[node];
            if !summary.intersects(view) {
                continue;
            }
            let record_end = end.min(self.records);
            if summary.contained(view) && start / width == (record_end - 1) / width {
                let slot = &mut buckets[start / width - base_bucket];
                *slot = Summary::merge(*slot, summary);
                visible += summary.count;
            } else if node < self.base {
                let middle = start + (end - start) / 2;
                stack.push((node * 2 + 1, middle, end));
                stack.push((node * 2, start, middle));
            } else {
                for record in start..record_end {
                    if record % 4096 == 0 {
                        ensure!(!cancel.load(Ordering::Relaxed), "plot cancelled");
                    }
                    if let Some(sample) = read(record)? {
                        if (view[0]..=view[1]).contains(&sample.point[0]) {
                            buckets[record / width - base_bucket].add(sample);
                            visible += 1;
                        }
                    }
                }
            }
        }
        if visible <= target {
            // Preserve every point when zoomed in, including duplicate X values.
            for (i, leaf) in self.nodes[self.base..].iter().enumerate() {
                if !leaf.intersects(view) {
                    continue;
                }
                for record in i * self.leaf_size..((i + 1) * self.leaf_size).min(self.records) {
                    if record % 4096 == 0 {
                        ensure!(!cancel.load(Ordering::Relaxed), "plot cancelled");
                    }
                    if let Some(sample) = read(record)? {
                        if (view[0]..=view[1]).contains(&sample.point[0]) {
                            output.samples.push(sample);
                        }
                    }
                }
            }
        } else {
            for bucket in buckets {
                if let (Some(low), Some(high)) = (bucket.low, bucket.high) {
                    output.samples.push(low);
                    if low.record != high.record {
                        output.samples.push(high);
                    }
                }
            }
            output.samples.sort_unstable_by_key(|s| s.record);
            if target == 2 && output.samples.len() > 2 {
                let mut summary = Summary::default();
                for sample in &output.samples {
                    summary.add(*sample);
                }
                output.samples = vec![summary.low.unwrap(), summary.high.unwrap()];
                output.samples.sort_unstable_by_key(|s| s.record);
                output.samples.dedup_by_key(|s| s.record);
            }
        }
        Ok(output)
    }
}
