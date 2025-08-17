mod app;
mod config;
mod data;
mod plotting;
mod stream;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Plotter Max",
        native_options,
        Box::new(|cc| Box::new(app::PlotterApp::new(cc))),
    )?;
    Ok(())
}
