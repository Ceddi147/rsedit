//! Editing operations, as pure functions from (text, selections) to a change.
//! Every operation works on all selections at once (multi-cursor).

use std::ops::Range;

use ropey::Rope;

use crate::buffer::{
    Edit, EditKind, Selection, Selections, line_col, line_len, map_pos, new_starts, sort_edits,
    word_at, word_end, word_start,
};

/// How a file is indented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Indent {
    pub tabs: bool,
    pub width: usize,
}

impl Default for Indent {
    fn default() -> Self {
        Self {
            tabs: false,
            width: 4,
        }
    }
}

impl Indent {
    pub fn unit(&self) -> String {
        if self.tabs {
            "\t".to_owned()
        } else {
            " ".repeat(self.width)
        }
    }

    /// Guesses the indentation style of a text from its leading whitespace.
    pub fn detect(rope: &Rope) -> Self {
        let (mut tabs, mut spaces) = (0usize, 0usize);
        let mut widths = [0usize; 9];
        let mut prev = 0usize;
        for line in rope.lines().take(2000) {
            let mut n = 0;
            let mut chars = line.chars();
            match chars.next() {
                Some('\t') => {
                    tabs += 1;
                    continue;
                }
                Some(' ') => n = 1 + chars.take_while(|&c| c == ' ').count(),
                Some(c) if c != '\n' => {}
                _ => continue, // blank lines say nothing
            }
            if n > 0 {
                spaces += 1;
            }
            let delta = n.abs_diff(prev);
            if (2..=8).contains(&delta) {
                widths[delta] += 1;
            }
            prev = n;
        }
        if tabs > spaces {
            return Self {
                tabs: true,
                width: 4,
            };
        }
        let width = [2, 4, 8, 3]
            .into_iter()
            .max_by_key(|&w| (widths[w], w == 4))
            .unwrap_or(4);
        Self {
            tabs: false,
            width: if widths[width] == 0 { 4 } else { width },
        }
    }
}

/// The edits an operation wants to make, and the selections afterwards.
pub struct Change {
    pub edits: Vec<Edit>,
    pub after: Selections,
    pub kind: EditKind,
}

/// Settings that some operations depend on.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub indent: Indent,
    /// Line comment token, e.g. `//`.
    pub comment: Option<&'a str>,
    /// Whether `'` should be auto-closed (not in Rust, where it starts lifetimes).
    pub close_single_quote: bool,
}

impl Default for Ctx<'_> {
    fn default() -> Self {
        Self {
            indent: Indent::default(),
            comment: Some("//"),
            close_single_quote: true,
        }
    }
}

// ------------------------------------------------------------------ helpers

fn line_start(rope: &Rope, line: usize) -> usize {
    rope.line_to_char(line)
}

fn line_end(rope: &Rope, line: usize) -> usize {
    rope.line_to_char(line) + line_len(rope, line)
}

fn char_at(rope: &Rope, pos: usize) -> Option<char> {
    (pos < rope.len_chars()).then(|| rope.char(pos))
}

fn char_before(rope: &Rope, pos: usize) -> Option<char> {
    (pos > 0).then(|| rope.char(pos - 1))
}

fn leading_ws(rope: &Rope, line: usize) -> String {
    rope.line(line)
        .chars()
        .take_while(|&c| c == ' ' || c == '\t')
        .collect()
}

/// Lines covered by a selection. A selection ending at the very start of a line
/// doesn't include that line.
pub fn sel_lines(rope: &Rope, s: &Selection) -> Range<usize> {
    let first = rope.char_to_line(s.start());
    let mut last = rope.char_to_line(s.end());
    if !s.is_empty() && last > first && s.end() == line_start(rope, last) {
        last -= 1;
    }
    first..last + 1
}

/// Line blocks of all selections, merged when they touch.
fn line_blocks(rope: &Rope, sels: &Selections) -> Vec<Range<usize>> {
    let mut blocks: Vec<Range<usize>> = Vec::new();
    for s in sels.iter() {
        let b = sel_lines(rope, s);
        match blocks.last_mut() {
            Some(last) if b.start <= last.end => last.end = last.end.max(b.end),
            _ => blocks.push(b),
        }
    }
    blocks
}

