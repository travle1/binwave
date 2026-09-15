//! Debounce browser plot gestures without repeating reads or truncating file indices.
#[derive(Default)]
pub struct ViewportRefresh {
    pending: Option<(u64, u64, f64)>,
    requested: Option<(u64, u64)>,
}
impl ViewportRefresh {
    pub fn reset(&mut self) { *self = Self::default(); }
    pub fn observe(&mut self, first: f64, last: f64, count: u64, now: f64) {
        if !first.is_finite() || !last.is_finite() || first >= last { return; }
        let range = ((first.max(0.).floor() as u64).min(count),
            (last.max(0.).ceil() as u64).saturating_add(1).min(count));
        if range.0 >= range.1 || self.requested == Some(range) { self.pending = None; return; }
        self.pending = Some((range.0, range.1, now));
    }
    pub fn ready(&mut self, now: f64) -> Option<(u64,u64)> {
        let (first,last,at) = self.pending?;
        if now-at < 0.25 { return None; }
        self.pending = None;
        self.requested = Some((first,last));
        Some((first,last))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gestures_settle_and_only_the_latest_range_is_read() {
        let mut v=ViewportRefresh::default();
        v.observe(0.,1_000_000.,2_000_000,0.);
        assert_eq!(v.ready(0.2),None);
        v.observe(100.,500.,2_000_000,0.2);
        assert_eq!(v.ready(0.3),None);
        assert_eq!(v.ready(0.5),Some((100,501)));
        v.observe(100.,500.,2_000_000,0.6);
        assert_eq!(v.ready(1.),None);
    }
    #[test]
    fn offsets_above_wasm_memory_range_are_preserved_and_bounds_are_clamped() {
        let mut v=ViewportRefresh::default();
        v.observe(5_000_000_000.,5_000_000_512.,6_000_000_000,0.);
        assert_eq!(v.ready(1.),Some((5_000_000_000,5_000_000_513)));
        v.reset(); v.observe(-10.,1000.,512,0.);
        assert_eq!(v.ready(1.),Some((0,512)));
        v.observe(f64::NAN,500.,512,2.);
        assert_eq!(v.ready(3.),None);
    }
}

/// A tap chooses the nearest sample by horizontal position; drags are handled separately.
pub fn clicked_sample(x: f64, count: u64) -> Option<u64> {
    (x.is_finite() && count > 0).then(|| (x.round().max(0.) as u64).min(count - 1))
}

#[cfg(test)]
mod click_tests {
    use super::clicked_sample;
    #[test]
    fn clicks_round_clamp_and_keep_large_indices() {
        assert_eq!(clicked_sample(16.2, 512), Some(16));
        assert_eq!(clicked_sample(16.8, 512), Some(17));
        assert_eq!(clicked_sample(-2., 512), Some(0));
        assert_eq!(clicked_sample(600., 512), Some(511));
        assert_eq!(clicked_sample(5_000_000_000., 6_000_000_000), Some(5_000_000_000));
        assert_eq!(clicked_sample(f64::NAN, 512), None);
        assert_eq!(clicked_sample(0., 0), None);
    }
}
