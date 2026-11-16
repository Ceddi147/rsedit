//! Keeps copies of unsaved edits, so a crash, a kill or a power cut loses at
//! most about a second of typing.
//!
//! While rsedit runs, every document with unsaved changes is copied (at most
//! once a second, on a background thread) into
//! `$XDG_STATE_HOME/rsedit/recovery/<pid>/`. A panic writes the latest edits
//! right away. Quitting normally removes the folder. On the next start, folders
//! whose process is gone are claimed (renamed, so two instances never take the
//! same one) and their documents come back as unsaved tabs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ropey::Rope;

use crate::document::{Document, LineEnding};

/// How often changed documents are copied.
const EVERY: Duration = Duration::from_secs(1);
const MAGIC: &str = "rsedit-recovery 1";

/// What's needed to bring one document back.
#[derive(Clone)]
struct Snapshot {
    path: Option<PathBuf>,
    line_ending: LineEnding,
    /// Cheap to clone: ropes share their text.
    rope: Rope,
}

/// A document recovered from an earlier run.
pub struct Recovered {
    pub path: Option<PathBuf>,
    pub line_ending: LineEnding,
    pub text: String,
}

enum Job {
    Write(PathBuf, Snapshot),
    Remove(PathBuf),
    RemoveAll(PathBuf),
}

pub struct Recovery {
    /// This process's folder; `None` if there is no state directory.
    dir: Option<PathBuf>,
    /// Revision last copied, per document.
    written: HashMap<u64, u64>,
    last_tick: Option<Instant>,
    jobs: Sender<Job>,
    /// The latest snapshot of every unsaved document, for the panic hook.
    latest: Arc<Mutex<HashMap<u64, Snapshot>>>,
}

/// `$XDG_STATE_HOME/rsedit/recovery` (default `~/.local/state/...`).
fn default_root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("rsedit/recovery"))
}

fn file_for(dir: &Path, doc: u64) -> PathBuf {
    dir.join(format!("{doc}.txt"))
}

/// Writes a snapshot so that a half-written file never replaces a whole one.
fn write_snapshot(file: &Path, s: &Snapshot) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("tmp");
    let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    let path = s
        .path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ending = match s.line_ending {
        LineEnding::Lf => "lf",
        LineEnding::CrLf => "crlf",
    };
    write!(w, "{MAGIC}\npath\t{path}\nending\t{ending}\n\n")?;
    for chunk in s.rope.chunks() {
        w.write_all(chunk.as_bytes())?;
    }
    w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(&tmp, file)
}

fn read_snapshot(file: &Path) -> Option<Recovered> {
    let text = std::fs::read_to_string(file).ok()?;
    let rest = text.strip_prefix(MAGIC)?.strip_prefix('\n')?;
    let (head, body) = rest.split_once("\n\n")?;
    let mut path = None;
    let mut line_ending = LineEnding::Lf;
    for line in head.lines() {
        match line.split_once('\t') {
            Some(("path", p)) if !p.is_empty() => path = Some(PathBuf::from(p)),
            Some(("ending", "crlf")) => line_ending = LineEnding::CrLf,
            _ => {}
        }
    }
    Some(Recovered {
        path,
        line_ending,
        text: body.to_owned(),
    })
}

/// Whether process `pid` is a running rsedit.
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "rsedit")
}

impl Recovery {
    pub fn new() -> Self {
        Self::with_root(default_root())
    }

    /// Recovery under `root`, for tests elsewhere in the app.
    #[cfg(test)]
    pub fn at(root: &Path) -> Self {
        Self::with_root(Some(root.to_path_buf()))
    }

    /// Leaves a copy of `text` behind as if process `pid` had crashed.
    #[cfg(test)]
    pub fn leave_behind(root: &Path, pid: u32, path: Option<PathBuf>, text: &str) {
        let s = Snapshot {
            path,
            line_ending: LineEnding::Lf,
            rope: Rope::from_str(text),
        };
        write_snapshot(&file_for(&root.join(pid.to_string()), 1), &s).unwrap();
    }

    /// Keeps nothing (tests of the rest of the app).
    pub fn disabled() -> Self {
        Self::with_root(None)
    }