/// Applies one edit per selection; `f` returns the replaced range, the new text,
/// and the new selection as (anchor, head) offsets into the inserted text.
fn per_selection(
    sels: &Selections,
    kind: EditKind,
    mut f: impl FnMut(&Selection) -> (Range<usize>, String, (usize, usize)),
) -> Change {
    let mut items: Vec<(usize, Edit, (usize, usize))> = sels
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (range, text, offs) = f(s);
            (i, Edit::new(range, text), offs)
        })
        .collect();
    items.sort_by_key(|(_, e, _)| (e.range.start, e.range.end));
    // Overlapping edits (e.g. two carets deleting the same char) collapse to one.
    let mut kept: Vec<(usize, Edit, (usize, usize))> = Vec::with_capacity(items.len());
    for item in items {
        if kept
            .last()
            .is_some_and(|(_, l, _)| item.1.range.start < l.range.end)
        {
            continue;
        }
        kept.push(item);
    }
    let edits: Vec<Edit> = kept.iter().map(|(_, e, _)| e.clone()).collect();
    let starts = new_starts(&edits);
    let primary = sels.primary_index();
    let mut new_primary = 0;
    let after: Vec<Selection> = kept
        .iter()
        .zip(starts)
        .enumerate()
        .map(|(k, ((i, _, (a, h)), start))| {
            if *i == primary {
                new_primary = k;
            }
            Selection::new(start + a, start + h)
        })
        .collect();
    let edits: Vec<Edit> = edits
        .into_iter()
        .filter(|e| !(e.range.is_empty() && e.text.is_empty()))
        .collect();
    Change {
        edits,
        after: Selections::from_vec(after, new_primary),
        kind,
    }
}

/// Maps selections through edits. Endpoints at line starts that are in `stick`
/// stay put instead of moving right with inserted text.
fn map_sels(sels: &Selections, edits: &[Edit], stick_at: impl Fn(usize) -> bool) -> Selections {
    let m = |p: usize| {
        if stick_at(p)
            && edits
                .iter()
                .any(|e| e.range.start == p && e.range.is_empty())
        {
            map_pos(edits, p)
                - edits
                    .iter()
                    .find(|e| e.range.start == p)
                    .map_or(0, |e| e.text.chars().count())
        } else {
            map_pos(edits, p)
        }
    };
    sels.map(|s| Selection::new(m(s.anchor), m(s.head)))
}

// ------------------------------------------------------------------ typing

const PAIRS: [(char, char); 6] = [
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
];

fn closer_for(c: char) -> Option<char> {
    PAIRS.iter().find(|(o, _)| *o == c).map(|(_, cl)| *cl)
}

/// Types text at every selection, with bracket/quote auto-closing, wrapping
/// selections in brackets, and typing over an existing closing character.
pub fn type_text(rope: &Rope, sels: &Selections, text: &str, cx: Ctx) -> Change {
    let mut chars = text.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    per_selection(sels, EditKind::Typing, |s| {
        let Some(c) = single else {
            let n = text.chars().count();
            return (s.range(), text.to_owned(), (n, n));
        };
        let next = char_at(rope, s.end());
        let prev = char_before(rope, s.start());
        let is_quote = matches!(c, '"' | '\'' | '`');
        if c == '\'' && !cx.close_single_quote {
            return (s.range(), c.to_string(), (1, 1));
        }
        // Wrap a selection.
        if let (false, Some(close)) = (s.is_empty(), closer_for(c)) {
            let inner: String = rope.slice(s.range()).to_string();
            let n = inner.chars().count();
            return (s.range(), format!("{c}{inner}{close}"), (1, 1 + n));
        }
        // Type over a closing char that is already there.
        if s.is_empty() && next == Some(c) && (matches!(c, ')' | ']' | '}') || is_quote) {
            return (s.end()..s.end(), String::new(), (1, 1));
        }
        if let Some(close) = closer_for(c).filter(|_| s.is_empty()) {
            let next_ok = next.is_none_or(|n| {
                n.is_whitespace() || matches!(n, ')' | ']' | '}' | ';' | ',' | ':')
            });
            let prev_ok = !is_quote
                || prev.is_none_or(|p| !(p.is_alphanumeric() || p == '_' || p == c || p == '\\'));
            if next_ok && prev_ok {
                return (s.range(), format!("{c}{close}"), (1, 1));
            }
        }
        (s.range(), c.to_string(), (1, 1))
    })
}

/// Pastes text. With as many lines as cursors, each cursor gets one line (like VSCode).
/// `whole_lines` marks text copied without a selection: it goes above the caret's line.
pub fn paste(rope: &Rope, sels: &Selections, text: &str, whole_lines: bool) -> Change {
    let parts: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    let distribute = sels.len() > 1 && parts.len() == sels.len();
    let mut i = 0;
    per_selection(sels, EditKind::Other, |s| {
        let piece = if distribute {
            i += 1;
            parts[i - 1].to_owned()
        } else {
            text.to_owned()
        };
        if whole_lines && s.is_empty() && !distribute {
            let (line, col) = line_col(rope, s.head);
            let start = line_start(rope, line);
            let n = piece.chars().count();
            return (start..start, piece, (n + col, n + col));
        }
        let n = piece.chars().count();
        (s.range(), piece, (n, n))
    })
}

