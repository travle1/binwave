//! Column-wise min/max over raw records, for plotting many channels at once.
//!
//! A block of records is a samples × channels matrix: each row holds `channels`
//! values of one type at an even spacing. Reducing it to per-channel min/max
//! works on the raw values in their own type, row by row, so the compiler can
//! turn the inner loop into SIMD instructions. Nothing is converted to `f64`
//! until the few values that are drawn.
use std::ops::Range;

/// A value type the reduction handles, read straight from bytes.
pub trait Sample: Copy + PartialOrd + Send + Sync + std::fmt::Debug + 'static {
    const WIDTH: usize;
    /// Starting points: every real value replaces them. For floats these are
    /// infinities, so NaN (which compares false) never becomes a min or max.
    const LOWEST: Self;
    const HIGHEST: Self;
    fn read_le(bytes: &[u8]) -> Self;
    fn read_be(bytes: &[u8]) -> Self;
    fn to_f64(self) -> f64;
}

macro_rules! sample {
    ($ty:ty, $lowest:expr, $highest:expr) => {
        impl Sample for $ty {
            const WIDTH: usize = std::mem::size_of::<$ty>();
            const LOWEST: Self = $lowest;
            const HIGHEST: Self = $highest;
            #[inline(always)]
            fn read_le(bytes: &[u8]) -> Self {
                <$ty>::from_le_bytes(bytes.try_into().expect("one value's bytes"))
            }
            #[inline(always)]
            fn read_be(bytes: &[u8]) -> Self {
                <$ty>::from_be_bytes(bytes.try_into().expect("one value's bytes"))
            }
            #[inline(always)]
            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    };
}
sample!(u8, u8::MIN, u8::MAX);
sample!(i8, i8::MIN, i8::MAX);
sample!(u16, u16::MIN, u16::MAX);
sample!(i16, i16::MIN, i16::MAX);
sample!(u32, u32::MIN, u32::MAX);
sample!(i32, i32::MIN, i32::MAX);
sample!(u64, u64::MIN, u64::MAX);
sample!(i64, i64::MIN, i64::MAX);
sample!(f32, f32::NEG_INFINITY, f32::INFINITY);
sample!(f64, f64::NEG_INFINITY, f64::INFINITY);

/// Where a block's channels sit: `channels` values starting `first` bytes into
/// each row, one after another, with rows `row_stride` bytes apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Matrix {
    pub first: usize,
    pub channels: usize,
    pub row_stride: usize,
    pub little_endian: bool,
}

/// Running per-channel min/max, in the channels' own type.
#[derive(Clone, Debug, PartialEq)]
pub struct MinMax<T: Sample> {
    pub min: Vec<T>,
    pub max: Vec<T>,
}

impl<T: Sample> MinMax<T> {
    pub fn new(channels: usize) -> Self {
        Self { min: vec![T::HIGHEST; channels], max: vec![T::LOWEST; channels] }
    }

    /// Widen by another result over the same channels.
    #[cfg(test)]
    pub fn merge(&mut self, other: &Self) {
        for (lo, &other) in self.min.iter_mut().zip(&other.min) {
            if other < *lo { *lo = other; }
        }
        for (hi, &other) in self.max.iter_mut().zip(&other.max) {
            if other > *hi { *hi = other; }
        }
    }

    /// Fold `rows` rows of `bytes` (row 0 at byte 0) into the running min/max.
    pub fn reduce(&mut self, bytes: &[u8], matrix: &Matrix, rows: Range<usize>) {
        assert_eq!(self.min.len(), matrix.channels, "one slot per channel");
        let width = matrix.channels * T::WIDTH;
        assert!(rows.is_empty() || (rows.end - 1) * matrix.row_stride + matrix.first + width <= bytes.len(),
            "rows extend past the bytes");
        if matrix.little_endian {
            reduce_rows::<T, true>(bytes, matrix, rows, &mut self.min, &mut self.max);
        } else {
            reduce_rows::<T, false>(bytes, matrix, rows, &mut self.min, &mut self.max);
        }
    }
}

