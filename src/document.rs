use std::collections::HashMap;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use ropey::Rope;

use crate::buffer::{
    Applied, Buffer, ChangeLog, Edit, EditKind, LspChange, Selection, Selections, line_len,
    map_pos, sort_edits,
};
use crate::highlight::Highlighter;
use crate::ops::{self, Change, Ctx, Indent};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
        }
    }
}

/// Per-group state of a document: each editor group has its own cursors.
#[derive(Default)]
pub struct View {
    pub sel: Selections,
    /// Scroll the primary cursor into view on the next frame (`Some(true)` = centre it).
    pub reveal: Option<bool>,
}

/// A problem shown in the editor (converted from a language server diagnostic).
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub range: Range<usize>,
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
}

/// What to search for (find widget and find-in-files).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Query {
    pub text: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

impl Query {
    pub fn build(&self) -> Option<Result<regex::Regex, regex::Error>> {
        if self.text.is_empty() {
            return None;
        }
        let mut pat = if self.regex {
            self.text.clone()
        } else {
            regex::escape(&self.text)
        };
        if self.whole_word {
            pat = format!(r"\b(?:{pat})\b");
        }
        Some(
            regex::RegexBuilder::new(&pat)
                .case_insensitive(!self.case_sensitive)
                .multi_line(true)
                .build(),
        )
    }
}

struct SearchCache {
    rev: u64,
    query: Query,
    matches: Vec<Range<usize>>,
}

/// The most matches a find will collect (keeps memory bounded on huge files).
pub const MAX_MATCHES: usize = 100_000;

pub enum DiskStatus {
    Unchanged,
    Reloaded,
    /// Changed on disk while there are unsaved edits here.
    Conflict,
}

/// One open file (or untitled buffer), shared by every group that shows it.
pub struct Document {
    pub id: u64,
    pub path: Option<PathBuf>,
    buffer: Buffer,
    saved_rev: u64,
    pub line_ending: LineEnding,
    saved_line_ending: LineEnding,
    pub indent: Indent,
    untitled_index: usize,
    views: HashMap<u64, View>,
    highlighter: Option<Highlighter>,
    /// Modification time when last loaded/saved, to notice outside changes.
    mtime: Option<SystemTime>,
    /// Longest line seen (in chars); only grows, used for the horizontal scroll range.
    max_line_chars: usize,
    search: Option<SearchCache>,
    /// Git gutter marks (line ranges vs. the staged version), and the revision they're for.
    pub git_marks: Vec<(Range<usize>, crate::git::Mark)>,
    pub git_marks_rev: Option<u64>,
    /// When the text last changed without the marks being updated (to wait for a pause).
    pub git_marks_stale_since: Option<(u64, Instant)>,
    /// Generated documents (e.g. a diff view) have a name and language instead of a file.
    virtual_title: Option<String>,
    syntax_override: Option<String>,
    /// Language server problems, sorted by position; they follow edits until the
    /// server sends fresh ones.
    pub problems: Vec<Problem>,
    /// Which batch of server diagnostics `problems` came from.
    pub problems_gen: u64,
}

