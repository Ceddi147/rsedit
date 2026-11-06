use std::path::{Path, PathBuf};
use std::time::Instant;

use egui::text::{CCursor, CCursorRange};
use egui::{Color32, CursorIcon, FontId, Key, KeyboardShortcut, Modifiers, Rect, Sense, Stroke, pos2, vec2};

use crate::document::{Document, line_col};
use crate::explorer::Node;

const FONT_SIZE: f32 = 14.0;
/// Time per frame spent highlighting, so big files never block the UI.
const HIGHLIGHT_BUDGET: std::time::Duration = std::time::Duration::from_millis(8);
/// Width of the draggable divider between editor groups.
const DIVIDER: f32 = 5.0;
/// Smallest width a group can be resized to.
const MIN_PANE_WIDTH: f32 = 120.0;

/// Something that needs a "Save changes?" answer before it can happen.
#[derive(Clone, Copy)]
enum Pending {
    CloseTab { pane: u64, doc: u64 },
    Quit,
}

/// An editor group: a column with its own tabs, shown side by side with the others.
/// Groups refer to documents by id, so one buffer can be visible in several groups.
struct Pane {
    id: u64,
    tabs: Vec<u64>,
    active: usize,
    /// Share of the editor area's width; all panes sum to 1.
    width: f32,
}

impl Pane {
    fn new(id: u64, tabs: Vec<u64>, width: f32) -> Self {
        Self { id, tabs, active: 0, width }
    }

    fn active_doc(&self) -> Option<u64> {
        self.tabs.get(self.active).copied()
    }
}

enum TabAction {
    Activate(usize),
    Close(usize),
    SplitRight(usize),
    Move(usize, isize),
}

pub struct EditorApp {
    docs: Vec<Document>,
    panes: Vec<Pane>,
    /// Index of the group that receives opened files and shortcuts.
    focused: usize,
    /// Move keyboard focus into the focused group's editor on the next frame.
    focus_pending: bool,
    next_pane_id: u64,
    workspace: Option<Node>,
    show_sidebar: bool,
    untitled_counter: usize,
    pending: Option<Pending>,
    allow_quit: bool,
    status: Option<(String, Instant)>,
    window_title: String,
}

