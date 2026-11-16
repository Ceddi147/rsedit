//! Workspace-wide search: the file index behind quick open, fuzzy matching, and
//! find-in-files. Both walks run on background threads so the UI never waits.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::document::Query;

/// Directories never walked or shown.
pub const IGNORED: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
    ".cache",
    "dist",
    "build",
];
/// Stop indexing past this many files.
const MAX_FILES: usize = 200_000;
/// Files larger than this are skipped by find-in-files.
const MAX_SEARCH_BYTES: u64 = 4 * 1024 * 1024;
/// Find-in-files stops collecting past this many matches.
pub const MAX_RESULTS: usize = 5_000;

/// The files git would show under `root`: tracked and untracked, without what
/// .gitignore excludes. `None` outside a git repository.
fn git_files(root: &Path) -> Option<Vec<PathBuf>> {
    let out = crate::git::git_raw(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
    .ok()?;
    let mut files: Vec<PathBuf> = out
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            use std::os::unix::ffi::OsStrExt;
            root.join(std::ffi::OsStr::from_bytes(p))
        })
        .collect();
    files.sort();
    files.dedup(); // a conflicted file is listed once per stage
    Some(files)
}

/// Calls `f` for each file of the workspace: in a git repository the files git
/// knows (so .gitignore'd build output is skipped), elsewhere every file except
/// in a few well-known generated folders. Stops when `f` returns false or
/// `cancel` is set.
fn walk(root: &Path, cancel: &AtomicBool, mut f: impl FnMut(&Path) -> bool) {
    let Some(files) = git_files(root) else {
        return walk_dirs(root, cancel, f);
    };
    for path in files {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        // Listed but deleted, or a symlink or submodule: like the walk, files only.
        let is_file = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
        if is_file && !f(&path) {
            return;
        }
    }
}

/// Walks `root` depth-first without following symlinks, calling `f` for each file.
/// Stops when `f` returns false or `cancel` is set.
fn walk_dirs(root: &Path, cancel: &AtomicBool, mut f: impl FnMut(&Path) -> bool) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        let mut subdirs = Vec::new();
        for e in entries {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let name = e.file_name();
            if IGNORED.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                subdirs.push(e.path());
            } else if ft.is_file() && !f(&e.path()) {
                return;
            }
        }
        stack.extend(subdirs.into_iter().rev());
    }
}