impl Document {
    fn with_text(
        path: Option<PathBuf>,
        text: &str,
        line_ending: LineEnding,
        untitled_index: usize,
    ) -> Self {
        let buffer = Buffer::new(text);
        let rope = buffer.rope();
        let max_line_chars = longest_line(text);
        let indent = Indent::detect(rope);
        let mtime = path.as_deref().and_then(mtime_of);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            path,
            saved_rev: buffer.rev(),
            buffer,
            line_ending,
            saved_line_ending: line_ending,
            indent,
            untitled_index,
            views: HashMap::new(),
            highlighter: None,
            mtime,
            max_line_chars,
            search: None,
            git_marks: Vec::new(),
            git_marks_rev: None,
            git_marks_stale_since: None,
            virtual_title: None,
            syntax_override: None,
            problems: Vec::new(),
            problems_gen: 0,
        }
    }

    /// A document brought back from a crash: its recovered text, unsaved.
    pub fn recovered(
        path: Option<PathBuf>,
        text: &str,
        line_ending: LineEnding,
        untitled_index: usize,
    ) -> Self {
        let mut doc = Self::with_text(path, text, line_ending, untitled_index);
        doc.saved_rev = u64::MAX; // never equal to a real revision: unsaved
        doc
    }

    /// Generated documents (e.g. a diff view) have no file of their own.
    pub fn is_generated(&self) -> bool {
        self.virtual_title.is_some()
    }

    pub fn new_untitled(untitled_index: usize) -> Self {
        Self::with_text(None, "", LineEnding::Lf, untitled_index)
    }

    pub fn open(path: &Path) -> std::io::Result<Self> {
        let (text, ending) = read_text(path)?;
        Ok(Self::with_text(Some(path.to_path_buf()), &text, ending, 0))
    }

    pub fn rope(&self) -> &Rope {
        self.buffer.rope()
    }

    pub fn rev(&self) -> u64 {
        self.buffer.rev()
    }

    pub fn max_line_chars(&self) -> usize {
        self.max_line_chars
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.rev() != self.saved_rev || self.line_ending != self.saved_line_ending
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| std::io::Error::other("no path"))?;
        let rope = self.buffer.rope();
        let line_ending = self.line_ending;
        write_file(&path, |w| {
            for chunk in rope.chunks() {
                match line_ending {
                    LineEnding::Lf => w.write_all(chunk.as_bytes())?,
                    LineEnding::CrLf => {
                        for (i, part) in chunk.split('\n').enumerate() {
                            if i > 0 {
                                w.write_all(b"\r\n")?;
                            }
                            w.write_all(part.as_bytes())?;
                        }
                    }
                }
            }
            Ok(())
        })?;
        self.saved_rev = self.buffer.rev();
        self.saved_line_ending = self.line_ending;
        self.mtime = mtime_of(&path);
        Ok(())
    }

    pub fn save_as(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.path = Some(path);
        self.highlighter = None; // the language may have changed
        self.save()
    }

    /// Reloads the file if it changed on disk and has no unsaved edits here.
    pub fn check_disk(&mut self) -> DiskStatus {
        let Some(path) = self.path.clone() else {
            return DiskStatus::Unchanged;
        };
        let now = mtime_of(&path);
        if now.is_none() || now == self.mtime {
            return DiskStatus::Unchanged;
        }
        if self.is_dirty() {
            self.mtime = now; // warn once
            return DiskStatus::Conflict;
        }
        match read_text(&path) {
            Ok((text, ending)) => {
                self.max_line_chars = longest_line(&text);
                self.buffer.reset(&text);
                self.saved_rev = self.buffer.rev();
                self.line_ending = ending;
                self.saved_line_ending = ending;
                self.mtime = now;
                self.highlighter = None;
                self.search = None;
                let len = self.rope().len_chars();
                for v in self.views.values_mut() {
                    v.sel = v.sel.clamp(len);
                }
                DiskStatus::Reloaded
            }
            Err(_) => DiskStatus::Unchanged,
        }
    }

    /// A generated document that isn't backed by a file (e.g. a diff).
    pub fn virtual_doc(title: &str, text: &str, syntax: &str) -> Self {
        let mut d = Self::with_text(None, text, LineEnding::Lf, 0);
        d.virtual_title = Some(title.to_owned());
        d.syntax_override = Some(syntax.to_owned());
        d
    }

    pub fn virtual_title(&self) -> Option<&str> {
        self.virtual_title.as_deref()
    }

    /// Replaces the whole text (used to refresh generated documents).
    pub fn reset_text(&mut self, text: &str) {
        self.buffer.reset(text);
        self.saved_rev = self.buffer.rev();
        self.highlighter = None;
        let len = self.rope().len_chars();
        for v in self.views.values_mut() {
            v.sel = v.sel.clamp(len);
        }
    }

    pub fn title(&self) -> String {
        if let Some(t) = &self.virtual_title {
            return t.clone();
        }
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            None => format!("Untitled-{}", self.untitled_index),
        }
    }

    /// Extension used by syntect to pick a grammar.
    pub fn syntax_token(&self) -> String {
        if let Some(s) = &self.syntax_override {
            return s.clone();
        }
        self.path
            .as_ref()
            .and_then(|p| {
                p.extension()
                    .map(|e| e.to_string_lossy().into_owned())
                    .or_else(|| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            })
            .unwrap_or_else(|| "txt".to_owned())
    }

    pub fn language_name(&self) -> &'static str {
        match self.syntax_token().as_str() {
            "rs" => "Rust",
            "py" => "Python",
            "js" | "mjs" | "cjs" => "JavaScript",
            "ts" | "tsx" => "TypeScript",
            "json" => "JSON",
            "toml" => "TOML",
            "yaml" | "yml" => "YAML",
            "md" => "Markdown",
            "html" | "htm" => "HTML",
            "css" => "CSS",
            "c" | "h" => "C",
            "cpp" | "cc" | "hpp" => "C++",
            "go" => "Go",
            "java" => "Java",
            "sh" | "bash" | "zsh" => "Shell",
            "lua" => "Lua",
            "rb" => "Ruby",
            "xml" => "XML",
            "sql" => "SQL",
            _ => "Plain Text",
        }
    }

    fn comment_token(&self) -> Option<&'static str> {
        Some(match self.syntax_token().as_str() {
            "rs" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "c" | "h" | "cpp" | "cc" | "hpp"
            | "go" | "java" | "css" | "json" | "kt" | "swift" | "cs" | "scala" | "dart" | "zig" => {
                "//"
            }
            "py" | "sh" | "bash" | "zsh" | "toml" | "yaml" | "yml" | "rb" | "pl" | "r" | "conf"
            | "ini" | "nix" | "Makefile" | "Dockerfile" | "gitignore" => "#",
            "lua" | "sql" | "hs" => "--",
            "vim" => "\"",
            "el" | "lisp" | "clj" | "asm" => ";",
            _ => return None,
        })
    }

    /// Settings for editing operations.
    pub fn ctx(&self) -> Ctx<'static> {
        Ctx {
            indent: self.indent,
            comment: self.comment_token(),
            close_single_quote: self.syntax_token() != "rs",
        }
    }

    // ------------------------------------------------------------- views

    pub fn view(&mut self, view: u64) -> &mut View {
        self.views.entry(view).or_default()
    }

    pub fn selections(&self, view: u64) -> Selections {
        self.views
            .get(&view)
            .map(|v| v.sel.clone())
            .unwrap_or_default()
    }

    pub fn drop_view(&mut self, view: u64) {
        self.views.remove(&view);
    }

    /// Copies cursors from one view to another (when splitting a tab).
    pub fn clone_view(&mut self, from: u64, to: u64) {
        let sel = self.selections(from);
        self.views.insert(
            to,
            View {
                sel,
                reveal: Some(true),
            },
        );
    }

    pub fn set_selections(&mut self, view: u64, sel: Selections, reveal: bool) {
        let v = self.view(view);
        v.sel = sel;
        if reveal {
            v.reveal.get_or_insert(false);
        }
    }

    /// Selects a char range and scrolls it to the middle.
    pub fn goto(&mut self, view: u64, range: Range<usize>) {
        let len = self.rope().len_chars();
        let v = self.view(view);
        v.sel = Selections::single(Selection::new(range.start.min(len), range.end.min(len)));
        v.reveal = Some(true);
    }

    // ------------------------------------------------------------- editing

    /// Applies an editing operation made from this view's selections.
    pub fn commit(&mut self, view: u64, change: Change) {
        let before = self.selections(view);
        let edits = sort_edits(change.edits);
        if edits.is_empty() {
            self.set_selections(view, change.after, true);
            return;
        }
        if let Some(applied) = self.buffer.edit(edits, before, change.after, change.kind) {
            self.after_edit(view, applied);
        }
    }

    /// Runs an operation on this view's selections and commits it.
    pub fn run(&mut self, view: u64, op: impl FnOnce(&Rope, &Selections, Ctx) -> Option<Change>) {
        let sel = self.selections(view);
        if let Some(change) = op(self.rope(), &sel, self.ctx()) {
            self.commit(view, change);
        }
    }

    /// Runs an operation that always produces a change.
    pub fn apply(&mut self, view: u64, op: impl FnOnce(&Rope, &Selections, Ctx) -> Change) {
        self.run(view, |r, s, c| Some(op(r, s, c)));
    }

    /// Runs a cursor movement on this view's selections.
    pub fn motion(&mut self, view: u64, op: impl FnOnce(&Rope, &Selections) -> Selections) {
        let sel = self.selections(view);
        let new = op(self.rope(), &sel);
        self.set_selections(view, new, true);
    }

    pub fn undo(&mut self, view: u64) {
        if let Some(applied) = self.buffer.undo() {
            self.after_edit(view, applied);
        }
    }

    pub fn redo(&mut self, view: u64) {
        if let Some(applied) = self.buffer.redo() {
            self.after_edit(view, applied);
        }
    }

    fn after_edit(&mut self, view: u64, applied: Applied) {
        for c in &applied.lines {
            if let Some(h) = &mut self.highlighter {
                h.splice(*c);
            }
            let rope = self.buffer.rope();
            for l in c.first..(c.first + c.new_lines).min(rope.len_lines()) {
                self.max_line_chars = self.max_line_chars.max(line_len(rope, l));
            }
        }
        // Problems stay on the text they were reported for.
        for step in &applied.edits {
            for p in &mut self.problems {
                let start = map_pos(step, p.range.start);
                let end = map_pos(step, p.range.end).max(start);
                p.range = start..end;
            }
        }
        // Other groups showing this document keep their cursors on the same text.
        for (id, v) in self.views.iter_mut() {
            if *id != view {
                let mut sel = v.sel.clone();
                for step in &applied.edits {
                    sel =
                        sel.map(|s| Selection::new(map_pos(step, s.anchor), map_pos(step, s.head)));
                }
                v.sel = sel;
            }
        }
        let v = self.view(view);
        v.sel = applied.selections;
        v.reveal.get_or_insert(false);
    }

    // ------------------------------------------------------------- highlighting

    /// Highlights up to line `target` within the deadline; true when done.
    pub fn highlight(&mut self, dark: bool, target: usize, deadline: Instant) -> bool {
        let lang = self.syntax_token();
        let lines = self.rope().len_lines();
        if !self
            .highlighter
            .as_ref()
            .is_some_and(|h| h.matches(&lang, dark))
        {
            self.highlighter = Some(Highlighter::new(&lang, dark, lines));
        }
        let h = self.highlighter.as_mut().expect("set above");
        h.work(self.buffer.rope(), target, deadline)
    }

    pub fn highlighter(&self) -> Option<&Highlighter> {
        self.highlighter.as_ref()
    }

    // ------------------------------------------------------------- search

    /// All matches of a query (char ranges), cached until the text or query changes.
    pub fn find(&mut self, query: &Query) -> Result<&[Range<usize>], String> {
        let fresh = self
            .search
            .as_ref()
            .is_some_and(|s| s.rev == self.rev() && s.query == *query);
        if !fresh {
            let matches = match query.build() {
                None => Vec::new(),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(re)) => {
                    let rope = self.buffer.rope();
                    let text = rope.to_string();
                    re.find_iter(&text)
                        .filter(|m| !m.is_empty())
                        .take(MAX_MATCHES)
                        .map(|m| rope.byte_to_char(m.start())..rope.byte_to_char(m.end()))
                        .collect()
                }
            };
            self.search = Some(SearchCache {
                rev: self.rev(),
                query: query.clone(),
                matches,
            });
        }
        Ok(&self.search.as_ref().expect("set above").matches)
    }

    /// Replaces one match (char range) with `with`, expanding `$1` etc. for regex queries.
    pub fn replace(&mut self, view: u64, query: &Query, range: Range<usize>, with: &str) {
        let text = self.expand(query, range.clone(), with);
        let after = Selections::single(Selection::caret(range.start + text.chars().count()));
        self.commit(
            view,
            Change {
                edits: vec![Edit::new(range, text)],
                after,
                kind: EditKind::Other,
            },
        );
    }

    /// Applies a language server's edits (server positions) as one undoable change;
    /// returns how many were made. Carets follow the text.
    pub fn apply_lsp_edits(&mut self, edits: &[crate::lsp::TextEdit], utf16: bool) -> usize {
        let edits = self.lsp_edits(edits, utf16);
        let view = self.views.keys().next().copied().unwrap_or(0);
        self.apply_edits(view, edits)
    }

    /// Server edits (server positions) as char-range edits of the current text.
    pub fn lsp_edits(&self, edits: &[crate::lsp::TextEdit], utf16: bool) -> Vec<Edit> {
        edits
            .iter()
            .map(|(s, e, text)| {
                let a = crate::completion::char_pos(self.rope(), *s, utf16);
                let b = crate::completion::char_pos(self.rope(), *e, utf16).max(a);
                Edit::new(a..b, text.clone())
            })
            .collect()
    }

    /// Applies edits as one undoable change, moving `view`'s carets along; returns
    /// how many were made.
    pub fn apply_edits(&mut self, view: u64, edits: Vec<Edit>) -> usize {
        let edits = sort_edits(edits);
        let n = edits.len();
        let after = self
            .selections(view)
            .map(|s| Selection::new(map_pos(&edits, s.anchor), map_pos(&edits, s.head)));
        self.commit(
            view,
            Change {
                edits,
                after,
                kind: EditKind::Other,
            },
        );
        n
    }

    /// Replaces every match; returns how many.
    pub fn replace_all(&mut self, view: u64, query: &Query, with: &str) -> usize {
        let Ok(matches) = self.find(query) else {
            return 0;
        };
        let matches = matches.to_vec();
        let edits: Vec<Edit> = matches
            .iter()
            .map(|r| Edit::new(r.clone(), self.expand(query, r.clone(), with)))
            .collect();
        let n = edits.len();
        let caret = self.selections(view).primary().head;
        let after = Selections::single(Selection::caret(map_pos(&edits, caret)));
        self.commit(
            view,
            Change {
                edits,
                after,
                kind: EditKind::Other,
            },
        );
        n
    }

    fn expand(&self, query: &Query, range: Range<usize>, with: &str) -> String {
        if !query.regex {
            return with.to_owned();
        }
        let found = self.rope().slice(range).to_string();
        match query.build() {
            Some(Ok(re)) => re.replace(&found, with).into_owned(),
            _ => with.to_owned(),
        }
    }

    // ------------------------------------------------------------- language servers

    /// Starts (or stops) recording edits for a language server.
    pub fn set_change_log(&mut self, on: bool) {
        self.buffer.log = if on {
            ChangeLog::Changes(Vec::new())
        } else {
            ChangeLog::Off
        };
    }

    /// Edits since the last call: `Some(changes)`, or `None` when the server
    /// needs the whole text again (after a reload or very many edits).
    pub fn take_changes(&mut self) -> Option<Vec<LspChange>> {
        match std::mem::replace(&mut self.buffer.log, ChangeLog::Changes(Vec::new())) {
            ChangeLog::Changes(c) => Some(c),
            ChangeLog::Resync => None,
            ChangeLog::Off => {
                self.buffer.log = ChangeLog::Off;
                Some(Vec::new())
            }
        }
    }

    pub fn toggle_comment(&mut self, view: u64) {
        self.run(view, ops::toggle_comment);
    }
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Reads a UTF-8 file, converting CRLF to LF and reporting which ending it used.
/// Chars in the longest line of `text` (lines split at `\n`).
fn longest_line(text: &str) -> usize {
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).chars().count())
        .max()
        .unwrap_or(0)
}

