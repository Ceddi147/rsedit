//! The completion popup: suggestions from a language server, merged with words
//! from the document and language keywords (which also work without a server).

use std::collections::HashSet;

use egui::{Color32, Key, Modifiers, Rect, RichText, pos2, vec2};
use ropey::Rope;

use crate::buffer::{Edit, EditKind, Selection, Selections};
use crate::lsp::{CompletionItem, ServerKey};
use crate::ops::Change;
use crate::search::fuzzy_score;

/// Lines around the caret scanned for words.
const WORD_SCAN_LINES: usize = 1500;
const MAX_SHOWN: usize = 200;
const ROWS: usize = 10;

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "Some", "None", "Ok",
    "Err", "String", "Vec", "Option", "Result", "Box", "println!", "format!", "vec!", "assert_eq!",
];
const CPP_KEYWORDS: &[&str] = &[
    "auto", "bool", "break", "case", "catch", "char", "class", "const", "constexpr", "continue", "default", "delete",
    "do", "double", "else", "enum", "explicit", "extern", "false", "float", "for", "friend", "if", "inline", "int",
    "long", "namespace", "new", "noexcept", "nullptr", "operator", "override", "private", "protected", "public",
    "return", "short", "signed", "sizeof", "static", "static_cast", "struct", "switch", "template", "this", "throw",
    "true", "try", "typedef", "typename", "union", "unsigned", "using", "virtual", "void", "volatile", "while",
    "std", "size_t", "string", "vector", "unique_ptr", "shared_ptr", "include", "define", "ifndef", "endif",
];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Server,
    Keyword,
    Word,
}

struct Item {
    label: String,
    detail: String,
    kind: u8,
    insert: String,
    filter: String,
    sort: String,
    /// Where the server wants the text to go (start, as a char position).
    replace_start: Option<usize>,
    additional: Vec<Edit>,
    source: Source,
}

pub fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Start of the identifier that ends at `pos`.
pub fn word_start(rope: &Rope, mut pos: usize) -> usize {
    while pos > 0 && is_ident(rope.char(pos - 1)) {
        pos -= 1;
    }
    pos
}

/// Whether typing `c` (after `prev`) should open member completion.
pub fn is_trigger(c: char, prev: Option<char>) -> bool {
    c == '.' || (c == ':' && prev == Some(':')) || (c == '>' && prev == Some('-'))
}

/// Char position of a server position (line, column in chars or UTF-16 units).
pub fn char_pos(rope: &Rope, (line, col): (usize, usize), utf16: bool) -> usize {
    let line = line.min(rope.len_lines() - 1);
    let start = rope.line_to_char(line);
    let len = crate::buffer::line_len(rope, line);
    if !utf16 {
        return start + col.min(len);
    }
    let mut units = 0;
    for (i, c) in rope.line(line).chars().take(len).enumerate() {
        if units >= col {
            return start + i;
        }
        units += c.len_utf16();
    }
    start + len
}

#[derive(Default)]
pub struct Completion {
    pub open: bool,
    pub doc: u64,
    pub view: u64,
    /// Where the word being completed starts.
    pub start: usize,
    prefix: String,
    items: Vec<Item>,
    shown: Vec<usize>,
    selected: usize,
    /// The latest completion request, whose answer we're waiting for.
    pub request: Option<(ServerKey, i64)>,
    /// The server said its list depends on more typing; ask again as the prefix grows.
    pub incomplete: bool,
}

pub enum KeyResult {
    None,
    Accept,
    Close,
}

impl Completion {
    pub fn close(&mut self) {
        *self = Self::default(); // also frees the items
    }

    /// Starts a completion session at `start`, with local suggestions if `words`.
    pub fn begin(&mut self, doc: u64, view: u64, start: usize, rope: &Rope, caret: usize, lang: &str, words: bool) {
        self.close();
        self.open = true;
        self.doc = doc;
        self.view = view;
        self.start = start;
        if words {
            self.add_local(rope, caret, lang);
        }
        self.update_prefix(rope, caret);
    }

