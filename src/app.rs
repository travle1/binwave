use crate::config::{AppConfig, FileKind};
use crate::data::{load_data, DataStore};
use crate::plotting::PlotBuilder;
use crate::stream::StreamingState;
use anyhow::Result;
use egui::{Context, Ui};
use egui_plot::{Legend, Plot, PlotBounds, PlotUi};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

pub struct PlotterApp {
    cfg: AppConfig,
    data: Option<DataStore>,
    streaming: Option<StreamingState>,
    visible: Vec<bool>,
    search: String,
    status: String,
    loader_rx: Option<Receiver<Result<DataStore>>>,
}

impl PlotterApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = AppConfig::default();
        Self {
            cfg,
            data: None,
            streaming: None,
            visible: Vec::new(),
            search: String::new(),
            status: "Drop a CSV or load config to begin".into(),
            loader_rx: None,
        }
    }

    fn begin_load(&mut self, cfg: AppConfig) {
        self.status = format!("Loading: {:?}", cfg.data_path);
        // If binary_fixed with binary.streaming == true, use streaming path
        if matches!(cfg.format, FileKind::BinaryFixed)
            && cfg.binary.as_ref().and_then(|b| b.streaming).unwrap_or(false)
        {
            if let Some(path) = cfg.data_path.clone() {
                match StreamingState::new(&path, &cfg) {
                    Ok(s) => {
                        let names = s.signal_names();
                        self.visible = vec![true; names.len()];
                        self.streaming = Some(s);
                        self.data = None;
                        self.status = format!("Streaming from {:?} ({} signals)", path, names.len());
                        self.cfg = cfg;
                    }
                    Err(err) => {
                        self.status = format!("Streaming init failed: {err}");
                        self.streaming = None;
                    }
                }
                return;
            }
        }

        // Default: full load in background
        let (tx, rx) = mpsc::channel();
        self.loader_rx = Some(rx);
        thread::spawn(move || {
            let res = load_data(&cfg);
            let _ = tx.send(res);
        });
    }

    fn ui_left_panel(&mut self, ui: &mut Ui) {
        ui.heading("Controls");

        if ui.button("Open CSV...").clicked() {
            if let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).pick_file() {
                let mut cfg = self.cfg.clone();
                cfg.data_path = Some(path.into());
                cfg.format = crate::config::FileKind::Csv;
                self.begin_load(cfg);
            }
        }

        if ui.button("Open Config JSON...").clicked() {
            if let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {
                match AppConfig::load_from_file(&path) {
                    Ok(cfg) => self.begin_load(cfg),
                    Err(err) => self.status = format!("Config error: {err}")
                }
            }
        }

        ui.separator();
        ui.label(&self.status);

        if let Some(ds) = &self.data {
            ui.separator();
            ui.label(format!(
                "Time: [{:.3}, {:.3}]  signals: {}",
                ds.time_min,
                ds.time_max,
                ds.signals.len()
            ));

            ui.horizontal(|ui| {
                if ui.button("Select All").clicked() {
                    self.visible.iter_mut().for_each(|v| *v = true);
                }
                if ui.button("Select None").clicked() {
                    self.visible.iter_mut().for_each(|v| *v = false);
                }
            });

            ui.text_edit_singleline(&mut self.search);
            let search_lower = self.search.to_lowercase();

            for (i, s) in ds.signals.iter().enumerate() {
                if !search_lower.is_empty() && !s.name.to_lowercase().contains(&search_lower) {
                    continue;
                }
                let mut vis = self.visible.get(i).copied().unwrap_or(true);
                if ui.checkbox(&mut vis, &s.name).changed() {
                    if i < self.visible.len() {
                        self.visible[i] = vis;
                    }
                }
            }
        } else if let Some(stream) = &self.streaming {
            ui.separator();
            let (t0, t1) = stream.cached_time_bounds();
            ui.label(format!("Time: [{:.3}, {:.3}]", t0, t1));
            let names = stream.signal_names();

            if self.visible.len() != names.len() {
                self.visible = vec![true; names.len()];
            }

            ui.horizontal(|ui| {
                if ui.button("Select All").clicked() {
                    self.visible.iter_mut().for_each(|v| *v = true);
                }
                if ui.button("Select None").clicked() {
                    self.visible.iter_mut().for_each(|v| *v = false);
                }
            });

            ui.text_edit_singleline(&mut self.search);
            let search_lower = self.search.to_lowercase();
            for (i, name) in names.iter().enumerate() {
                if !search_lower.is_empty() && !name.to_lowercase().contains(&search_lower) {
                    continue;
                }
                let mut vis = self.visible.get(i).copied().unwrap_or(true);
                if ui.checkbox(&mut vis, name).changed() {
                    if i < self.visible.len() {
                        self.visible[i] = vis;
                    }
                }
            }
        }
    }
}