/// Enter: newline keeping the indentation, one level more after an opening
/// bracket, and the closing bracket on its own line when the caret sits between a pair.
pub fn newline(rope: &Rope, sels: &Selections, cx: Ctx) -> Change {
    let unit = cx.indent.unit();
    per_selection(sels, EditKind::Other, |s| {
        let (line, col) = line_col(rope, s.start());
        let base: String = leading_ws(rope, line).chars().take(col).collect();
        let before = rope.slice(line_start(rope, line)..s.start()).to_string();
        let prev = before.chars().rev().find(|c| !c.is_whitespace());
        let next = char_at(rope, s.end());
        let opens = matches!(prev, Some('{' | '[' | '('));
        let between = opens
            && matches!(
                (prev, next),
                (Some('{'), Some('}')) | (Some('['), Some(']')) | (Some('('), Some(')'))
            );
        let first = format!("\n{base}{}", if opens { unit.as_str() } else { "" });
        let n = first.chars().count();
        let text = if between {
            format!("{first}\n{base}")
        } else {
            first
        };
        (s.range(), text, (n, n))
    })
}

/// Backspace (or Ctrl+Backspace with `word`).
pub fn backspace(rope: &Rope, sels: &Selections, word: bool, cx: Ctx) -> Change {
    per_selection(sels, EditKind::Deleting, |s| {
        if !s.is_empty() {
            return (s.range(), String::new(), (0, 0));
        }
        let pos = s.head;
        if word {
            return (word_start(rope, pos)..pos, String::new(), (0, 0));
        }
        if pos == 0 {
            return (0..0, String::new(), (0, 0));
        }
        // Delete an auto-closed pair together.
        if let (Some(p), Some(n)) = (char_before(rope, pos), char_at(rope, pos))
            && closer_for(p) == Some(n)
        {
            return (pos - 1..pos + 1, String::new(), (0, 0));
        }
        // In leading spaces, go back to the previous indent stop.
        let (line, col) = line_col(rope, pos);
        let before = rope.slice(line_start(rope, line)..pos);
        if !cx.indent.tabs && col > 0 && before.chars().all(|c| c == ' ') {
            let back = (col - 1) % cx.indent.width + 1;
            return (pos - back..pos, String::new(), (0, 0));
        }
        (pos - 1..pos, String::new(), (0, 0))
    })
}

/// Delete key (or Ctrl+Delete with `word`).
pub fn delete_forward(rope: &Rope, sels: &Selections, word: bool) -> Change {
    let len = rope.len_chars();
    per_selection(sels, EditKind::Deleting, |s| {
        if !s.is_empty() {
            return (s.range(), String::new(), (0, 0));
        }
        let end = if word {
            word_end(rope, s.head)
        } else {
            (s.head + 1).min(len)
        };
        (s.head..end, String::new(), (0, 0))
    })
}

/// Tab: indents selected lines, or inserts indentation at each caret.
pub fn tab(rope: &Rope, sels: &Selections, cx: Ctx) -> Change {
    if sels.iter().any(|s| sel_lines(rope, s).len() > 1) {
        return indent_lines(rope, sels, cx);
    }
    per_selection(sels, EditKind::Typing, |s| {
        let text = if cx.indent.tabs {
            "\t".to_owned()
        } else {
            let (_, col) = line_col(rope, s.start());
            " ".repeat(cx.indent.width - col % cx.indent.width)
        };
        let n = text.chars().count();
        (s.range(), text, (n, n))
    })
}

/// Indents every line touched by a selection (Ctrl+] / Tab on a multi-line selection).
pub fn indent_lines(rope: &Rope, sels: &Selections, cx: Ctx) -> Change {
    let unit = cx.indent.unit();
    let mut edits = Vec::new();
    for b in line_blocks(rope, sels) {
        for line in b {
            if line_len(rope, line) > 0 {
                edits.push(Edit::insert(line_start(rope, line), unit.clone()));
            }
        }
    }
    let after = map_sels(sels, &edits, |p| {
        let line = rope.char_to_line(p);
        p == line_start(rope, line) && sels.iter().any(|s| !s.is_empty())
    });
    Change {
        edits,
        after,
        kind: EditKind::Other,
    }
}

