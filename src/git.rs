//! Git integration through the `git` command line (same behaviour, config, hooks
//! and credentials as in a terminal). Every git call runs on a background thread.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// How a changed line range looks in the gutter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Added,
    Modified,
    /// Lines were deleted just before this line.
    Deleted,
}

/// One entry of `git status`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Absolute path.
    pub path: PathBuf,
    /// Index (staged) status: `.` unchanged, `M`, `A`, `D`, `R`, ...
    pub index: char,
    /// Working tree status: `.` unchanged, `M`, `D`, ...
    pub worktree: char,
    pub untracked: bool,
    pub conflict: bool,
    /// For renames: where it came from.
    pub orig: Option<PathBuf>,
}

impl Entry {
    pub fn staged(&self) -> bool {
        !self.untracked && !self.conflict && self.index != '.'
    }

    pub fn unstaged(&self) -> bool {
        self.untracked || self.conflict || self.worktree != '.'
    }

    /// Letter shown next to the file (VSCode style).
    pub fn letter(&self) -> char {
        if self.conflict {
            '!'
        } else if self.untracked {
            'U'
        } else if self.worktree != '.' {
            self.worktree
        } else {
            self.index
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// No commits yet.
    pub initial: bool,
    pub entries: Vec<Entry>,
    /// Folders containing changes (for the explorer).
    pub dirty_dirs: HashSet<PathBuf>,
    /// Paths .gitignore excludes (whole folders as one entry), absolute.
    pub ignored: HashSet<PathBuf>,
    by_path: HashMap<PathBuf, usize>,
}

impl Status {
    pub fn entry(&self, path: &Path) -> Option<&Entry> {
        self.by_path.get(path).map(|&i| &self.entries[i])
    }

    /// Whether `path` is ignored by git, itself or through an ignored folder.
    pub fn is_ignored(&self, path: &Path) -> bool {
        !self.ignored.is_empty() && path.ancestors().any(|a| self.ignored.contains(a))
    }
}

/// Parses `git ls-files -z --others --ignored --exclude-standard --directory`.
pub fn parse_ignored(root: &Path, out: &str) -> HashSet<PathBuf> {
    out.split('\0')
        .filter(|l| !l.is_empty())
        .map(|l| root.join(l.trim_end_matches('/')))
        .collect()
}

/// Parses `git status --porcelain=v2 --branch -z`.
pub fn parse_status(root: &Path, out: &str) -> Status {
    let mut s = Status::default();
    let mut fields = out.split('\0').peekable();
    while let Some(rec) = fields.next() {
        if let Some(h) = rec.strip_prefix("# ") {
            if let Some(v) = h.strip_prefix("branch.head ") {
                s.branch = v.to_owned();
            } else if let Some(v) = h.strip_prefix("branch.upstream ") {
                s.upstream = Some(v.to_owned());
            } else if h == "branch.oid (initial)" {
                s.initial = true;
            } else if let Some(v) = h.strip_prefix("branch.ab ") {
                let mut ab = v.split(' ');
                s.ahead = ab
                    .next()
                    .and_then(|a| a.trim_start_matches('+').parse().ok())
                    .unwrap_or(0);
                s.behind = ab
                    .next()
                    .and_then(|b| b.trim_start_matches('-').parse().ok())
                    .unwrap_or(0);
            }
            continue;
        }
        let mut parts = rec.splitn(2, ' ');
        let kind = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        let entry = match kind {
            // 1 XY sub mH mI mW hH hI path
            "1" => rest.splitn(8, ' ').collect::<Vec<_>>().get(..8).map(|f| {
                let xy: Vec<char> = f[0].chars().collect();
                Entry {
                    path: root.join(f[7]),
                    index: xy[0],
                    worktree: xy[1],
                    untracked: false,
                    conflict: false,
                    orig: None,
                }
            }),
            // 2 XY sub mH mI mW hH hI Xscore path, then the original path as its own field
            "2" => rest.splitn(9, ' ').collect::<Vec<_>>().get(..9).map(|f| {
                let xy: Vec<char> = f[0].chars().collect();
                let orig = fields.next().map(|o| root.join(o));
                Entry {
                    path: root.join(f[8]),
                    index: xy[0],
                    worktree: xy[1],
                    untracked: false,
                    conflict: false,
                    orig,
                }
            }),
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => rest
                .splitn(10, ' ')
                .collect::<Vec<_>>()
                .get(..10)
                .map(|f| Entry {
                    path: root.join(f[9]),
                    index: 'U',
                    worktree: 'U',
                    untracked: false,
                    conflict: true,
                    orig: None,
                }),
            "?" => Some(Entry {
                path: root.join(rest),
                index: '.',
                worktree: '?',
                untracked: true,
                conflict: false,
                orig: None,
            }),
            _ => None,
        };
        if let Some(e) = entry {
            let mut dir = e.path.parent();
            while let Some(d) = dir {
                if !d.starts_with(root) || !s.dirty_dirs.insert(d.to_path_buf()) {
                    break;
                }
                dir = d.parent();
            }
            s.by_path.insert(e.path.clone(), s.entries.len());
            s.entries.push(e);
        }
    }
    s.entries.sort_by(|a, b| a.path.cmp(&b.path));
    s.by_path = s
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.path.clone(), i))
        .collect();
    s
}