impl EditorApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, arg: Option<PathBuf>) -> Self {
        crate::highlight::preload();
        let mut app = Self {
            docs: Vec::new(),
            panes: vec![Pane::new(0, Vec::new(), 1.0)],
            focused: 0,
            focus_pending: false,
            next_pane_id: 1,
            workspace: None,
            show_sidebar: true,
            untitled_counter: 0,
            pending: None,
            allow_quit: false,
            status: None,
            window_title: String::new(),
        };
        match arg {
            Some(p) if p.is_file() => {
                if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    app.workspace = Some(Node::root(dir));
                }
                app.open_path(&p);
            }
            Some(p) if p.is_dir() => app.workspace = Some(Node::root(&p)),
            _ => {
                if let Ok(cwd) = std::env::current_dir() {
                    app.workspace = Some(Node::root(&cwd));
                }
            }
        }
        app
    }

    // ---------------------------------------------------------------- actions

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    fn doc_index(&self, id: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    /// Index into `docs` of the document shown in the focused group.
    fn active_doc_index(&self) -> Option<usize> {
        self.panes[self.focused].active_doc().and_then(|id| self.doc_index(id))
    }

    fn focus_pane(&mut self, idx: usize) {
        if idx < self.panes.len() {
            self.focused = idx;
            self.focus_pending = true;
        }
    }

    /// Shows a document in the focused group, adding a tab next to the active one if needed.
    fn show_in_focused(&mut self, doc: u64) {
        let pane = &mut self.panes[self.focused];
        match pane.tabs.iter().position(|&d| d == doc) {
            Some(t) => pane.active = t,
            None => {
                let at = (pane.active + 1).min(pane.tabs.len());
                pane.tabs.insert(at, doc);
                pane.active = at;
            }
        }
        self.focus_pending = true;
    }

    fn new_file(&mut self) {
        self.untitled_counter += 1;
        let doc = Document::new_untitled(self.untitled_counter);
        let id = doc.id;
        self.docs.push(doc);
        self.show_in_focused(id);
    }

    fn open_path(&mut self, path: &Path) {
        if let Some(id) = self.docs.iter().find(|d| d.path.as_deref() == Some(path)).map(|d| d.id) {
            self.show_in_focused(id);
            return;
        }
        match Document::open(path) {
            Ok(doc) => {
                let id = doc.id;
                self.docs.push(doc);
                self.show_in_focused(id);
            }
            Err(e) => self.set_status(format!("Could not open {}: {e}", path.display())),
        }
    }

    /// Opens the given tab in a new group directly to the right of `from`.
    fn split_right(&mut self, from: usize, tab: usize) {
        let Some(&doc) = self.panes[from].tabs.get(tab) else { return };
        let half = self.panes[from].width / 2.0;
        self.panes[from].width = half;
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        self.panes.insert(from + 1, Pane::new(id, vec![doc], half));
        self.focus_pane(from + 1);
    }

    /// Moves a tab to the neighbouring group (`dir` = -1 left, +1 right).
    /// Moving right out of the last group creates a new one.
    fn move_tab(&mut self, from: usize, tab: usize, dir: isize) {
        let Some(&doc) = self.panes[from].tabs.get(tab) else { return };
        let Some(to) = from.checked_add_signed(dir) else { return };
        if to >= self.panes.len() {
            self.split_right(from, tab);
        } else {
            self.focused = to;
            self.show_in_focused(doc);
        }
        // The target is never left of `from` in a way that shifts it, so `from` is still valid.
        self.remove_tab(from, tab);
    }

    /// Removes a tab from a group without touching the document. Empty groups collapse.
    fn remove_tab(&mut self, pane: usize, tab: usize) {
        let p = &mut self.panes[pane];
        p.tabs.remove(tab);
        if p.active > tab || p.active >= p.tabs.len() {
            p.active = p.active.saturating_sub(1);
        }
        if p.tabs.is_empty() && self.panes.len() > 1 {
            self.remove_pane(pane);
        }
    }

    fn remove_pane(&mut self, idx: usize) {
        if self.panes.len() <= 1 {
            return;
        }
        let width = self.panes.remove(idx).width;
        // Give the freed space to the neighbour on the left (or right for the first group).
        self.panes[idx.saturating_sub(1)].width += width;
        if self.focused > idx || self.focused >= self.panes.len() {
            self.focused = self.focused.saturating_sub(1);
        }
        self.focus_pending = true;
    }

    /// Asks before closing a tab if that would discard unsaved changes.
    fn request_close(&mut self, pane: usize, tab: usize) {
        let Some(&doc) = self.panes.get(pane).and_then(|p| p.tabs.get(tab)) else { return };
        let other_view = self.panes.iter().enumerate().any(|(i, p)| i != pane && p.tabs.contains(&doc));
        let dirty = self.doc_index(doc).is_some_and(|i| self.docs[i].is_dirty());
        if dirty && !other_view {
            self.pending = Some(Pending::CloseTab { pane: self.panes[pane].id, doc });
        } else {
            self.close_tab(pane, tab);
        }
    }

    /// Closes a tab and drops its document once no group shows it any more.
    fn close_tab(&mut self, pane: usize, tab: usize) {
        let Some(&doc) = self.panes.get(pane).and_then(|p| p.tabs.get(tab)) else { return };
        self.remove_tab(pane, tab);
        if !self.panes.iter().any(|p| p.tabs.contains(&doc)) {
            if let Some(i) = self.doc_index(doc) {
                self.docs.remove(i);
            }
        }
    }

    fn open_file_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new().pick_file() {
            self.open_path(&p);
        }
    }

    fn open_folder_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new().pick_folder() {
            self.workspace = Some(Node::root(&p));
            self.show_sidebar = true;
        }
    }

    /// Saves the doc at `idx`. Returns false if it wasn't saved (error or cancelled).
    fn save(&mut self, idx: usize, force_dialog: bool) -> bool {
        let Some(doc) = self.docs.get_mut(idx) else { return false };
        let result = if doc.path.is_none() || force_dialog {
            match rfd::FileDialog::new().set_file_name(doc.title()).save_file() {
                Some(p) => doc.save_as(p),
                None => return false,
            }
        } else {
            doc.save()
        };
        match result {
            Ok(()) => {
                let msg = format!("Saved {}", doc.title());
                self.set_status(msg);
                if let Some(ws) = &mut self.workspace {
                    ws.refresh();
                }
                true
            }
            Err(e) => {
                self.set_status(format!("Save failed: {e}"));
                false
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let pressed = |m, k| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));

        // Shift variants first: consume_shortcut ignores extra Shift.
        let active = self.active_doc_index();
        if pressed(shift, Key::S) {
            active.map(|i| self.save(i, true));
        } else if pressed(cmd, Key::S) {
            active.map(|i| self.save(i, false));
        }
        if pressed(cmd, Key::N) {
            self.new_file();
        }
        if pressed(cmd, Key::O) {
            self.open_file_dialog();
        }
        if pressed(cmd, Key::W) {
            self.close_active();
        }
        if pressed(cmd, Key::Backslash) {
            self.split_active();
        }
        const GROUP_KEYS: [Key; 9] = [
            Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9,
        ];
        for (i, key) in GROUP_KEYS.into_iter().enumerate() {
            if pressed(cmd, key) {
                self.focus_pane(i);
            }
        }
        if pressed(cmd, Key::B) {
            self.show_sidebar = !self.show_sidebar;
        }
    }

    /// Ctrl+W: close the focused tab, or the focused group if it's already empty.
    fn close_active(&mut self) {
        let pane = &self.panes[self.focused];
        if pane.tabs.is_empty() {
            self.remove_pane(self.focused);
        } else {
            self.request_close(self.focused, pane.active);
        }
    }

    /// Moves all tabs of a group into its left neighbour (right for the first group) and removes it.
    fn join_group(&mut self, idx: usize) {
        if self.panes.len() <= 1 {
            return;
        }
        let target_id = self.panes[if idx == 0 { 1 } else { idx - 1 }].id;
        let tabs = std::mem::take(&mut self.panes[idx].tabs);
        self.remove_pane(idx);
        let target = self.panes.iter().position(|p| p.id == target_id).unwrap_or(0);
        self.focused = target;
        for doc in tabs {
            self.show_in_focused(doc);
        }
    }

    fn split_active(&mut self) {
        let pane = &self.panes[self.focused];
        if !pane.tabs.is_empty() {
            self.split_right(self.focused, pane.active);
        }
    }

    // --------------------------------------------------------------------- UI

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("New File").shortcut_text("Ctrl+N")).clicked() {
                    self.new_file();
                }
                if ui.add(egui::Button::new("Open File…").shortcut_text("Ctrl+O")).clicked() {
                    self.open_file_dialog();
                }
                if ui.button("Open Folder…").clicked() {
                    self.open_folder_dialog();
                }
                ui.separator();
                let active = self.active_doc_index();
                let has_doc = active.is_some();
                if ui.add_enabled(has_doc, egui::Button::new("Save").shortcut_text("Ctrl+S")).clicked() {
                    active.map(|i| self.save(i, false));
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Save As…").shortcut_text("Ctrl+Shift+S"))
                    .clicked()
                {
                    active.map(|i| self.save(i, true));
                }
                ui.separator();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Close Tab").shortcut_text("Ctrl+W"))
                    .clicked()
                {
                    self.close_active();
                }
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                if ui
                    .add(egui::Button::selectable(self.show_sidebar, "Explorer").shortcut_text("Ctrl+B"))
                    .clicked()
                {
                    self.show_sidebar = !self.show_sidebar;
                }
                ui.separator();
                let has_doc = self.active_doc_index().is_some();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Split Editor Right").shortcut_text("Ctrl+\\"))
                    .clicked()
                {
                    self.split_active();
                }
                if ui
                    .add_enabled(self.panes.len() > 1, egui::Button::new("Join Editor Group"))
                    .on_hover_text("Move this group's tabs into the neighbouring group")
                    .clicked()
                {
                    self.join_group(self.focused);
                }
                ui.separator();
                for i in 0..self.panes.len().min(9) {
                    let label = format!("Focus Group {}", i + 1);
                    let btn = egui::Button::selectable(i == self.focused, label).shortcut_text(format!("Ctrl+{}", i + 1));
                    if ui.add(btn).clicked() {
                        self.focus_pane(i);
                    }
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some((msg, at)) = &self.status {
                if at.elapsed().as_secs() < 4 {
                    ui.label(msg);
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let Some(doc) = self.active_doc_index().map(|i| &self.docs[i]) else { return };
                ui.label(doc.language_name());
                ui.separator();
                ui.label("UTF-8");
                ui.separator();
                let id = editor_id(self.panes[self.focused].id, doc.id);
                let cursor = egui::TextEdit::load_state(ui.ctx(), id)
                    .and_then(|s| s.cursor.char_range())
                    .unwrap_or_default();
                let (line, col) = line_col(&doc.text, cursor.primary.index.0);
                let sel = cursor.primary.index.0.abs_diff(cursor.secondary.index.0);
                if sel > 0 {
                    ui.label(format!("Ln {line}, Col {col} ({sel} selected)"));
                } else {
                    ui.label(format!("Ln {line}, Col {col}"));
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let active = self.active_doc_index().and_then(|i| self.docs[i].path.clone());
        let mut to_open = None;
        match &mut self.workspace {
            Some(root) => {
                ui.horizontal(|ui| {
                    ui.strong(root.name.to_uppercase());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("🔄").on_hover_text("Refresh").clicked() {
                            root.refresh();
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    to_open = root.ui(ui, active.as_deref(), 0);
                });
            }
            None => {
                ui.label("No folder open.");
                if ui.button("Open Folder…").clicked() {
                    self.open_folder_dialog();
                }
            }
        }
        if let Some(p) = to_open {
            self.open_path(&p);
        }
    }

    /// All editor groups side by side, with draggable dividers between them.
    fn editor_area(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());
        let n = self.panes.len();
        let content_w = (area.width() - DIVIDER * (n - 1) as f32).max(1.0);

        let mut action = None;
        let mut x = area.left();
        for i in 0..n {
            let w = if i + 1 == n { area.right() - x } else { self.panes[i].width * content_w };
            let rect = Rect::from_min_max(pos2(x, area.top()), pos2(x + w, area.bottom()));
            if let Some(a) = self.pane_ui(ui, i, rect) {
                action = Some((i, a));
            }
            x += w;

            if i + 1 < n {
                let divider = Rect::from_min_max(pos2(x, area.top()), pos2(x + DIVIDER, area.bottom()));
                let resp = ui.interact(divider, ui.scope_id().with(("divider", self.panes[i].id)), Sense::drag());
                let active = resp.hovered() || resp.dragged();
                if active {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                }
                if resp.dragged() {
                    // Only trade width between the two groups touching this divider.
                    let pair = self.panes[i].width + self.panes[i + 1].width;
                    let min = (MIN_PANE_WIDTH / content_w).min(pair / 2.0);
                    let left = (self.panes[i].width + resp.drag_delta().x / content_w).max(min).min(pair - min);
                    self.panes[i].width = left;
                    self.panes[i + 1].width = pair - left;
                }
                let visuals = ui.visuals();
                let stroke = if active {
                    Stroke::new(2.0, visuals.selection.bg_fill)
                } else {
                    visuals.widgets.noninteractive.bg_stroke
                };
                ui.painter().vline(divider.center().x, area.y_range(), stroke);
                x += DIVIDER;
            }
        }

        if let Some((i, action)) = action {
            match action {
                TabAction::Activate(t) => {
                    self.panes[i].active = t;
                    self.focus_pane(i);
                }
                TabAction::Close(t) => self.request_close(i, t),
                TabAction::SplitRight(t) => self.split_right(i, t),
                TabAction::Move(t, dir) => self.move_tab(i, t, dir),
            }
        }
    }

    /// One editor group: its tab bar plus the active document's editor.
    fn pane_ui(&mut self, ui: &mut egui::Ui, idx: usize, rect: Rect) -> Option<TabAction> {
        let pane_id = self.panes[idx].id;
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(("pane", pane_id)));
        ui.set_clip_rect(rect.intersect(ui.clip_rect()));
        let is_focused = idx == self.focused;

        let mut action = None;
        if !self.panes[idx].tabs.is_empty() {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(6, 4))
                .show(&mut ui, |ui| action = self.tab_bar(ui, idx));
            ui.separator();
        }

        match self.panes[idx].active_doc().and_then(|d| self.doc_index(d)) {
            Some(di) => {
                let doc = &mut self.docs[di];
                if is_focused && self.focus_pending {
                    ui.memory_mut(|m| m.request_focus(editor_id(pane_id, doc.id)));
                    self.focus_pending = false;
                }
                if code_editor(&mut ui, doc, pane_id).gained_focus() {
                    self.focused = idx;
                }
            }
            None => {
                if is_focused {
                    self.focus_pending = false;
                }
                let resp = ui.interact(ui.available_rect_before_wrap(), ui.scope_id().with("empty"), Sense::click());
                if resp.clicked() {
                    self.focused = idx;
                }
                welcome(&mut ui);
            }
        }

        // Mark the focused group when there is more than one.
        if is_focused && self.panes.len() > 1 {
            ui.painter().hline(rect.x_range(), rect.top() + 1.0, Stroke::new(2.0, ui.visuals().selection.bg_fill));
        }
        action
    }

    fn tab_bar(&self, ui: &mut egui::Ui, idx: usize) -> Option<TabAction> {
        let pane = &self.panes[idx];
        let can_move_right = idx + 1 < self.panes.len() || pane.tabs.len() > 1;
        let mut action = None;
        egui::ScrollArea::horizontal().id_salt(("tabs", pane.id)).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (t, &doc_id) in pane.tabs.iter().enumerate() {
                    let Some(doc) = self.docs.iter().find(|d| d.id == doc_id) else { continue };
                    let dirty = if doc.is_dirty() { " ●" } else { "" };
                    let resp = ui.add(egui::Button::selectable(t == pane.active, format!("{}{dirty}", doc.title())));
                    if resp.clicked() {
                        action = Some(TabAction::Activate(t));
                    }
                    if resp.middle_clicked() {
                        action = Some(TabAction::Close(t));
                    }
                    let resp = match &doc.path {
                        Some(p) => resp.on_hover_text(p.display().to_string()),
                        None => resp,
                    };
                    resp.context_menu(|ui| {
                        if ui.add(egui::Button::new("Split Right").shortcut_text("Ctrl+\\")).clicked() {
                            action = Some(TabAction::SplitRight(t));
                        }
                        if ui.add_enabled(idx > 0, egui::Button::new("Move to Left Group")).clicked() {
                            action = Some(TabAction::Move(t, -1));
                        }
                        if ui.add_enabled(can_move_right, egui::Button::new("Move to Right Group")).clicked() {
                            action = Some(TabAction::Move(t, 1));
                        }
                        ui.separator();
                        if ui.button("Close").clicked() {
                            action = Some(TabAction::Close(t));
                        }
                    });
                    if ui.small_button("×").clicked() {
                        action = Some(TabAction::Close(t));
                    }
                    ui.add_space(6.0);
                }
            });
        });
        action
    }

    /// "Save changes?" dialog for closing a dirty tab or quitting.
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending else { return };
        let what = match pending {
            Pending::CloseTab { doc, .. } => match self.doc_index(doc) {
                Some(i) => format!("Do you want to save the changes you made to {}?", self.docs[i].title()),
                None => {
                    self.pending = None;
                    return;
                }
            },
            Pending::Quit => "You have unsaved changes. Save them before quitting?".to_owned(),
        };

        let mut choice = None;
        egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(what);
            ui.label(egui::RichText::new("Your changes will be lost if you don't save them.").weak());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some(true);
                }
                if ui.button("Don't Save").clicked() {
                    choice = Some(false);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.pending = None;
                }
            });
        });

        let Some(save) = choice else { return };
        self.pending = None;
        match pending {
            Pending::CloseTab { pane, doc } => {
                let Some(di) = self.doc_index(doc) else { return };
                if !save || self.save(di, false) {
                    let pane = self.panes.iter().position(|p| p.id == pane);
                    let tab = pane.and_then(|p| self.panes[p].tabs.iter().position(|&d| d == doc));
                    if let (Some(pane), Some(tab)) = (pane, tab) {
                        self.close_tab(pane, tab);
                    }
                }
            }
            Pending::Quit => {
                let all_saved = !save
                    || (0..self.docs.len()).all(|i| !self.docs[i].is_dirty() || self.save(i, false));
                if all_saved {
                    self.allow_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.active_doc_index().map(|i| &self.docs[i]) {
            Some(d) => format!("{}{} — rsedit", if d.is_dirty() { "● " } else { "" }, d.title()),
            None => "rsedit".to_owned(),
        };
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
    }
}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_quit
            && self.docs.iter().any(|d| d.is_dirty())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending = Some(Pending::Quit);
        }

        if self.pending.is_none() {
            self.handle_shortcuts(&ctx);
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.show_sidebar {
            egui::Panel::left("explorer")
                .resizable(true)
                .default_size(230.0)
                .size_range(140.0..=500.0)
                .show(ui, |ui| self.sidebar(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(0))
            .show(ui, |ui| self.editor_area(ui));

        self.confirm_dialog(&ctx);
        self.update_title(&ctx);
    }
}

/// Each group gets its own editor state (cursor, scroll) even when showing the same document.
fn editor_id(pane_id: u64, doc_id: u64) -> egui::Id {
    egui::Id::unique(("editor", pane_id, doc_id))
}

/// Line-numbered, syntax-highlighted editor filling the remaining space of `ui`.
fn code_editor(ui: &mut egui::Ui, doc: &mut Document, pane_id: u64) -> egui::Response {
    let font_id = FontId::monospace(FONT_SIZE);
    let digit_w = ui.fonts_mut(|f| f.glyph_width(&font_id, '0'));
    let line_count = doc.text.bytes().filter(|&b| b == b'\n').count() + 1;
    let gutter_w = line_count.to_string().len().max(3) as f32 * digit_w + 24.0;

    // Split the available space into a fixed gutter and a scrolling editor.
    let avail = ui.available_rect_before_wrap();
    ui.allocate_rect(avail, Sense::hover());
    let gutter_rect = Rect::from_min_size(avail.min, vec2(gutter_w, avail.height()));
    let editor_rect = Rect::from_min_max(pos2(gutter_rect.right(), avail.top()), avail.max);

    let lang = doc.syntax_token();
    let dark = ui.visuals().dark_mode;
    let Document { text, syntax, id: doc_id, .. } = doc;
    let mut highlight_done = true;
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let (mut job, done) = syntax.layout_job(buf.as_str(), &lang, dark, &font_id, HIGHLIGHT_BUDGET);
        highlight_done = done;
        job.wrap.max_width = f32::INFINITY; // no soft wrapping
        ui.fonts_mut(|f| f.layout_job(job))
    };

    let id = editor_id(pane_id, *doc_id);
    let mut editor_ui = ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));
    let (output, line_bg) = egui::ScrollArea::both()
        .id_salt(("editor-scroll", pane_id, *doc_id))
        .auto_shrink(false)
        .show(&mut editor_ui, |ui| {
            // Reserve a slot under the text for the current-line highlight.
            let line_bg = ui.painter().add(egui::Shape::Noop);
            let output = egui::TextEdit::multiline(text)
                .id(id)
                .font(font_id.clone())
                .code_editor()
                .lock_focus(true)
                .frame(egui::Frame::NONE)
                .margin(egui::Margin::symmetric(8, 0))
                .desired_width(f32::INFINITY)
                .min_size(ui.available_size())
                .layouter(&mut layouter)
                .show(ui);
            (output, line_bg)
        })
        .inner;
    if !highlight_done {
        ui.ctx().request_repaint();
    }

    // Current line.
    let cursor = output.cursor_range.unwrap_or(CCursorRange::one(CCursor::default()));
    let (cur_line, _) = line_col(text, cursor.primary.index.0);
    // No soft wrap, so galley row N is exactly line N+1.
    let rows = &output.galley.rows;
    let row_rect = |n: usize| rows[n.min(rows.len() - 1)].rect().translate(output.galley_pos.to_vec2());
    let cur = row_rect(cur_line - 1);
    let line_rect = Rect::from_x_y_ranges(editor_rect.x_range(), cur.y_range());
    let visuals = ui.visuals();
    let line_fill = if visuals.dark_mode {
        Color32::from_white_alpha(8)
    } else {
        Color32::from_black_alpha(10)
    };
    let response = output.response.response;
    if response.has_focus() {
        ui.painter().set(line_bg, egui::Shape::rect_filled(line_rect, 0.0, line_fill));
    }

    // Gutter with line numbers, aligned to the scrolled text.
    let painter = ui.painter_at(gutter_rect);
    painter.rect_filled(gutter_rect, 0.0, visuals.extreme_bg_color);
    let first = rows.partition_point(|r| r.rect().max.y + output.galley_pos.y < gutter_rect.top());
    for n in first..rows.len() {
        let y = row_rect(n).top();
        if y > gutter_rect.bottom() {
            break;
        }
        let color = if n + 1 == cur_line {
            visuals.strong_text_color()
        } else {
            visuals.weak_text_color()
        };
        painter.text(
            pos2(gutter_rect.right() - 12.0, y),
            egui::Align2::RIGHT_TOP,
            (n + 1).to_string(),
            font_id.clone(),
            color,
        );
    }
    response
}

fn welcome(ui: &mut egui::Ui) {
    ui.centered_and_justified(|ui| {
        ui.label(
            egui::RichText::new(
                "rsedit\n\nCtrl+N  New File\nCtrl+O  Open File\nCtrl+S  Save\nCtrl+W  Close Tab\nCtrl+\\  Split Editor\nCtrl+B  Toggle Explorer",
            )
            .size(16.0)
            .weak(),
        );
    });
}
