use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::search::IGNORED;

/// Git decoration of an explorer entry: (path, is_dir) -> (label, colour).
pub type Deco<'a> = dyn Fn(&Path, bool) -> Option<(String, egui::Color32)> + 'a;

/// Something the user asked for in the explorer.
pub enum Action {
    /// Open a file; `true` = in a new tab (middle click).
    Open(PathBuf, bool),
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
    CopyPath(PathBuf),
}

/// A lazily-loaded file tree node.
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub expanded: bool,
    children: Option<Vec<Node>>,
}

impl Node {
    pub fn root(path: &Path) -> Self {
        let mut n = Self::new(path.to_path_buf(), true);
        n.expanded = true;
        n
    }

    fn new(path: PathBuf, is_dir: bool) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            path,
            name,
            is_dir,
            expanded: false,
            children: None,
        }
    }

    /// Re-reads loaded directories from disk, keeping expanded folders expanded.
    pub fn refresh(&mut self) {
        let Some(old) = self.children.take() else {
            return;
        };
        let mut old: HashMap<PathBuf, Node> =
            old.into_iter().map(|n| (n.path.clone(), n)).collect();
        let mut fresh = read_dir_sorted(&self.path);
        for n in &mut fresh {
            if let Some(mut o) = old.remove(&n.path)
                && o.is_dir == n.is_dir
            {
                o.refresh();
                *n = o;
            }
        }
        self.children = Some(fresh);
    }

    /// Expands the folders leading to `path` (to reveal the active file).
    pub fn reveal(&mut self, path: &Path) {
        for child in self.children().iter_mut() {
            if child.is_dir && path.starts_with(&child.path) {
                child.expanded = true;
                child.reveal(path);
            }
        }
    }

    fn children(&mut self) -> &mut Vec<Node> {
        self.children
            .get_or_insert_with(|| read_dir_sorted(&self.path))
    }

    /// Draws the tree and returns what the user asked for, if anything.
    /// `deco` gives a file's git decoration: a short label (e.g. "M") and a colour.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        active: Option<&Path>,
        depth: usize,
        deco: &Deco,
    ) -> Option<Action> {
        let mut action = None;
        for child in self.children().iter_mut() {
            let row_h = ui.text_style_height(&egui::TextStyle::Body) + 10.0;
            let (rect, resp) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), row_h),
                egui::Sense::click(),
            );
            let visuals = ui.visuals();
            let p = crate::theme::palette(ui);
            let selected = active == Some(child.path.as_path());
            if selected {
                ui.painter().rect_filled(rect, 6.0, p.raised2);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 6.0, p.raised);
            }
            let x = rect.left() + 6.0 + depth as f32 * 14.0;
            let decoration = deco(&child.path, child.is_dir);
            // The name stays neutral; the git letter on the right carries the color.
            let color = if selected { p.strong } else { p.text };
            if let Some((label, c)) = &decoration {
                ui.painter().text(
                    egui::pos2(rect.right() - 8.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    label,
                    egui::TextStyle::Small.resolve(ui.style()),
                    *c,
                );
            }
            if child.is_dir {
                chevron(
                    ui.painter(),
                    egui::pos2(x + 4.0, rect.center().y),
                    child.expanded,
                    visuals.weak_text_color(),
                );
            }
            let icon = crate::file_icons::FileIcon::for_entry(
                &child.name,
                child.is_dir,
                child.expanded,
            );
            let icon_color = if selected { p.text } else { p.mid };
            icon.paint(ui.painter(), egui::pos2(x + 23.0, rect.center().y), 15.0, icon_color);
            ui.painter().text(
                egui::pos2(x + 36.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                &child.name,
                egui::TextStyle::Body.resolve(ui.style()),
                color,
            );
            if resp.clicked() {
                if child.is_dir {
                    child.expanded = !child.expanded;
                } else {
                    action = Some(Action::Open(child.path.clone(), false));
                }
            } else if resp.middle_clicked() && !child.is_dir {
                action = Some(Action::Open(child.path.clone(), true));
            }
            let resp = resp.on_hover_text(child.path.display().to_string());
            resp.context_menu(|ui| {
                let dir = if child.is_dir {
                    child.path.clone()
                } else {
                    child.path.parent().unwrap_or(&child.path).to_path_buf()
                };
                if !child.is_dir && ui.button("Open in New Tab").clicked() {
                    action = Some(Action::Open(child.path.clone(), true));
                }
                if ui.button("New File…").clicked() {
                    action = Some(Action::NewFile(dir.clone()));
                }
                if ui.button("New Folder…").clicked() {
                    action = Some(Action::NewFolder(dir));
                }
                ui.separator();
                if ui.button("Rename…").clicked() {
                    action = Some(Action::Rename(child.path.clone()));
                }
                if ui.button("Delete").clicked() {
                    action = Some(Action::Delete(child.path.clone()));
                }
                ui.separator();
                if ui.button("Copy Path").clicked() {
                    action = Some(Action::CopyPath(child.path.clone()));
                }
            });
            if child.is_dir
                && child.expanded
                && let Some(a) = child.ui(ui, active, depth + 1, deco)
            {
                action = Some(a);
            }
        }
        action
    }
}

fn read_dir_sorted(dir: &Path) -> Vec<Node> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes: Vec<Node> = rd
        .flatten()
        .filter(|e| !IGNORED.contains(&e.file_name().to_string_lossy().as_ref()))
        .map(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            Node::new(e.path(), is_dir)
        })
        .collect();
    nodes.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    nodes
}

/// A small triangle pointing right (closed) or down (open), centred on `c`.
pub fn chevron(painter: &egui::Painter, c: egui::Pos2, open: bool, color: egui::Color32) {
    let r = 3.5;
    let points = if open {
        vec![
            c + egui::vec2(-r, -r * 0.6),
            c + egui::vec2(r, -r * 0.6),
            c + egui::vec2(0.0, r * 0.7),
        ]
    } else {
        vec![
            c + egui::vec2(-r * 0.6, -r),
            c + egui::vec2(-r * 0.6, r),
            c + egui::vec2(r * 0.7, 0.0),
        ]
    };
    painter.add(egui::Shape::convex_polygon(
        points,
        color,
        egui::Stroke::NONE,
    ));
}

/// A clickable chevron, for expand/collapse toggles.
pub fn chevron_button(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::click());
    let color = if resp.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    chevron(ui.painter(), rect.center(), open, color);
    resp
}
