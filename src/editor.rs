//! The code editor widget. Unlike egui's `TextEdit` it only lays out and paints
//! the lines that are on screen, so cost and memory don't grow with file size.

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::text::{CCursor, LayoutJob, LayoutSection, TextFormat};
use egui::{
    Color32, CursorIcon, Event, EventFilter, FontId, Galley, Key, Pos2, Rect, Sense, Stroke, Vec2,
    pos2, vec2,
};

use crate::buffer::{Selection, Selections, line_col, line_len, word_at};
use crate::document::{Document, Query};
use crate::ops;

/// Time per frame spent on syntax highlighting.
const HIGHLIGHT_BUDGET: Duration = Duration::from_millis(6);
/// Highlight this many lines past the bottom of the screen, so scrolling looks right.
const LOOKAHEAD: usize = 40;
const MARGIN: f32 = 8.0;
/// Lines longer than this (in chars) are laid out only around the visible
/// columns: a minified file can have one line of megabytes.
const LONG_LINE: usize = 4096;
/// The laid out part of a long line starts and ends on multiples of this, so it
/// stays the same while scrolling (and egui's layout cache keeps it).
const LONG_LINE_BLOCK: usize = 512;

/// Whether text was copied as whole lines (Ctrl+C without a selection); pasting such
/// text with a plain caret inserts it above the line, like VSCode.
#[derive(Clone, Default)]
struct LineClipboard(Option<String>);

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Char,
    Word,
    Line,
}

/// Mouse state of one editor between frames.
#[derive(Clone, Default)]
struct Pointer {
    last_press: Option<(f64, Pos2)>,
    clicks: u32,
    /// While drag-selecting: what was selected by the press, and in which unit.
    drag: Option<(Range<usize>, Unit)>,
}

/// Geometry of the text area for one frame.
struct Layout {
    font: FontId,
    row_h: f32,
    char_w: f32,
    /// Screen position of the top-left of line 0.
    origin: Pos2,
    /// Columns that can be on screen; long lines are laid out only for these.
    cols: Range<usize>,
}

/// A laid out line, or for a long line only the part around the visible columns.
struct LineGalley {
    galley: Arc<Galley>,
    /// Column of the galley's first char.
    start: usize,
    /// Chars in the galley.
    len: usize,
    /// Width of one char outside the galley: the galley's own average, so
    /// positions continue smoothly past both of its ends.
    advance: f32,
}

/// The columns of a long line to lay out for the visible `cols`: whole blocks,
/// with a block of slack on each side.
fn long_line_window(cols: &Range<usize>, line_len: usize) -> Range<usize> {
    let start = (cols.start / LONG_LINE_BLOCK).saturating_sub(1) * LONG_LINE_BLOCK;
    let end = (cols.end / LONG_LINE_BLOCK + 2) * LONG_LINE_BLOCK;
    start.min(line_len)..end.min(line_len)
}

impl Layout {
    fn galley(&self, ui: &egui::Ui, doc: &Document, line: usize) -> LineGalley {
        let len = line_len(doc.rope(), line);
        let window = if len > LONG_LINE {
            long_line_window(&self.cols, len)
        } else {
            0..len
        };
        let job = line_job(doc, line, window.clone(), &self.font, ui.visuals().text_color());
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        let advance = if window.is_empty() {
            self.char_w
        } else {
            galley.rect.width() / window.len() as f32
        };
        LineGalley {
            galley,
            start: window.start,
            len: window.len(),
            advance,
        }
    }

    /// x of a column relative to the start of the text. Outside the laid out
    /// part of a long line, columns are a char width apart (monospace).
    fn rel_x(&self, g: &LineGalley, col: usize) -> f32 {
        let x0 = g.start as f32 * g.advance;
        if col < g.start {
            col as f32 * g.advance
        } else if col <= g.start + g.len {
            x0 + g.galley.pos_from_cursor(CCursor::new(col - g.start)).min.x
        } else {
            x0 + g.galley.rect.width() + (col - g.start - g.len) as f32 * g.advance
        }
    }

    /// The column boundary nearest to `x` (relative to the start of the text).
    fn rel_col(&self, g: &LineGalley, x: f32) -> usize {
        let x0 = g.start as f32 * g.advance;
        let w = g.galley.rect.width();
        if x < x0 {
            (x.max(0.0) / g.advance).round() as usize
        } else if x <= x0 + w {
            g.start
                + g.galley
                    .cursor_from_pos(vec2(x - x0, self.row_h * 0.5))
                    .index
                    .0
        } else {
            g.start + g.len + ((x - x0 - w) / g.advance).round() as usize
        }
    }

    fn line_y(&self, line: usize) -> f32 {
        self.origin.y + line as f32 * self.row_h
    }

    /// x of a column in a laid out line, in screen coordinates.
    fn col_x(&self, galley: &LineGalley, col: usize) -> f32 {
        self.origin.x + MARGIN + self.rel_x(galley, col)
    }

    /// The char position under a screen point.
    fn char_at(&self, ui: &egui::Ui, doc: &Document, p: Pos2) -> usize {
        let rope = doc.rope();
        let line = ((p.y - self.origin.y) / self.row_h)
            .floor()
            .clamp(0.0, (rope.len_lines() - 1) as f32) as usize;
        let galley = self.galley(ui, doc, line);
        let col = self.rel_col(&galley, p.x - self.origin.x - MARGIN);
        rope.line_to_char(line) + col.min(line_len(rope, line))
    }

    /// The char whose cell contains a screen point (None past the end of a line).
    fn char_under(&self, ui: &egui::Ui, doc: &Document, p: Pos2) -> Option<usize> {
        let rope = doc.rope();
        let x = p.x - self.origin.x - MARGIN;
        if p.y < self.origin.y || x < 0.0 {
            return None;
        }
        let line = ((p.y - self.origin.y) / self.row_h).floor() as usize;
        if line >= rope.len_lines() {
            return None;
        }
        let galley = self.galley(ui, doc, line);
        let len = line_len(rope, line);
        let mut col = self.rel_col(&galley, x).min(len);
        // The cursor snaps to the nearest boundary; the char under x may be before it.
        if col > 0 && x < self.rel_x(&galley, col) {
            col -= 1;
        }
        (col < len).then(|| rope.line_to_char(line) + col)
    }

    /// Column in `line` closest to screen x.
    fn col_at_x(&self, ui: &egui::Ui, doc: &Document, line: usize, x: f32) -> usize {
        let galley = self.galley(ui, doc, line);
        let col = self.rel_col(&galley, x - self.origin.x - MARGIN);
        col.min(line_len(doc.rope(), line))
    }

