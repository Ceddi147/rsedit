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

fn main() -> eframe::Result {
    let arg = std::env::args_os().nth(1).map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rsedit")
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([500.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "rsedit",
        options,
        Box::new(move |cc| Ok(Box::new(app::EditorApp::new(cc, arg)))),
    )
}