/// Gutter marks for `text` compared with `base`: line ranges of `text`.
pub fn line_marks(base: &str, text: &str) -> Vec<(Range<usize>, Mark)> {
    use similar::DiffOp;
    let diff = similar::TextDiff::configure()
        .deadline(Instant::now() + Duration::from_millis(30))
        .diff_lines(base, text);
    let mut marks = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Insert {
                new_index, new_len, ..
            } => marks.push((new_index..new_index + new_len, Mark::Added)),
            DiffOp::Delete { new_index, .. } => {
                marks.push((new_index..new_index + 1, Mark::Deleted))
            }
            DiffOp::Replace {
                new_index, new_len, ..
            } => marks.push((new_index..new_index + new_len, Mark::Modified)),
        }
    }
    marks
}

/// A unified diff for the "Open Changes" view.
pub fn unified(base: &str, text: &str, name: &str) -> String {
    let diff = similar::TextDiff::from_lines(base, text);
    let out = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{name}"), &format!("b/{name}"))
        .to_string();
    if out.is_empty() {
        format!("No changes in {name}.\n")
    } else {
        out
    }
}

/// Finds the repository containing `dir`.
pub fn find_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    Some(PathBuf::from(out.trim()))
}

/// Runs git synchronously (only used from background threads and for quick queries).
pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_raw(dir, args).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// [`git`] with the output as bytes (file names need not be UTF-8).
pub fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = command(dir, args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(last_error(&out.stderr))
    }
}

/// A git command as rsedit runs every one of them.
fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    // A repository's own config can name a program for git to run on every
    // `status` (core.fsmonitor). rsedit runs status by itself as soon as a
    // folder opens, so that would run code from a downloaded repository.
    cmd.args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(dir)
        // Never wait for a password on a terminal we don't have; don't take locks for reads.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

/// The last non-empty line git printed on stderr (its actual complaint).
fn last_error(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("failed")
        .trim()
        .to_owned()
}

/// Network commands give up after this long: a stalled connection would keep
/// "Pull" busy until the system gives up on it, which can take very long.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(300);

/// [`git`] that gives up after `timeout`, killing git.
fn git_with_timeout(dir: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    let mut child = command(dir, args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = pipe.read_to_end(&mut b);
            b
        })
    };
    let out = read(Box::new(child.stdout.take().expect("piped")));
    let err = read(Box::new(child.stderr.take().expect("piped")));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                // Not waiting for the readers: a child of git (ssh) may still
                // hold the pipes open.
                return Err(format!("timed out after {} s", timeout.as_secs()));
            }
            Err(e) => return Err(format!("git: {e}")),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    } else {
        Err(last_error(&stderr))
    }
}

pub enum Done {
    Status(Result<Status, String>),
    /// `git status` gave the same answer as last time.
    StatusUnchanged,
    /// Staged text of a file (None: not tracked).
    Base {
        path: PathBuf,
        text: Option<String>,
    },
    /// A command finished: what it was, and its outcome.
    Ran {
        label: String,
        result: Result<String, String>,
        refresh: bool,
    },
}