    fn caret_rect(&self, ui: &egui::Ui, doc: &Document, pos: usize) -> Rect {
        let (line, col) = line_col(doc.rope(), pos);
        let x = self.col_x(&self.galley(ui, doc, line), col);
        Rect::from_min_size(pos2(x, self.line_y(line)), vec2(self.char_w, self.row_h))
    }
}

/// Builds the layout job for the chars `cols` of one line (without its line
/// break) from its highlighting.
fn line_job(
    doc: &Document,
    line: usize,
    cols: Range<usize>,
    font: &FontId,
    fallback: Color32,
) -> LayoutJob {
    let full = doc.rope().line(line);
    let slice = full.slice(cols.clone());
    // Highlight spans are byte ranges of the whole line.
    let offset = full.char_to_byte(cols.start);
    let mut text = String::with_capacity(slice.len_bytes());
    for chunk in slice.chunks() {
        text.push_str(chunk);
    }
    if text.ends_with('\n') {
        text.pop();
    }
    let plain = doc.highlighter().map_or(fallback, |h| h.plain);
    let mut job = LayoutJob {
        text,
        ..Default::default()
    };
    job.wrap.max_width = f32::INFINITY;
    let len = job.text.len();
    let fmt = |color, italics, underline: bool| TextFormat {
        font_id: font.clone(),
        color,
        italics,
        underline: if underline {
            Stroke::new(1.0, color)
        } else {
            Stroke::NONE
        },
        ..Default::default()
    };
    let mut pos = 0;
    if let Some(styles) = doc.highlighter().and_then(|h| h.line_styles(line)) {
        for (range, style) in styles {
            if range.end <= offset {
                continue;
            }
            // Spans can be slightly stale right after an edit: keep them on char boundaries.
            let mut end = (range.end - offset).min(len);
            while !job.text.is_char_boundary(end) {
                end -= 1;
            }
            if end <= pos {
                continue;
            }
            job.sections.push(LayoutSection {
                leading_space: 0.0,
                byte_range: egui::text::ByteIndex(pos)..egui::text::ByteIndex(end),
                format: fmt(style.color, style.italics, style.underline),
            });
            pos = end;
        }
    }
    if pos < len || job.sections.is_empty() {
        job.sections.push(LayoutSection {
            leading_space: 0.0,
            byte_range: egui::text::ByteIndex(pos)..egui::text::ByteIndex(len),
            format: fmt(plain, false, false),
        });
    }
    job
}

/// Options for drawing an editor.
pub struct Options<'a> {
    pub font_size: f32,
    /// Matches of the find widget to highlight.
    pub find: Option<&'a Query>,
    /// Language server hover docs (None for files without a server).
    pub hover: Option<(&'a mut Hover, &'a mut egui_commonmark::CommonMarkCache)>,
    /// Line numbers count from the caret line (which is 1) instead of the file's start.
    pub relative_numbers: bool,
    /// Let the caret fade in and out.
    pub blink: bool,
}

/// Caret blinking: a cycle of fully visible, fading out, hidden and fading in.
const BLINK_VISIBLE: f64 = 0.6;
const BLINK_FADE: f64 = 0.15;
const BLINK_HIDDEN: f64 = 0.3;
/// After this long without typing or moving the caret, it stays solid: an idle
/// editor stops drawing frames (and can hand back its window buffers).
const BLINK_STOP_AFTER: f64 = 10.0;

/// The caret's opacity (0–1) in this frame; asks for the frame where it next
/// changes. `key` changes with every edit and caret move, which restarts the
/// cycle fully visible, so the caret never hides while you type.
fn caret_alpha(ui: &egui::Ui, id: egui::Id, key: u64) -> f32 {
    let now = ui.input(|i| i.time);
    let state = id.with("blink");
    let since = match ui.data(|d| d.get_temp::<(u64, f64)>(state)) {
        Some((k, since)) if k == key => since,
        _ => {
            ui.data_mut(|d| d.insert_temp(state, (key, now)));
            now
        }
    };
    // In a background window the caret just stays put.
    if ui.input(|i| i.viewport().focused) == Some(false) {
        return 1.0;
    }
    let t = now - since;
    if t >= BLINK_STOP_AFTER {
        return 1.0;
    }
    let smooth = |x: f64| (x * x * (3.0 - 2.0 * x)) as f32;
    let fade_out = BLINK_VISIBLE;
    let hidden = fade_out + BLINK_FADE;
    let fade_in = hidden + BLINK_HIDDEN;
    let phase = t % (fade_in + BLINK_FADE);
    let (alpha, next_change) = if phase < fade_out {
        (1.0, fade_out - phase)
    } else if phase < hidden {
        (1.0 - smooth((phase - fade_out) / BLINK_FADE), 0.0)
    } else if phase < fade_in {
        (0.0, fade_in - phase)
    } else {
        (smooth((phase - fade_in) / BLINK_FADE), 0.0)
    };
    // Also wake up when the blinking stops, to draw the caret solid.
    let wait = next_change.min(BLINK_STOP_AFTER - t);
    if wait <= 0.0 {
        ui.ctx().request_repaint();
    } else {
        ui.ctx().request_repaint_after_secs(wait as f32);
    }
    alpha
}