    fn add_local(&mut self, rope: &Rope, caret: usize, lang: &str) {
        let keywords = match lang {
            "rs" => RUST_KEYWORDS,
            "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh" => CPP_KEYWORDS,
            _ => &[],
        };
        let mut seen: HashSet<String> = HashSet::new();
        for k in keywords {
            seen.insert(k.to_string());
            self.items.push(local_item(k, 14, Source::Keyword));
        }
        // Identifiers near the caret (skipping the one being typed).
        let line = rope.char_to_line(caret);
        let from = rope.line_to_char(line.saturating_sub(WORD_SCAN_LINES));
        let to = rope.line_to_char((line + WORD_SCAN_LINES).min(rope.len_lines() - 1));
        let typing = word_start(rope, caret)..caret;
        let mut word = String::new();
        let mut word_at = from;
        for (i, c) in rope.slice(from..to).chars().enumerate().chain(std::iter::once((to - from, ' '))) {
            if is_ident(c) {
                if word.is_empty() {
                    word_at = from + i;
                }
                word.push(c);
                continue;
            }
            let starts_alpha = word.chars().next().is_some_and(|c| !c.is_ascii_digit());
            if word.chars().count() >= 3 && starts_alpha && word_at != typing.start && !seen.contains(&word) {
                seen.insert(word.clone());
                self.items.push(local_item(&word, 1, Source::Word));
            }
            word.clear();
        }
    }

    /// Replaces server suggestions with a new answer.
    pub fn set_server_items(&mut self, items: Vec<CompletionItem>, incomplete: bool, rope: &Rope, utf16: bool) {
        self.items.retain(|i| i.source != Source::Server);
        self.incomplete = incomplete;
        let pos = |p| char_pos(rope, p, utf16);
        for it in items {
            self.items.push(Item {
                replace_start: it.replace.map(|(s, _)| pos(s)),
                additional: it.additional.iter().map(|(s, e, t)| Edit::new(pos(*s)..pos(*e), t.clone())).collect(),
                label: it.label,
                detail: it.detail,
                kind: it.kind,
                insert: it.insert,
                filter: it.filter,
                sort: it.sort,
                source: Source::Server,
            });
        }
        // With good server results, plain words are noise (keywords stay for the
        // cases a server doesn't cover).
        if self.items.iter().any(|i| i.source == Source::Server) {
            self.items.retain(|i| i.source != Source::Word);
        }
        self.refilter();
    }

    /// Follows the caret. Returns false if the session should end (caret left the word).
    pub fn update_prefix(&mut self, rope: &Rope, caret: usize) -> bool {
        if caret < self.start || rope.char_to_line(caret) != rope.char_to_line(self.start) {
            return false;
        }
        let prefix: String = rope.slice(self.start..caret).to_string();
        if !prefix.chars().all(is_ident) {
            return false;
        }
        if prefix != self.prefix || self.shown.is_empty() {
            self.prefix = prefix;
            self.refilter();
        }
        true
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(i64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.label != self.prefix || it.source == Source::Server)
            .filter_map(|(i, it)| Some((fuzzy_score(&self.prefix, &it.filter)?, i)))
            .collect();
        let items = &self.items;
        let prefix = &self.prefix;
        // Prefix matches first, then server suggestions over keywords over words,
        // then how well it matches, then the server's own ranking.
        scored.sort_by(|a, b| {
            let (x, y) = (&items[a.1], &items[b.1]);
            let (px, py) = (x.filter.starts_with(prefix.as_str()), y.filter.starts_with(prefix.as_str()));
            py.cmp(&px).then(x.source.cmp(&y.source)).then(b.0.cmp(&a.0)).then_with(|| x.sort.cmp(&y.sort))
        });
        if self.prefix.is_empty() {
            // Nothing typed yet: keep the server's order.
            scored.sort_by(|a, b| {
                let (x, y) = (&items[a.1], &items[b.1]);
                x.source.cmp(&y.source).then_with(|| x.sort.cmp(&y.sort))
            });
        }
        self.shown = scored.into_iter().take(MAX_SHOWN).map(|(_, i)| i).collect();
        self.selected = 0;
    }

