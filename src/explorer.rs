use std::path::{Path, PathBuf};

const IGNORED: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__"];

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
        Self { path, name, is_dir, expanded: false, children: None }
    }

    /// Drop cached children so the tree is re-read from disk.
    pub fn refresh(&mut self) {
        self.children = None;
    }

    fn children(&mut self) -> &mut Vec<Node> {
        self.children.get_or_insert_with(|| read_dir_sorted(&self.path))
    }

    /// Draws the tree. Returns a file path when the user clicks a file.
    pub fn ui(&mut self, ui: &mut egui::Ui, active: Option<&Path>, depth: usize) -> Option<PathBuf> {
        let mut clicked = None;
        for child in self.children().iter_mut() {
            let icon = if child.is_dir {
                if child.expanded { "▾ 📂" } else { "▸ 📁" }
            } else {
                "   📄"
            };
            let selected = active == Some(child.path.as_path());
            let label = format!("{}{} {}", "    ".repeat(depth), icon, child.name);
            let resp = ui.add(
                egui::Button::selectable(selected, label)
                    .frame_when_inactive(false)
                    .min_size(egui::vec2(ui.available_width(), 0.0)),
            );
            if resp.clicked() {
                if child.is_dir {
                    child.expanded = !child.expanded;
                } else {
                    clicked = Some(child.path.clone());
                }
            }
            resp.on_hover_text(child.path.display().to_string());
            if child.is_dir && child.expanded {
                if let Some(p) = child.ui(ui, active, depth + 1) {
                    clicked = Some(p);
                }
            }
        }
        clicked
    }
}

fn read_dir_sorted(dir: &Path) -> Vec<Node> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
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