/// Changes whenever the text or any caret or selection does.
fn caret_key(doc: &Document, view: u64) -> u64 {
    let mut h = doc.rev().wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for s in doc.selections(view).iter() {
        for v in [s.head, s.anchor] {
            h = (h.rotate_left(7) ^ v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    h
}

/// How long the pointer must rest on a word before hover docs are requested.
const HOVER_DWELL: f32 = 0.4;

/// The word under the pointer, at a document revision.
#[derive(Clone, Debug, PartialEq)]
pub struct HoverAt {
    pub doc: u64,
    pub rev: u64,
    pub word: Range<usize>,
}

/// Hover docs shared between the editor (which sees the pointer) and the app (which
/// owns the language servers).
#[derive(Default)]
pub struct Hover {
    /// The editor wants docs for this word; the app takes it and asks the server.
    pub want: Option<HoverAt>,
    /// The request in flight: what it is for, the server and the request id.
    pub asked: Option<(HoverAt, crate::lsp::ServerKey, i64)>,
    /// The latest answer (Markdown, empty if the server had nothing).
    pub answer: Option<(HoverAt, String)>,
}

/// Draws an editor for `doc` as seen from `view` (an editor group), filling the
/// remaining space of `ui`, and handles its input when focused.
pub fn show(
    ui: &mut egui::Ui,
    id: egui::Id,
    doc: &mut Document,
    view: u64,
    opts: Options,
) -> egui::Response {
    let font = FontId::monospace(opts.font_size);
    let (relative_numbers, blink) = (opts.relative_numbers, opts.blink);
    let (row_h, char_w) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    let line_count = doc.rope().len_lines();
    let digits = line_count.to_string().len().max(3) as f32;
    let gutter_w = digits * char_w + 28.0;

    let area = ui.available_rect_before_wrap();
    ui.allocate_rect(area, Sense::hover());
    let gutter = Rect::from_min_size(area.min, vec2(gutter_w, area.height()));
    let text_area = Rect::from_min_max(pos2(gutter.right(), area.top()), area.max);

    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(text_area));
    let mut scroll = egui::ScrollArea::both()
        .id_salt(("editor-scroll", id))
        .auto_shrink(false);
    // Scroll the primary caret into view by setting the offset directly, so it takes
    // effect this very frame (no animation, no waiting for more frames).
    if let Some(center) = doc.view(view).reveal.take() {
        let scroll_id: Option<egui::Id> = ui.data(|d| d.get_temp(id.with("scroll-id")));
        let mut off = scroll_id
            .and_then(|sid| egui::scroll_area::State::load(ui.ctx(), sid))
            .map_or(Vec2::ZERO, |s| s.clamped_offset());
        let (line, col) = line_col(doc.rope(), doc.selections(view).primary().head);
        let size = text_area.size() - vec2(12.0, 12.0); // minus scroll bars
        let y = line as f32 * row_h;
        if y < off.y || y + row_h > off.y + size.y {
            off.y = if center {
                y - (size.y - row_h) / 2.0
            } else if y < off.y {
                y
            } else {
                y + row_h - size.y
            };
        }
        let len = line_len(doc.rope(), line);
        let x = MARGIN
            + if len > LONG_LINE {
                col as f32 * char_w
            } else {
                let job = line_job(doc, line, 0..len, &font, Color32::GRAY);
                let galley = ui.fonts_mut(|f| f.layout_job(job));
                galley.pos_from_cursor(CCursor::new(col)).min.x
            };
        if x < off.x + 4.0 * char_w {
            off.x = x - 8.0 * char_w;
        } else if x > off.x + size.x - 4.0 * char_w {
            off.x = x - size.x + 8.0 * char_w;
        }
        scroll = scroll.scroll_offset(off.max(Vec2::ZERO));
    }
    let out = scroll.show_viewport(&mut child, |ui, viewport| {
        let width = MARGIN * 2.0 + (doc.max_line_chars() + 2) as f32 * char_w;
        let height = line_count as f32 * row_h + (viewport.height() - 2.0 * row_h).max(0.0);
        let (rect, _) =
            ui.allocate_exact_size(vec2(width, height).max(viewport.size()), Sense::hover());
        let resp = ui.interact(rect, id, Sense::click_and_drag());
        let first_col = ((viewport.min.x - MARGIN) / char_w).floor().max(0.0) as usize;
        let last_col = ((viewport.max.x - MARGIN) / char_w).ceil().max(0.0) as usize + 1;
        let layout = Layout {
            font: font.clone(),
            row_h,
            char_w,
            origin: rect.min,
            cols: first_col..last_col,
        };

        handle_pointer(ui, &resp, &layout, doc, view);
        if resp.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: false,
                    },
                )
            });
            handle_keys(ui, &layout, doc, view, viewport.height());
        }
        if resp.hovered() {
            let ctrl = ui.input(|i| i.modifiers.command);
            ui.ctx().set_cursor_icon(if ctrl {
                CursorIcon::PointingHand
            } else {
                CursorIcon::Text
            });
            tooltip(ui, &resp, &layout, doc, opts.hover);
        }

        // Visible lines, after this frame's edits.
        let line_count = doc.rope().len_lines();
        let first = ((viewport.min.y / row_h).floor().max(0.0) as usize).min(line_count - 1);
        let last = ((viewport.max.y / row_h).ceil() as usize + 1).min(line_count);
        let dark = ui.visuals().dark_mode;
        if !doc.highlight(dark, last + LOOKAHEAD, Instant::now() + HIGHLIGHT_BUDGET) {
            ui.ctx().request_repaint();
        }

        // The caret's opacity, while focused.
        let caret = resp.has_focus().then(|| {
            if blink {
                caret_alpha(ui, id, caret_key(doc, view))
            } else {
                1.0
            }
        });
        paint(ui, &layout, doc, view, first..last, caret, opts.find);
        if resp.has_focus() {
            let caret = layout.caret_rect(ui, doc, doc.selections(view).primary().head);
            // Where the caret is on screen, for popups such as completion.
            ui.data_mut(|d| d.insert_temp(id.with("caret-rect"), caret));
            ui.output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect: text_area,
                    cursor_rect: caret,
                    should_interrupt_composition: false,
                })
            });
        }
        (resp, rect.min.y, first..last)
    });
    let (resp, origin_y, visible) = out.inner;
    ui.data_mut(|d| d.insert_temp(id.with("scroll-id"), out.id));
    #[cfg(test)]
    ui.data_mut(|d| d.insert_temp(id.with("scroll-y"), out.state.clamped_offset().y));

    // Gutter with line numbers, aligned with the scrolled text.
    let visuals = ui.visuals();
    let painter = ui.painter_at(gutter);
    painter.rect_filled(gutter, 0.0, visuals.extreme_bg_color);
    let cur_lines: Vec<usize> = doc
        .selections(view)
        .iter()
        .map(|s| doc.rope().char_to_line(s.head))
        .collect();
    // Git change bars at the left edge of the gutter.
    let marks = &doc.git_marks;
    let first_mark = marks.partition_point(|(r, _)| r.end <= visible.start);
    for (range, mark) in marks[first_mark..]
        .iter()
        .take_while(|(r, _)| r.start < visible.end)
    {
        use crate::git::Mark;
        let x = gutter.left() + 2.0;
        let top = origin_y + range.start as f32 * row_h;
        match mark {
            Mark::Added | Mark::Modified => {
                let color = if *mark == Mark::Added {
                    Color32::from_rgb(88, 166, 92)
                } else {
                    Color32::from_rgb(66, 135, 205)
                };
                let bottom = origin_y + range.end as f32 * row_h;
                painter.rect_filled(
                    Rect::from_min_max(pos2(x, top), pos2(x + 3.0, bottom)),
                    0.0,
                    color,
                );
            }
            Mark::Deleted => {
                // A small wedge on the line boundary where lines were removed.
                let c = pos2(x, top);
                let pts = vec![c + vec2(0.0, -4.0), c + vec2(5.0, 0.0), c + vec2(0.0, 4.0)];
                painter.add(egui::Shape::convex_polygon(
                    pts,
                    Color32::from_rgb(205, 70, 70),
                    Stroke::NONE,
                ));
            }
        }
    }
    let caret_line = doc
        .rope()
        .char_to_line(doc.selections(view).primary().head);
    for line in visible {
        let y = origin_y + line as f32 * row_h;
        let color = if cur_lines.contains(&line) {
            visuals.strong_text_color()
        } else {
            visuals.weak_text_color()
        };
        painter.text(
            pos2(gutter.right() - 14.0, y),
            egui::Align2::RIGHT_TOP,
            gutter_number(line, caret_line, relative_numbers).to_string(),
            font.clone(),
            color,
        );
    }
    resp
}

