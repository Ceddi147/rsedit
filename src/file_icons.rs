//! Line icons for the file tree: one shape per kind of file, drawn with
//! strokes in a single color (no images, so no textures or extra memory).
//!
//! Shapes are designed on a 16×16 grid and scaled to the requested size.

use egui::{Color32, Painter, Pos2, Shape, Stroke, epaint::PathShape, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    /// Draws the icon `size` points wide, centered on `center`.
    pub fn paint(self, painter: &Painter, center: Pos2, size: f32, color: Color32) {
        use FileIcon::*;
        let s = size / 16.0;
        let origin = center - vec2(8.0, 8.0) * s;
        let p = |x: f32, y: f32| origin + vec2(x, y) * s;
        let stroke = Stroke::new(1.2 * s, color);
        let line = |pts: &[(f32, f32)]| {
            Shape::Path(PathShape::line(pts.iter().map(|&(x, y)| p(x, y)).collect(), stroke))
        };
        let closed = |pts: &[(f32, f32)]| {
            Shape::Path(PathShape::closed_line(
                pts.iter().map(|&(x, y)| p(x, y)).collect(),
                stroke,
            ))
        };
        let rect = |x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
            Shape::rect_stroke(
                egui::Rect::from_min_max(p(x0, y0), p(x1, y1)),
                r * s,
                stroke,
                egui::StrokeKind::Middle,
            )
        };
        let circle = |x: f32, y: f32, r: f32| Shape::circle_stroke(p(x, y), r * s, stroke);
        let dot = |x: f32, y: f32, r: f32| Shape::circle_filled(p(x, y), r * s, color);
        const HEX: [(f32, f32); 6] = [
            (8.0, 1.5),
            (13.6, 4.75),
            (13.6, 11.25),
            (8.0, 14.5),
            (2.4, 11.25),
            (2.4, 4.75),
        ];
        const DOC: [(f32, f32); 5] = [(3.5, 1.5), (9.5, 1.5), (12.5, 4.5), (12.5, 14.5), (3.5, 14.5)];
        const FOLD: [(f32, f32); 3] = [(9.5, 1.5), (9.5, 4.5), (12.5, 4.5)];
        // Headers: dashed hexagon. Sources: solid.
        let hexagon = |dashed: bool| -> Vec<Shape> {
            if dashed {
                let mut pts: Vec<Pos2> = HEX.iter().map(|&(x, y)| p(x, y)).collect();
                pts.push(pts[0]);
                Shape::dashed_line(&pts, stroke, 2.2 * s, 1.6 * s)
            } else {
                vec![closed(&HEX)]
            }
        };
        // Letters are strokes too, so they stay crisp at 16 px.
        let letter_c = |cx: f32| {
            let pts: Vec<(f32, f32)> = (0..=10)
                .map(|i| {
                    let a = (45.0 + 27.0 * i as f32).to_radians();
                    (cx + 2.3 * a.cos(), 8.0 - 2.5 * a.sin())
                })
                .collect();
            line(&pts)
        };
        let letter_h = |cx: f32| {
            vec![
                line(&[(cx - 1.8, 5.5), (cx - 1.8, 10.5)]),
                line(&[(cx + 1.8, 5.5), (cx + 1.8, 10.5)]),
                line(&[(cx - 1.8, 8.0), (cx + 1.8, 8.0)]),
            ]
        };
        let pluses = || {
            vec![
                line(&[(10.1, 6.4), (12.1, 6.4)]),
                line(&[(11.1, 5.4), (11.1, 7.4)]),
                line(&[(10.1, 9.6), (12.1, 9.6)]),
                line(&[(11.1, 8.6), (11.1, 10.6)]),
            ]
        };

        let shapes: Vec<Shape> = match self {
            Folder => vec![closed(&[
                (1.5, 4.0),
                (5.8, 4.0),
                (7.3, 5.5),
                (14.5, 5.5),
                (14.5, 13.0),
                (1.5, 13.0),
            ])],
            FolderOpen => vec![
                line(&[(1.5, 13.0), (1.5, 4.0), (5.8, 4.0), (7.3, 5.5), (13.0, 5.5), (13.0, 7.5)]),
                closed(&[(1.5, 13.0), (3.6, 7.5), (14.6, 7.5), (12.5, 13.0)]),
            ],
            Rust => {
                let mut v = vec![circle(8.0, 8.0, 4.4)];
                for i in 0..8 {
                    let a = (i as f32 * 45.0).to_radians();
                    let (dx, dy) = (a.cos(), a.sin());
                    v.push(line(&[
                        (8.0 + 4.6 * dx, 8.0 + 4.6 * dy),
                        (8.0 + 6.3 * dx, 8.0 + 6.3 * dy),
                    ]));
                }
                // The R: stem, bowl and leg.
                v.push(line(&[
                    (6.7, 10.4),
                    (6.7, 5.6),
                    (8.6, 5.6),
                    (9.3, 6.2),
                    (9.3, 7.4),
                    (8.6, 8.0),
                    (6.7, 8.0),
                ]));
                v.push(line(&[(7.9, 8.0), (9.4, 10.4)]));
                v
            }
            CppSource | CppHeader | CSource | CHeader => {
                let header = matches!(self, CppHeader | CHeader);
                let cpp = matches!(self, CppSource | CppHeader);
                let mut v = hexagon(header);
                let cx = if cpp { 5.9 } else { 8.0 };
                if header {
                    v.extend(letter_h(cx));
                } else {
                    v.push(letter_c(cx));
                }
                if cpp {
                    v.extend(pluses());
                }
                v
            }
            Toml => vec![
                closed(&DOC),
                line(&FOLD),
                line(&[(5.5, 8.5), (10.5, 8.5)]),
                line(&[(5.5, 11.5), (10.5, 11.5)]),
                dot(7.3, 8.5, 1.0),
                dot(9.0, 11.5, 1.0),
            ],
            Markdown => vec![
                rect(1.5, 3.5, 14.5, 12.5, 1.5),
                line(&[(4.0, 10.0), (4.0, 6.0), (5.8, 8.0), (7.6, 6.0), (7.6, 10.0)]),
                line(&[(11.0, 6.0), (11.0, 10.0)]),
                line(&[(9.6, 8.6), (11.0, 10.0), (12.4, 8.6)]),
            ],
            Lock => {
                let shackle: Vec<(f32, f32)> = std::iter::once((5.5, 7.0))
                    .chain((0..=8).map(|i| {
                        let a = (180.0 - 22.5 * i as f32).to_radians();
                        (8.0 + 2.5 * a.cos(), 5.2 - 2.5 * a.sin())
                    }))
                    .chain(std::iter::once((10.5, 7.0)))
                    .collect();
                vec![rect(3.0, 7.0, 13.0, 14.5, 1.5), line(&shackle)]
            }
            Shell => vec![
                rect(1.5, 2.5, 14.5, 13.5, 2.0),
                line(&[(4.5, 6.5), (6.5, 8.0), (4.5, 9.5)]),
                line(&[(8.5, 10.0), (11.0, 10.0)]),
            ],
            Git => vec![
                circle(4.5, 3.8, 1.6),
                circle(4.5, 12.2, 1.6),
                circle(11.5, 6.4, 1.6),
                line(&[(4.5, 5.4), (4.5, 10.6)]),
                line(&[(11.5, 8.0), (11.2, 9.6), (9.5, 10.6), (6.1, 11.4)]),
            ],
            Image => vec![
                rect(1.5, 2.5, 14.5, 13.5, 1.5),
                circle(5.5, 6.2, 1.2),
                line(&[(1.5, 12.0), (5.5, 8.5), (8.5, 11.0), (11.0, 9.0), (14.5, 12.0)]),
            ],
            File => vec![closed(&DOC), line(&FOLD)],
        };
        painter.extend(shapes);
    }
}

#[cfg(test)]
mod tests {
    use super::FileIcon::{self, *};

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

    /// Saves every icon at 16 and 32 points to `$RSEDIT_SNAPSHOT/icons.ppm`.
    #[test]
    #[ignore = "writes an image to look at, run with --ignored"]
    fn icon_sheet_snapshot() {
        use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
        let Some(out_dir) = std::env::var_os("RSEDIT_SNAPSHOT") else {
            return;
        };
        let all = [
            Folder, FolderOpen, Rust, CppSource, CppHeader, CSource, CHeader, Toml, Markdown,
            Lock, Shell, Git, Image, File,
        ];
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
                icon.paint(ui.painter(), egui::pos2(x, 24.0), 16.0, p.mid);
                icon.paint(ui.painter(), egui::pos2(x, 66.0), 32.0, p.mid);
            }
        });
        let mut textures = std::mem::take(&mut out.textures_delta);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        let mut buf = vec![[0u8; 4]; w * h];
        EguiSoftwareRender::new(ColorFieldOrder::Bgra).with_caching(false).render(
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