/// Removes one level of indentation from every line touched by a selection.
pub fn outdent_lines(rope: &Rope, sels: &Selections, cx: Ctx) -> Change {
    let mut edits = Vec::new();
    for b in line_blocks(rope, sels) {
        for line in b {
            let start = line_start(rope, line);
            let n = match rope.line(line).chars().next() {
                Some('\t') => 1,
                _ => rope
                    .line(line)
                    .chars()
                    .take(cx.indent.width)
                    .take_while(|&c| c == ' ')
                    .count(),
            };
            if n > 0 {
                edits.push(Edit::new(start..start + n, ""));
            }
        }
    }
    let after = map_sels(sels, &edits, |_| false);
    Change {
        edits,
        after,
        kind: EditKind::Other,
    }
}

/// Ctrl+/: comments out the touched lines, or uncomments them if all are commented.
pub fn toggle_comment(rope: &Rope, sels: &Selections, cx: Ctx) -> Option<Change> {
    let token = cx.comment?;
    let lines: Vec<usize> = line_blocks(rope, sels).into_iter().flatten().collect();
    let content = |line: usize| -> Option<(usize, String)> {
        let ws = leading_ws(rope, line);
        let rest: String = rope.line(line).chars().skip(ws.chars().count()).collect();
        let rest = rest.trim_end_matches('\n').to_owned();
        (!rest.trim().is_empty()).then(|| (ws.chars().count(), rest))
    };
    let non_blank: Vec<(usize, usize, String)> = lines
        .iter()
        .filter_map(|&l| content(l).map(|(ws, rest)| (l, ws, rest)))
        .collect();
    if non_blank.is_empty() {
        return None;
    }
    let all_commented = non_blank.iter().all(|(_, _, rest)| rest.starts_with(token));
    let mut edits = Vec::new();
    if all_commented {
        for (l, ws, rest) in &non_blank {
            let start = line_start(rope, *l) + ws;
            let mut n = token.chars().count();
            if rest[token.len()..].starts_with(' ') {
                n += 1;
            }
            edits.push(Edit::new(start..start + n, ""));
        }
    } else {
        let col = non_blank.iter().map(|(_, ws, _)| *ws).min().unwrap_or(0);
        for (l, _, _) in &non_blank {
            edits.push(Edit::insert(
                line_start(rope, *l) + col,
                format!("{token} "),
            ));
        }
    }
    let after = map_sels(sels, &edits, |_| false);
    Some(Change {
        edits,
        after,
        kind: EditKind::Other,
    })
}

/// Alt+Up/Down: moves the touched lines up or down by one.
pub fn move_lines(rope: &Rope, sels: &Selections, up: bool) -> Option<Change> {
    let last_line = rope.len_lines() - 1;
    let blocks = line_blocks(rope, sels);
    if blocks
        .iter()
        .any(|b| if up { b.start == 0 } else { b.end > last_line })
    {
        return None;
    }
    let text_of = |lines: Range<usize>| -> String {
        let end = if lines.end > last_line {
            rope.len_chars()
        } else {
            line_start(rope, lines.end)
        };
        rope.slice(line_start(rope, lines.start)..end).to_string()
    };
    let mut edits = Vec::new();
    let mut deltas: Vec<(Range<usize>, isize)> = Vec::new();
    for b in blocks {
        let (region, first, second, delta) = if up {
            let prev = text_of(b.start - 1..b.start);
            (
                b.start - 1..b.end,
                text_of(b.clone()),
                prev.clone(),
                -(prev.trim_end_matches('\n').chars().count() as isize + 1),
            )
        } else {
            let next = text_of(b.end..b.end + 1);
            (
                b.start..b.end + 1,
                next.clone(),
                text_of(b.clone()),
                next.trim_end_matches('\n').chars().count() as isize + 1,
            )
        };
        // Join the two parts. The region's original last part is the one now
        // placed first, so its trailing newline decides the region's.
        let ends_nl = first.ends_with('\n');
        let joined = format!(
            "{}\n{}",
            first.trim_end_matches('\n'),
            second.trim_end_matches('\n')
        );
        let joined = if ends_nl { joined + "\n" } else { joined };
        let start = line_start(rope, region.start);
        let end = if region.end > last_line {
            rope.len_chars()
        } else {
            line_start(rope, region.end)
        };
        edits.push(Edit::new(start..end, joined));
        let lines = line_start(rope, b.start)..if b.end > last_line {
            rope.len_chars() + 1
        } else {
            line_start(rope, b.end)
        };
        deltas.push((lines, delta));
    }
    let after = sels.map(|s| {
        let d = deltas
            .iter()
            .find(|(r, _)| r.contains(&s.start()))
            .map_or(0, |(_, d)| *d);
        let m = |p: usize| (p as isize + d).max(0) as usize;
        Selection::new(m(s.anchor), m(s.head))
    });
    Some(Change {
        edits,
        after,
        kind: EditKind::Other,
    })
}

