use crate::config::AppConfig;
use crate::data::DataStore;
use anyhow::Result;
use std::sync::atomic::AtomicBool;

pub type PlotLines = Vec<(usize, String, Vec<[f64; 2]>)>;

pub struct PlotBuilder<'a> {
    pub store: &'a DataStore,
    pub cfg: &'a AppConfig,
}

impl PlotBuilder<'_> {
    pub fn lines_for_range(
        &self,
        visible: &[bool],
        bounds: [f64; 2],
        target: usize,
        cancel: &AtomicBool,
    ) -> Result<PlotLines> {
        self.store
            .signals
            .iter()
            .enumerate()
            .filter(|(i, _)| visible.get(*i) == Some(&true))
            .take(self.cfg.view.max_lines_in_plot)
            .map(|(i, signal)| {
                let points = self
                    .store
                    .get(&signal.name)
                    .unwrap()
                    .points
                    .for_range_cancel(bounds[0], bounds[1], target, cancel)?;
                Ok((i, signal.name.clone(), points))
            })
            .collect()
    }
}
