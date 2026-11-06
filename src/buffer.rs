//! Text storage: a rope with multiple selections and an undo history that only
//! records what changed (never whole-text snapshots).
//!
//! All positions are char indices into the rope. Lines are separated by `\n` only.

use std::ops::Range;
use std::time::{Duration, Instant};

use ropey::Rope;

/// One selection. `anchor == head` is a plain caret.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Horizontal pixel position kept across vertical moves.
    pub goal_x: Option<f32>,
}

impl Selection {
    pub fn caret(pos: usize) -> Self {
        Self { anchor: pos, head: pos, goal_x: None }
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head, goal_x: None }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// A non-empty, sorted set of non-overlapping selections, one of which is primary
/// (the most recently added; it drives scrolling and the status bar).
#[derive(Clone, Debug, PartialEq)]
pub struct Selections {
    list: Vec<Selection>,
    primary: usize,
}

impl Default for Selections {
    fn default() -> Self {
        Self::single(Selection::caret(0))
    }
}

impl Selections {
    pub fn single(sel: Selection) -> Self {
        Self { list: vec![sel], primary: 0 }
    }

    pub fn from_vec(list: Vec<Selection>, primary: usize) -> Self {
        assert!(!list.is_empty());
        let mut s = Self { primary: primary.min(list.len() - 1), list };
        s.normalize();
        s
    }

