//! Incremental, lazy syntax highlighting over a rope.
//!
//! * Only lines up to what is on screen get highlighted, a few milliseconds per frame.
//! * Per line we keep compact spans (8 bytes each, styles interned in a palette).
//! * syntect's parser state is large (kilobytes), so it's kept only every
//!   [`CHECKPOINT_EVERY`] lines. After an edit, highlighting restarts at the nearest
//!   checkpoint above it and stops as soon as it reaches a checkpoint below the edit
//!   whose state matches: everything after that would come out the same.

use std::sync::OnceLock;
use std::time::Instant;

use egui::Color32;
use ropey::Rope;
use syntect::highlighting::{FontStyle, HighlightIterator, HighlightState, Highlighter as SynHighlighter, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

use crate::buffer::LineChange;

const DARK_THEME: &str = "base16-mocha.dark";
const LIGHT_THEME: &str = "Solarized (light)";
/// Parser state is saved every this many lines.
const CHECKPOINT_EVERY: usize = 16;
/// Lines longer than this aren't parsed (minified files); they'd stall the UI.
const MAX_LINE_BYTES: usize = 20_000;

struct Assets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

static ASSETS: OnceLock<Assets> = OnceLock::new();

fn assets() -> &'static Assets {
    ASSETS.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        themes: ThemeSet::load_defaults(),
    })
}

/// Loads grammars and themes in the background so the first file opens instantly.
pub fn preload() {
    std::thread::spawn(assets);
}

#[derive(Clone, PartialEq)]
struct State {
    parse: ParseState,
    highlight: HighlightState,
}

#[derive(Clone, Copy, PartialEq)]
pub struct Style {
    pub color: Color32,
    pub italics: bool,
    pub underline: bool,
}

/// `len` bytes of a line drawn in palette style `style`.
#[derive(Clone, Copy)]
struct Span {
    len: u32,
    style: u16,
}

#[derive(Default)]
struct LineHl {
    spans: Box<[Span]>,
    checkpoint: Option<Box<State>>,
}

pub struct Highlighter {
    lang: String,
    dark: bool,
    syn: SynHighlighter<'static>,
    initial: State,
    palette: Vec<Style>,
    /// Text colour for lines that aren't highlighted yet.
    pub plain: Color32,
    lines: Vec<LineHl>,
    /// Lines before this are highlighted and valid.
    frontier: usize,
    /// Lines in `frontier..stale_until` hold highlighting from before an edit. It's
    /// still shown, and becomes valid again if highlighting converges onto it.
    stale_until: usize,
    /// Convergence is only allowed from this line on (it's after the edited lines).
    converge_from: usize,
    /// Parser state at the start of line `frontier`.
    state: State,
    since_checkpoint: usize,
    scratch: String,
}

