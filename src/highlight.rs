//! Incremental syntax highlighting.
//!
//! Re-highlighting a whole file on every keystroke is O(file size) and gets
//! noticeably slow past a few hundred lines. Instead we remember syntect's parser
//! state at the start of every line. After an edit, only lines from the first
//! changed one onwards are re-highlighted, and we stop as soon as we reach an
//! unchanged line whose starting state matches the one we have, since everything
//! after it would come out identical.

use std::ops::Range;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat};
use egui::{Color32, FontId, Stroke};
use syntect::highlighting::{FontStyle, HighlightIterator, HighlightState, Highlighter, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

const DARK_THEME: &str = "base16-mocha.dark";
const LIGHT_THEME: &str = "Solarized (light)";

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

/// Loads the grammars and themes in the background so opening the first file
/// doesn't pay for it. Anyone who needs them earlier simply waits for this.
pub fn preload() {
    std::thread::spawn(assets);
}

#[derive(Clone, PartialEq)]
struct State {
    parse: ParseState,
    highlight: HighlightState,
}

struct Span {
    range: Range<usize>,
    color: Color32,
    italics: bool,
    underline: bool,
}

struct Line {
    /// Length in bytes, including the newline.
    len: usize,
    /// Parser state before this line.
    start: State,
    spans: Vec<Span>,
}

/// Highlighting state of one document. Keep one per buffer.
#[derive(Default)]
pub struct SyntaxCache {
    inner: Option<Inner>,
}

struct Inner {
    lang: String,
    dark: bool,
    highlighter: Highlighter<'static>,
    /// Colour for text that hasn't been highlighted yet.
    plain: Color32,
    /// The text everything below refers to.
    text: String,
    /// Highlighted lines: a prefix of `text`'s lines, which grows a little every frame.
    lines: Vec<Line>,
    /// Parser state after the last entry of `lines`.
    end: State,
    /// Old lines kept around after an edit, in case highlighting converges back onto them.
    reuse: Option<Reuse>,
    job: LayoutJob,
}

/// Lines that followed the edited region, before the edit.
struct Reuse {
    /// Highlighted lines that came after the unchanged prefix.
    rest: Vec<Line>,
    /// Old line index of `rest[0]`.
    rest_start: usize,
    /// First line of the unchanged suffix, in old and new line numbers.
    old_suffix_start: usize,
    new_suffix_start: usize,
    /// Parser state after `rest`'s last line.
    end: State,
}

impl SyntaxCache {
    /// Returns a layout job for `text`, reusing as much previous work as possible.
    /// `lang` is a file extension or syntax name.
    ///
    /// Highlighting work is capped at `budget` per call; the second value is false
    /// while some of the text is still unhighlighted, and the caller should then
    /// call again on a later frame.
    pub fn layout_job(
        &mut self,
        text: &str,
        lang: &str,
        dark: bool,
        font_id: &FontId,
        budget: Duration,
    ) -> (LayoutJob, bool) {
        let deadline = Instant::now() + budget;
        let stale = self.inner.as_ref().is_none_or(|i| i.lang != lang || i.dark != dark);
        if stale {
            self.inner = Some(Inner::new(lang, dark));
        }
        let inner = self.inner.as_mut().expect("initialised above");

        let changed = inner.text != text;
        if changed {
            inner.set_text(text);
        }
        let total = LinesWithEndings::from(text).count();
        let was_done = inner.lines.len() >= total;
        if !was_done {
            inner.highlight_more(deadline);
        }
        let font_changed = inner.job.sections.first().is_none_or(|s| &s.format.font_id != font_id);
        if changed || stale || !was_done || font_changed {
            inner.job = inner.build_job(font_id);
        }
        (inner.job.clone(), inner.lines.len() >= total)
    }
}

impl Inner {
    fn new(lang: &str, dark: bool) -> Self {
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
        let highlighter = Highlighter::new(theme);
        let end = State {
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&highlighter, ScopeStack::new()),
        };
        Self {
            lang: lang.to_owned(),
            dark,
            highlighter,
            plain,
            text: String::new(),
            lines: Vec::new(),
            end,
            reuse: None,
            job: LayoutJob::default(),
        }
    }

    /// Switches to a new version of the text, keeping highlighted lines that are still valid.
    fn set_text(&mut self, text: &str) {
        let old_text = std::mem::replace(&mut self.text, text.to_owned());
        let old: Vec<&str> = LinesWithEndings::from(&old_text).collect();
        let new: Vec<&str> = LinesWithEndings::from(text).collect();

        // Lines identical at the start and end don't need re-highlighting, as long
        // as the state entering them turns out to be unchanged.
        let prefix = old.iter().zip(&new).take_while(|(o, n)| o == n).count();
        let max_suffix = old.len().min(new.len()) - prefix;
        let suffix = old.iter().rev().zip(new.iter().rev()).take(max_suffix).take_while(|(o, n)| o == n).count();

        // Only the highlighted part of the unchanged prefix can be kept as is.
        let keep = prefix.min(self.lines.len());
        let rest = self.lines.split_off(keep);
        let resume = match rest.first() {
            Some(l) => l.start.clone(),
            None => self.end.clone(),
        };
        let old_end = std::mem::replace(&mut self.end, resume);
        self.reuse = (!rest.is_empty()).then(|| Reuse {
            rest,
            rest_start: keep,
            old_suffix_start: old.len() - suffix,
            new_suffix_start: new.len() - suffix,
            end: old_end,
        });
    }

    /// Highlights further lines until the text is done or the deadline passes.
    fn highlight_more(&mut self, deadline: Instant) {
        let text = std::mem::take(&mut self.text);
        let mut lines = LinesWithEndings::from(&text).skip(self.lines.len());
        let mut j = self.lines.len();
        let mut did_any = false;
        let mut finished = true;
        while let Some(line) = lines.next() {
            if let Some(r) = &mut self.reuse
                && j >= r.new_suffix_start
            {
                let oi = r.old_suffix_start + (j - r.new_suffix_start);
                let idx = oi.checked_sub(r.rest_start).filter(|&i| i < r.rest.len());
                if let Some(idx) = idx
                    && r.rest[idx].start == self.end
                {
                    // Converged: the old lines from here on come out exactly the same.
                    let tail: Vec<Line> = r.rest.drain(idx..).collect();
                    let n = tail.len();
                    self.lines.extend(tail);
                    self.end = self.reuse.take().expect("checked above").end;
                    for _ in 1..n {
                        lines.next();
                    }
                    j += n;
                    continue;
                }
            }
            if did_any && Instant::now() >= deadline {
                finished = false;
                break;
            }
            let mut next = self.end.clone();
            let spans = self.highlight_line(line, &mut next);
            let start = std::mem::replace(&mut self.end, next);
            self.lines.push(Line { len: line.len(), start, spans });
            did_any = true;
            j += 1;
        }
        if finished {
            self.reuse = None;
        }
        self.text = text;
    }

    fn highlight_line(&self, line: &str, state: &mut State) -> Vec<Span> {
        let ops = state.parse.parse_line(line, &assets().syntaxes).unwrap_or_default();
        let mut pos = 0;
        HighlightIterator::new(&mut state.highlight, &ops, line, &self.highlighter)
            .map(|(style, piece)| {
                let range = pos..pos + piece.len();
                pos = range.end;
                let fg = style.foreground;
                Span {
                    range,
                    color: Color32::from_rgb(fg.r, fg.g, fg.b),
                    italics: style.font_style.contains(FontStyle::ITALIC),
                    underline: style.font_style.contains(FontStyle::UNDERLINE),
                }
            })
            .collect()
    }

    fn build_job(&self, font_id: &FontId) -> LayoutJob {
        let mut job = LayoutJob { text: self.text.clone(), ..Default::default() };
        let mut offset = 0;
        for line in &self.lines {
            for span in &line.spans {
                job.sections.push(LayoutSection {
                    leading_space: 0.0,
                    byte_range: ByteIndex(offset + span.range.start)..ByteIndex(offset + span.range.end),
                    format: TextFormat {
                        font_id: font_id.clone(),
                        color: span.color,
                        italics: span.italics,
                        underline: if span.underline { Stroke::new(1.0, span.color) } else { Stroke::NONE },
                        ..Default::default()
                    },
                });
            }
            offset += line.len;
        }
        if offset < self.text.len() {
            // Not highlighted yet.
            job.sections.push(LayoutSection {
                leading_space: 0.0,
                byte_range: ByteIndex(offset)..ByteIndex(self.text.len()),
                format: TextFormat { font_id: font_id.clone(), color: self.plain, ..Default::default() },
            });
        }
        if job.sections.is_empty() {
            // An empty buffer still needs the font, or the cursor row gets the wrong height.
            job.sections.push(LayoutSection {
                leading_space: 0.0,
                byte_range: ByteIndex(0)..ByteIndex(0),
                format: TextFormat { font_id: font_id.clone(), ..Default::default() },
            });
        }
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(text: &str) -> LayoutJob {
        let (job, done) = SyntaxCache::default().layout_job(text, "rs", true, &FontId::monospace(14.0), Duration::MAX / 4);
        assert!(done);
        job
    }

    /// Calls `layout_job` with a tiny budget until highlighting is complete.
    fn finish(cache: &mut SyntaxCache, text: &str) -> (LayoutJob, usize) {
        let font = FontId::monospace(14.0);
        for calls in 1.. {
            let (job, done) = cache.layout_job(text, "rs", true, &font, Duration::ZERO);
            if done {
                return (job, calls);
            }
        }
        unreachable!()
    }

    #[test]
    fn incremental_matches_full_highlight() {
        let base = include_str!("app.rs");
        let mid = base.len() / 2;
        let mid = (mid..).find(|&i| base.is_char_boundary(i)).unwrap();
        let edits: Vec<String> = vec![
            base.to_owned(),
            format!("{}x{}", &base[..mid], &base[mid..]),           // typing mid-file
            format!("{}/*{}", &base[..mid], &base[mid..]),          // opens a comment: all later lines change
            base.to_owned(),                                        // ...and closing it again
            format!("{}\n\n\"unterminated\n{}", &base[..mid], &base[mid..]),
            base[..mid].to_owned(),                                 // big deletion
            format!("{base}fn tail() {{}}\n"),                      // append
            String::new(),
            base.to_owned(),
        ];
        let mut cache = SyntaxCache::default();
        for (i, text) in edits.iter().enumerate() {
            let (incremental, _) = finish(&mut cache, text);
            assert_eq!(incremental.text, *text, "edit {i}");
            assert_eq!(incremental.sections, fresh(text).sections, "edit {i}");
        }

        // Editing while highlighting is only partly done must also come out right.
        let font = FontId::monospace(14.0);
        let mut cache = SyntaxCache::default();
        for (i, text) in edits.iter().enumerate() {
            for _ in 0..50 {
                let (job, _) = cache.layout_job(text, "rs", true, &font, Duration::ZERO);
                assert_eq!(job.text, *text);
            }
            let (incremental, _) = finish(&mut cache, text);
            assert_eq!(incremental.sections, fresh(text).sections, "partial edit {i}");
        }
    }

    #[test]
    fn small_edit_rehighlights_few_lines() {
        let base = include_str!("app.rs");
        let mut cache = SyntaxCache::default();
        finish(&mut cache, base);
        let pos = base.find("fn new_file").unwrap();
        let edited = format!("{}x{}", &base[..pos], &base[pos..]);
        // One line per call with a zero budget: the parser state differs for a few lines
        // (`xfn` is no longer a keyword) and must then converge, not redo the whole file.
        let (_, calls) = finish(&mut cache, &edited);
        let total = base.lines().count();
        assert!(calls < 20, "took {calls} calls for a {total}-line file");
    }

    #[test]
    #[ignore = "timing, run with --ignored --nocapture"]
    fn timing() {
        let font = FontId::monospace(14.0);
        let text: String = include_str!("app.rs").lines().cycle().take(800).collect::<Vec<_>>().join("\n");
        let t = std::time::Instant::now();
        assets();
        println!("load grammars: {:?}", t.elapsed());
        let mut cache = SyntaxCache::default();
        let t = std::time::Instant::now();
        cache.layout_job(&text, "rs", true, &font, Duration::from_millis(8));
        println!("first frame of 800-line file (8ms budget): {:?}", t.elapsed());
        finish(&mut cache, &text);
        let t = std::time::Instant::now();
        SyntaxCache::default().layout_job(&text, "rs", true, &font, Duration::MAX / 4);
        println!("full highlight of another 800-line file, unbudgeted: {:?}", t.elapsed());
        let mid = text.len() / 2;
        let mid = (mid..).find(|&i| text.is_char_boundary(i)).unwrap();
        let edited = format!("{}x{}", &text[..mid], &text[mid..]);
        let t = std::time::Instant::now();
        cache.layout_job(&edited, "rs", true, &font, Duration::from_millis(8));
        println!("keystroke: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        cache.layout_job(&edited, "rs", true, &font, Duration::from_millis(8));
        println!("unchanged frame: {:?}", t.elapsed());
    }
}