fn handle_pointer(
    ui: &egui::Ui,
    resp: &egui::Response,
    layout: &Layout,
    doc: &mut Document,
    view: u64,
) {
    let (pressed, down, pos, mods, time) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.interact_pos(),
            i.modifiers,
            i.time,
        )
    });
    let mut state: Pointer = ui.data(|d| d.get_temp(resp.id)).unwrap_or_default();
    let Some(pos) = pos else { return };

    if pressed && resp.contains_pointer() {
        resp.request_focus();
        // Count clicks for double/triple click.
        let quick = state
            .last_press
            .is_some_and(|(t, p)| time - t < 0.4 && p.distance(pos) < 6.0);
        state.clicks = if quick { state.clicks % 3 + 1 } else { 1 };
        state.last_press = Some((time, pos));

        let p = layout.char_at(ui, doc, pos);
        let rope = doc.rope();
        let (range, unit) = match state.clicks {
            2 => (word_at(rope, p), Unit::Word),
            3 => {
                let line = rope.char_to_line(p);
                let end = if line + 1 < rope.len_lines() {
                    rope.line_to_char(line + 1)
                } else {
                    rope.len_chars()
                };
                (rope.line_to_char(line)..end, Unit::Line)
            }
            _ => (p..p, Unit::Char),
        };
        let sels = doc.selections(view);
        let new = if mods.alt {
            let mut s = sels;
            s.push(Selection::new(range.start, range.end));
            s
        } else if mods.shift && unit == Unit::Char {
            Selections::single(Selection::new(sels.primary().anchor, p))
        } else {
            Selections::single(Selection::new(range.start, range.end))
        };
        doc.set_selections(view, new, false);
        if mods.command && !mods.shift && !mods.alt && state.clicks == 1 {
            // Ctrl+click: the app asks for the definition of what's under the caret.
            ui.data_mut(|d| d.insert_temp(resp.id.with("goto-definition"), true));
        }
        state.drag = Some(if mods.shift && unit == Unit::Char {
            let a = doc.selections(view).primary().anchor;
            (a..a, Unit::Char)
        } else {
            (range, unit)
        });
    } else if down && let Some((anchor, unit)) = state.drag.clone() {
        if resp.dragged() || pos.y < resp.rect.top() || pos.y > resp.rect.bottom() {
            let p = layout.char_at(ui, doc, pos);
            let rope = doc.rope();
            let target = match unit {
                Unit::Char => p..p,
                Unit::Word => word_at(rope, p),
                Unit::Line => {
                    let line = rope.char_to_line(p);
                    let end = if line + 1 < rope.len_lines() {
                        rope.line_to_char(line + 1)
                    } else {
                        rope.len_chars()
                    };
                    rope.line_to_char(line)..end
                }
            };
            let sel = if target.start < anchor.start {
                Selection::new(anchor.end, target.start)
            } else {
                Selection::new(anchor.start, target.end.max(anchor.end))
            };
            let new = doc.selections(view).with_primary(sel);
            doc.set_selections(view, new, false);
            // Scroll while dragging past the edge of the view.
            let clip = ui.clip_rect();
            if pos.y < clip.top()
                || pos.y > clip.bottom()
                || pos.x < clip.left()
                || pos.x > clip.right()
            {
                ui.scroll_to_rect_animation(
                    layout.caret_rect(ui, doc, sel.head),
                    None,
                    egui::style::ScrollAnimation::none(),
                );
                ui.ctx().request_repaint();
            }
        }
    }
    if !down {
        state.drag = None;
    }
    ui.data_mut(|d| d.insert_temp(resp.id, state));
}

fn handle_keys(ui: &egui::Ui, layout: &Layout, doc: &mut Document, view: u64, page_h: f32) {
    let events = ui.input(|i| i.events.clone());
    let page = ((page_h / layout.row_h) as isize - 2).max(1);
    let clip_id = egui::Id::unique("rsedit-line-clipboard");
    for event in events {
        match event {
            Event::Text(t) => doc.apply(view, |r, s, c| ops::type_text(r, s, &t, c)),
            Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() => {
                doc.apply(view, |r, s, c| ops::type_text(r, s, &t, c))
            }
            Event::Paste(t) => {
                let lines = ui
                    .data(|d| d.get_temp::<LineClipboard>(clip_id))
                    .and_then(|c| c.0)
                    .is_some_and(|c| c == t);
                doc.apply(view, |r, s, _| ops::paste(r, s, &t, lines));
            }
            Event::Copy => {
                let (text, lines) = ops::copy_text(doc.rope(), &doc.selections(view));
                ui.data_mut(|d| d.insert_temp(clip_id, LineClipboard(lines.then(|| text.clone()))));
                ui.ctx().copy_text(text);
            }
            Event::Cut => {
                let (text, lines, change) = ops::cut(doc.rope(), &doc.selections(view));
                ui.data_mut(|d| d.insert_temp(clip_id, LineClipboard(lines.then(|| text.clone()))));
                ui.ctx().copy_text(text);
                doc.commit(view, change);
            }
            Event::Key {
                key,
                pressed: true,
                modifiers: m,
                ..
            } => {
                key_press(ui, layout, doc, view, key, m, page);
            }
            _ => {}
        }
    }
}