impl Highlighter {
    /// `lang` is a file extension or syntax name.
    pub fn new(lang: &str, dark: bool, line_count: usize) -> Self {
        let a = assets();
        let syntax = a
            .syntaxes
            .find_syntax_by_extension(lang)
            .or_else(|| a.syntaxes.find_syntax_by_name(lang))
            .unwrap_or_else(|| a.syntaxes.find_syntax_plain_text());
        let theme = &a.themes.themes[if dark { DARK_THEME } else { LIGHT_THEME }];
        let plain = theme.settings.foreground.map_or(
            if dark { Color32::LIGHT_GRAY } else { Color32::DARK_GRAY },
            |c| Color32::from_rgb(c.r, c.g, c.b),
        );
        let syn = SynHighlighter::new(theme);
        let initial = State {
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&syn, ScopeStack::new()),
        };
        let mut lines = Vec::with_capacity(line_count);
        lines.resize_with(line_count, LineHl::default);
        Self {
            lang: lang.to_owned(),
            dark,
            syn,
            state: initial.clone(),
            initial,
            palette: Vec::new(),
            plain,
            lines,
            frontier: 0,
            stale_until: 0,
            converge_from: 0,
            since_checkpoint: 0,
            scratch: String::new(),
        }
    }

    pub fn matches(&self, lang: &str, dark: bool) -> bool {
        self.lang == lang && self.dark == dark
    }

    /// Updates bookkeeping after lines were replaced by an edit.
    pub fn splice(&mut self, c: LineChange) {
        let old_end = c.first + c.old_lines;
        let new_end = c.first + c.new_lines;
        let shift = |l: usize| {
            if l >= old_end {
                l + c.new_lines - c.old_lines
            } else {
                l.min(c.first)
            }
        };

        if self.frontier > c.first {
            // The edit touched valid lines. Remember the state at the frontier so a
            // converged pass can continue from there, unless stale lines follow it
            // (they were computed from an older state).
            if self.stale_until <= self.frontier && self.frontier < self.lines.len() {
                self.lines[self.frontier].checkpoint = Some(Box::new(self.state.clone()));
            }
            let valid_end = self.frontier.max(self.stale_until);
            self.stale_until = if valid_end >= old_end { shift(valid_end) } else { new_end };
            self.converge_from = if self.converge_from > c.first { shift(self.converge_from).max(new_end) } else { new_end };
            self.lines.splice(c.first..old_end, std::iter::repeat_with(LineHl::default).take(c.new_lines));
            // Restart from the nearest checkpoint above the edit.
            let restart = (0..c.first).rev().find(|&l| self.lines[l].checkpoint.is_some());
            match restart {
                Some(l) => {
                    self.state = (**self.lines[l].checkpoint.as_ref().expect("found above")).clone();
                    self.frontier = l;
                }
                None => {
                    self.state = self.initial.clone();
                    self.frontier = 0;
                }
            }
            self.since_checkpoint = 0;
        } else {
            // The edit is past everything valid; `state` still holds.
            if self.stale_until > c.first {
                // A gap inside stale lines would break convergence; drop them.
                self.stale_until = self.frontier;
            }
            self.lines.splice(c.first..old_end, std::iter::repeat_with(LineHl::default).take(c.new_lines));
        }
        self.stale_until = self.stale_until.clamp(self.frontier, self.lines.len());
    }

    /// Highlights lines until `target` (exclusive) is valid or `deadline` passes.
    /// Returns true when lines up to `target` are done.
    pub fn work(&mut self, rope: &Rope, target: usize, deadline: Instant) -> bool {
        let target = target.min(self.lines.len());
        let mut did_any = false;
        while self.frontier < target {
            let line = self.frontier;
            if line >= self.converge_from
                && line < self.stale_until
                && self.lines[line].checkpoint.as_deref() == Some(&self.state)
            {
                // Converged: stale lines from here on are valid again.
                self.frontier = self.stale_until;
                if let Some(cp) = self.lines.get(self.frontier).and_then(|l| l.checkpoint.as_deref()) {
                    self.state = cp.clone();
                }
                self.since_checkpoint = 0;
                continue;
            }
            if did_any && Instant::now() >= deadline {
                return false;
            }
            self.highlight_line(rope, line);
            did_any = true;
        }
        if self.stale_until < self.frontier {
            self.stale_until = self.frontier;
        }
        true
    }

    fn highlight_line(&mut self, rope: &Rope, line: usize) {
        let checkpoint = line == 0 || self.since_checkpoint >= CHECKPOINT_EVERY;
        self.lines[line].checkpoint = checkpoint.then(|| Box::new(self.state.clone()));
        self.since_checkpoint = if checkpoint { 1 } else { self.since_checkpoint + 1 };

        self.scratch.clear();
        for chunk in rope.line(line).chunks() {
            self.scratch.push_str(chunk);
        }
        let mut spans: Vec<Span> = Vec::new();
        if self.scratch.len() <= MAX_LINE_BYTES {
            let a = assets();
            let ops = self.state.parse.parse_line(&self.scratch, &a.syntaxes).unwrap_or_default();
            for (style, piece) in HighlightIterator::new(&mut self.state.highlight, &ops, &self.scratch, &self.syn) {
                let fg = style.foreground;
                let s = Style {
                    color: Color32::from_rgb(fg.r, fg.g, fg.b),
                    italics: style.font_style.contains(FontStyle::ITALIC),
                    underline: style.font_style.contains(FontStyle::UNDERLINE),
                };
                let idx = match self.palette.iter().position(|p| *p == s) {
                    Some(i) => i,
                    None => {
                        self.palette.push(s);
                        self.palette.len() - 1
                    }
                } as u16;
                match spans.last_mut() {
                    Some(last) if last.style == idx => last.len += piece.len() as u32,
                    _ => spans.push(Span { len: piece.len() as u32, style: idx }),
                }
            }
        }
        self.lines[line].spans = spans.into_boxed_slice();
        self.frontier = line + 1;
    }

    /// Styled byte ranges of a line, if it has (possibly slightly stale) highlighting.
    pub fn line_styles(&self, line: usize) -> Option<impl Iterator<Item = (std::ops::Range<usize>, Style)> + '_> {
        let l = self.lines.get(line)?;
        let usable = line < self.frontier.max(self.stale_until);
        if !usable || l.spans.is_empty() {
            return None;
        }
        let mut pos = 0;
        Some(l.spans.iter().map(move |s| {
            let r = pos..pos + s.len as usize;
            pos = r.end;
            (r, self.palette[s.style as usize])
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Buffer, Edit, EditKind, Selections};
    use std::time::Duration;

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    type Snapshot = Vec<Vec<(std::ops::Range<usize>, [u8; 4])>>;

    fn snapshot(h: &Highlighter, n: usize) -> Snapshot {
        (0..n)
            .map(|l| {
                h.line_styles(l)
                    .map(|it| it.map(|(r, s)| (r, s.color.to_array())).collect())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn fresh(rope: &Rope) -> Snapshot {
        let mut h = Highlighter::new("rs", true, rope.len_lines());
        assert!(h.work(rope, usize::MAX, far()));
        snapshot(&h, rope.len_lines())
    }

    /// Edits a buffer and keeps the highlighter in sync, like the editor does.
    fn edit(b: &mut Buffer, h: &mut Highlighter, edits: Vec<Edit>) {
        let s = Selections::default();
        if let Some(applied) = b.edit(edits, s.clone(), s, EditKind::Other) {
            for c in applied.lines {
                h.splice(c);
            }
        }
    }

    #[test]
    fn incremental_matches_fresh_after_edits() {
        let base = include_str!("app.rs");
        let mut b = Buffer::new(base);
        let mut h = Highlighter::new("rs", true, b.rope().len_lines());
        assert!(h.work(b.rope(), usize::MAX, far()));
        let mid = b.rope().len_chars() / 2;
        let edits: Vec<Vec<Edit>> = vec![
            vec![Edit::insert(mid, "x")],
            vec![Edit::insert(mid, "/*")], // opens a comment: everything after changes
            vec![Edit::new(mid..mid + 2, "")],
            vec![Edit::insert(10, "\n\n\"unterminated\n")],
            vec![Edit::insert(0, "// a\n"), Edit::insert(mid, "\n}\n")],
            vec![Edit::new(100..mid, "")], // big deletion
            vec![Edit::insert(mid / 3, "fn tail() {}\n")],
        ];
        for (i, e) in edits.into_iter().enumerate() {
            edit(&mut b, &mut h, e);
            assert!(h.work(b.rope(), usize::MAX, far()));
            assert_eq!(snapshot(&h, b.rope().len_lines()), fresh(b.rope()), "edit {i}");
        }
    }

    #[test]
    fn edits_while_partially_highlighted() {
        let base = include_str!("app.rs");
        let mut b = Buffer::new(base);
        let mut h = Highlighter::new("rs", true, b.rope().len_lines());
        let mut seed = 12345u64;
        let mut rand = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        for round in 0..80 {
            // Highlight a random amount, then make a random edit.
            let target = rand(b.rope().len_lines() + 1);
            h.work(b.rope(), target, Instant::now());
            let len = b.rope().len_chars();
            let at = rand(len);
            let end = (at + rand(40)).min(len);
            let text = ["", "x", "/*", "*/", "\n", "\"", "{\n", "}\n\n"][rand(8)];
            edit(&mut b, &mut h, vec![Edit::new(at..end, text)]);
            if round % 3 == 0 {
                h.work(b.rope(), rand(b.rope().len_lines() + 1), far());
            }
            if round % 10 == 0 {
                assert!(h.work(b.rope(), usize::MAX, far()));
                assert_eq!(snapshot(&h, b.rope().len_lines()), fresh(b.rope()), "round {round}");
            }
        }
        assert!(h.work(b.rope(), usize::MAX, far()));
        assert_eq!(snapshot(&h, b.rope().len_lines()), fresh(b.rope()));
    }

    #[test]
    fn small_edit_converges_quickly() {
        let base = include_str!("app.rs");
        let mut b = Buffer::new(base);
        let mut h = Highlighter::new("rs", true, b.rope().len_lines());
        assert!(h.work(b.rope(), usize::MAX, far()));
        let pos = b.rope().byte_to_char(base.find("fn new_file").unwrap());
        edit(&mut b, &mut h, vec![Edit::insert(pos + 3, "x")]);
        // One line per call: should take a few checkpoints' worth, not the whole file.
        let mut calls = 1;
        while !h.work(b.rope(), usize::MAX, Instant::now()) {
            calls += 1;
        }
        assert!(calls < 3 * CHECKPOINT_EVERY, "{calls} calls for {} lines", b.rope().len_lines());
    }

    #[test]
    #[ignore = "timing/memory, run with --release --ignored --nocapture --test-threads=1"]
    fn timing() {
        use crate::alloc_probe::live;
        let text: String = include_str!("app.rs").lines().cycle().take(20_000).collect::<Vec<_>>().join("\n");
        let t = Instant::now();
        assets();
        println!("load grammars: {:?}", t.elapsed());
        let rope = Rope::from_str(&text);
        let mut h = Highlighter::new("rs", true, rope.len_lines());
        let t = Instant::now();
        h.work(&rope, 60, far());
        println!("first screen (60 lines): {:?}", t.elapsed());
        let before = live();
        let t = Instant::now();
        h.work(&rope, usize::MAX, far());
        let el = t.elapsed();
        println!("all 20k lines: {el:?} ({:.3} ms/line)", el.as_secs_f64() * 1000.0 / 20_000.0);
        println!("highlighter heap for 20k lines: {:.1} MiB", (live() - before) as f64 / 1048576.0);
    }
}