    /// Labels currently offered, best first.
    #[cfg(test)]
    pub fn labels(&self) -> Vec<String> {
        self.shown.iter().map(|&i| self.items[i].label.clone()).collect()
    }

    pub fn has_items(&self) -> bool {
        !self.shown.is_empty()
    }

    /// Up/Down/Enter/Tab/Escape go to the popup while it shows suggestions.
    pub fn handle_keys(&mut self, ctx: &egui::Context) -> KeyResult {
        if !self.open {
            return KeyResult::None;
        }
        let none = Modifiers::NONE;
        if ctx.input_mut(|i| i.consume_key(none, Key::Escape)) {
            return KeyResult::Close;
        }
        if self.shown.is_empty() {
            return KeyResult::None;
        }
        let n = self.shown.len();
        ctx.input_mut(|i| {
            if i.consume_key(none, Key::ArrowDown) {
                self.selected = (self.selected + 1) % n;
            }
            if i.consume_key(none, Key::ArrowUp) {
                self.selected = (self.selected + n - 1) % n;
            }
            if i.consume_key(none, Key::PageDown) {
                self.selected = (self.selected + ROWS).min(n - 1);
            }
            if i.consume_key(none, Key::PageUp) {
                self.selected = self.selected.saturating_sub(ROWS);
            }
        });
        if ctx.input_mut(|i| i.consume_key(none, Key::Enter) || i.consume_key(none, Key::Tab)) {
            return KeyResult::Accept;
        }
        KeyResult::None
    }

    /// The edits that insert the selected suggestion at every caret.
    pub fn accept(&self, rope: &Rope, sels: &Selections) -> Option<Change> {
        let item = &self.items[*self.shown.get(self.selected)?];
        let primary = sels.primary().head;
        let start = item.replace_start.filter(|s| *s <= primary).unwrap_or(self.start);
        let typed = primary - start;
        let mut text = item.insert.clone();
        // Functions get their parentheses, with the caret inside if they take arguments.
        let mut back = 0;
        let next = (primary < rope.len_chars()).then(|| rope.char(primary));
        if matches!(item.kind, 2..=4) && !text.contains('(') && next != Some('(') && item.source == Source::Server {
            text.push_str("()");
            if takes_args(item) {
                back = 1;
            }
        }
        let n = text.chars().count();
        let mut edits = Vec::new();
        let mut after = Vec::new();
        for s in sels.iter() {
            // Other carets get the same completion if they have the same word typed.
            let ok = s.is_empty() && s.head >= typed && rope.slice(s.head - typed..s.head) == rope.slice(start..primary);
            if ok {
                edits.push(Edit::new(s.head - typed..s.head, text.clone()));
            }
        }
        edits.extend(item.additional.iter().cloned());
        let edits = crate::buffer::sort_edits(edits);
        let starts = crate::buffer::new_starts(&edits);
        let mut primary_idx = 0;
        for (e, st) in edits.iter().zip(starts) {
            if e.text == text && e.range.end <= rope.len_chars() && sels.iter().any(|s| s.head == e.range.end) {
                if e.range.end == primary {
                    primary_idx = after.len();
                }
                after.push(Selection::caret(st + n - back));
            }
        }
        if after.is_empty() {
            return None;
        }
        Some(Change { edits, after: Selections::from_vec(after, primary_idx), kind: EditKind::Other })
    }