fn key_press(
    ui: &egui::Ui,
    layout: &Layout,
    doc: &mut Document,
    view: u64,
    key: Key,
    m: egui::Modifiers,
    page: isize,
) {
    let (ctrl, shift, alt) = (m.command, m.shift, m.alt);
    match key {
        Key::ArrowLeft | Key::ArrowRight if !alt => doc.motion(view, |r, s| {
            ops::move_horizontal(r, s, key == Key::ArrowRight, ctrl, shift)
        }),
        Key::ArrowUp | Key::ArrowDown => {
            let up = key == Key::ArrowUp;
            if alt && shift {
                doc.apply(view, |r, s, _| ops::copy_lines(r, s, up));
            } else if alt && ctrl {
                add_cursor_vertical(ui, layout, doc, view, up);
            } else if alt {
                doc.run(view, |r, s, _| ops::move_lines(r, s, up));
            } else if !ctrl {
                move_vertical(ui, layout, doc, view, if up { -1 } else { 1 }, shift);
            }
        }
        Key::PageUp | Key::PageDown => move_vertical(
            ui,
            layout,
            doc,
            view,
            if key == Key::PageUp { -page } else { page },
            shift,
        ),
        Key::Home if ctrl => doc.motion(view, |_, s| ops::move_to(s, 0, shift)),
        Key::End if ctrl => doc.motion(view, |r, s| ops::move_to(s, r.len_chars(), shift)),
        Key::Home => doc.motion(view, |r, s| ops::home(r, s, shift)),
        Key::End => doc.motion(view, |r, s| ops::end(r, s, shift)),
        Key::Backspace => doc.apply(view, |r, s, c| ops::backspace(r, s, ctrl, c)),
        Key::Delete if shift && !ctrl => {
            // Shift+Delete cuts the line, like VSCode.
            let (text, _, change) = ops::cut(doc.rope(), &doc.selections(view));
            ui.ctx().copy_text(text);
            doc.commit(view, change);
        }
        Key::Delete => doc.apply(view, |r, s, _| ops::delete_forward(r, s, ctrl)),
        Key::Enter if ctrl => doc.apply(view, |r, s, _| ops::insert_line(r, s, shift)),
        Key::Enter => doc.apply(view, |r, s, c| ops::newline(r, s, c)),
        Key::Tab if shift => doc.apply(view, |r, s, c| ops::outdent_lines(r, s, c)),
        Key::Tab => doc.apply(view, |r, s, c| ops::tab(r, s, c)),
        Key::Escape => {
            let sels = doc.selections(view);
            if sels.len() > 1 {
                doc.set_selections(view, sels.collapse_to_primary(), true);
            }
        }
        Key::A if ctrl => doc.motion(view, |r, _| ops::select_all(r)),
        Key::Z if ctrl && shift => doc.redo(view),
        Key::Z if ctrl => doc.undo(view),
        Key::Y if ctrl => doc.redo(view),
        Key::D if ctrl => doc.motion(view, ops::add_next_occurrence),
        Key::L if ctrl && shift => doc.motion(view, ops::select_all_occurrences),
        Key::L if ctrl => doc.motion(view, ops::select_line),
        Key::K if ctrl && shift => doc.apply(view, |r, s, _| ops::delete_lines(r, s)),
        Key::Slash if ctrl => doc.toggle_comment(view),
        Key::CloseBracket if ctrl => doc.apply(view, |r, s, c| ops::indent_lines(r, s, c)),
        Key::OpenBracket if ctrl => doc.apply(view, |r, s, c| ops::outdent_lines(r, s, c)),
        _ => {}
    }
}

/// Up/Down/PageUp/PageDown, keeping the horizontal position across lines.
fn move_vertical(
    ui: &egui::Ui,
    layout: &Layout,
    doc: &mut Document,
    view: u64,
    delta: isize,
    extend: bool,
) {
    let sels = doc.selections(view);
    let last = doc.rope().len_lines() - 1;
    let len = doc.rope().len_chars();
    let new = sels.map(|s| {
        let (line, col) = line_col(doc.rope(), s.head);
        let x = s
            .goal_x
            .unwrap_or_else(|| layout.col_x(&layout.galley(ui, doc, line), col));
        let target = line as isize + delta;
        let head = if target < 0 {
            0
        } else if target as usize > last {
            len
        } else {
            let t = target as usize;
            doc.rope().line_to_char(t) + layout.col_at_x(ui, doc, t, x)
        };
        let mut n = if extend {
            Selection::new(s.anchor, head)
        } else {
            Selection::caret(head)
        };
        n.goal_x = Some(x);
        n
    });
    doc.set_selections(view, new, true);
}

/// Ctrl+Alt+Up/Down: adds a caret on the line above the topmost (or below the
/// bottommost) caret.
fn add_cursor_vertical(ui: &egui::Ui, layout: &Layout, doc: &mut Document, view: u64, up: bool) {
    let sels = doc.selections(view);
    let edge = if up {
        sels.iter().next()
    } else {
        sels.iter().last()
    }
    .copied()
    .expect("non-empty");
    let (line, col) = line_col(doc.rope(), edge.head);
    let target = if up {
        line.checked_sub(1)
    } else {
        (line + 1 < doc.rope().len_lines()).then_some(line + 1)
    };
    let Some(target) = target else { return };
    let x = edge
        .goal_x
        .unwrap_or_else(|| layout.col_x(&layout.galley(ui, doc, line), col));
    let pos = doc.rope().line_to_char(target) + layout.col_at_x(ui, doc, target, x);
    let mut s = sels;
    let mut caret = Selection::caret(pos);
    caret.goal_x = Some(x);
    s.push(caret);
    doc.set_selections(view, s, true);
}

