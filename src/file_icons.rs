//! Icons for the file tree. Each one is an SVG file in `assets/icons/`
//! (white strokes on a 16×16 grid), built into the binary, rasterized once
//! per pixel size and tinted with the theme's color when drawn.

use std::collections::HashMap;

use egui::{Color32, Pos2, Rect, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileIcon {
    Folder,
    FolderOpen,
    Rust,
    CppSource,
    CppHeader,
    CSource,
    CHeader,
    Toml,
    Markdown,
    Lock,
    Shell,
    Git,
    Image,
    File,
}

impl FileIcon {
    /// The icon for a tree entry named `name`.
    pub fn for_entry(name: &str, is_dir: bool, expanded: bool) -> Self {
        use FileIcon::*;
        if is_dir {
            return if expanded { FolderOpen } else { Folder };
        }
        if name.starts_with(".git") {
            return Git;
        }
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "rs" => Rust,
            "cpp" | "cc" | "cxx" | "c++" | "ipp" | "tpp" => CppSource,
            "hpp" | "hh" | "hxx" | "h++" | "inl" => CppHeader,
            "c" => CSource,
            "h" => CHeader,
            "toml" => Toml,
            "md" | "markdown" => Markdown,
            "lock" => Lock,
            "sh" | "bash" | "zsh" | "fish" => Shell,
            "svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => Image,
            _ => File,
        }
    }

    /// The icon's SVG source.
    fn svg(self) -> &'static str {
        use FileIcon::*;
        match self {
            Folder => include_str!("../assets/icons/folder.svg"),
            FolderOpen => include_str!("../assets/icons/folder-open.svg"),
            Rust => include_str!("../assets/icons/rust.svg"),
            CppSource => include_str!("../assets/icons/cpp.svg"),
            CppHeader => include_str!("../assets/icons/hpp.svg"),
            CSource => include_str!("../assets/icons/c.svg"),
            CHeader => include_str!("../assets/icons/h.svg"),
            Toml => include_str!("../assets/icons/toml.svg"),
            Markdown => include_str!("../assets/icons/markdown.svg"),
            Lock => include_str!("../assets/icons/lock.svg"),
            Shell => include_str!("../assets/icons/shell.svg"),
            Git => include_str!("../assets/icons/git.svg"),
            Image => include_str!("../assets/icons/image.svg"),
            File => include_str!("../assets/icons/file.svg"),
        }
    }

    /// Renders the SVG into a `px`×`px` image (white, premultiplied alpha).
    fn rasterize(self, px: u32) -> egui::ColorImage {
        use resvg::{tiny_skia, usvg};
        let side = px as usize;
        let tree = match usvg::Tree::from_str(self.svg(), &usvg::Options::default()) {
            Ok(t) => t,
            Err(_) => return egui::ColorImage::filled([side, side], Color32::TRANSPARENT),
        };
        let Some(mut pixmap) = tiny_skia::Pixmap::new(px, px) else {
            return egui::ColorImage::filled([side, side], Color32::TRANSPARENT);
        };
        let scale = px as f32 / tree.size().width().max(1.0);
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        egui::ColorImage::from_rgba_premultiplied([side, side], pixmap.data())
    }

    /// Draws the icon `size` points wide, centered on `center`, in `color`.
    pub fn paint(self, ui: &egui::Ui, center: Pos2, size: f32, color: Color32) {
        let ppp = ui.pixels_per_point();
        let px = (size * ppp).round().max(1.0) as u32;
        let key = (self, px);
        let id = egui::Id::NULL.with("file-icon-textures");
        let cached = ui.ctx().data(|d| {
            d.get_temp::<IconCache>(id)
                .and_then(|c| c.0.get(&key).cloned())
        });
        let texture = match cached {
            Some(t) => t,
            None => {
                // Not inside `data_mut`: loading a texture locks the context too.
                let t = ui.ctx().load_texture(
                    format!("file-icon-{self:?}-{px}"),
                    self.rasterize(px),
                    egui::TextureOptions::LINEAR,
                );
                ui.ctx().data_mut(|d| {
                    d.get_temp_mut_or_default::<IconCache>(id)
                        .0
                        .insert(key, t.clone())
                });
                t
            }
        };
        // On whole pixels, so the strokes stay as sharp as they were rasterized.
        let side = px as f32 / ppp;
        let min = ((center - vec2(side, side) / 2.0) * ppp).round() / ppp;
        let rect = Rect::from_min_size(min, vec2(side, side));
        let uv = Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0));
        ui.painter().image(texture.id(), rect, uv, color);
    }
}