fn read_text(path: &Path) -> std::io::Result<(String, LineEnding)> {
    // Only regular files: reading a named pipe or a device would block (or
    // never end) on the UI thread.
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "file is not valid UTF-8")
    })?;
    // One pass each, and the CRLF count only when there is a \r at all.
    let lf = text.bytes().filter(|&b| b == b'\n').count();
    let crlf = if text.as_bytes().contains(&b'\r') {
        text.matches("\r\n").count()
    } else {
        0
    };
    if crlf > 0 && crlf * 2 >= lf {
        Ok((text.replace("\r\n", "\n"), LineEnding::CrLf))
    } else {
        Ok((text, LineEnding::Lf))
    }
}

/// Writes a file so that a crash, a full disk or a failing write never leaves
/// it half-written: the text goes to a new file next to it, which then replaces
/// it in one step (the old text stays until the new one is complete).
///
/// It writes in place instead where replacing would change what the file is:
/// a file with other hard links (they would keep the old text), one owned by
/// someone else (it would become ours), anything that isn't a regular file,
/// and folders we can't create files in. Saving through a symlink writes the
/// file it points to and keeps the link.
fn write_file(
    path: &Path,
    write: impl Fn(&mut dyn Write) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::BufWriter;
    // In place, the whole text is produced first, so only the final write
    // itself (not producing the text) can fail with the file truncated.
    let in_place = |target: &Path| -> std::io::Result<()> {
        let mut bytes = Vec::new();
        write(&mut bytes)?;
        std::fs::write(target, bytes)
    };
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let old = std::fs::metadata(&target).ok();
    #[cfg(unix)]
    let shared = {
        use std::os::unix::fs::MetadataExt;
        old.as_ref().is_some_and(|m| m.nlink() > 1)
    };
    #[cfg(not(unix))]
    let shared = false;
    if shared || old.as_ref().is_some_and(|m| !m.is_file()) {
        return in_place(&target);
    }
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return in_place(&target);
    };
    let tmp = dir.join(format!(
        ".{}.rsedit-save-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    // `create_new` never follows or reuses an existing file or symlink there.
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        Ok(f) => f,
        Err(_) => return in_place(&target),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ours = file.metadata().map(|m| m.uid()).ok();
        if let (Some(old), Some(ours)) = (&old, ours)
            && old.uid() != ours
        {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            return in_place(&target);
        }
    }
    let result = (|| {
        let mut w = BufWriter::new(file);
        write(&mut w)?;
        let file = w.into_inner().map_err(|e| e.into_error())?;
        if let Some(old) = &old {
            file.set_permissions(old.permissions())?;
        }
        file.sync_all()?;
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_roundtrip_and_dirty_tracking() {
        let dir = std::env::temp_dir().join(format!("rsedit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("crlf.txt");
        std::fs::write(&p, "a\r\nb\r\n").unwrap();
        let mut d = Document::open(&p).unwrap();
        assert_eq!(d.line_ending, LineEnding::CrLf);
        assert_eq!(d.rope().to_string(), "a\nb\n");
        assert!(!d.is_dirty());
        d.apply(1, |r, s, c| ops::type_text(r, s, "x", c));
        assert!(d.is_dirty());
        d.undo(1);
        assert!(!d.is_dirty(), "undoing back to the saved text is clean");
        d.apply(1, |r, s, c| ops::type_text(r, s, "x", c));
        d.save().unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "xa\r\nb\r\n");
        assert!(!d.is_dirty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn longest_line_counts_chars_without_line_breaks() {
        assert_eq!(longest_line(""), 0);
        assert_eq!(longest_line("ab\nabcd\r\nä€\n"), 4);
        assert_eq!(longest_line("x\nüüüüü"), 5);
    }

    /// Opening a named pipe must fail right away, not wait for a writer forever.
    #[cfg(unix)]
    #[test]
    fn only_regular_files_open() {
        let dir = std::env::temp_dir().join(format!("rsedit-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            let err = Document::open(&fifo).err().expect("a pipe must not open");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert!(Document::open(&dir).is_err(), "nor a folder");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn saving_replaces_the_file_but_keeps_links_and_permissions() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("rsedit-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write = |text: &'static str| move |w: &mut dyn Write| w.write_all(text.as_bytes());

        // A regular file is replaced (new inode), keeping its permissions.
        let file = dir.join("a.sh");
        std::fs::write(&file, "old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
        let inode = std::fs::metadata(&file).unwrap().ino();
        write_file(&file, write("new")).unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert_ne!(meta.ino(), inode, "written to a new file, then renamed");
        assert_eq!(meta.permissions().mode() & 0o777, 0o750);

        // Through a symlink: the target changes, the link stays a link.
        let link = dir.join("link.sh");
        symlink(&file, &link).unwrap();
        write_file(&link, write("via link")).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "via link");

        // Hard links all see the new text (written in place).
        let hard = dir.join("hard.sh");
        std::fs::hard_link(&file, &hard).unwrap();
        write_file(&hard, write("both")).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "both");

        // A failing write leaves the old text and no temporary file behind,
        // both when replacing and in place (`file` has a second link now).
        let fail = |_: &mut dyn Write| Err(std::io::Error::other("disk full"));
        let single = dir.join("single.txt");
        std::fs::write(&single, "kept").unwrap();
        assert!(write_file(&single, fail).is_err());
        assert_eq!(std::fs::read_to_string(&single).unwrap(), "kept");
        assert!(write_file(&file, fail).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "both");
        std::fs::remove_file(&single).unwrap();
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(left.len(), 3, "a.sh, link.sh, hard.sh only");

        // New files work too.
        write_file(&dir.join("new.txt"), write("fresh")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap(), "fresh");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn other_views_follow_edits() {
        let mut d = Document::new_untitled(1);
        d.apply(1, |r, s, c| ops::type_text(r, s, "hello world", c));
        d.goto(2, 6..6); // second group has its caret before "world"
        d.goto(1, 0..0);
        d.apply(1, |r, s, c| ops::type_text(r, s, ">> ", c));
        assert_eq!(d.selections(2).primary().head, 9);
    }

    #[test]
    fn find_and_replace_all() {
        let mut d = Document::new_untitled(1);
        d.apply(1, |r, s, c| ops::type_text(r, s, "foo Foo food", c));
        let q = Query {
            text: "foo".into(),
            ..Default::default()
        };
        assert_eq!(d.find(&q).unwrap().len(), 3);
        let q = Query {
            text: "foo".into(),
            whole_word: true,
            ..Default::default()
        };
        assert_eq!(d.find(&q).unwrap().to_vec(), vec![0..3, 4..7]);
        let q = Query {
            text: r"(f)oo".into(),
            regex: true,
            case_sensitive: true,
            ..Default::default()
        };
        assert_eq!(d.replace_all(1, &q, "${1}u"), 2);
        assert_eq!(d.rope().to_string(), "fu Foo fud");
    }

    #[test]
    fn applies_lsp_edits_as_one_undo() {
        let mut d = Document::new_untitled(1);
        d.apply(1, |r, s, c| ops::type_text(r, s, "é😀 foo(foo)", c));
        // UTF-16: é = 1 unit, 😀 = 2, so "foo" starts at column 4 and 8.
        let edits = vec![
            ((0, 8), (0, 11), "x".to_owned()),
            ((0, 4), (0, 7), "bar".to_owned()),
        ];
        let before = d.rope().to_string();
        assert_eq!(d.apply_lsp_edits(&edits, true), 2);
        assert_eq!(d.rope().to_string(), "é😀 bar(x)");
        d.undo(1);
        assert_eq!(d.rope().to_string(), before);
    }
}