    fn with_root(root: Option<PathBuf>) -> Self {
        let (jobs, rx) = channel::<Job>();
        // Without a folder there's nothing to write (the jobs go nowhere).
        if root.is_some() {
            std::thread::spawn(move || {
            for job in rx {
                match job {
                    Job::Write(file, s) => {
                        let _ = write_snapshot(&file, &s);
                    }
                    Job::Remove(file) => {
                        let _ = std::fs::remove_file(file);
                    }
                    Job::RemoveAll(dir) => {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
            }
            });
        }
        Self {
            dir: root.map(|r| r.join(std::process::id().to_string())),
            written: HashMap::new(),
            last_tick: None,
            jobs,
            latest: Default::default(),
        }
    }

    /// Makes a panic write the latest snapshots before the process ends.
    pub fn install_panic_hook(&self) {
        let (Some(dir), latest) = (self.dir.clone(), self.latest.clone()) else {
            return;
        };
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // `try_lock`: the panic may have happened while it was held.
            if let Ok(latest) = latest.try_lock() {
                for (doc, s) in latest.iter() {
                    let _ = write_snapshot(&file_for(&dir, *doc), s);
                }
            }
            previous(info);
        }));
    }

    /// Copies documents whose unsaved text changed, and drops copies of ones
    /// that were saved or closed. Cheap to call every frame.
    pub fn tick(&mut self, docs: &[Document]) {
        let Some(dir) = &self.dir else { return };
        // The panic hook always gets the newest text (ropes are cheap to clone).
        {
            let mut latest = self.latest.lock().expect("recovery lock");
            for d in docs.iter().filter(|d| d.is_dirty() && !d.is_generated()) {
                latest.insert(
                    d.id,
                    Snapshot {
                        path: d.path.clone(),
                        line_ending: d.line_ending,
                        rope: d.rope().clone(),
                    },
                );
            }
            latest.retain(|id, _| docs.iter().any(|d| d.id == *id && d.is_dirty()));
        }
        if self.last_tick.is_some_and(|t| t.elapsed() < EVERY) {
            return;
        }
        self.last_tick = Some(Instant::now());
        let latest = self.latest.lock().expect("recovery lock");
        for d in docs {
            if let Some(s) = latest.get(&d.id)
                && self.written.get(&d.id) != Some(&d.rev())
            {
                self.written.insert(d.id, d.rev());
                let _ = self.jobs.send(Job::Write(file_for(dir, d.id), s.clone()));
            }
        }
        // Saved, discarded or closed: the copy has to go.
        self.written.retain(|id, _| {
            let keep = latest.contains_key(id);
            if !keep {
                let _ = self.jobs.send(Job::Remove(file_for(dir, *id)));
            }
            keep
        });
    }

    /// Quitting normally: nothing to recover from this run.
    pub fn discard(&mut self) {
        if let Some(dir) = &self.dir {
            self.latest.lock().expect("recovery lock").clear();
            self.written.clear();
            let _ = self.jobs.send(Job::RemoveAll(dir.clone()));
        }
    }

    /// Documents left behind by earlier runs that ended without quitting.
    /// Their folders are claimed (renamed) first, then removed once read.
    pub fn take_stale(&self) -> Vec<Recovered> {
        let Some(root) = self.dir.as_ref().and_then(|d| d.parent()) else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        let me = std::process::id();
        let mut out = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(pid) = name.parse::<u32>() else { continue };
            if pid == me || alive(pid) {
                continue;
            }
            let claimed = root.join(format!("claimed-{me}-{pid}"));
            if std::fs::rename(e.path(), &claimed).is_err() {
                continue; // another instance got it
            }
            if let Ok(files) = std::fs::read_dir(&claimed) {
                let mut files: Vec<_> = files.flatten().map(|f| f.path()).collect();
                files.sort();
                out.extend(files.iter().filter_map(|f| read_snapshot(f)));
            }
            let _ = std::fs::remove_dir_all(&claimed);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops;

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rsedit-recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Waits for the writer thread to catch up.
    fn settle() {
        std::thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn unsaved_text_survives_and_comes_back() {
        let root = root("roundtrip");
        let mut doc = Document::new_untitled(1);
        doc.apply(1, |r, s, c| ops::type_text(r, s, "unsaved work\r\nline", c));
        let mut rec = Recovery::with_root(Some(root.clone()));
        rec.tick(std::slice::from_ref(&doc));
        settle();
        // Pretend that process died: its folder belongs to a pid that's gone.
        let dead = root.join("4294967295");
        std::fs::rename(root.join(std::process::id().to_string()), &dead).unwrap();

        let back = Recovery::with_root(Some(root.clone())).take_stale();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].text, doc.rope().to_string());
        assert_eq!(back[0].path, None);
        assert!(!dead.exists(), "claimed and cleaned up");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn saved_or_quit_leaves_nothing_behind() {
        let root = root("clean");
        let mut doc = Document::new_untitled(1);
        doc.apply(1, |r, s, c| ops::type_text(r, s, "x", c));
        let mut rec = Recovery::with_root(Some(root.clone()));
        rec.tick(std::slice::from_ref(&doc));
        settle();
        let mine = root.join(std::process::id().to_string());
        assert_eq!(std::fs::read_dir(&mine).unwrap().count(), 1);

        // Undoing back to the saved text: nothing unsaved anymore.
        doc.undo(1);
        rec.last_tick = None;
        rec.tick(std::slice::from_ref(&doc));
        settle();
        assert_eq!(std::fs::read_dir(&mine).unwrap().count(), 0);

        rec.discard();
        settle();
        assert!(!mine.exists());
        assert!(Recovery::with_root(Some(root.clone())).take_stale().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn running_instances_are_left_alone() {
        let root = root("alive");
        let mut doc = Document::new_untitled(1);
        doc.apply(1, |r, s, c| ops::type_text(r, s, "mine", c));
        let mut rec = Recovery::with_root(Some(root.clone()));
        rec.tick(std::slice::from_ref(&doc));
        settle();
        // Our own folder (a live process) is never taken.
        assert!(Recovery::with_root(Some(root.clone())).take_stale().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