/// Shift+Alt+Up/Down: duplicates the touched lines; the selection ends up on the
/// lower copy when going down.
pub fn copy_lines(rope: &Rope, sels: &Selections, up: bool) -> Change {
    let last_line = rope.len_lines() - 1;
    let mut edits = Vec::new();
    let mut shifts: Vec<(Range<usize>, usize)> = Vec::new();
    for b in line_blocks(rope, sels) {
        let start = line_start(rope, b.start);
        let (at, text) = if b.end > last_line {
            let body = rope.slice(start..).to_string();
            (rope.len_chars(), format!("\n{body}"))
        } else {
            let end = line_start(rope, b.end);
            (end, rope.slice(start..end).to_string())
        };
        let n = text.chars().count();
        edits.push(Edit::insert(at, text));
        shifts.push((start..at + 1, n));
    }
    let after = if up {
        // Keep the selection on the upper (original) lines.
        map_sels(sels, &edits, |_| true)
    } else {
        let edits_ref = &edits;
        sels.map(|s| {
            let n = shifts
                .iter()
                .find(|(r, _)| r.contains(&s.start()))
                .map_or(0, |(_, n)| *n);
            let base = |p: usize| {
                map_pos(edits_ref, p)
                    - if edits_ref.iter().any(|e| e.range.start == p) {
                        n
                    } else {
                        0
                    }
            };
            Selection::new(base(s.anchor) + n, base(s.head) + n)
        })
    };
    Change {
        edits,
        after,
        kind: EditKind::Other,
    }
}

/// Ctrl+Shift+K: deletes the touched lines.
pub fn delete_lines(rope: &Rope, sels: &Selections) -> Change {
    let last_line = rope.len_lines() - 1;
    let mut edits = Vec::new();
    for b in line_blocks(rope, sels) {
        let start = line_start(rope, b.start);
        let range = if b.end > last_line {
            start.saturating_sub(usize::from(b.start > 0))..rope.len_chars()
        } else {
            start..line_start(rope, b.end)
        };
        edits.push(Edit::new(range, ""));
    }
    let edits = sort_edits(edits);
    // Keep each caret's column on the line that takes the deleted lines' place.
    let blocks = line_blocks(rope, sels);
    let after = sels.map(|s| {
        let (_, col) = line_col(rope, s.head);
        let line = rope.char_to_line(s.head);
        let b = blocks
            .iter()
            .find(|b| b.contains(&line))
            .cloned()
            .unwrap_or(line..line + 1);
        let (next, len) = if b.end <= last_line {
            (line_start(rope, b.end), line_len(rope, b.end))
        } else if b.start > 0 {
            (line_start(rope, b.start - 1), line_len(rope, b.start - 1))
        } else {
            (0, 0)
        };
        Selection::caret(map_pos(&edits, next) + col.min(len))
    });
    Change {
        edits,
        after,
        kind: EditKind::Other,
    }
}

/// Ctrl+Enter / Ctrl+Shift+Enter: starts a new line below/above, keeping indentation.
pub fn insert_line(rope: &Rope, sels: &Selections, above: bool) -> Change {
    per_selection(sels, EditKind::Other, |s| {
        let line = rope.char_to_line(s.head);
        let ws = leading_ws(rope, line);
        let n = ws.chars().count();
        if above {
            let at = line_start(rope, line);
            (at..at, format!("{ws}\n"), (n, n))
        } else {
            let at = line_end(rope, line);
            (at..at, format!("\n{ws}"), (n + 1, n + 1))
        }
    })
}

/// Text to copy: the selections joined by newlines, or whole lines when nothing
/// is selected. The flag says whether it's whole lines.
pub fn copy_text(rope: &Rope, sels: &Selections) -> (String, bool) {
    if sels.iter().all(|s| s.is_empty()) {
        let mut out = String::new();
        for b in line_blocks(rope, sels) {
            let start = line_start(rope, b.start);
            let end = if b.end >= rope.len_lines() {
                rope.len_chars()
            } else {
                line_start(rope, b.end)
            };
            out.push_str(&rope.slice(start..end).to_string());
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        (out, true)
    } else {
        let parts: Vec<String> = sels
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| rope.slice(s.range()).to_string())
            .collect();
        (parts.join("\n"), false)
    }
}

/// Cut: like copy, then deletes the selections (or whole lines).
pub fn cut(rope: &Rope, sels: &Selections) -> (String, bool, Change) {
    let (text, lines) = copy_text(rope, sels);
    let change = if lines {
        delete_lines(rope, sels)
    } else {
        per_selection(sels, EditKind::Other, |s| {
            (s.range(), String::new(), (0, 0))
        })
    };
    (text, lines, change)
}