fn paint(
    ui: &egui::Ui,
    layout: &Layout,
    doc: &mut Document,
    view: u64,
    lines: Range<usize>,
    caret: Option<f32>,
    find: Option<&Query>,
) {
    let focused = caret.is_some();
    let caret_alpha = caret.unwrap_or(0.0);
    let visuals = ui.visuals().clone();
    let sels = doc.selections(view);
    let rope_len = doc.rope().len_chars();
    let first_char = doc.rope().line_to_char(lines.start);
    let last_char = if lines.end >= doc.rope().len_lines() {
        rope_len
    } else {
        doc.rope().line_to_char(lines.end)
    };

    // Find matches in view.
    let mut matches: Vec<Range<usize>> = Vec::new();
    let mut current_match = None;
    if let Some(q) = find
        && let Ok(all) = doc.find(q)
    {
        let i = all.partition_point(|m| m.end < first_char);
        matches = all[i..]
            .iter()
            .take_while(|m| m.start <= last_char)
            .cloned()
            .collect();
        let p = sels.primary();
        current_match = matches.iter().position(|m| *m == p.range());
    }
    let doc = &*doc;
    let rope = doc.rope();

    // Other occurrences of a short single-line selection.
    let mut occurrences: Vec<Range<usize>> = Vec::new();
    let p = sels.primary();
    if sels.len() == 1 && !p.is_empty() && p.end() - p.start() <= 200 && find.is_none() {
        let needle = rope.slice(p.range()).to_string();
        if !needle.contains('\n') && needle.trim().len() > 0 {
            let hay = rope.slice(first_char..last_char).to_string();
            for (b, _) in hay.match_indices(&needle) {
                let start = first_char + hay[..b].chars().count();
                if start != p.start() {
                    occurrences.push(start..start + needle.chars().count());
                }
            }
        }
    }
    let bracket = (focused && p.is_empty())
        .then(|| ops::matching_bracket(rope, p.head))
        .flatten();

    let painter = ui.painter();
    let clip = ui.clip_rect();
    let sel_fill = if focused {
        visuals.selection.bg_fill
    } else {
        visuals.selection.bg_fill.gamma_multiply(0.5)
    };
    let match_fill = Color32::from_rgba_unmultiplied(234, 92, 0, 70);
    let occ_fill = visuals.selection.bg_fill.gamma_multiply(0.35);
    let guide = visuals
        .widgets
        .noninteractive
        .bg_stroke
        .color
        .gamma_multiply(0.6);
    let caret_color = visuals.text_cursor.stroke.color.gamma_multiply(caret_alpha);
    let caret_lines: Vec<usize> = sels
        .iter()
        .filter(|s| s.is_empty())
        .map(|s| rope.char_to_line(s.head))
        .collect();
    let indent_w = doc.indent.width;

    let mut prev_indent = 0usize;
    for line in lines {
        let galley = layout.galley(ui, doc, line);
        let y = layout.line_y(line);
        let row = Rect::from_min_size(pos2(clip.left(), y), vec2(clip.width(), layout.row_h));
        let ls = rope.line_to_char(line);
        let le = ls + line_len(rope, line);

        if focused && caret_lines.contains(&line) {
            painter.rect_filled(row, 0.0, crate::theme::palette(ui).current_line);
        }
        let span_rect = |r: &Range<usize>, newline_extra: bool| -> Option<Rect> {
            if r.end < ls || r.start > le || (r.start == r.end) {
                return None;
            }
            let a = r.start.max(ls) - ls;
            let b = r.end.min(le) - ls;
            let x0 = layout.col_x(&galley, a);
            let mut x1 = layout.col_x(&galley, b);
            if newline_extra && r.end > le {
                x1 += layout.char_w * 0.6;
            }
            (x1 > x0).then(|| Rect::from_min_max(pos2(x0, y), pos2(x1, y + layout.row_h)))
        };
        for r in &occurrences {
            if let Some(rect) = span_rect(r, false) {
                painter.rect_filled(rect, 2.0, occ_fill);
            }
        }
        for (i, m) in matches.iter().enumerate() {
            if let Some(rect) = span_rect(m, false) {
                if Some(i) == current_match {
                    painter.rect_stroke(
                        rect,
                        2.0,
                        Stroke::new(1.5, Color32::from_rgb(234, 150, 60)),
                        egui::StrokeKind::Inside,
                    );
                }
                painter.rect_filled(rect, 2.0, match_fill);
            }
        }
        for s in sels.iter() {
            if let Some(rect) = span_rect(&s.range(), true) {
                painter.rect_filled(rect, 2.0, sel_fill);
            }
        }
        if let Some((a, b)) = bracket {
            for at in [a, b] {
                if let Some(rect) = span_rect(&(at..at + 1), false) {
                    painter.rect_stroke(
                        rect,
                        1.0,
                        Stroke::new(1.0, visuals.weak_text_color()),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }

        // Indent guides, carried over blank lines.
        let ws: usize = rope
            .line(line)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { indent_w } else { 1 })
            .sum();
        let blank = line_len(rope, line)
            == rope
                .line(line)
                .chars()
                .take_while(|c| c.is_whitespace() && *c != '\n')
                .count();
        let indent = if blank { prev_indent } else { ws };
        prev_indent = indent;
        for col in (0..indent).step_by(indent_w.max(1)) {
            let x = layout.origin.x + MARGIN + col as f32 * layout.char_w + 0.5;
            painter.vline(x, y..=y + layout.row_h, Stroke::new(1.0, guide));
        }

        painter.galley(
            pos2(layout.origin.x + MARGIN + layout.rel_x(&galley, galley.start), y),
            galley.galley.clone(),
            visuals.text_color(),
        );

        // Wavy underlines under language server problems.
        for p in doc
            .problems
            .iter()
            .filter(|p| p.severity < 4 && p.range.start <= le && p.range.end >= ls)
        {
            let a = p.range.start.max(ls) - ls;
            let b = p.range.end.min(le) - ls;
            let x0 = layout.col_x(&galley, a);
            // Empty ranges (e.g. "missing ;") still get a short squiggle.
            let x1 = layout.col_x(&galley, b).max(x0 + layout.char_w);
            squiggle(
                painter,
                x0,
                x1,
                y + layout.row_h - 2.0,
                problem_color(p.severity),
            );
        }

        if focused && caret_alpha > 0.0 {
            for s in sels.iter() {
                if rope.char_to_line(s.head) == line {
                    let x = layout.col_x(&galley, s.head - ls);
                    painter.rect_filled(
                        Rect::from_min_size(pos2(x - 1.0, y), vec2(2.0, layout.row_h)),
                        0.0,
                        caret_color,
                    );
                }
            }
        }
    }
}

/// The number shown next to `line` (0-based): its line number, or with
/// `relative` its distance from the caret line, counting that line as 1.
fn gutter_number(line: usize, caret_line: usize, relative: bool) -> usize {
    if relative {
        line.abs_diff(caret_line) + 1
    } else {
        line + 1
    }
}

pub fn problem_color(severity: u8) -> Color32 {
    match severity {
        1 => Color32::from_rgb(235, 85, 75),
        2 => Color32::from_rgb(225, 185, 60),
        _ => Color32::from_rgb(90, 160, 235),
    }
}

/// A wavy line from x0 to x1 along y.
fn squiggle(painter: &egui::Painter, x0: f32, x1: f32, y: f32, color: Color32) {
    let (step, amp) = (2.0, 1.5);
    let mut points = Vec::with_capacity(((x1 - x0) / step) as usize + 2);
    let mut x = x0;
    let mut up = true;
    while x < x1 {
        points.push(pos2(x, if up { y - amp } else { y + amp }));
        x += step;
        up = !up;
    }
    points.push(pos2(x1, if up { y - amp } else { y + amp }));
    painter.add(egui::Shape::line(points, Stroke::new(1.0, color)));
}

/// Draws language server docs (Markdown) in a tooltip.
pub fn hover_ui(ui: &mut egui::Ui, md: &mut egui_commonmark::CommonMarkCache, text: &str) {
    egui_commonmark::CommonMarkViewer::new().show(ui, md, text);
}

/// One tooltip for the problems under the mouse pointer, then (when a language
/// server knows the word) its docs. Asks for docs once the pointer has rested on a
/// word; the only repaint it requests is the end of that short wait.
fn tooltip(
    ui: &egui::Ui,
    resp: &egui::Response,
    layout: &Layout,
    doc: &Document,
    mut hover: Option<(&mut Hover, &mut egui_commonmark::CommonMarkCache)>,
) {
    let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let mut hits: Vec<&crate::document::Problem> = Vec::new();
    if !doc.problems.is_empty() {
        let at = layout.char_at(ui, doc, pos);
        hits = doc
            .problems
            .iter()
            .filter(|p| {
                p.severity < 4
                    && (p.range.contains(&at) || (p.range.is_empty() && p.range.start == at))
            })
            .collect();
    }
    let mut docs = String::new();
    if let Some((h, _)) = hover.as_mut()
        && !ui.input(|i| i.pointer.any_down())
        && let Some(idx) = layout.char_under(ui, doc, pos)
        && crate::buffer::char_class(doc.rope().char(idx)) == crate::buffer::CharClass::Word
    {
        let target = HoverAt {
            doc: doc.id,
            rev: doc.rev(),
            word: word_at(doc.rope(), idx),
        };
        if let Some((at, text)) = &h.answer
            && *at == target
        {
            docs.clone_from(text);
        } else if h.asked.as_ref().is_none_or(|(at, ..)| *at != target) {
            let still = ui.input(|i| i.pointer.time_since_last_movement());
            if still >= HOVER_DWELL {
                h.want = Some(target);
            } else {
                ui.ctx().request_repaint_after_secs(HOVER_DWELL - still);
            }
        }
    }
    if hits.is_empty() && docs.is_empty() {
        return;
    }
    let has_problems = !hits.is_empty();
    resp.clone().on_hover_ui_at_pointer(|ui| {
        ui.set_max_width(520.0);
        for p in hits {
            let kind = match p.severity {
                1 => "error",
                2 => "warning",
                _ => "info",
            };
            ui.horizontal_top(|ui| {
                ui.colored_label(problem_color(p.severity), kind);
                ui.label(&p.message);
            });
        }
        if let Some((_, md)) = hover
            && !docs.is_empty()
        {
            if has_problems {
                ui.separator();
            }
            hover_ui(ui, md, &docs);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Selection, Selections};

    /// A long line is laid out only near the visible columns, yet columns and
    /// x positions still map onto each other everywhere along it.
    #[test]
    fn long_lines_map_columns_everywhere() {
        let text = "abcdefghij".repeat(2_000); // 20,000 chars
        let mut doc = Document::new_untitled(1);
        doc.apply(1, |r, s, c| ops::type_text(r, s, &text, c));
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let font = FontId::monospace(14.0);
            let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
            let layout = Layout {
                font,
                row_h: 18.0,
                char_w,
                origin: Pos2::ZERO,
                cols: 9_000..9_100,
            };
            let g = layout.galley(ui, &doc, 0);
            assert!(g.start > 0 && g.len < 4_000, "only a window: {}..+{}", g.start, g.len);
            for col in [0, 1, 500, g.start, g.start + 7, 9_050, g.start + g.len, 15_000, 20_000] {
                let x = layout.rel_x(&g, col);
                // (egui puts glyphs on whole pixels.)
                assert!((x - col as f32 * g.advance).abs() <= 1.0, "col {col}: x {x}");
                assert!((g.advance - char_w).abs() < 0.05, "advance {}", g.advance);
                assert_eq!(layout.rel_col(&g, x), col, "round trip at col {col}");
            }
        });
    }

    #[test]
    fn relative_numbers_count_from_the_caret_line() {
        assert_eq!(gutter_number(9, 9, true), 1);
        assert_eq!(gutter_number(10, 9, true), 2);
        assert_eq!(gutter_number(7, 9, true), 3);
        assert_eq!(gutter_number(9, 9, false), 10);
    }

    /// The caret is solid, fades out, hides, fades back in; an edit or caret
    /// move shows it again at once; after a while without input it stays
    /// solid and no more frames are asked for.
    #[test]
    fn caret_blinks_then_rests() {
        let ctx = egui::Context::default();
        let id = egui::Id::NULL.with("blink-test");
        let frame = |time: f64, key: u64| {
            let input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            let mut alpha = 0.0;
            let out = ctx.run_ui(input, |ui| alpha = caret_alpha(ui, id, key));
            let delay = out.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            (alpha, delay)
        };
        // egui asks for extra passes on its first frames; let those settle.
        for _ in 0..3 {
            let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        }
        let (a, delay) = frame(0.0, 1);
        assert_eq!(a, 1.0);
        assert!((delay.as_secs_f64() - BLINK_VISIBLE).abs() < 0.01, "{delay:?}");
        let (a, delay) = frame(BLINK_VISIBLE + BLINK_FADE / 2.0, 1);
        assert!((a - 0.5).abs() < 0.01, "half-way through fading: {a}");
        assert!(delay.is_zero(), "fades run every frame");
        let (a, _) = frame(BLINK_VISIBLE + BLINK_FADE + 0.1, 1);
        assert_eq!(a, 0.0);
        let moved = BLINK_VISIBLE + BLINK_FADE + 0.1;
        let (a, _) = frame(moved, 2);
        assert_eq!(a, 1.0, "a caret move shows it again at once");
        let (_, delay) = frame(moved + BLINK_STOP_AFTER - 0.5, 2);
        assert!(delay < std::time::Duration::MAX, "the move restarted the timer");
        let (a, _) = frame(moved + BLINK_STOP_AFTER + 0.5, 2);
        assert_eq!(a, 1.0);
        // (egui still delivers the wake-up the previous frame asked for.)
        let (a, delay) = frame(moved + BLINK_STOP_AFTER + 0.6, 2);
        assert_eq!(a, 1.0);
        assert_eq!(delay, std::time::Duration::MAX, "resting: no more frames");
    }

    /// Runs one frame of an editor filling an 1200x800 window.
    fn frame(ctx: &egui::Context, doc: &mut Document, events: Vec<Event>) -> Duration {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
            events,
            ..Default::default()
        };
        let t = Instant::now();
        let mut out = ctx.run_ui(input, |ui| {
            let id = egui::Id::unique("test-editor");
            ui.memory_mut(|m| m.request_focus(id));
            show(
                ui,
                id,
                doc,
                1,
                Options {
                    font_size: 14.0,
                    find: None,
                    hover: None,
                    relative_numbers: false,
                    blink: false,
                },
            );
        });
        let took = t.elapsed();
        out.textures_delta.clear(); // no GPU here
        took
    }

    #[test]
    fn typing_goes_through_the_widget() {
        let ctx = egui::Context::default();
        let mut doc = Document::new_untitled(1);
        frame(&ctx, &mut doc, vec![]);
        // One event per typed character, as a keyboard delivers them.
        let typed = "fn main() {"
            .chars()
            .map(|c| Event::Text(c.to_string()))
            .collect();
        frame(&ctx, &mut doc, typed);
        assert_eq!(
            doc.rope().to_string(),
            "fn main() {}",
            "brackets auto-close, `)` types over"
        );
        frame(
            &ctx,
            &mut doc,
            vec![Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
        );
        frame(&ctx, &mut doc, vec![Event::Text("x".into())]);
        assert_eq!(doc.rope().to_string(), "fn main() {\n    x\n}");
        // Ctrl+Z undoes the last typing step.
        let z = Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        frame(&ctx, &mut doc, vec![z]);
        assert_eq!(doc.rope().to_string(), "fn main() {\n    \n}");
    }

    #[test]
    fn reveal_scrolls_a_new_editor_to_the_caret() {
        let ctx = egui::Context::default();
        let mut doc = Document::new_untitled(1);
        let text: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        doc.apply(1, |r, s, _| ops::paste(r, s, &text, false));
        let pos = doc.rope().line_to_char(1000);
        doc.goto(1, pos..pos);
        // Must work in the very first frame: there may be no second one until input arrives.
        frame(&ctx, &mut doc, vec![]);
        assert!(doc.view(1).reveal.is_none(), "reveal finished");
        let offset = scroll_offset(&ctx);
        assert!(
            offset > 900.0 * 14.0,
            "scrolled near line 1000, offset {offset}"
        );
    }

    /// Vertical scroll offset of the test editor after the last frame.
    fn scroll_offset(ctx: &egui::Context) -> f32 {
        ctx.data(|d| d.get_temp(egui::Id::unique("test-editor").with("scroll-y")))
            .unwrap_or(0.0)
    }

    #[test]
    #[ignore = "timing, run with --release --ignored --nocapture"]
    fn frame_times_on_a_big_file() {
        let text: String = include_str!("app/ui.rs")
            .lines()
            .cycle()
            .take(20_000)
            .collect::<Vec<_>>()
            .join("\n");
        let dir = std::env::temp_dir().join(format!("rsedit-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.rs");
        std::fs::write(&path, &text).unwrap();
        let ctx = egui::Context::default();
        let t = Instant::now();
        let mut doc = Document::open(&path).unwrap();
        println!("open 20k lines: {:?}", t.elapsed());
        let first = frame(&ctx, &mut doc, vec![]);
        println!("first frame: {first:?}");
        for _ in 0..3 {
            frame(&ctx, &mut doc, vec![]);
        }
        let mut idle = Duration::ZERO;
        for _ in 0..20 {
            idle += frame(&ctx, &mut doc, vec![]);
        }
        println!("idle frame: {:?}", idle / 20);
        // Jump to the end and type there (highlighting has to catch up first).
        let end = doc.rope().len_chars();
        doc.set_selections(1, Selections::single(Selection::caret(end - 100)), true);
        let mut jump = Duration::ZERO;
        let mut frames = 0;
        while frames < 1000 {
            let d = frame(&ctx, &mut doc, vec![]);
            jump = jump.max(d);
            frames += 1;
            if !ctx.has_requested_repaint() {
                break;
            }
        }
        println!("after jumping to the end: {frames} frames to settle, slowest {jump:?}");
        let mut typing = Duration::ZERO;
        let mut worst = Duration::ZERO;
        for c in "let x = 42;".chars() {
            let d = frame(&ctx, &mut doc, vec![Event::Text(c.to_string())]);
            typing += d;
            worst = worst.max(d);
        }
        println!(
            "keystroke frame at line 20k: avg {:?}, worst {worst:?}",
            typing / 11
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One frame at `time` seconds with hover docs enabled.
    fn hover_frame(
        ctx: &egui::Context,
        doc: &mut Document,
        hover: &mut Hover,
        md: &mut egui_commonmark::CommonMarkCache,
        events: Vec<Event>,
        time: f64,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
            events,
            time: Some(time),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            show(
                ui,
                egui::Id::unique("hover-editor"),
                doc,
                1,
                Options {
                    font_size: 14.0,
                    find: None,
                    hover: Some((hover, md)),
                    relative_numbers: false,
                    blink: false,
                },
            );
        });
        out.textures_delta.clear();
    }

    #[test]
    fn hover_waits_for_the_pointer_to_rest_on_a_word() {
        let ctx = egui::Context::default();
        let mut doc = Document::new_untitled(1);
        doc.apply(1, |r, s, c| ops::type_text(r, s, "hello world\n  \n", c));
        let (mut hover, mut md) = (Hover::default(), Default::default());
        let moved = |x: f32| vec![Event::PointerMoved(pos2(x, 10.0))];
        // Over "hello": nothing yet, then after the dwell the word is wanted.
        hover_frame(&ctx, &mut doc, &mut hover, &mut md, vec![], 0.0);
        hover_frame(&ctx, &mut doc, &mut hover, &mut md, moved(100.0), 0.1);
        assert!(hover.want.is_none(), "too early");
        hover_frame(&ctx, &mut doc, &mut hover, &mut md, vec![], 0.7);
        let want = hover.want.take().expect("asks after resting");
        assert_eq!(want.word, 0..5);
        assert_eq!(want.rev, doc.rev());
        // Once answered, it doesn't ask again for the same word.
        hover.answer = Some((want.clone(), "docs".into()));
        hover_frame(&ctx, &mut doc, &mut hover, &mut md, vec![], 0.8);
        assert!(hover.want.is_none());
        // An edit is a new revision, so it asks again.
        doc.apply(1, |r, s, c| ops::type_text(r, s, "x", c));
        hover_frame(&ctx, &mut doc, &mut hover, &mut md, vec![], 1.5);
        assert!(hover.want.is_some());
        // Whitespace is never asked about.
        hover.want = None;
        hover_frame(&ctx, &mut doc, &mut hover, &mut md, moved(300.0), 2.0);
        hover_frame(&ctx, &mut doc, &mut hover, &mut md, vec![], 3.0);
        assert!(hover.want.is_none());
    }
}