/// All files of the workspace (relative paths), filled in the background.
pub struct FileIndex {
    pub root: PathBuf,
    files: Arc<Mutex<Vec<String>>>,
    done: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl FileIndex {
    pub fn build(root: &Path, ctx: egui::Context) -> Self {
        let files = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let (f, d, c, r) = (
            files.clone(),
            done.clone(),
            cancel.clone(),
            root.to_path_buf(),
        );
        std::thread::spawn(move || {
            let mut batch = Vec::new();
            let mut count = 0;
            walk(&r, &c, |p| {
                if let Ok(rel) = p.strip_prefix(&r) {
                    batch.push(rel.to_string_lossy().into_owned());
                }
                count += 1;
                if batch.len() >= 2000 {
                    f.lock().expect("index lock").append(&mut batch);
                    ctx.request_repaint();
                }
                count < MAX_FILES
            });
            let mut all = f.lock().expect("index lock");
            all.append(&mut batch);
            all.shrink_to_fit();
            d.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
        Self {
            root: root.to_path_buf(),
            files,
            done,
            cancel,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn with_files<R>(&self, f: impl FnOnce(&[String]) -> R) -> R {
        f(&self.files.lock().expect("index lock"))
    }
}

impl Drop for FileIndex {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Fuzzy match score of `query` against `candidate` (higher is better), or None
/// if the query's characters don't all appear in order. Matches in the file name,
/// at word starts, and runs of consecutive characters score higher.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let name_start = candidate.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let cand: Vec<char> = candidate.chars().collect();
    let name_start_chars = candidate[..name_start].chars().count();
    let mut score = 0i64;
    let mut ci = 0;
    let mut prev_match: Option<usize> = None;
    for q in query.chars().filter(|c| !c.is_whitespace()) {
        let ql = q.to_ascii_lowercase();
        let found = (ci..cand.len()).find(|&i| cand[i].to_ascii_lowercase() == ql)?;
        let c = cand[found];
        let prev = found.checked_sub(1).map(|i| cand[i]);
        score += 1;
        // Consecutive runs matter most, then the start of the file name, then word starts.
        if prev_match == Some(found.wrapping_sub(1)) {
            score += 8;
        }
        if prev.is_none_or(|p| matches!(p, '/' | '\\')) {
            score += 6;
        } else if prev.is_some_and(|p| matches!(p, '_' | '-' | '.' | ' '))
            || (c.is_uppercase() && prev.is_some_and(|p| p.is_lowercase()))
        {
            score += 4;
        }
        if found >= name_start_chars {
            score += 3;
        }
        if c == q {
            score += 1;
        }
        prev_match = Some(found);
        ci = found + 1;
    }
    // Prefer shorter paths.
    Some(score * 16 - cand.len() as i64)
}

/// Indices of the best `limit` matches of `query` among `items`, best first.
pub fn fuzzy_filter(query: &str, items: &[String], limit: usize) -> Vec<usize> {
    let mut scored: Vec<(i64, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, s)| fuzzy_score(query, s).map(|sc| (sc, i)))
        .collect();
    let limit = limit.min(scored.len());
    if limit == 0 {
        return Vec::new();
    }
    scored.select_nth_unstable_by(limit - 1, |a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(limit);
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// One find-in-files hit.
#[derive(Clone)]
pub struct FileMatch {
    pub path: PathBuf,
    /// 0-based line.
    pub line: usize,
    /// Match position in the line, in chars.
    pub cols: std::ops::Range<usize>,
    /// The line (shortened around the match), and the match's byte range in it.
    pub preview: String,
    pub preview_match: std::ops::Range<usize>,
}

#[derive(Default)]
pub struct SearchResults {
    pub matches: Vec<FileMatch>,
    pub files_with_matches: usize,
    pub done: bool,
    pub truncated: bool,
    pub error: Option<String>,
}

/// A running (or finished) find-in-files.
pub struct SearchJob {
    pub query: Query,
    pub results: Arc<Mutex<SearchResults>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for SearchJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl SearchJob {
    /// A finished job showing results that came from elsewhere (e.g. a language server).
    pub fn finished(query: Query, matches: Vec<FileMatch>) -> Self {
        let mut files: Vec<&Path> = matches.iter().map(|m| m.path.as_path()).collect();
        files.dedup();
        let files_with_matches = files.len();
        let truncated = matches.len() > MAX_RESULTS;
        let mut matches = matches;
        matches.truncate(MAX_RESULTS);
        let results = SearchResults {
            matches,
            files_with_matches,
            done: true,
            truncated,
            error: None,
        };
        Self {
            query,
            results: Arc::new(Mutex::new(results)),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start(root: &Path, query: Query, ctx: egui::Context) -> Self {
        let results = Arc::new(Mutex::new(SearchResults::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let re = match query.build() {
            Some(Ok(re)) => re,
            Some(Err(e)) => {
                let mut r = results.lock().expect("results lock");
                r.error = Some(e.to_string());
                r.done = true;
                drop(r);
                return Self {
                    query,
                    results,
                    cancel,
                };
            }
            None => {
                results.lock().expect("results lock").done = true;
                return Self {
                    query,
                    results,
                    cancel,
                };
            }
        };
        let (res, c, root) = (results.clone(), cancel.clone(), root.to_path_buf());
        std::thread::spawn(move || {
            let mut total = 0;
            walk(&root, &c, |path| {
                let found = search_file(path, &re);
                if !found.is_empty() {
                    let mut r = res.lock().expect("results lock");
                    r.files_with_matches += 1;
                    for m in found {
                        if total >= MAX_RESULTS {
                            r.truncated = true;
                            break;
                        }
                        r.matches.push(m);
                        total += 1;
                    }
                    ctx.request_repaint();
                }
                total < MAX_RESULTS
            });
            res.lock().expect("results lock").done = true;
            ctx.request_repaint();
        });
        Self {
            query,
            results,
            cancel,
        }
    }
}

fn search_file(path: &Path, re: &regex::Regex) -> Vec<FileMatch> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if meta.len() > MAX_SEARCH_BYTES {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Vec::new(); // binary
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (line_no, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        for m in re.find_iter(line).filter(|m| !m.is_empty()) {
            out.push(file_match(path, line_no, line, m.range()));
            if out.len() >= MAX_RESULTS {
                return out;
            }
        }
    }
    out
}

/// A hit with a short preview: some context before the match, the rest after.
/// `bytes` is the match's byte range in `line`.
pub fn file_match(
    path: &Path,
    line_no: usize,
    line: &str,
    bytes: std::ops::Range<usize>,
) -> FileMatch {
    let col = line[..bytes.start].chars().count();
    let len = line[bytes.clone()].chars().count();
    let mut from = bytes.start.saturating_sub(40);
    while !line.is_char_boundary(from) {
        from -= 1;
    }
    let mut to = (bytes.end + 120).min(line.len());
    while !line.is_char_boundary(to) {
        to += 1;
    }
    let lead = line[from..bytes.start].trim_start();
    let start_in_preview = lead.len();
    FileMatch {
        path: path.to_path_buf(),
        line: line_no,
        cols: col..col + len,
        preview_match: start_in_preview..start_in_preview + bytes.len(),
        preview: format!("{lead}{}", &line[bytes.start..to]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In a repository the walk is what git shows: tracked and untracked files,
    /// not what .gitignore excludes (whatever its folder is called).
    #[test]
    fn walk_follows_gitignore_in_a_repository() {
        let dir = std::env::temp_dir().join(format!("rsedit-gitwalk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("build-gcc")).unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        crate::git::git(&dir, &["init", "-q"]).unwrap();
        std::fs::write(dir.join(".gitignore"), "build-gcc/\n").unwrap();
        std::fs::write(dir.join("tracked.rs"), "x").unwrap();
        std::fs::write(dir.join("build/source.c"), "x").unwrap();
        std::fs::write(dir.join("build-gcc/out.o"), "x").unwrap();
        crate::git::git(&dir, &["add", "tracked.rs"]).unwrap();
        std::fs::write(dir.join("untracked.txt"), "x").unwrap();
        let mut seen = Vec::new();
        walk(&dir, &AtomicBool::new(false), |p| {
            seen.push(p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned());
            true
        });
        assert_eq!(
            seen,
            [
                ".gitignore",
                "build/source.c",
                "tracked.rs",
                "untracked.txt"
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuzzy_prefers_file_names_and_word_starts() {
        let items: Vec<String> = [
            "src/app.rs",
            "src/apple/pie.rs",
            "docs/a_p_p.md",
            "src/editor.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let order = fuzzy_filter("app", &items, 10);
        assert_eq!(items[order[0]], "src/app.rs");
        assert!(fuzzy_score("xyz", "src/app.rs").is_none());
        let order = fuzzy_filter("ed", &items, 10);
        assert_eq!(items[order[0]], "src/editor.rs");
    }

    #[test]
    fn find_in_files_reports_lines_and_columns() {
        let dir = std::env::temp_dir().join(format!("rsedit-search-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo needle\nnéedle needle\n").unwrap();
        std::fs::write(dir.join("target/skip.txt"), "needle").unwrap();
        let re = Query {
            text: "needle".into(),
            ..Default::default()
        }
        .build()
        .unwrap()
        .unwrap();
        let m = search_file(&dir.join("a.txt"), &re);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].line, m[0].cols.clone()), (1, 4..10));
        assert_eq!((m[1].line, m[1].cols.clone()), (2, 7..13));
        let mut seen = Vec::new();
        walk(&dir, &AtomicBool::new(false), |p| {
            seen.push(p.file_name().unwrap().to_string_lossy().into_owned());
            true
        });
        assert_eq!(seen, vec!["a.txt"], "ignored directories are skipped");
        std::fs::remove_dir_all(&dir).ok();
    }
}
