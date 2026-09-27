#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#[cfg(test)]
mod egui_compat;

pub(crate) const APP_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

#[cfg(not(target_arch = "wasm32"))]
mod app;
#[cfg(not(target_arch = "wasm32"))]
mod binary;
#[cfg(not(target_arch = "wasm32"))]
mod binary_ui;
#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(any(test, target_arch = "wasm32"))]
mod browser_viewport;
#[cfg(target_arch = "wasm32")]
mod browser_decoder;
mod byte_display;
#[cfg(not(target_arch = "wasm32"))]
mod byte_store;
#[cfg(target_arch = "wasm32")]
#[path = "browser_bytes.rs"]
mod byte_store;
#[cfg(not(target_arch = "wasm32"))]
mod cache;
#[cfg(not(target_arch = "wasm32"))]
mod config;
#[cfg(not(target_arch = "wasm32"))]
mod csv_table;
#[cfg(not(target_arch = "wasm32"))]
mod data;
#[cfg(not(target_arch = "wasm32"))]
mod field_preview;
mod field_edit;
#[cfg(not(target_arch = "wasm32"))]
mod file_dialog;
mod file_range;
#[cfg(not(target_arch = "wasm32"))]
mod file_source;
#[cfg(not(target_arch = "wasm32"))]
mod hex_editor;
#[cfg(not(target_arch = "wasm32"))]
mod loaders;
#[cfg(not(target_arch = "wasm32"))]
mod plot_index;
mod plot_trace;
#[cfg(not(target_arch = "wasm32"))]
mod plot_raster;
#[cfg(not(target_arch = "wasm32"))]
mod plot_gpu;
mod plot_presentation;
#[cfg(not(target_arch = "wasm32"))]
mod plotting;
#[cfg(not(target_arch = "wasm32"))]
mod python_decoder;
mod source_editor;
mod direct_authoring;
mod app_menu;
mod decoder_ui;
mod demos;
mod structure;
mod structure_plot;
mod structure_selection;
mod theme;
#[cfg(not(target_arch = "wasm32"))]
mod sidebar;
#[cfg(not(target_arch = "wasm32"))]
mod window_frame;
#[cfg(not(target_arch = "wasm32"))]
mod structure_ui;
#[cfg(not(target_arch = "wasm32"))]
mod structure_views;
mod workspace_inspector;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod inspector_parity_tests;
mod workspace_layout;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("binwave=info")).init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Keep the existing settings location through the product rename.
            .with_app_id("Plotter Max")
            .with_inner_size([1280., 850.])
            .with_min_inner_size([1000., 650.])
            .with_decorations(false)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "Binwave",
        native_options,
        Box::new(|cc| {
            if let Some(render_state) = &cc.wgpu_render_state {
                plot_gpu::configure(render_state.target_format);
                let adapter = render_state.adapter.get_info();
                log::info!("Graphics adapter: {} ({:?}, {:?})", adapter.name, adapter.backend, adapter.device_type);
            }
            Ok(Box::new(app::BinwaveApp::new(cc)))
        }),
    )?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast;

    console_error_panic_hook::set_once();
    if browser_decoder::in_worker() {
        return;
    }
    wasm_bindgen_futures::spawn_local(async {
        let mut failures = Vec::new();
        for (name, backend) in [("WebGPU", wgpu::Backends::BROWSER_WEBGPU), ("WebGL2", wgpu::Backends::GL)] {
            // Backend selection alone does not retry a rejected requestDevice.
            // A failed WebGPU surface can also claim the canvas context type.
            let runner = eframe::WebRunner::new();
            let mut options = eframe::WebOptions::default();
            if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
                setup.instance_descriptor.backends = backend;
            }
            let canvas = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.get_element_by_id("binwave"))
                .and_then(|element| element.dyn_into::<web_sys::HtmlCanvasElement>().ok());
            let Some(canvas) = canvas else {
                browser::startup_error("Could not find the BinWave canvas.");
                return;
            };
            match runner.start(canvas, options,
                Box::new(|cc| Ok(Box::new(browser::BrowserApp::new(cc))))).await {
                Ok(()) => return,
                Err(error) => {
                    failures.push(format!("{name}: {error:?}"));
                    runner.destroy();
                    if backend == wgpu::Backends::BROWSER_WEBGPU {
                        if let Err(error) = browser::fresh_canvas() {
                            failures.push(format!("Canvas reset: {error:?}"));
                            break;
                        }
                    }
                }
            }
        }
        browser::startup_error(&format!("Could not start graphics:\n{}", failures.join("\n\n")));
    });
}