    pub fn primary(&self) -> Selection {
        self.list[self.primary]
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Selection> {
        self.list.iter()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Adds a selection and makes it primary.
    pub fn push(&mut self, sel: Selection) {
        self.list.push(sel);
        self.primary = self.list.len() - 1;
        self.normalize();
    }

    /// Transforms every selection, keeping which one is primary.
    pub fn map(&self, f: impl FnMut(Selection) -> Selection) -> Self {
        Self::from_vec(self.list.iter().copied().map(f).collect(), self.primary)
    }

    /// Replaces the primary selection (e.g. while drag-selecting).
    pub fn with_primary(&self, sel: Selection) -> Self {
        let mut list = self.list.clone();
        list[self.primary] = sel;
        Self::from_vec(list, self.primary)
    }

    /// Keeps only the primary selection.
    pub fn collapse_to_primary(&self) -> Self {
        Self::single(self.primary())
    }

    /// Clamps positions to a text of `len` chars.
    pub fn clamp(&self, len: usize) -> Self {
        self.map(|s| Selection { anchor: s.anchor.min(len), head: s.head.min(len), goal_x: s.goal_x })
    }

    /// Sorts and merges overlapping selections (and carets at the same spot).
    fn normalize(&mut self) {
        let primary = self.list[self.primary];
        let mut indexed: Vec<(usize, Selection)> = self.list.drain(..).enumerate().collect();
        indexed.sort_by_key(|(_, s)| (s.start(), s.end()));
        let mut merged: Vec<Selection> = Vec::with_capacity(indexed.len());
        let mut new_primary = 0;
        for (i, s) in indexed {
            let is_primary = i == self.primary;
            if let Some(last) = merged.last_mut() {
                let overlaps = s.start() < last.end()
                    || s.start() == last.start()
                    || (s.start() == last.end() && (s.is_empty() || last.is_empty()));
                if overlaps {
                    let (start, end) = (last.start(), last.end().max(s.end()));
                    *last = if last.head >= last.anchor {
                        Selection::new(start, end)
                    } else {
                        Selection::new(end, start)
                    };
                    if is_primary {
                        new_primary = merged.len() - 1;
                    }
                    continue;
                }
            }
            if is_primary {
                new_primary = merged.len();
            }
            merged.push(s);
        }
        self.list = merged;
        self.primary = new_primary;
        // Keep the primary's goal column if it survived unmerged.
        if self.list[self.primary].range() == primary.range() {
            self.list[self.primary].goal_x = primary.goal_x;
        }
    }
}

/// Replace `range` (char indices in the text *before* the edit) with `text`.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

impl Edit {
    pub fn new(range: Range<usize>, text: impl Into<String>) -> Self {
        Self { range, text: text.into() }
    }

    pub fn insert(pos: usize, text: impl Into<String>) -> Self {
        Self::new(pos..pos, text)
    }
}

/// Sorts edits and drops ones that overlap an earlier edit (which would be ambiguous).
pub fn sort_edits(mut edits: Vec<Edit>) -> Vec<Edit> {
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    let mut out: Vec<Edit> = Vec::with_capacity(edits.len());
    for e in edits {
        if out.last().is_some_and(|l| e.range.start < l.range.end) {
            continue;
        }
        if e.range.is_empty() && e.text.is_empty() {
            continue;
        }
        out.push(e);
    }
    out
}

/// Where position `pos` (before the edits) ends up afterwards. Positions inside a
/// replaced range, and at an insertion point, move to the end of the inserted text.
pub fn map_pos(edits: &[Edit], pos: usize) -> usize {
    let mut delta: isize = 0;
    for e in edits {
        let inserted = e.text.chars().count() as isize;
        if e.range.end <= pos {
            delta += inserted - e.range.len() as isize;
        } else if e.range.start < pos {
            return (e.range.start as isize + delta + inserted) as usize;
        } else {
            break;
        }
    }
    (pos as isize + delta) as usize
}

/// Start of each edit in the text *after* all edits are applied.
pub fn new_starts(edits: &[Edit]) -> Vec<usize> {
    let mut delta: isize = 0;
    edits
        .iter()
        .map(|e| {
            let start = (e.range.start as isize + delta) as usize;
            delta += e.text.chars().count() as isize - e.range.len() as isize;
            start
        })
        .collect()
}

/// Lines `first .. first + old_lines` were replaced by `first .. first + new_lines`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineChange {
    pub first: usize,
    pub old_lines: usize,
    pub new_lines: usize,
}

/// Consecutive edits of the same kind are undone together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Typing,
    Deleting,
    Other,
}

const MERGE_WINDOW: Duration = Duration::from_millis(1500);
const MAX_UNDO: usize = 1000;

struct Change {
    start: usize,
    removed: String,
    inserted: String,
}

struct Transaction {
    /// Each step is a list of changes in the coordinates of the text before that step.
    steps: Vec<Vec<Change>>,
    before: Selections,
    after: Selections,
    rev_before: u64,
    rev_after: u64,
    kind: EditKind,
    at: Instant,
}

/// The result of an edit, undo or redo.
pub struct Applied {
    /// What changed, as edits against the previous text. Undo/redo of a merged
    /// transaction yield one entry per step, to be applied in order.
    pub edits: Vec<Vec<Edit>>,
    pub lines: Vec<LineChange>,
    pub selections: Selections,
}

pub struct Buffer {
    rope: Rope,
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    rev: u64,
    next_rev: u64,
}

impl Buffer {
    pub fn new(text: &str) -> Self {
        Self { rope: Rope::from_str(text), undo: Vec::new(), redo: Vec::new(), rev: 0, next_rev: 1 }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Changes whenever the text changes; undoing back restores the earlier value.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    #[cfg(test)]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Replaces the whole text without undo history (used for reloading from disk).
    pub fn reset(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
        self.undo.clear();
        self.redo.clear();
        self.rev = self.next_rev;
        self.next_rev += 1;
    }

    /// Applies edits (sorted, non-overlapping, see [`sort_edits`]) and records them for undo.
    pub fn edit(&mut self, edits: Vec<Edit>, before: Selections, after: Selections, kind: EditKind) -> Option<Applied> {
        if edits.is_empty() {
            return None;
        }
        let (changes, line) = self.apply(&edits);
        let rev_before = self.rev;
        self.rev = self.next_rev;
        self.next_rev += 1;
        self.redo.clear();

        let multiline = changes.iter().any(|c| c.inserted.contains('\n'));
        if let Some(last) = self.undo.last_mut()
            && kind != EditKind::Other
            && last.kind == kind
            && !multiline
            && last.at.elapsed() < MERGE_WINDOW
            && last.after == before
        {
            last.steps.push(changes);
            last.after = after.clone();
            last.rev_after = self.rev;
            last.at = Instant::now();
        } else {
            self.undo.push(Transaction {
                steps: vec![changes],
                before,
                after: after.clone(),
                rev_before,
                rev_after: self.rev,
                kind,
                at: Instant::now(),
            });
            if self.undo.len() > MAX_UNDO {
                self.undo.remove(0);
            }
        }
        Some(Applied { edits: vec![edits], lines: vec![line], selections: after })
    }

    pub fn undo(&mut self) -> Option<Applied> {
        let t = self.undo.pop()?;
        let mut edits = Vec::new();
        let mut lines = Vec::new();
        for step in t.steps.iter().rev() {
            // Inverse edits, in the coordinates of the text after this step.
            let mut delta: isize = 0;
            let inverse: Vec<Edit> = step
                .iter()
                .map(|c| {
                    let start = (c.start as isize + delta) as usize;
                    let ins = c.inserted.chars().count();
                    delta += ins as isize - c.removed.chars().count() as isize;
                    Edit::new(start..start + ins, c.removed.clone())
                })
                .collect();
            lines.push(self.apply(&inverse).1);
            edits.push(inverse);
        }
        self.rev = t.rev_before;
        let selections = t.before.clamp(self.rope.len_chars());
        self.redo.push(t);
        Some(Applied { edits, lines, selections })
    }

    pub fn redo(&mut self) -> Option<Applied> {
        let t = self.redo.pop()?;
        let mut edits = Vec::new();
        let mut lines = Vec::new();
        for step in &t.steps {
            let forward: Vec<Edit> = step
                .iter()
                .map(|c| Edit::new(c.start..c.start + c.removed.chars().count(), c.inserted.clone()))
                .collect();
            lines.push(self.apply(&forward).1);
            edits.push(forward);
        }
        self.rev = t.rev_after;
        let selections = t.after.clamp(self.rope.len_chars());
        self.undo.push(t);
        Some(Applied { edits, lines, selections })
    }

    /// Applies sorted, non-overlapping edits to the rope.
    fn apply(&mut self, edits: &[Edit]) -> (Vec<Change>, LineChange) {
        let first_line = self.rope.char_to_line(edits[0].range.start);
        let last_old = edits.last().expect("non-empty").range.end;
        let old_last_line = self.rope.char_to_line(last_old);

        let changes: Vec<Change> = edits
            .iter()
            .map(|e| Change {
                start: e.range.start,
                removed: self.rope.slice(e.range.clone()).to_string(),
                inserted: e.text.clone(),
            })
            .collect();
        // Back to front, so earlier positions stay valid.
        for e in edits.iter().rev() {
            if !e.range.is_empty() {
                self.rope.remove(e.range.clone());
            }
            if !e.text.is_empty() {
                self.rope.insert(e.range.start, &e.text);
            }
        }
        let new_last = map_pos(edits, last_old).min(self.rope.len_chars());
        let new_last_line = self.rope.char_to_line(new_last);
        let line = LineChange {
            first: first_line,
            old_lines: old_last_line - first_line + 1,
            new_lines: new_last_line - first_line + 1,
        };
        (changes, line)
    }
}

// ------------------------------------------------------------------ helpers

/// Length of a line in chars, without its newline.
pub fn line_len(rope: &Rope, line: usize) -> usize {
    let slice = rope.line(line);
    let n = slice.len_chars();
    if n > 0 && slice.char(n - 1) == '\n' { n - 1 } else { n }
}

/// (line, column) of a char position, both 0-based.
pub fn line_col(rope: &Rope, pos: usize) -> (usize, usize) {
    let line = rope.char_to_line(pos);
    (line, pos - rope.line_to_char(line))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    Space,
    Word,
    Punct,
    Newline,
}

pub fn char_class(c: char) -> CharClass {
    if c == '\n' {
        CharClass::Newline
    } else if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Position after the next word to the right (Ctrl+Right).
pub fn word_end(rope: &Rope, mut pos: usize) -> usize {
    let len = rope.len_chars();
    if pos >= len {
        return len;
    }
    if rope.char(pos) == '\n' {
        return pos + 1;
    }
    while pos < len && char_class(rope.char(pos)) == CharClass::Space {
        pos += 1;
    }
    if pos < len {
        let class = char_class(rope.char(pos));
        if class != CharClass::Newline {
            while pos < len && char_class(rope.char(pos)) == class {
                pos += 1;
            }
        }
    }
    pos
}

/// Position before the previous word to the left (Ctrl+Left).
pub fn word_start(rope: &Rope, mut pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    if rope.char(pos - 1) == '\n' {
        return pos - 1;
    }
    while pos > 0 && char_class(rope.char(pos - 1)) == CharClass::Space {
        pos -= 1;
    }
    if pos > 0 {
        let class = char_class(rope.char(pos - 1));
        if class != CharClass::Newline {
            while pos > 0 && char_class(rope.char(pos - 1)) == class {
                pos -= 1;
            }
        }
    }
    pos
}

/// The word (or run of same-class chars) around `pos`, for double-click.
pub fn word_at(rope: &Rope, pos: usize) -> Range<usize> {
    let len = rope.len_chars();
    let is_word = |i: usize| char_class(rope.char(i)) == CharClass::Word;
    // Prefer a word on either side of the caret over spaces or punctuation.
    let probe = if pos < len && is_word(pos) {
        pos
    } else if pos > 0 && is_word(pos - 1) {
        pos - 1
    } else if pos < len && char_class(rope.char(pos)) != CharClass::Newline {
        pos
    } else if pos > 0 {
        pos - 1
    } else {
        return pos..pos;
    };
    let class = char_class(rope.char(probe));
    if class == CharClass::Newline {
        return pos..pos;
    }
    let mut start = probe;
    while start > 0 && char_class(rope.char(start - 1)) == class {
        start -= 1;
    }
    let mut end = probe + 1;
    while end < len && char_class(rope.char(end)) == class {
        end += 1;
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(b: &Buffer) -> String {
        b.rope().to_string()
    }

    #[test]
    fn edit_undo_redo_roundtrip() {
        let mut b = Buffer::new("hello world\nsecond line\n");
        let sel = Selections::default();
        // Replace "hello", insert at the start of line 2, delete " line" (keeping its newline).
        let edits = sort_edits(vec![Edit::new(0..5, "HEY"), Edit::insert(12, ">> "), Edit::new(18..23, "")]);
        let rev0 = b.rev();
        b.edit(edits, sel.clone(), sel.clone(), EditKind::Other).unwrap();
        assert_eq!(text(&b), "HEY world\n>> second\n");
        assert_ne!(b.rev(), rev0);
        b.undo().unwrap();
        assert_eq!(text(&b), "hello world\nsecond line\n");
        assert_eq!(b.rev(), rev0, "undo restores the revision, so the doc is clean again");
        b.redo().unwrap();
        assert_eq!(text(&b), "HEY world\n>> second\n");
    }

    #[test]
    fn typing_merges_into_one_undo_step() {
        let mut b = Buffer::new("");
        let mut sel = Selections::default();
        for (i, c) in "abc".chars().enumerate() {
            let after = Selections::single(Selection::caret(i + 1));
            b.edit(vec![Edit::insert(i, c.to_string())], sel, after.clone(), EditKind::Typing);
            sel = after;
        }
        assert_eq!(text(&b), "abc");
        let applied = b.undo().unwrap();
        assert_eq!(text(&b), "");
        assert_eq!(applied.selections.primary().head, 0);
        assert!(!b.can_undo());
        b.redo();
        assert_eq!(text(&b), "abc");
    }

    #[test]
    fn line_change_reports_spliced_lines() {
        let mut b = Buffer::new("a\nb\nc\nd\n");
        let s = Selections::default();
        // Replace "b\nc" with "x\ny\nz".
        let applied = b.edit(vec![Edit::new(2..5, "x\ny\nz")], s.clone(), s, EditKind::Other).unwrap();
        assert_eq!(applied.lines[0], LineChange { first: 1, old_lines: 2, new_lines: 3 });
    }

    #[test]
    fn map_pos_through_edits() {
        let edits = vec![Edit::new(2..4, "xyz"), Edit::insert(10, "!!")];
        assert_eq!(map_pos(&edits, 0), 0);
        assert_eq!(map_pos(&edits, 3), 5, "inside a replaced range moves to its end");
        assert_eq!(map_pos(&edits, 5), 6);
        assert_eq!(map_pos(&edits, 10), 13, "an insertion at the caret pushes it right");
    }

    #[test]
    fn selections_merge_and_keep_primary() {
        let s = Selections::from_vec(
            vec![Selection::new(10, 15), Selection::caret(3), Selection::new(12, 20), Selection::caret(3)],
            2,
        );
        let v: Vec<_> = s.iter().map(|s| s.range()).collect();
        assert_eq!(v, vec![3..3, 10..20]);
        assert_eq!(s.primary().range(), 10..20);
    }

    #[test]
    fn word_motion() {
        let r = Rope::from_str("foo.bar  baz\nqux");
        assert_eq!(word_end(&r, 0), 3);
        assert_eq!(word_end(&r, 3), 4);
        assert_eq!(word_end(&r, 7), 12);
        assert_eq!(word_end(&r, 12), 13, "newline is its own stop");
        assert_eq!(word_start(&r, 12), 9);
        assert_eq!(word_start(&r, 13), 12);
        assert_eq!(word_at(&r, 5), 4..7);
    }
}