/// The hot loop, one copy per type and byte order so the read inlines and the
/// per-channel compares vectorise.
#[inline(never)]
// `as_chunks` needs a literal size; the width comes from the generic type.
#[allow(clippy::chunks_exact_to_as_chunks)]
fn reduce_rows<T: Sample, const LITTLE: bool>(bytes: &[u8], matrix: &Matrix, rows: Range<usize>,
    min: &mut [T], max: &mut [T]) {
    let width = matrix.channels * T::WIDTH;
    for row in rows {
        let start = row * matrix.row_stride + matrix.first;
        let values = &bytes[start..start + width];
        for ((lo, hi), raw) in min.iter_mut().zip(max.iter_mut()).zip(values.chunks_exact(T::WIDTH)) {
            let value = if LITTLE { T::read_le(raw) } else { T::read_be(raw) };
            if value < *lo { *lo = value; }
            if value > *hi { *hi = value; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator, so tests need no extra crate.
    fn noise(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    fn check<T: Sample>(little: bool, make: impl Fn(u64) -> T, bytes_of: impl Fn(T) -> Vec<u8>) {
        let (channels, rows, first, pad) = (37, 513, 3, 5);
        let row_stride = first + channels * T::WIDTH + pad;
        let mut seed = 0x9E37_79B9_7F4A_7C15;
        let mut bytes = vec![0xEE; rows * row_stride];
        let mut values = vec![Vec::new(); channels];
        for row in 0..rows {
            for (channel, column) in values.iter_mut().enumerate() {
                let value = make(noise(&mut seed));
                let at = row * row_stride + first + channel * T::WIDTH;
                bytes[at..at + T::WIDTH].copy_from_slice(&bytes_of(value));
                column.push(value);
            }
        }
        let matrix = Matrix { first, channels, row_stride, little_endian: little };
        for range in [0..rows, 17..18, 100..400, 0..0] {
            let mut result = MinMax::<T>::new(channels);
            result.reduce(&bytes, &matrix, range.clone());
            for (channel, column) in values.iter().enumerate() {
                let seen = &column[range.clone()];
                let lowest = seen.iter().copied().filter(|v| v == v)
                    .fold(T::HIGHEST, |a, b| if b < a { b } else { a });
                let highest = seen.iter().copied().filter(|v| v == v)
                    .fold(T::LOWEST, |a, b| if b > a { b } else { a });
                assert_eq!((result.min[channel], result.max[channel]), (lowest, highest),
                    "{} channel {channel} rows {range:?}", std::any::type_name::<T>());
            }
        }
        // Reducing halves separately and merging gives the same result.
        let (mut left, mut right, mut whole) = (MinMax::<T>::new(channels), MinMax::<T>::new(channels), MinMax::<T>::new(channels));
        left.reduce(&bytes, &matrix, 0..200);
        right.reduce(&bytes, &matrix, 200..rows);
        whole.reduce(&bytes, &matrix, 0..rows);
        left.merge(&right);
        assert_eq!(left, whole);
    }

    #[test]
    fn every_type_and_byte_order_matches_a_plain_scan() {
        for little in [true, false] {
            macro_rules! int {
                ($ty:ty) => {
                    check::<$ty>(little, |n| n as $ty,
                        |v| if little { v.to_le_bytes().to_vec() } else { v.to_be_bytes().to_vec() })
                };
            }
            int!(u8); int!(i8); int!(u16); int!(i16); int!(u32); int!(i32); int!(u64); int!(i64);
            check::<f32>(little, |n| if n % 29 == 0 { f32::NAN } else { (n % 100_000) as f32 / 7. - 5_000. },
                |v| if little { v.to_le_bytes().to_vec() } else { v.to_be_bytes().to_vec() });
            check::<f64>(little, |n| if n % 31 == 0 { f64::NAN } else { (n % 1_000_000) as f64 / 3. - 1e5 },
                |v| if little { v.to_le_bytes().to_vec() } else { v.to_be_bytes().to_vec() });
        }
    }

    #[test]
    fn a_channel_that_is_always_nan_keeps_its_starting_points() {
        let bytes: Vec<u8> = (0..4).flat_map(|_| f32::NAN.to_le_bytes()).collect();
        let mut result = MinMax::<f32>::new(1);
        result.reduce(&bytes, &Matrix { first: 0, channels: 1, row_stride: 4, little_endian: true }, 0..4);
        assert_eq!((result.min[0], result.max[0]), (f32::INFINITY, f32::NEG_INFINITY), "no value seen");
    }

    /// Throughput on synthetic u16 matrices, in memory and through file reads.
    /// Run: `BENCH_DIR=<scratch dir> cargo test --release --bin binwave reduce_throughput -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark; writes ~400 MB to BENCH_DIR"]
    #[cfg(unix)]
    fn reduce_throughput() {
        use rayon::prelude::*;
        use std::os::unix::fs::FileExt;
        let dir = std::env::var("BENCH_DIR").expect("BENCH_DIR names a scratch directory");
        for channels in [108usize, 432] {
            let rows = 2_000_000 * 108 / channels;
            let row_stride = channels * 2;
            let mut seed = 1u64;
            let bytes: Vec<u8> = (0..rows * channels).flat_map(|_| ((noise(&mut seed) % 40_000) as u16).to_le_bytes()).collect();
            let matrix = Matrix { first: 0, channels, row_stride, little_endian: true };
            let bucket = 4096;
            let t = std::time::Instant::now();
            let mut total = MinMax::<u16>::new(channels);
            for start in (0..rows).step_by(bucket) {
                let mut part = MinMax::<u16>::new(channels);
                part.reduce(&bytes, &matrix, start..(start + bucket).min(rows));
                total.merge(&part);
            }
            let one = t.elapsed().as_secs_f64();
            let t = std::time::Instant::now();
            let parts: Vec<MinMax<u16>> = (0..rows).step_by(bucket).collect::<Vec<_>>().par_iter().map(|&start| {
                let mut part = MinMax::<u16>::new(channels);
                part.reduce(&bytes, &matrix, start..(start + bucket).min(rows));
                part
            }).collect();
            let all = t.elapsed().as_secs_f64();
            assert_eq!(parts.len(), rows.div_ceil(bucket));
            let path = std::path::Path::new(&dir).join(format!("matrix{channels}.bin"));
            std::fs::write(&path, &bytes).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let chunk_rows = (8 << 20) / row_stride;
            let t = std::time::Instant::now();
            let starts: Vec<usize> = (0..rows).step_by(chunk_rows).collect();
            let _: Vec<MinMax<u16>> = starts.par_iter().map(|&start| {
                let count = chunk_rows.min(rows - start);
                let mut buffer = vec![0u8; count * row_stride];
                file.read_exact_at(&mut buffer, (start * row_stride) as u64).unwrap();
                let mut part = MinMax::<u16>::new(channels);
                for first in (0..count).step_by(bucket) { part.reduce(&buffer, &matrix, first..(first + bucket).min(count)); }
                part
            }).collect();
            let file_all = t.elapsed().as_secs_f64();
            eprintln!("{channels} ch, {rows} samples ({} MB): one core {:.0} M samples/s, all cores {:.0} M samples/s, with file reads {:.0} M samples/s",
                bytes.len() >> 20, rows as f64 / one / 1e6, rows as f64 / all / 1e6, rows as f64 / file_all / 1e6);
            std::fs::remove_file(&path).unwrap();
        }
    }
}