// ------------------------------------------------------------------ movement

/// Left/Right (with Ctrl: by word). Without `extend`, a selection collapses to its edge.
pub fn move_horizontal(
    rope: &Rope,
    sels: &Selections,
    right: bool,
    word: bool,
    extend: bool,
) -> Selections {
    let len = rope.len_chars();
    sels.map(|s| {
        if !extend && !s.is_empty() && !word {
            return Selection::caret(if right { s.end() } else { s.start() });
        }
        let head = match (right, word) {
            (true, true) => word_end(rope, s.head),
            (true, false) => (s.head + 1).min(len),
            (false, true) => word_start(rope, s.head),
            (false, false) => s.head.saturating_sub(1),
        };
        if extend {
            Selection::new(s.anchor, head)
        } else {
            Selection::caret(head)
        }
    })
}

/// Home: toggles between the first non-blank character and column 0.
pub fn home(rope: &Rope, sels: &Selections, extend: bool) -> Selections {
    sels.map(|s| {
        let line = rope.char_to_line(s.head);
        let start = line_start(rope, line);
        let first = start
            + leading_ws(rope, line)
                .chars()
                .count()
                .min(line_len(rope, line));
        let head = if s.head == first { start } else { first };
        if extend {
            Selection::new(s.anchor, head)
        } else {
            Selection::caret(head)
        }
    })
}

pub fn end(rope: &Rope, sels: &Selections, extend: bool) -> Selections {
    sels.map(|s| {
        let head = line_end(rope, rope.char_to_line(s.head));
        if extend {
            Selection::new(s.anchor, head)
        } else {
            Selection::caret(head)
        }
    })
}

/// Moves to a fixed position (document start/end).
pub fn move_to(sels: &Selections, pos: usize, extend: bool) -> Selections {
    let p = sels.primary();
    Selections::single(if extend {
        Selection::new(p.anchor, pos)
    } else {
        Selection::caret(pos)
    })
}

pub fn select_all(rope: &Rope) -> Selections {
    Selections::single(Selection::new(0, rope.len_chars()))
}

/// Ctrl+L: selects whole lines; repeating extends by a line.
pub fn select_line(rope: &Rope, sels: &Selections) -> Selections {
    let last_line = rope.len_lines() - 1;
    sels.map(|s| {
        let lines = sel_lines(rope, &s);
        let full = s.start() == line_start(rope, lines.start)
            && (lines.end > last_line || s.end() == line_start(rope, lines.end));
        let end_line = if full && !s.is_empty() {
            lines.end + 1
        } else {
            lines.end
        };
        let end = if end_line > last_line {
            rope.len_chars()
        } else {
            line_start(rope, end_line)
        };
        Selection::new(line_start(rope, lines.start), end)
    })
}

/// Ctrl+D: selects the word under the caret, or adds the next occurrence of the
/// primary selection as a new selection.
pub fn add_next_occurrence(rope: &Rope, sels: &Selections) -> Selections {
    let p = sels.primary();
    if p.is_empty() {
        return sels.map(|s| {
            let w = word_at(rope, s.head);
            Selection::new(w.start, w.end)
        });
    }
    let needle = rope.slice(p.range()).to_string();
    let hay = rope.to_string();
    let from = rope.char_to_byte(sels.iter().map(|s| s.end()).max().unwrap_or(0));
    let found = hay[from..]
        .find(&needle)
        .map(|b| b + from)
        .or_else(|| hay.find(&needle));
    let mut out = sels.clone();
    if let Some(b) = found {
        let start = rope.byte_to_char(b);
        let sel = Selection::new(start, start + needle.chars().count());
        if !sels.iter().any(|s| s.range() == sel.range()) {
            out.push(sel);
        }
    }
    out
}

/// Ctrl+Shift+L: selects every occurrence of the primary selection (or word).
pub fn select_all_occurrences(rope: &Rope, sels: &Selections) -> Selections {
    let p = sels.primary();
    let range = if p.is_empty() {
        word_at(rope, p.head)
    } else {
        p.range()
    };
    if range.is_empty() {
        return sels.clone();
    }
    let needle = rope.slice(range.clone()).to_string();
    let hay = rope.to_string();
    let n = needle.chars().count();
    let mut list = Vec::new();
    let mut primary = 0;
    for (b, _) in hay.match_indices(&needle) {
        let start = rope.byte_to_char(b);
        if start == range.start {
            primary = list.len();
        }
        list.push(Selection::new(start, start + n));
    }
    if list.is_empty() {
        sels.clone()
    } else {
        Selections::from_vec(list, primary)
    }
}

