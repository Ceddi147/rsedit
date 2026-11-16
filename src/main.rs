#[cfg(test)]
mod alloc_probe;
mod app;
mod buffer;
mod commands;
mod completion;
mod cppflags;
mod damage;
mod document;
mod editor;
mod explorer;
mod file_icons;
mod git;
mod highlight;
mod lsp;
mod ops;
mod palette;
mod recovery;
mod search;
mod session;
mod settings;
mod signature;
mod theme;
mod trust;
mod window;

use std::path::PathBuf;

fn main() -> std::process::ExitCode {
    let arg = std::env::args_os().nth(1).map(PathBuf::from);
    let viewport = egui::ViewportBuilder::default()
        .with_title("rsedit")
        // Matches rsedit.desktop, so launchers and taskbars show the right name and icon.
        .with_app_id("rsedit")
        .with_inner_size([1200.0, 780.0])
        .with_min_inner_size([500.0, 300.0]);
    match window::run(viewport, arg) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rsedit: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