/// The repository of the open folder, and its background jobs.
pub struct Repo {
    pub root: PathBuf,
    pub status: Option<Status>,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    ctx: egui::Context,
    status_running: bool,
    status_again: bool,
    /// Commands in flight (shown as a spinner).
    pub busy: Vec<String>,
    /// Local branches, read when first needed after each status refresh.
    branch_list: Option<Vec<String>>,
    /// Raw output of the last `git status`, to tell whether anything changed.
    last_raw: std::sync::Arc<std::sync::Mutex<String>>,
    /// git's own folder, where `index` and `HEAD` show commits, staging and
    /// checkouts made outside rsedit.
    git_dir: Option<PathBuf>,
    /// Modification times of `index` and `HEAD` at the last refresh.
    watched: Option<[Option<std::time::SystemTime>; 2]>,
    last_refresh: Instant,
}

/// Without any other reason, status is read again after this long (files
/// changed by other programs, e.g. a build, while rsedit has focus).
const REFRESH_AT_LEAST: Duration = Duration::from_secs(30);

impl Repo {
    pub fn open(root: PathBuf, ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let mut repo = Self {
            root,
            status: None,
            tx,
            rx,
            ctx,
            status_running: false,
            status_again: false,
            busy: Vec::new(),
            branch_list: None,
            last_raw: Default::default(),
            git_dir: None,
            watched: None,
            last_refresh: Instant::now(),
        };
        repo.git_dir = git(&repo.root, &["rev-parse", "--absolute-git-dir"])
            .ok()
            .map(|d| PathBuf::from(d.trim()));
        repo.watched = Some(repo.watched_times());
        repo.refresh();
        repo
    }

    fn watched_times(&self) -> [Option<std::time::SystemTime>; 2] {
        let time = |name| {
            let dir = self.git_dir.as_ref()?;
            std::fs::metadata(dir.join(name)).and_then(|m| m.modified()).ok()
        };
        [time("index"), time("HEAD")]
    }

    /// Refreshes status if git's index or HEAD changed (a commit, staging or a
    /// checkout outside rsedit) or it's been a while. Two `stat`s otherwise:
    /// cheap enough to call every couple of seconds, unlike `git status`,
    /// which reads the whole working tree.
    pub fn refresh_if_stale(&mut self) {
        let now = Some(self.watched_times());
        if now != self.watched || self.last_refresh.elapsed() >= REFRESH_AT_LEAST {
            self.watched = now;
            self.refresh();
        }
    }

