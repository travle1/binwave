mod app;
mod binary;
mod binary_ui;
mod byte_store;
mod cache;
mod config;
mod csv_table;
mod data;
mod field_preview;
mod file_dialog;
mod file_source;
mod hex_editor;
mod loaders;
mod plot_index;
mod plotting;
mod python_decoder;
mod structure;
mod structure_plot;
mod structure_ui;
mod structure_views;
mod workspace_layout;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280., 850.])
            .with_min_inner_size([1000., 650.]),
        ..Default::default()
    };
    eframe::run_native(
        "Plotter Max",
        native_options,
        Box::new(|cc| Box::new(app::PlotterApp::new(cc))),
    )?;
    Ok(())
}