    /// Draws the popup below the caret. Returns true if a row was clicked (accept it).
    pub fn ui(&mut self, ctx: &egui::Context, caret: Rect, loading: bool) -> bool {
        if !self.open || (self.shown.is_empty() && !loading) {
            return false;
        }
        let mut clicked = false;
        let width = 460.0;
        let screen = ctx.content_rect();
        let row_h = 20.0;
        let height = row_h * self.shown.len().clamp(1, ROWS) as f32 + 12.0;
        // Below the caret, or above it when there's no room.
        let y = if caret.bottom() + height < screen.bottom() { caret.bottom() + 2.0 } else { caret.top() - height - 2.0 };
        let x = caret.left().min(screen.right() - width - 4.0).max(screen.left());
        egui::Area::new(egui::Id::unique("completion"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(x, y))
            .interactable(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).inner_margin(4).show(ui, |ui| {
                    ui.set_width(width);
                    if self.shown.is_empty() {
                        ui.label(RichText::new("Loading…").weak());
                        return;
                    }
                    let first = self.selected.saturating_sub(ROWS - 1).min(self.shown.len().saturating_sub(ROWS));
                    for (row, &i) in self.shown.iter().enumerate().skip(first).take(ROWS) {
                        let it = &self.items[i];
                        let selected = row == self.selected;
                        let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), egui::Sense::click());
                        if selected {
                            ui.painter().rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
                        }
                        let (tag, color) = kind_tag(it.kind, it.source);
                        let mono = egui::FontId::monospace(12.0);
                        let p = ui.painter();
                        p.text(pos2(rect.left() + 6.0, rect.center().y), egui::Align2::LEFT_CENTER, tag, mono.clone(), color);
                        let label_galley = p.layout_no_wrap(it.label.clone(), egui::FontId::monospace(13.0), ui.visuals().strong_text_color());
                        let label_w = label_galley.size().x;
                        p.galley(pos2(rect.left() + 40.0, rect.center().y - label_galley.size().y / 2.0), label_galley, Color32::PLACEHOLDER);
                        // Detail, right-aligned and clipped so it never covers the label.
                        let detail_left = rect.left() + 52.0 + label_w;
                        if !it.detail.is_empty() && detail_left < rect.right() - 20.0 {
                            let clip = Rect::from_min_max(pos2(detail_left, rect.top()), rect.max);
                            ui.painter().with_clip_rect(clip).text(
                                pos2(rect.right() - 6.0, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                &it.detail,
                                egui::FontId::proportional(12.0),
                                ui.visuals().weak_text_color(),
                            );
                        }
                        if resp.clicked() {
                            self.selected = row;
                            clicked = true;
                        }
                    }
                });
            });
        clicked
    }
}

fn local_item(word: &str, kind: u8, source: Source) -> Item {
    Item {
        label: word.to_owned(),
        detail: String::new(),
        kind,
        insert: word.to_owned(),
        filter: word.to_owned(),
        sort: word.to_owned(),
        replace_start: None,
        additional: Vec::new(),
        source,
    }
}

/// Whether a function suggestion takes arguments (so the caret goes inside the parens).
fn takes_args(it: &Item) -> bool {
    let sig = if it.label.contains('(') { &it.label } else { &it.detail };
    let Some(open) = sig.find('(') else { return true };
    let close = sig[open..].find(')').map_or(sig.len(), |c| open + c);
    let args = sig[open + 1..close].trim();
    let args = args.trim_start_matches("&mut self").trim_start_matches("&self").trim_start_matches("self");
    !args.trim_start_matches(',').trim().is_empty() && args.trim() != "void"
}

