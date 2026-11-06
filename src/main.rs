#[cfg(test)]
mod alloc_probe;
mod app;
mod buffer;
mod commands;
mod completion;
mod document;
mod editor;
mod explorer;
mod git;
mod highlight;
mod lsp;
mod ops;
mod palette;
mod search;
mod session;

use std::path::PathBuf;

use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew, wgpu::Backends};

fn options(backends: Backends) -> eframe::NativeOptions {
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = backends;
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rsedit")
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([500.0, 300.0]),
        wgpu_options: WgpuConfiguration { wgpu_setup: WgpuSetup::CreateNew(setup), ..Default::default() },
        ..Default::default()
    }
}

fn run(backends: Backends, arg: Option<PathBuf>) -> eframe::Result {
    eframe::run_native("rsedit", options(backends), Box::new(move |cc| Ok(Box::new(app::EditorApp::new(cc, arg)))))
}

fn main() -> eframe::Result {
    let arg = std::env::args_os().nth(1).map(PathBuf::from);
    // Only Vulkan: probing every graphics API also loads the OpenGL driver (on
    // Mesa that's ~70 MiB of libraries plus its heap) just to not use it.
    // `WGPU_BACKEND=gl` etc. still overrides this.
    match Backends::from_env() {
        Some(chosen) => run(chosen, arg),
        None => run(Backends::VULKAN, arg.clone()).or_else(|e| {
            eprintln!("rsedit: Vulkan unavailable ({e}), trying other graphics backends");
            run(Backends::all(), arg)
        }),
    }
}
