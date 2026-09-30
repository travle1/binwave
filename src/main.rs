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
#[cfg(target_arch = "wasm32")]
mod browser_tree;
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
mod format;
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
mod launch_args;
#[cfg(not(target_arch = "wasm32"))]
mod plot_index;
mod plot_trace;
#[cfg(not(target_arch = "wasm32"))]
mod plot_raster;
mod plot_gpu;
mod plot_presentation;
mod plot_series;
mod collection_navigation;
#[cfg(not(target_arch = "wasm32"))]
mod plotting;
#[cfg(not(target_arch = "wasm32"))]
mod python_decoder;
mod source_editor;
mod direct_authoring;
mod app_menu;
mod decoder_ui;
#[cfg(not(target_arch = "wasm32"))]
mod decoder_library;
mod demos;
mod structure;
mod structure_plot;
mod structure_selection;
mod theme;
#[cfg(not(target_arch = "wasm32"))]
mod sidebar;
#[cfg(not(target_arch = "wasm32"))]
mod start_screen;
#[cfg(not(target_arch = "wasm32"))]
mod overview_strip;
#[cfg(not(target_arch = "wasm32"))]
mod plot_reduce;
#[cfg(not(target_arch = "wasm32"))]
mod plot_pyramid;
#[cfg(not(target_arch = "wasm32"))]
mod plot_columns_gpu;
#[cfg(not(target_arch = "wasm32"))]
mod plot_summary;
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
    let launch = match launch_args::parse(std::env::args_os().skip(1)) {
        Ok(launch_args::Parsed::Help) => {
            print!("{}", launch_args::USAGE);
            return Ok(());
        }
        Ok(launch_args::Parsed::Launch(launch)) => launch,
        Err(error) => {
            eprintln!("{error}\n\n{}", launch_args::USAGE);
            std::process::exit(2);
        }
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Keep the existing settings location through the product rename.
            .with_app_id("Plotter Max")
            .with_inner_size(theme::DARK.size.window_default)
            .with_min_inner_size(theme::DARK.size.window_min)
            .with_decorations(false)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "Binwave",
        native_options,
        Box::new(move |cc| {
            if let Some(render_state) = &cc.wgpu_render_state {
                plot_gpu::configure(render_state.target_format);
                let adapter = render_state.adapter.get_info();
                log::info!("Graphics adapter: {} ({:?}, {:?})", adapter.name, adapter.backend, adapter.device_type);
            }
            let mut app = app::BinwaveApp::new(cc);
            app.open_launch_paths(launch.data, launch.decoder, &cc.egui_ctx);
            Ok(Box::new(app))
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
                Box::new(move |cc| {
                    if let Some(state) = &cc.wgpu_render_state {
                        // Storage-buffer shaders need WebGPU; WebGL keeps the
                        // shared adaptive trace fallback.
                        if state.device.limits().max_storage_buffers_per_shader_stage >= 2 {
                            plot_gpu::configure(state.target_format);
                        }
                    }
                    browser::graphics_backend(name, plot_gpu::GpuPlot::available());
                    Ok(Box::new(browser::BrowserApp::new(cc)))
                })).await {
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