impl eframe::App for PlotterApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Check loader
        if let Some(rx) = &self.loader_rx {
            if let Ok(res) = rx.try_recv() {
                match res {
                    Ok(ds) => {
                        self.status = format!("Loaded: {} signals", ds.signals.len());
                        self.visible = vec![true; ds.signals.len()];
                        self.data = Some(ds);
                    }
                    Err(err) => self.status = format!("Load error: {err}"),
                }
                self.loader_rx = None;
            }
        }

        egui::SidePanel::left("left").show(ctx, |ui| {
            self.ui_left_panel(ui);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Handle drag-and-drop for CSV or JSON
            if !ui.input(|i| i.raw.dropped_files.is_empty()) {
                let dropped: Vec<_> = ui.input(|i| i.raw.dropped_files.clone());
                for f in dropped {
                    if let Some(path) = f.path {
                        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                        if ext == "json" {
                            match AppConfig::load_from_file(&path) {
                                Ok(cfg) => self.begin_load(cfg),
                                Err(err) => self.status = format!("Config error: {err}"),
                            }
                        } else if ext == "csv" {
                            let mut cfg = self.cfg.clone();
                            cfg.data_path = Some(path);
                            cfg.format = FileKind::Csv;
                            self.begin_load(cfg);
                        }
                    }
                }
            }
            if let Some(ds) = &self.data {
                let cfg = &self.cfg;
                let builder = PlotBuilder { store: ds, cfg };
                let plot = Plot::new("plot")
                    .legend(Legend::default())
                    .allow_zoom(true)
                    .allow_scroll(true)
                    .allow_boxed_zoom(true)
                    .allow_drag(true);
                let pixels = ui.available_width();
                plot.show(ui, |plot_ui| {
                    // Determine current view bounds for decimation
                    let pb = plot_ui.plot_bounds();
                    let tmin = pb.min()[0];
                    let tmax = pb.max()[0];
                    let pixels = pixels;

                    for (i, s) in ds.signals.iter().enumerate() {
                        if i >= self.visible.len() || !self.visible[i] {
                            continue;
                        }
                        if let Some(line) = builder.line_for_range(&s.name, tmin, tmax, pixels) {
                            plot_ui.line(line);
                        }
                    }
                });
            } else if let Some(stream) = &mut self.streaming {
                let plot = Plot::new("plot_stream")
                    .legend(Legend::default())
                    .allow_zoom(true)
                    .allow_scroll(true)
                    .allow_boxed_zoom(true)
                    .allow_drag(true);
                let pixels = ui.available_width();
                plot.show(ui, |plot_ui| {
                    let pb = plot_ui.plot_bounds();
                    let tmin = pb.min()[0];
                    let tmax = pb.max()[0];
                    let _ = stream.ensure_window(tmin, tmax);
                    let pixels = pixels;
                    let names = stream.signal_names();
                    let window_signals = stream.window_signals();
                    for (i, sig) in window_signals.iter().enumerate() {
                        if i >= self.visible.len() || !self.visible[i] { continue; }
                        // decimate per current view
                        let target = (self.cfg.view.target_points_per_line as f32)
                            .min(pixels.max(100.0)) as usize;
                        let pts = crate::data::decimate_minmax(&sig.points, tmin, tmax, target);
                        if !pts.is_empty() {
                            let plot_points: egui_plot::PlotPoints = pts.into_iter().map(|p| [p[0], p[1]]).collect();
                            plot_ui.line(egui_plot::Line::new(plot_points).name(sig.name.clone()));
                        }
                    }
                });
            } else {
                ui.centered_and_justified(|ui| ui.label("Load data to begin"));
            }
        });
    }
}
