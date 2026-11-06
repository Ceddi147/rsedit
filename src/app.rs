use std::path::{Path, PathBuf};
use std::time::Instant;

use egui::text::{CCursor, CCursorRange};
use egui::{Color32, FontId, Key, KeyboardShortcut, Modifiers, Rect, Sense, vec2};
use egui_extras::syntax_highlighting::{CodeTheme, highlight};

use crate::document::{Document, line_col};
use crate::explorer::Node;

const FONT_SIZE: f32 = 14.0;

/// Something that needs a "Save changes?" answer before it can happen.
#[derive(Clone, Copy)]
enum Pending {
    CloseTab(usize),
    Quit,
}

pub struct EditorApp {
    docs: Vec<Document>,
    active: usize,
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
        let mut app = Self {
            docs: Vec::new(),
            active: 0,
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

    fn new_file(&mut self) {
        self.untitled_counter += 1;
        self.docs.push(Document::new_untitled(self.untitled_counter));
        self.active = self.docs.len() - 1;
    }

    fn open_path(&mut self, path: &Path) {
        if let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path)) {
            self.active = i;
            return;
        }
        match Document::open(path) {
            Ok(doc) => {
                self.docs.push(doc);
                self.active = self.docs.len() - 1;
            }
            Err(e) => self.set_status(format!("Could not open {}: {e}", path.display())),
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

    fn request_close(&mut self, idx: usize) {
        if self.docs.get(idx).is_some_and(|d| d.is_dirty()) {
            self.pending = Some(Pending::CloseTab(idx));
        } else {
            self.close(idx);
        }
    }

    fn close(&mut self, idx: usize) {
        if idx >= self.docs.len() {
            return;
        }
        self.docs.remove(idx);
        if self.active > idx || self.active >= self.docs.len() {
            self.active = self.active.saturating_sub(1);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let pressed = |m, k| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));

        // Shift variants first: consume_shortcut ignores extra Shift.
        if pressed(shift, Key::S) {
            self.save(self.active, true);
        } else if pressed(cmd, Key::S) {
            self.save(self.active, false);
        }
        if pressed(cmd, Key::N) {
            self.new_file();
        }
        if pressed(cmd, Key::O) {
            self.open_file_dialog();
        }
        if pressed(cmd, Key::W) && !self.docs.is_empty() {
            self.request_close(self.active);
        }
        if pressed(cmd, Key::B) {
            self.show_sidebar = !self.show_sidebar;
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
                let has_doc = !self.docs.is_empty();
                if ui.add_enabled(has_doc, egui::Button::new("Save").shortcut_text("Ctrl+S")).clicked() {
                    self.save(self.active, false);
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Save As…").shortcut_text("Ctrl+Shift+S"))
                    .clicked()
                {
                    self.save(self.active, true);
                }
                ui.separator();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Close Tab").shortcut_text("Ctrl+W"))
                    .clicked()
                {
                    self.request_close(self.active);
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
                let Some(doc) = self.docs.get(self.active) else { return };
                ui.label(doc.language_name());
                ui.separator();
                ui.label("UTF-8");
                ui.separator();
                let id = editor_id(doc);
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
        let active = self.docs.get(self.active).and_then(|d| d.path.clone());
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

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        egui::ScrollArea::horizontal().id_salt("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, doc) in self.docs.iter().enumerate() {
                    let dirty = if doc.is_dirty() { " ●" } else { "" };
                    let resp = ui.add(egui::Button::selectable(i == self.active, format!("{}{dirty}", doc.title())));
                    if resp.clicked() {
                        self.active = i;
                    }
                    if resp.middle_clicked() {
                        close = Some(i);
                    }
                    if let Some(p) = &doc.path {
                        resp.on_hover_text(p.display().to_string());
                    }
                    if ui.small_button("×").clicked() {
                        close = Some(i);
                    }
                    ui.add_space(6.0);
                }
            });
        });
        if let Some(i) = close {
            self.request_close(i);
        }
    }

    fn editor(&mut self, ui: &mut egui::Ui) {
        let Some(doc) = self.docs.get_mut(self.active) else {
            welcome(ui);
            return;
        };

        let font_id = FontId::monospace(FONT_SIZE);
        let digit_w = ui.fonts_mut(|f| f.glyph_width(&font_id, '0'));
        let line_count = doc.text.bytes().filter(|&b| b == b'\n').count() + 1;
        let gutter_w = line_count.to_string().len().max(3) as f32 * digit_w + 24.0;

        // Split the available space into a fixed gutter and a scrolling editor.
        let avail = ui.available_rect_before_wrap();
        ui.allocate_rect(avail, Sense::hover());
        let gutter_rect = Rect::from_min_size(avail.min, vec2(gutter_w, avail.height()));
        let editor_rect = Rect::from_min_max(egui::pos2(gutter_rect.right(), avail.top()), avail.max);

        let theme = CodeTheme::from_style(ui.style()).with_font_size(FONT_SIZE);
        let lang = doc.syntax_token();
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
            let mut job = highlight(ui.ctx(), ui.style(), &theme, buf.as_str(), &lang);
            job.wrap.max_width = f32::INFINITY; // no soft wrapping
            ui.fonts_mut(|f| f.layout_job(job))
        };

        let id = editor_id(doc);
        let mut editor_ui = ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));
        let (output, line_bg) = egui::ScrollArea::both()
            .id_salt(("editor-scroll", doc.id))
            .auto_shrink(false)
            .show(&mut editor_ui, |ui| {
                // Reserve a slot under the text for the current-line highlight.
                let line_bg = ui.painter().add(egui::Shape::Noop);
                let output = egui::TextEdit::multiline(&mut doc.text)
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

        // Current line.
        let cursor = output.cursor_range.unwrap_or(CCursorRange::one(CCursor::default()));
        let (cur_line, _) = line_col(&doc.text, cursor.primary.index.0);
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
        if output.response.response.has_focus() {
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
                egui::pos2(gutter_rect.right() - 12.0, y),
                egui::Align2::RIGHT_TOP,
                (n + 1).to_string(),
                font_id.clone(),
                color,
            );
        }
    }

    /// "Save changes?" dialog for closing a dirty tab or quitting.
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending else { return };
        let what = match pending {
            Pending::CloseTab(i) => match self.docs.get(i) {
                Some(d) => format!("Do you want to save the changes you made to {}?", d.title()),
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
            Pending::CloseTab(i) => {
                if !save || self.save(i, false) {
                    self.close(i);
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
        let title = match self.docs.get(self.active) {
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
            .show(ui, |ui| {
                if !self.docs.is_empty() {
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(6, 4))
                        .show(ui, |ui| self.tab_bar(ui));
                    ui.separator();
                }
                self.editor(ui);
            });

        self.confirm_dialog(&ctx);
        self.update_title(&ctx);
    }
}

fn editor_id(doc: &Document) -> egui::Id {
    egui::Id::unique(("editor", doc.id))
}

fn welcome(ui: &mut egui::Ui) {
    ui.centered_and_justified(|ui| {
        ui.label(
            egui::RichText::new(
                "rsedit\n\nCtrl+N  New File\nCtrl+O  Open File\nCtrl+S  Save\nCtrl+W  Close Tab\nCtrl+B  Toggle Explorer",
            )
            .size(16.0)
            .weak(),
        );
    });
}
