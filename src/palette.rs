//! The quick-open / command palette overlay (Ctrl+P, Ctrl+Shift+P, Ctrl+G).
//! The first character picks the mode, like VSCode: `>` commands, `:` go to line,
//! anything else searches file names.

use egui::{Key, Rect, RichText, pos2};

use crate::commands::Command;
use crate::search::{FileIndex, fuzzy_filter, fuzzy_score};

const ROWS: usize = 14;

pub enum Pick {
    /// A workspace file, relative to the index root.
    File(String),
    Command(Command),
    /// 1-based line and optional column.
    Line(usize, Option<usize>),
}

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    query: String,
    selected: usize,
    just_opened: bool,
    /// File results for (query, number of indexed files).
    cache: Option<(String, usize, Vec<usize>)>,
}

enum Item {
    File(usize),
    Command(Command),
    Line(usize, Option<usize>),
    Note(String),
}

impl Palette {
    pub fn show_with(&mut self, prefix: &str) {
        self.open = true;
        self.query = prefix.to_owned();
        self.selected = 0;
        self.just_opened = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.cache = None; // don't hold on to results
    }

    /// Draws the palette over `area` (the editor area) and returns what was picked.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        area: Rect,
        files: Option<&FileIndex>,
    ) -> Option<Pick> {
        if !self.open {
            return None;
        }
        let (up, down, enter, escape) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, Key::Enter),
                i.consume_key(egui::Modifiers::NONE, Key::Escape),
            )
        });
        if escape {
            self.close();
            return None;
        }

        let items = self.items(files);
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        if down {
            self.selected = (self.selected + 1).min(items.len().saturating_sub(1));
        }
        self.selected = self.selected.min(items.len().saturating_sub(1));

        let width = (area.width() - 40.0).clamp(200.0, 640.0);
        let mut picked = None;
        let mut clicked_row = None;
        let resp = egui::Area::new(egui::Id::unique("palette"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(area.center().x - width / 2.0, area.top() + 6.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(width);
                    let hint = "Search files by name (> commands, : go to line)";
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .desired_width(f32::INFINITY)
                            .hint_text(hint),
                    );
                    if self.just_opened {
                        edit.request_focus();
                        // Put the caret after the prefix.
                        let mut state =
                            egui::TextEdit::load_state(ctx, edit.id).unwrap_or_default();
                        let end = egui::text::CCursor::new(self.query.chars().count());
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(end)));
                        state.store(ctx, edit.id);
                        self.just_opened = false;
                    }
                    if edit.changed() {
                        self.selected = 0;
                    }
                    ui.add_space(4.0);
                    for (row, item) in items.iter().enumerate().take(ROWS) {
                        let selected = row == self.selected;
                        let resp = match item {
                            Item::File(i) => {
                                let path = files
                                    .map(|f| f.with_files(|all| all[*i].clone()))
                                    .unwrap_or_default();
                                let (dir, name) = match path.rfind('/') {
                                    Some(p) => (&path[..p], &path[p + 1..]),
                                    None => ("", path.as_str()),
                                };
                                let mut job = egui::text::LayoutJob::default();
                                let style = ui.style();
                                RichText::new(name).strong().append_to(
                                    &mut job,
                                    style,
                                    egui::FontSelection::Default,
                                    egui::Align::Center,
                                );
                                RichText::new(format!("   {dir}")).weak().small().append_to(
                                    &mut job,
                                    style,
                                    egui::FontSelection::Default,
                                    egui::Align::Center,
                                );
                                ui.add(
                                    egui::Button::selectable(selected, job)
                                        .min_size(egui::vec2(width, 0.0)),
                                )
                            }
                            Item::Command(c) => ui.add(
                                egui::Button::selectable(selected, c.label())
                                    .shortcut_text(c.shortcut_text())
                                    .min_size(egui::vec2(width, 0.0)),
                            ),
                            Item::Line(l, col) => {
                                let text = match col {
                                    Some(c) => format!("Go to line {l}, column {c}"),
                                    None => format!("Go to line {l}"),
                                };
                                ui.add(
                                    egui::Button::selectable(selected, text)
                                        .min_size(egui::vec2(width, 0.0)),
                                )
                            }
                            Item::Note(t) => ui.label(RichText::new(t).weak()),
                        };
                        if selected && (up || down) {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            clicked_row = Some(row);
                        }
                    }
                    if items.len() > ROWS {
                        ui.label(
                            RichText::new(format!("{} more…", items.len() - ROWS))
                                .weak()
                                .small(),
                        );
                    }
                });
            })
            .response;

        let row = clicked_row.or(enter.then_some(self.selected));
        if let Some(row) = row {
            picked = match items.get(row) {
                Some(Item::File(i)) => {
                    files.map(|f| Pick::File(f.with_files(|all| all[*i].clone())))
                }
                Some(Item::Command(c)) => Some(Pick::Command(*c)),
                Some(Item::Line(l, c)) => Some(Pick::Line(*l, *c)),
                _ => None,
            };
            if picked.is_some() {
                self.close();
            }
        }
        // Clicking elsewhere closes it.
        if ctx.input(|i| i.pointer.any_pressed()) && !resp.contains_pointer() && picked.is_none() {
            self.close();
        }
        picked
    }

    fn items(&mut self, files: Option<&FileIndex>) -> Vec<Item> {
        if let Some(q) = self.query.strip_prefix('>') {
            let q = q.trim();
            let mut scored: Vec<(i64, Command)> = Command::ALL
                .iter()
                .filter_map(|c| fuzzy_score(q, c.label()).map(|s| (s, *c)))
                .collect();
            if !q.is_empty() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            return scored.into_iter().map(|(_, c)| Item::Command(c)).collect();
        }
        if let Some(q) = self.query.strip_prefix(':') {
            let mut parts = q.trim().splitn(2, [':', ',']);
            let line = parts.next().and_then(|s| s.trim().parse::<usize>().ok());
            let col = parts.next().and_then(|s| s.trim().parse::<usize>().ok());
            return match line {
                Some(l) if l > 0 => vec![Item::Line(l, col)],
                _ => vec![Item::Note("Type a line number, optionally :column".into())],
            };
        }
        let Some(index) = files else {
            return vec![Item::Note("Open a folder to search its files".into())];
        };
        let count = index.with_files(|f| f.len());
        let stale = self
            .cache
            .as_ref()
            .is_none_or(|(q, n, _)| *q != self.query || *n != count);
        if stale {
            let results = index.with_files(|all| fuzzy_filter(self.query.trim(), all, 200));
            self.cache = Some((self.query.clone(), count, results));
        }
        let results = &self.cache.as_ref().expect("set above").2;
        let mut items: Vec<Item> = results.iter().map(|&i| Item::File(i)).collect();
        if items.is_empty() {
            items.push(Item::Note(if index.is_done() {
                "No matching files".into()
            } else {
                "Indexing files…".into()
            }));
        }
        items
    }
}