    fn spawn(&self, f: impl FnOnce() -> Done + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let done = f();
            // An unchanged status needs no new frame (that would keep the app busy:
            // frames trigger status checks).
            let repaint = !matches!(done, Done::StatusUnchanged);
            let _ = tx.send(done);
            if repaint {
                ctx.request_repaint();
            }
        });
    }

    /// Re-reads `git status` (coalesced: at most one runs at a time).
    pub fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        if self.status_running {
            self.status_again = true;
            return;
        }
        self.status_running = true;
        let (root, last) = (self.root.clone(), self.last_raw.clone());
        self.spawn(move || {
            let status = git(
                &root,
                &[
                    "status",
                    "--porcelain=v2",
                    "--branch",
                    "-z",
                    "--untracked-files=all",
                ],
            );
            // What .gitignore hides, for the explorer to dim (folders once).
            let ignored = git(
                &root,
                &["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"],
            )
            .unwrap_or_default();
            match status {
                Ok(out) => {
                    let mut last = last.lock().expect("status lock");
                    let raw = format!("{out}\u{1}{ignored}");
                    if *last == raw {
                        return Done::StatusUnchanged;
                    }
                    let mut status = parse_status(&root, &out);
                    status.ignored = parse_ignored(&root, &ignored);
                    *last = raw;
                    Done::Status(Ok(status))
                }
                Err(e) => Done::Status(Err(e)),
            }
        });
    }

    /// Loads the staged version of a file (the gutter compares against it).
    pub fn load_base(&self, path: &Path) {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return;
        };
        let spec = format!(":{}", rel.to_string_lossy());
        let (root, path) = (self.root.clone(), path.to_path_buf());
        self.spawn(move || Done::Base {
            text: git(&root, &["show", &spec]).ok(),
            path,
        });
    }

    /// Runs a git command in the background; `refresh` re-reads status afterwards.
    pub fn run(&mut self, label: &str, args: Vec<String>, refresh: bool) {
        self.run_steps(label, vec![args], refresh);
    }

    /// Runs several git commands one after another (stopping at the first failure).
    pub fn run_steps(&mut self, label: &str, steps: Vec<Vec<String>>, refresh: bool) {
        self.busy.push(label.to_owned());
        let (root, label) = (self.root.clone(), label.to_owned());
        self.spawn(move || {
            let mut result = Ok(String::new());
            for args in steps {
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                result = match a.first() {
                    Some(&("pull" | "push" | "fetch")) => {
                        git_with_timeout(&root, &a, NETWORK_TIMEOUT)
                    }
                    _ => git(&root, &a),
                };
                if result.is_err() {
                    break;
                }
            }
            Done::Ran {
                result,
                label,
                refresh,
            }
        });
    }

    /// Collects finished jobs. Status results are applied here; the rest is returned.
    pub fn poll(&mut self) -> Vec<Done> {
        let mut out = Vec::new();
        while let Ok(done) = self.rx.try_recv() {
            match done {
                Done::StatusUnchanged => {
                    self.status_running = false;
                    if std::mem::take(&mut self.status_again) {
                        self.refresh();
                    }
                }
                Done::Status(r) => {
                    self.status_running = false;
                    self.branch_list = None;
                    if let Ok(s) = &r {
                        self.status = Some(s.clone());
                    }
                    if std::mem::take(&mut self.status_again) {
                        self.refresh();
                    }
                    out.push(Done::Status(r));
                }
                Done::Ran {
                    label,
                    result,
                    refresh,
                } => {
                    if let Some(i) = self.busy.iter().position(|b| *b == label) {
                        self.busy.remove(i);
                    }
                    if refresh {
                        self.refresh();
                    }
                    out.push(Done::Ran {
                        label,
                        result,
                        refresh,
                    });
                }
                other => out.push(other),
            }
        }
        out
    }

    pub fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    pub fn stage(&mut self, paths: &[PathBuf]) {
        let mut args = vec!["add".to_owned(), "-A".to_owned(), "--".to_owned()];
        args.extend(paths.iter().map(|p| self.rel(p)));
        self.run("Stage", args, true);
    }

    pub fn stage_all(&mut self) {
        self.run("Stage All", vec!["add".into(), "-A".into()], true);
    }

    pub fn unstage(&mut self, paths: &[PathBuf]) {
        let initial = self.status.as_ref().is_some_and(|s| s.initial);
        // Without a first commit there's nothing to restore from: just drop from the index.
        let mut args: Vec<String> = if initial {
            vec![
                "rm".into(),
                "--cached".into(),
                "-r".into(),
                "--quiet".into(),
                "--".into(),
            ]
        } else {
            vec!["restore".into(), "--staged".into(), "--".into()]
        };
        args.extend(paths.iter().map(|p| self.rel(p)));
        self.run("Unstage", args, true);
    }

    pub fn unstage_all(&mut self) {
        let staged: Vec<PathBuf> = self
            .status
            .iter()
            .flat_map(|s| {
                s.entries
                    .iter()
                    .filter(|e| e.staged())
                    .map(|e| e.path.clone())
            })
            .collect();
        if !staged.is_empty() {
            self.unstage(&staged);
        }
    }

    /// Throws away working tree changes (untracked files are deleted).
    pub fn discard(&mut self, entry: &Entry) {
        if entry.untracked {
            let r = std::fs::remove_file(&entry.path)
                .map(|_| String::new())
                .map_err(|e| e.to_string());
            let _ = self.tx.send(Done::Ran {
                label: "Discard".into(),
                result: r,
                refresh: true,
            });
            self.busy.push("Discard".into());
        } else {
            self.run(
                "Discard",
                vec!["restore".into(), "--".into(), self.rel(&entry.path)],
                true,
            );
        }
    }

    /// Commits what's staged; with `stage_all`, stages everything first.
    pub fn commit(&mut self, message: &str, stage_all: bool) {
        let mut steps = Vec::new();
        if stage_all {
            steps.push(vec!["add".into(), "-A".into()]);
        }
        steps.push(vec!["commit".into(), "-m".into(), message.to_owned()]);
        self.run_steps("Commit", steps, true);
    }

    pub fn push(&mut self) {
        let Some(s) = &self.status else { return };
        let args = if s.upstream.is_some() {
            vec!["push".to_owned()]
        } else {
            // First push of a branch: set its upstream.
            vec![
                "push".into(),
                "-u".into(),
                "origin".into(),
                s.branch.clone(),
            ]
        };
        self.run("Push", args, true);
    }

    pub fn branches(&mut self) -> &[String] {
        let root = &self.root;
        self.branch_list.get_or_insert_with(|| {
            git(root, &["branch", "--format=%(refname:short)"])
                .map(|o| o.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let root = Path::new("/r");
        let out = [
            "# branch.oid abc",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 h1 h2 src/a.rs",
            "1 A. N... 000000 100644 100644 h1 h2 b.txt",
            "2 R. N... 100644 100644 100644 h1 h2 R100 new name.rs",
            "old.rs",
            "u UU N... 1 2 3 4 h1 h2 h3 c.rs",
            "? notes/todo.md",
            "",
        ]
        .join("\0");
        let s = parse_status(root, &out);
        assert_eq!(
            (s.branch.as_str(), s.ahead, s.behind, s.initial),
            ("main", 2, 1, false)
        );
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!(s.entries.len(), 5);
        let a = s.entry(Path::new("/r/src/a.rs")).unwrap();
        assert!(a.unstaged() && !a.staged() && a.letter() == 'M');
        let b = s.entry(Path::new("/r/b.txt")).unwrap();
        assert!(b.staged() && b.letter() == 'A');
        let r = s.entry(Path::new("/r/new name.rs")).unwrap();
        assert_eq!(r.orig.as_deref(), Some(Path::new("/r/old.rs")));
        assert!(s.entry(Path::new("/r/c.rs")).unwrap().conflict);
        assert!(s.entry(Path::new("/r/notes/todo.md")).unwrap().untracked);
        assert!(
            s.dirty_dirs.contains(Path::new("/r/notes"))
                && s.dirty_dirs.contains(Path::new("/r/src"))
        );
    }

    #[test]
    fn marks_added_modified_deleted() {
        let base = "a\nb\nc\nd\n";
        let text = "a\nB\nc\nnew\n";
        // b→B modified; d deleted and "new" added count as one replace.
        let marks = line_marks(base, text);
        assert_eq!(marks, vec![(1..2, Mark::Modified), (3..4, Mark::Modified)]);
        assert_eq!(line_marks("a\nb\n", "a\nx\nb\n"), vec![(1..2, Mark::Added)]);
        assert_eq!(
            line_marks("a\nb\nc\n", "a\nc\n"),
            vec![(1..2, Mark::Deleted)]
        );
        assert!(unified(base, text, "f").contains("-b\n+B"));
    }

    #[test]
    #[ignore = "timing, run with --release --ignored --nocapture"]
    fn marks_timing() {
        let base: String = include_str!("app.rs")
            .lines()
            .cycle()
            .take(20_000)
            .map(|l| format!("{l}\n"))
            .collect();
        let mut text = base.clone();
        text.insert_str(base.len() / 2, "x");
        let t = Instant::now();
        let m = line_marks(&base, &text);
        println!(
            "20k-line diff, one change: {:?} ({} marks)",
            t.elapsed(),
            m.len()
        );
    }

    #[test]
    fn network_commands_time_out() {
        let dir = std::env::temp_dir();
        let t = Instant::now();
        let r = git_with_timeout(&dir, &["-c", "alias.stall=!sleep 5", "stall"], Duration::from_millis(200));
        assert_eq!(r, Err("timed out after 0 s".to_owned()));
        assert!(t.elapsed() < Duration::from_secs(2), "gave up in time: {:?}", t.elapsed());
        let ok = git_with_timeout(&dir, &["version"], Duration::from_secs(10)).unwrap();
        assert!(ok.starts_with("git version"));
    }

    /// Status is read again when git's index changes, not on every check.
    #[test]
    fn refresh_follows_the_index() {
        let dir = std::env::temp_dir().join(format!("rsedit-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let mut repo = Repo::open(dir.clone(), egui::Context::default());
        let opened = repo.last_refresh;
        std::thread::sleep(Duration::from_millis(20));
        repo.refresh_if_stale();
        assert_eq!(repo.last_refresh, opened, "nothing changed: no git status");
        git(&dir, &["add", "a.txt"]).unwrap(); // from "outside": the index changes
        repo.refresh_if_stale();
        assert!(repo.last_refresh > opened, "the index changed: status is read");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ignored_folders_cover_their_contents() {
        let root = Path::new("/r");
        let mut s = Status::default();
        s.ignored = parse_ignored(root, "target/\0notes.log\0");
        assert!(s.is_ignored(Path::new("/r/target")));
        assert!(s.is_ignored(Path::new("/r/target/release/rsedit")));
        assert!(s.is_ignored(Path::new("/r/notes.log")));
        assert!(!s.is_ignored(Path::new("/r/src/main.rs")));
        assert!(!s.is_ignored(Path::new("/r/targets")));
    }

    /// A repository's config must not get to run programs when rsedit
    /// merely looks at it.
    #[test]
    fn status_ignores_the_repositorys_fsmonitor() {
        let dir = std::env::temp_dir().join(format!("rsedit-fsmon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).unwrap();
        let marker = dir.join("ran");
        let hook = format!("touch '{}'; false", marker.display());
        git(&dir, &["config", "core.fsmonitor", &hook]).unwrap();
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        git(&dir, &["status", "--porcelain=v2"]).unwrap();
        assert!(!marker.exists(), "core.fsmonitor ran");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_repository_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rsedit-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| git(&dir, args);
        g(&["init", "-q", "-b", "main"]).unwrap();
        g(&["config", "user.email", "t@example.com"]).unwrap();
        g(&["config", "user.name", "T"]).unwrap();
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();

        let ctx = egui::Context::default();
        let mut repo = Repo::open(find_root(&dir).unwrap(), ctx);
        let wait = |repo: &mut Repo| {
            let until = Instant::now() + Duration::from_secs(10);
            let mut all = Vec::new();
            while Instant::now() < until
                && (repo.status_running || !repo.busy.is_empty() || repo.status.is_none())
            {
                all.extend(repo.poll());
                std::thread::sleep(Duration::from_millis(5));
            }
            all
        };
        wait(&mut repo);
        let s = repo.status.clone().unwrap();
        assert!(s.initial && s.branch == "main");
        assert!(s.entries[0].untracked);

        repo.stage_all();
        wait(&mut repo);
        assert!(repo.status.as_ref().unwrap().entries[0].staged());
        repo.unstage_all();
        wait(&mut repo);
        assert!(
            repo.status.as_ref().unwrap().entries[0].untracked,
            "unstaging works before the first commit"
        );

        repo.stage_all();
        wait(&mut repo);
        repo.commit("first", false);
        let done = wait(&mut repo);
        assert!(
            done.iter()
                .any(|d| matches!(d, Done::Ran { label, result: Ok(_), .. } if label == "Commit"))
        );
        assert!(repo.status.as_ref().unwrap().entries.is_empty());

        std::fs::write(dir.join("f.txt"), "two\n").unwrap();
        repo.refresh();
        wait(&mut repo);
        let e = repo.status.as_ref().unwrap().entries[0].clone();
        assert_eq!(e.letter(), 'M');
        repo.load_base(&e.path);
        let base = wait_base(&mut repo);
        assert_eq!(base.as_deref(), Some("one\n"));
        repo.discard(&e);
        wait(&mut repo);
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "one\n");
        std::fs::remove_dir_all(&dir).ok();

        fn wait_base(repo: &mut Repo) -> Option<String> {
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                for d in repo.poll() {
                    if let Done::Base { text, .. } = d {
                        return text;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            None
        }
    }
}