/// Rasterized icons by (icon, pixel size).
#[derive(Clone, Default)]
struct IconCache(HashMap<(FileIcon, u32), egui::TextureHandle>);

#[cfg(test)]
mod tests {
    use super::FileIcon::{self, *};

    const ALL: [FileIcon; 14] = [
        Folder, FolderOpen, Rust, CppSource, CppHeader, CSource, CHeader, Toml, Markdown, Lock,
        Shell, Git, Image, File,
    ];

    #[test]
    fn icons_follow_names() {
        let cases = [
            ("main.rs", Rust),
            ("codec.cpp", CppSource),
            ("codec.CC", CppSource),
            ("codec.hpp", CppHeader),
            ("simd.c", CSource),
            ("simd.h", CHeader),
            ("Cargo.toml", Toml),
            ("Cargo.lock", Lock),
            ("README.md", Markdown),
            ("install.sh", Shell),
            (".gitignore", Git),
            (".gitmodules", Git),
            ("rsedit.svg", Image),
            ("LICENSE", File),
            ("archive.tar.gz", File),
        ];
        for (name, icon) in cases {
            assert_eq!(FileIcon::for_entry(name, false, false), icon, "{name}");
        }
        assert_eq!(FileIcon::for_entry("src", true, false), Folder);
        assert_eq!(FileIcon::for_entry("src.rs", true, true), FolderOpen);
    }

    /// Every bundled SVG parses and draws something.
    #[test]
    fn every_icon_renders() {
        for icon in ALL {
            let img = icon.rasterize(32);
            let ink = img.pixels.iter().filter(|p| p.a() > 0).count();
            assert!(ink > 20, "{icon:?} drew {ink} pixels");
        }
    }

    /// Saves every icon at 16 and 32 points to `$RSEDIT_SNAPSHOT/icons.ppm`.
    #[test]
    #[ignore = "writes an image to look at, run with --ignored"]
    fn icon_sheet_snapshot() {
        use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
        let Some(out_dir) = std::env::var_os("RSEDIT_SNAPSHOT") else {
            return;
        };
        let all = ALL;
        let (w, h) = (all.len() * 48 + 16, 96);
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(w as f32, h as f32),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            let p = crate::theme::palette(ui);
            ui.painter().rect_filled(ui.max_rect(), 0.0, p.side);
            for (i, icon) in all.iter().enumerate() {
                let x = 32.0 + i as f32 * 48.0;
                icon.paint(ui, egui::pos2(x, 24.0), 16.0, p.mid);
                icon.paint(ui, egui::pos2(x, 66.0), 32.0, p.mid);
            }
        });
        let mut textures = std::mem::take(&mut out.textures_delta);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        let mut buf = vec![[0u8; 4]; w * h];
        EguiSoftwareRender::new(ColorFieldOrder::Bgra)
            .with_caching(false)
            .render(
                &mut BufferMutRef::new(&mut buf, w, h),
                &prims,
                &textures,
                out.pixels_per_point,
            );
        textures.clear();
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        ppm.extend(buf.iter().flat_map(|p| [p[2], p[1], p[0]]));
        std::fs::write(std::path::Path::new(&out_dir).join("icons.ppm"), ppm).unwrap();
    }
}