/// The bracket matching the one at or just before `pos`, if any (scans up to 50k chars).
pub fn matching_bracket(rope: &Rope, pos: usize) -> Option<(usize, usize)> {
    const LIMIT: usize = 50_000;
    let pairs = [('(', ')'), ('[', ']'), ('{', '}')];
    let candidates = [Some(pos), pos.checked_sub(1)];
    for at in candidates.into_iter().flatten() {
        let Some(c) = char_at(rope, at) else { continue };
        if let Some(&(open, close)) = pairs.iter().find(|(o, _)| *o == c) {
            let mut depth = 0usize;
            for (i, ch) in rope.chars_at(at).enumerate().take(LIMIT) {
                if ch == open {
                    depth += 1;
                } else if ch == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some((at, at + i));
                    }
                }
            }
            return None;
        }
        if let Some(&(open, close)) = pairs.iter().find(|(_, cl)| *cl == c) {
            let mut depth = 0usize;
            let mut it = rope.chars_at(at + 1);
            let mut i = at + 1;
            while let Some(ch) = it.prev() {
                i -= 1;
                if at - i > LIMIT {
                    break;
                }
                if ch == close {
                    depth += 1;
                } else if ch == open {
                    depth -= 1;
                    if depth == 0 {
                        return Some((i, at));
                    }
                }
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Buffer, Selections};

    /// Runs an op on text where `|` marks carets and `[..]` selections (anchor at `[`).
    fn parse(marked: &str) -> (String, Selections) {
        let mut text = String::new();
        let mut sels = Vec::new();
        let mut open = None;
        for c in marked.chars() {
            let pos = text.chars().count();
            match c {
                '|' => sels.push(Selection::caret(pos)),
                '«' => open = Some(pos),
                '»' => sels.push(Selection::new(open.take().unwrap(), pos)),
                _ => text.push(c),
            }
        }
        let n = sels.len();
        (text, Selections::from_vec(sels, n - 1))
    }

    fn render(text: &str, sels: &Selections) -> String {
        let mut marks: Vec<(usize, &str)> = Vec::new();
        for s in sels.iter() {
            if s.is_empty() {
                marks.push((s.head, "|"));
            } else {
                marks.push((s.start(), "«"));
                marks.push((s.end(), "»"));
            }
        }
        marks.sort_by_key(|(p, m)| (*p, *m == "«"));
        let mut out = String::new();
        let chars: Vec<char> = text.chars().collect();
        let mut mi = 0;
        for i in 0..=chars.len() {
            while mi < marks.len() && marks[mi].0 == i {
                out.push_str(marks[mi].1);
                mi += 1;
            }
            if i < chars.len() {
                out.push(chars[i]);
            }
        }
        out
    }

    fn run(marked: &str, op: impl Fn(&Rope, &Selections) -> Change) -> String {
        let (text, sels) = parse(marked);
        let mut b = Buffer::new(&text);
        let change = op(b.rope(), &sels);
        let after = change.after.clone();
        b.edit(change.edits, sels, after.clone(), change.kind);
        render(&b.rope().to_string(), &after)
    }

    fn cx() -> Ctx<'static> {
        Ctx::default()
    }

    #[test]
    fn typing_and_auto_close() {
        assert_eq!(run("a|b", |r, s| type_text(r, s, "x", cx())), "ax|b");
        assert_eq!(run("f|", |r, s| type_text(r, s, "(", cx())), "f(|)");
        assert_eq!(run("f(|)", |r, s| type_text(r, s, ")", cx())), "f()|");
        assert_eq!(run("«ab»", |r, s| type_text(r, s, "[", cx())), "[«ab»]");
        assert_eq!(
            run("don|", |r, s| type_text(r, s, "'", cx())),
            "don'|",
            "no quote pair after a letter"
        );
        assert_eq!(
            run("|x", |r, s| type_text(r, s, "(", cx())),
            "(|x",
            "no pair before a word"
        );
        assert_eq!(run("a|\nb|", |r, s| type_text(r, s, "!", cx())), "a!|\nb!|");
    }

    #[test]
    fn newline_indents() {
        let r = run("    if x {|}", |r, s| newline(r, s, cx()));
        assert_eq!(r, "    if x {\n        |\n    }");
        assert_eq!(run("  a|", |r, s| newline(r, s, cx())), "  a\n  |");
    }

    #[test]
    fn backspace_variants() {
        assert_eq!(run("ab|", |r, s| backspace(r, s, false, cx())), "a|");
        assert_eq!(run("(|)", |r, s| backspace(r, s, false, cx())), "|");
        assert_eq!(
            run("        |x", |r, s| backspace(r, s, false, cx())),
            "    |x"
        );
        assert_eq!(run("foo bar|", |r, s| backspace(r, s, true, cx())), "foo |");
        // Two carets deleting towards each other end up merged.
        assert_eq!(run("a|b|c", |r, s| backspace(r, s, false, cx())), "|c");
    }

    #[test]
    fn line_operations() {
        assert_eq!(
            run("a\nb|\nc", |r, s| move_lines(r, s, true).unwrap()),
            "b|\na\nc"
        );
        assert_eq!(
            run("a\nb|\nc", |r, s| move_lines(r, s, false).unwrap()),
            "a\nc\nb|"
        );
        assert_eq!(
            run("a\nb|\nc\n", |r, s| move_lines(r, s, false).unwrap()),
            "a\nc\nb|\n"
        );
        assert_eq!(
            run("a\nb|", |r, s| move_lines(r, s, true).unwrap()),
            "b|\na"
        );
        assert_eq!(run("a|\nb", |r, s| copy_lines(r, s, false)), "a\na|\nb");
        assert_eq!(run("a|\nb", |r, s| copy_lines(r, s, true)), "a|\na\nb");
        assert_eq!(run("a\nb|\nc", delete_lines), "a\nc|");
        assert_eq!(run("ab\nb|", delete_lines), "a|b");
        assert_eq!(run("only|", delete_lines), "|");
        assert_eq!(run("  a|", |r, s| insert_line(r, s, false)), "  a\n  |");
        assert_eq!(run("  a|", |r, s| insert_line(r, s, true)), "  |\n  a");
    }

    #[test]
    fn indentation() {
        assert_eq!(
            run("«a\nb»", |r, s| indent_lines(r, s, cx())),
            "«    a\n    b»"
        );
        assert_eq!(run("    a|", |r, s| outdent_lines(r, s, cx())), "a|");
        assert_eq!(run("ab|", |r, s| tab(r, s, cx())), "ab  |");
    }

    #[test]
    fn comments() {
        assert_eq!(
            run("  a|\n  b", |r, s| toggle_comment(r, s, cx()).unwrap()),
            "  // a|\n  b"
        );
        assert_eq!(
            run("  // a|", |r, s| toggle_comment(r, s, cx()).unwrap()),
            "  a|"
        );
        // Blank lines are skipped; the selection moves with the text.
        assert_eq!(
            run("«x\n\n  y»", |r, s| toggle_comment(r, s, cx()).unwrap()),
            "// «x\n\n//   y»"
        );
    }

    #[test]
    fn occurrences_and_lines() {
        let (text, sels) = parse("foo| bar foo foo");
        let rope = Rope::from_str(&text);
        let s1 = add_next_occurrence(&rope, &sels);
        assert_eq!(render(&text, &s1), "«foo» bar foo foo");
        let s2 = add_next_occurrence(&rope, &s1);
        assert_eq!(render(&text, &s2), "«foo» bar «foo» foo");
        let all = select_all_occurrences(&rope, &sels);
        assert_eq!(all.len(), 3);
        let (text, sels) = parse("a|\nb\nc");
        let rope = Rope::from_str(&text);
        let l = select_line(&rope, &sels);
        assert_eq!(render(&text, &l), "«a\n»b\nc");
        assert_eq!(render(&text, &select_line(&rope, &l)), "«a\nb\n»c");
    }

    #[test]
    fn copy_paste_lines() {
        let (text, sels) = parse("ab|\ncd");
        let rope = Rope::from_str(&text);
        assert_eq!(copy_text(&rope, &sels), ("ab\n".to_owned(), true));
        assert_eq!(
            run("x|y\nz", |r, s| paste(r, s, "ab\n", true)),
            "ab\nx|y\nz"
        );
        assert_eq!(run("|\n|", |r, s| paste(r, s, "1\n2", false)), "1|\n2|");
    }

    #[test]
    fn brackets() {
        let rope = Rope::from_str("f(a[1], {b})");
        assert_eq!(matching_bracket(&rope, 1), Some((1, 11)));
        assert_eq!(matching_bracket(&rope, 12), Some((1, 11)));
        assert_eq!(matching_bracket(&rope, 3), Some((3, 5)));
    }

    #[test]
    fn indent_detection() {
        assert_eq!(
            Indent::detect(&Rope::from_str("a\n  b\n    c\n  d\n")),
            Indent {
                tabs: false,
                width: 2
            }
        );
        assert_eq!(
            Indent::detect(&Rope::from_str("a\n\tb\n\t\tc\n")),
            Indent {
                tabs: true,
                width: 4
            }
        );
        assert_eq!(
            Indent::detect(&Rope::from_str("a\n    b\n")),
            Indent {
                tabs: false,
                width: 4
            }
        );
    }
}