/// Short tag and colour for a suggestion kind, shown at the start of each row.
fn kind_tag(kind: u8, source: Source) -> (&'static str, Color32) {
    if source == Source::Word {
        return ("abc", Color32::GRAY);
    }
    match kind {
        2 | 3 | 4 => ("fn", Color32::from_rgb(220, 180, 255)),
        5 | 10 => ("fld", Color32::from_rgb(120, 190, 255)),
        6 | 12 => ("var", Color32::from_rgb(120, 190, 255)),
        7 | 22 => ("st", Color32::from_rgb(240, 200, 100)),
        8 => ("trt", Color32::from_rgb(140, 220, 200)),
        9 => ("mod", Color32::from_rgb(200, 200, 200)),
        13 => ("enm", Color32::from_rgb(240, 200, 100)),
        20 => ("vnt", Color32::from_rgb(240, 200, 100)),
        14 => ("kw", Color32::from_rgb(230, 130, 130)),
        15 => ("snp", Color32::GRAY),
        21 => ("cst", Color32::from_rgb(120, 190, 255)),
        25 => ("T", Color32::from_rgb(140, 220, 200)),
        _ => ("•", Color32::GRAY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn session(text: &str, caret: usize) -> (Rope, Completion) {
        let rope = Rope::from_str(text);
        let mut c = Completion::default();
        c.begin(1, 1, word_start(&rope, caret), &rope, caret, "rs", true);
        (rope, c)
    }

    fn labels(c: &Completion) -> Vec<&str> {
        c.shown.iter().map(|&i| c.items[i].label.as_str()).collect()
    }

    #[test]
    fn local_words_and_keywords() {
        let text = "let counter = 1;\nlet total = cou";
        let (_, c) = session(text, text.len());
        assert_eq!(labels(&c)[0], "counter", "{:?}", &labels(&c)[..3]);
        let (_, c) = session("fn main() { ma", 14);
        assert!(labels(&c).contains(&"match"));
        assert!(!labels(&c).contains(&"ma"), "the word being typed isn't suggested");
    }

    #[test]
    fn server_items_replace_words_and_accept_inserts_parens() {
        let text = "fn f(v: Vec<u8>) { v.pu }";
        let (rope, mut c) = session(text, 23);
        let items = vec![
            CompletionItem { label: "push".into(), kind: 2, insert: "push".into(), filter: "push".into(), sort: "1".into(), detail: "fn(&mut self, value: T)".into(), ..Default::default() },
            CompletionItem { label: "len".into(), kind: 2, insert: "len".into(), filter: "len".into(), sort: "2".into(), detail: "fn(&self) -> usize".into(), ..Default::default() },
        ];
        c.set_server_items(items, false, &rope, false);
        assert_eq!(labels(&c)[0], "push");
        let mut b = Buffer::new(text);
        let sels = Selections::single(Selection::caret(23));
        let change = c.accept(&rope, &sels).unwrap();
        let after = change.after.clone();
        b.edit(change.edits, sels, after.clone(), change.kind);
        assert_eq!(b.rope().to_string(), "fn f(v: Vec<u8>) { v.push() }");
        assert_eq!(after.primary().head, 26, "caret inside the parens: push takes an argument");

        // Right after the dot (nothing typed): `len()` takes no arguments, so the
        // caret goes after the parens.
        assert!(c.update_prefix(&rope, 21));
        c.selected = c.shown.iter().position(|&i| c.items[i].label == "len").unwrap();
        let change = c.accept(&rope, &Selections::single(Selection::caret(21))).unwrap();
        assert_eq!(change.after.primary().head, 26);
        assert_eq!(change.edits[0].text, "len()");
    }

    #[test]
    fn prefix_tracking_ends_the_session() {
        let (rope, mut c) = session("abc abd", 7);
        assert!(c.update_prefix(&rope, 6));
        assert!(!c.update_prefix(&rope, 3), "caret moved before the word");
        assert!(is_trigger(':', Some(':')) && is_trigger('.', None) && !is_trigger(':', Some(' ')));
    }

    #[test]
    fn utf16_positions() {
        let rope = Rope::from_str("a😀b\n");
        assert_eq!(char_pos(&rope, (0, 3), true), 2, "the emoji is two UTF-16 units");
        assert_eq!(char_pos(&rope, (0, 2), false), 2);
    }
}
