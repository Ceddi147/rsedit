//! Format Document and format-on-save.
//!
//! A file is formatted by the external command configured for its language in
//! `settings.json` ("format.commands": the text goes to stdin, the result comes
//! back on stdout), else by the language server's `textDocument/formatting`.
//! Formatting runs off the UI thread; the result is applied as a minimal edit
//! (one undo step, carets stay put) only if the document didn't change meanwhile.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use super::EditorApp;
use crate::buffer::Edit;
use crate::lsp::ServerKey;

/// How long a formatter may take.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Bigger files aren't formatted (they'd be copied into the formatter and diffed).
const MAX_BYTES: usize = 5 * 1024 * 1024;

type Formatted = Result<(Vec<Edit>, Option<String>), String>;

pub(super) struct FormatJob {
    doc: u64,
    rev: u64,
    /// Save when done (format-on-save).
    then_save: bool,
    started: Instant,
    wait: Wait,
}

enum Wait {
    Process(Receiver<Formatted>),
    Lsp { key: ServerKey, id: i64 },
}

enum Formatter {
    Command(Vec<String>),
    Lsp(ServerKey),
}

/// The configured command for a file: its extension first, then its language id.
fn command_for<'a>(map: &'a HashMap<String, Vec<String>>, path: &Path) -> Option<&'a Vec<String>> {
    let ext = path.extension().and_then(|e| e.to_str());
    ext.and_then(|e| map.get(e)).or_else(|| {
        let (_, lang) = crate::lsp::server_for(path)?;
        map.get(lang)
    })
}

/// The command line: `{file}` becomes the path, a leading `~/` the home directory.
fn argv(cmd: &[String], path: &Path) -> Vec<String> {
    cmd.iter()
        .map(|a| {
            let a = a.replace("{file}", &path.to_string_lossy());
            match a.strip_prefix("~/") {
                Some(rest) => crate::settings::home()
                    .join(rest)
                    .to_string_lossy()
                    .into_owned(),
                None => a,
            }
        })
        .collect()
}

/// Runs a formatter on `text` (LF line endings) and returns the edits that turn the
/// text into its output, plus a warning when it exited with an error but still
/// printed a result (some tools do, e.g. with a missing optional header file).
fn run(argv: &[String], text: &str, cwd: &Path, timeout: Duration) -> Formatted {
    let (prog, args) = argv.split_first().ok_or("the formatter command is empty")?;
    let mut child = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't run {prog}: {e}"))?;
    // Feeding and draining happen on their own threads, so a tool that streams its
    // output while still reading can't deadlock on a big file.
    let mut stdin = child.stdin.take().expect("piped");
    let input = text.to_owned();
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let (tx, rx) = channel();
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        let _ = tx_out.send((true, b));
    });
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        let _ = tx.send((false, b));
    });
    let deadline = Instant::now() + timeout;
    let (mut stdout, mut stderr) = (None, None);
    while stdout.is_none() || stderr.is_none() {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((true, b)) => stdout = Some(b),
            Ok((false, b)) => stderr = Some(b),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{prog} timed out after {:.1} s",
                    timeout.as_secs_f32()
                ));
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let stderr = String::from_utf8_lossy(&stderr.unwrap_or_default()).into_owned();
    let first_error = stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned();
    let stdout = String::from_utf8(stdout.unwrap_or_default())
        .map_err(|_| format!("{prog} printed text that isn't UTF-8"))?
        .replace("\r\n", "\n");
    // A tool that printed nothing for a non-empty file did not format it.
    if stdout.is_empty() && !text.is_empty() {
        return Err(if first_error.is_empty() {
            format!("{prog} printed nothing ({status})")
        } else {
            first_error
        });
    }
    let warning = (!status.success()).then(|| {
        let code = status
            .code()
            .map_or("a signal".to_owned(), |c| format!("exit {c}"));
        if first_error.is_empty() {
            format!("{prog} {code}")
        } else {
            format!("{prog} {code}: {first_error}")
        }
    });
    Ok((minimal_edits(text, &stdout), warning))
}

/// The smallest few edits that turn `old` into `new`: a line diff, with the common
/// start and end of each changed block cut off, so re-indenting a line edits only
/// its whitespace and the caret stays on its token. Offsets are in chars.
fn minimal_edits(old: &str, new: &str) -> Vec<Edit> {
    use similar::DiffOp;
    if old == new {
        return Vec::new();
    }
    let lines = |t: &str| -> Vec<(usize, usize)> {
        // (start char, start byte) of every line, plus the end.
        let mut v = vec![(0, 0)];
        let (mut c, mut b) = (0, 0);
        for l in t.split_inclusive('\n') {
            c += l.chars().count();
            b += l.len();
            v.push((c, b));
        }
        v
    };
    let (old_lines, new_lines) = (lines(old), lines(new));
    let diff = similar::TextDiff::configure()
        .deadline(Instant::now() + Duration::from_millis(300))
        .diff_lines(old, new);
    let mut edits = Vec::new();
    for op in diff.ops() {
        let (oi, ol, ni, nl) = match *op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => (old_index, old_len, new_index, 0),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => (old_index, 0, new_index, new_len),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (old_index, old_len, new_index, new_len),
        };
        let old_block = &old[old_lines[oi].1..old_lines[oi + ol].1];
        let new_block = &new[new_lines[ni].1..new_lines[ni + nl].1];
        let prefix = old_block
            .chars()
            .zip(new_block.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let (old_len, new_len) = (old_block.chars().count(), new_block.chars().count());
        let suffix = old_block
            .chars()
            .rev()
            .zip(new_block.chars().rev())
            .take(old_len.min(new_len) - prefix)
            .take_while(|(a, b)| a == b)
            .count();
        let start = old_lines[oi].0 + prefix;
        let text: String = new_block
            .chars()
            .skip(prefix)
            .take(new_len - prefix - suffix)
            .collect();
        edits.push(Edit::new(start..old_lines[oi].0 + old_len - suffix, text));
    }
    edits
}

impl EditorApp {
    /// Which formatter handles document `i`, or why none does.
    fn formatter_for(&mut self, i: usize) -> Result<Formatter, &'static str> {
        const NONE: &str = "No formatter for this file";
        let doc = &self.docs[i];
        let Some(path) = doc.path.clone() else {
            return Err(NONE);
        };
        if doc.rope().len_bytes() > MAX_BYTES {
            return Err("File is too large to format");
        }
        if let Some(cmd) = command_for(&self.settings.format_commands, &path) {
            // An empty command means "don't format these files".
            return if cmd.is_empty() {
                Err(NONE)
            } else {
                Ok(Formatter::Command(argv(cmd, &path)))
            };
        }
        if self.lsp_enabled
            && let Some((key, _)) = self.server_key(i)
            && self.lsp.get(&key).is_some_and(|s| s.ready && s.formatting)
        {
            return Ok(Formatter::Lsp(key));
        }
        Err(NONE)
    }

    /// Starts formatting document `i`. Returns false (saying why in the status bar,
    /// unless this is part of a save) when there is nothing to format with.
    pub(super) fn start_format(&mut self, i: usize, then_save: bool, ctx: &egui::Context) -> bool {
        let doc_id = self.docs[i].id;
        if let Some(job) = self.format_jobs.iter_mut().find(|j| j.doc == doc_id) {
            if then_save {
                job.then_save = true; // a save pressed while formatting isn't lost
            } else {
                self.set_status("Already formatting");
            }
            return true;
        }
        let formatter = match self.formatter_for(i) {
            Ok(f) => f,
            Err(why) => {
                if !then_save {
                    self.set_status(why);
                }
                return false;
            }
        };
        let rev = self.docs[i].rev();
        let wait = match formatter {
            Formatter::Command(argv) => {
                let doc = &self.docs[i];
                let text = doc.rope().to_string();
                let cwd = doc
                    .path
                    .as_deref()
                    .and_then(Path::parent)
                    .unwrap_or(Path::new("."))
                    .to_path_buf();
                let (tx, rx) = channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(run(&argv, &text, &cwd, TIMEOUT));
                    ctx.request_repaint();
                });
                Wait::Process(rx)
            }
            Formatter::Lsp(key) => {
                self.sync_doc(i, None);
                let indent = self.docs[i].ctx().indent;
                let id = self
                    .lsp
                    .get_mut(&key)
                    .and_then(|s| s.formatting(doc_id, indent.width, !indent.tabs));
                let Some(id) = id else { return false };
                // One wake-up to notice a server that never answers; not a poll.
                ctx.request_repaint_after(TIMEOUT + Duration::from_millis(100));
                Wait::Lsp { key, id }
            }
        };
        self.format_jobs.push(FormatJob {
            doc: doc_id,
            rev,
            then_save,
            started: Instant::now(),
            wait,
        });
        true
    }

    /// The "Format Document" command.
    pub(super) fn format_document(&mut self, ctx: &egui::Context) {
        if let Some(i) = self.active_doc_index() {
            self.start_format(i, false, ctx);
        }
    }

    /// Saves document `i`, formatting it first when "format.onSave" is on and a
    /// formatter exists. A formatter that fails never blocks the save.
    pub(super) fn save_or_format(&mut self, i: usize, ctx: &egui::Context) {
        if self.settings.format_on_save && self.start_format(i, true, ctx) {
            return;
        }
        self.save(i, false);
    }

    /// Collects finished formatter processes, and gives up on silent servers.
    pub(super) fn poll_format(&mut self) {
        let mut finished = Vec::new();
        let mut i = 0;
        while i < self.format_jobs.len() {
            let job = &self.format_jobs[i];
            let outcome: Option<Formatted> = match &job.wait {
                Wait::Process(rx) => match rx.try_recv() {
                    Ok(r) => Some(r),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Err("the formatter stopped".into())),
                },
                Wait::Lsp { key, .. } if !self.lsp.contains_key(key) => {
                    Some(Err("the language server stopped".into()))
                }
                Wait::Lsp { .. } if job.started.elapsed() > TIMEOUT => {
                    Some(Err("the language server didn't answer".into()))
                }
                Wait::Lsp { .. } => None,
            };
            match outcome {
                Some(result) => finished.push((self.format_jobs.remove(i), result)),
                None => i += 1,
            }
        }
        for (job, result) in finished {
            self.finish_format(job, result);
        }
    }

    /// The language server answered a formatting request.
    pub(super) fn lsp_formatted(
        &mut self,
        key: &ServerKey,
        id: i64,
        utf16: bool,
        result: Result<Vec<crate::lsp::TextEdit>, String>,
    ) {
        let Some(at) = self
            .format_jobs
            .iter()
            .position(|j| matches!(&j.wait, Wait::Lsp { key: k, id: i } if k == key && *i == id))
        else {
            return;
        };
        let job = self.format_jobs.remove(at);
        let formatted = result.map(|edits| {
            // Positions only mean something for the text the server formatted.
            let edits = match self.doc_index(job.doc) {
                Some(i) if self.docs[i].rev() == job.rev => self.docs[i].lsp_edits(&edits, utf16),
                _ => Vec::new(),
            };
            (edits, None)
        });
        self.finish_format(job, formatted);
    }

    fn finish_format(&mut self, job: FormatJob, result: Formatted) {
        let Some(i) = self.doc_index(job.doc) else {
            return;
        };
        // `problem` is something the user should hear about even when we then save.
        let (message, problem) = if self.docs[i].rev() != job.rev {
            let m = "Document changed while formatting; not formatted".to_owned();
            (m.clone(), Some(m))
        } else {
            match result {
                Err(e) => (format!("Format failed: {e}"), Some(e)),
                Ok((edits, _)) if edits.is_empty() => ("Already formatted".to_owned(), None),
                Ok((edits, warning)) => {
                    let focused = &self.panes[self.focused];
                    let view = if focused.active_doc() == Some(job.doc) {
                        focused.id
                    } else {
                        self.panes
                            .iter()
                            .find(|p| p.active_doc() == Some(job.doc))
                            .map_or(focused.id, |p| p.id)
                    };
                    self.docs[i].apply_edits(view, edits);
                    match warning {
                        Some(w) => (format!("Formatted ({w})"), None),
                        None => ("Formatted".to_owned(), None),
                    }
                }
            }
        };
        if job.then_save {
            if self.save(i, false)
                && let Some(p) = problem
            {
                let title = self.docs[i].title();
                self.set_status(format!("Saved {title} (not formatted: {p})"));
            }
        } else {
            self.set_status(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(old: &str, edits: &[Edit]) -> String {
        let mut chars: Vec<char> = old.chars().collect();
        for e in edits.iter().rev() {
            chars.splice(e.range.clone(), e.text.chars());
        }
        chars.into_iter().collect()
    }

    #[test]
    fn minimal_edits_reproduce_the_new_text() {
        let cases = [
            ("", ""),
            ("a\nb\n", "a\nb\n"),
            ("", "new\n"),
            ("gone\n", ""),
            ("int  x ;\nint y;\n", "int x;\nint y;\n"),
            ("a\nb", "a\nb\n"),
            ("é😀 a\nb\n", "é😀  a\nb\nc\n"),
            ("1\n2\n3\n4\n", "1\nX\n3\nY\nZ\n4\n"),
            ("a\n\n\n\nb\n", "a\nb\n"),
        ];
        for (old, new) in cases {
            let edits = minimal_edits(old, new);
            assert_eq!(apply(old, &edits), new, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn re_indenting_edits_only_the_whitespace() {
        let old = "fn f() {\nlet x = 1;\n}\n";
        let new = "fn f() {\n    let x = 1;\n}\n";
        let edits = minimal_edits(old, new);
        assert_eq!(edits, vec![Edit::insert(9, "    ")]);
        // A caret on `x` follows its token.
        assert_eq!(crate::buffer::map_pos(&edits, 13), 17);
    }

    #[test]
    fn picks_the_command_for_a_file() {
        let mut map = HashMap::new();
        map.insert("cpp".to_owned(), vec!["lang".to_owned()]);
        map.insert("h".to_owned(), vec!["ext".to_owned()]);
        map.insert("rs".to_owned(), Vec::new());
        assert_eq!(command_for(&map, Path::new("a.hpp")).unwrap()[0], "lang");
        assert_eq!(
            command_for(&map, Path::new("a.h")).unwrap()[0],
            "ext",
            "extension wins"
        );
        assert!(
            command_for(&map, Path::new("a.rs")).unwrap().is_empty(),
            "[] disables"
        );
        assert!(command_for(&map, Path::new("a.py")).is_none());
    }

    #[test]
    fn expands_file_and_home() {
        let cmd = vec![
            "fmt".to_owned(),
            "--file={file}".to_owned(),
            "~/cfg".to_owned(),
        ];
        let out = argv(&cmd, Path::new("/p/a.cpp"));
        assert_eq!(out[1], "--file=/p/a.cpp");
        assert_eq!(
            out[2],
            crate::settings::home().join("cfg").to_string_lossy()
        );
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn runs_formatters() {
        let cwd = std::env::temp_dir();
        let long = Duration::from_secs(5);
        let (edits, warning) = run(&sh("tr a-z A-Z"), "hi\n", &cwd, long).unwrap();
        assert_eq!(apply("hi\n", &edits), "HI\n");
        assert!(warning.is_none());
        assert_eq!(
            run(&sh("echo boom >&2; exit 3"), "hi\n", &cwd, long),
            Err("boom".to_owned())
        );
        // An error exit with output still counts, with a warning.
        let (edits, warning) =
            run(&sh("tr a-z A-Z; echo oops >&2; exit 1"), "hi\n", &cwd, long).unwrap();
        assert_eq!(apply("hi\n", &edits), "HI\n");
        assert!(warning.unwrap().contains("exit 1: oops"));
        // Printing nothing for real input is a failure; CRLF output is normalised.
        assert!(run(&sh("cat >/dev/null"), "hi\n", &cwd, long).is_err());
        let (edits, _) = run(&sh("printf 'a\\r\\nb\\r\\n'"), "x\n", &cwd, long).unwrap();
        assert_eq!(apply("x\n", &edits), "a\nb\n");
        assert!(
            run(&["/nonexistent/formatter".into()], "x", &cwd, long)
                .unwrap_err()
                .contains("Couldn't run")
        );
    }

    #[test]
    fn slow_formatters_are_killed_and_big_input_does_not_deadlock() {
        let cwd = std::env::temp_dir();
        let started = Instant::now();
        let r = run(&sh("sleep 10"), "x\n", &cwd, Duration::from_millis(200));
        assert!(r.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
        let big = "line of text\n".repeat(100_000);
        let (edits, _) = run(&["cat".into()], &big, &cwd, Duration::from_secs(10)).unwrap();
        assert!(edits.is_empty());
    }

    // ------------------------------------------------ the whole flow in the app

    fn app_with(
        name: &str,
        text: &str,
        cmd: Option<Vec<String>>,
    ) -> (EditorApp, std::path::PathBuf, egui::Context) {
        let dir = std::env::temp_dir().join(format!("rsedit-format-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, text).unwrap();
        let mut app = EditorApp::empty();
        if let Some(cmd) = cmd {
            app.settings.format_commands.insert("txt".into(), cmd);
        }
        app.open_path(&file, true);
        (app, file, egui::Context::default())
    }

    fn settle(app: &mut EditorApp) {
        let start = Instant::now();
        while !app.format_jobs.is_empty() && start.elapsed() < Duration::from_secs(10) {
            app.poll_format();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.format_jobs.is_empty(), "formatting finished");
    }

    #[test]
    fn format_document_is_one_undo_step() {
        let (mut app, _, ctx) = app_with("undo", "hello\nworld\n", Some(sh("tr a-z A-Z")));
        assert!(app.start_format(0, false, &ctx));
        settle(&mut app);
        assert_eq!(app.docs[0].rope().to_string(), "HELLO\nWORLD\n");
        assert_eq!(app.status.as_ref().unwrap().0, "Formatted");
        let view = app.panes[app.focused].id;
        app.docs[0].undo(view);
        assert_eq!(app.docs[0].rope().to_string(), "hello\nworld\n");
    }

    #[test]
    fn a_failing_formatter_leaves_the_text_alone() {
        let (mut app, _, ctx) = app_with("fail", "keep\n", Some(sh("echo broken >&2; exit 2")));
        app.start_format(0, false, &ctx);
        settle(&mut app);
        assert_eq!(app.docs[0].rope().to_string(), "keep\n");
        assert_eq!(app.status.as_ref().unwrap().0, "Format failed: broken");
    }

    #[test]
    fn a_stale_result_is_dropped() {
        let (mut app, _, ctx) = app_with("stale", "abc\n", Some(sh("sleep 0.3; tr a-z A-Z")));
        app.start_format(0, false, &ctx);
        let view = app.panes[app.focused].id;
        app.docs[0].apply(view, |r, s, c| crate::ops::type_text(r, s, "x", c));
        settle(&mut app);
        assert_eq!(app.docs[0].rope().to_string(), "xabc\n");
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .0
                .contains("changed while formatting")
        );
    }

    #[test]
    fn format_on_save_writes_the_formatted_text() {
        let (mut app, file, ctx) = app_with("save", "hello\n", Some(sh("tr a-z A-Z")));
        app.settings.format_on_save = true;
        app.save_or_format(0, &ctx);
        app.save_or_format(0, &ctx); // a second save while formatting isn't lost or doubled
        assert_eq!(app.format_jobs.len(), 1);
        settle(&mut app);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "HELLO\n");
        assert!(!app.docs[0].is_dirty());
    }

    #[test]
    fn a_failing_formatter_does_not_block_saving() {
        let (mut app, file, ctx) = app_with("savefail", "hello\n", Some(sh("exit 2")));
        app.settings.format_on_save = true;
        let view = app.panes[app.focused].id;
        app.docs[0].apply(view, |r, s, c| crate::ops::type_text(r, s, "x", c));
        app.save_or_format(0, &ctx);
        settle(&mut app);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "xhello\n");
        assert!(app.status.as_ref().unwrap().0.contains("not formatted"));
    }

    #[test]
    fn files_without_a_formatter_just_save() {
        let (mut app, file, ctx) = app_with("none", "hello\n", None);
        app.settings.format_on_save = true;
        let view = app.panes[app.focused].id;
        app.docs[0].apply(view, |r, s, c| crate::ops::type_text(r, s, "x", c));
        app.save_or_format(0, &ctx);
        assert!(app.format_jobs.is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "xhello\n");
        app.start_format(0, false, &ctx);
        assert_eq!(app.status.as_ref().unwrap().0, "No formatter for this file");
    }

    /// The user's real uncrustify setup (skipped when it isn't installed).
    #[test]
    #[ignore]
    fn real_uncrustify() {
        let home = crate::settings::home();
        let (exe, cfg) = (
            home.join(".uncrustify/uncrustify"),
            home.join(".uncrustify/uncrustify.cfg"),
        );
        if !exe.exists() || !cfg.exists() {
            return;
        }
        let cmd = vec![
            exe.to_string_lossy().into_owned(),
            "-c".into(),
            cfg.to_string_lossy().into_owned(),
            "-q".into(),
            "--assume".into(),
            "{file}".into(),
        ];
        let cmd = argv(&cmd, Path::new("/tmp/x.cpp"));
        let old = "int  main(){return 0;}\n";
        let (edits, warning) = run(&cmd, old, &std::env::temp_dir(), TIMEOUT).unwrap();
        let new = apply(old, &edits);
        println!("{new}\nwarning: {warning:?}");
        assert_ne!(new, old);
        assert!(new.contains("main()"), "{new}");
    }

    /// clangd's own formatting, with a .clang-format in the project.
    #[test]
    #[ignore]
    fn clangd_formatting() {
        use crate::lsp::{Event, Server, ServerKey};
        let dir = std::env::temp_dir().join(format!("rsedit-clangd-format-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("compile_flags.txt"), "-std=c++20\n").unwrap();
        std::fs::write(dir.join(".clang-format"), "BasedOnStyle: LLVM\n").unwrap();
        let file = dir.join("a.cpp");
        let text = "int  x ;\n";
        std::fs::write(&file, text).unwrap();
        let key = ServerKey {
            command: "clangd",
            root: dir.clone(),
        };
        let mut s = Server::start(key, egui::Context::default(), None, false).unwrap();
        s.did_open(1, &file, "cpp", text.to_owned());
        let mut answer = None;
        let start = Instant::now();
        let mut asked = false;
        while start.elapsed().as_secs() < 30 && answer.is_none() {
            if s.ready && !asked {
                asked = s.formatting(1, 4, true).is_some();
            }
            for e in s.poll() {
                if let Event::Formatting { result, .. } = e {
                    answer = Some(result);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(s.formatting, "clangd advertises formatting");
        let edits = answer.expect("answered").unwrap();
        assert!(!edits.is_empty());
        let mut doc = crate::document::Document::open(&file).unwrap();
        doc.apply_lsp_edits(&edits, true);
        assert_eq!(doc.rope().to_string(), "int x;\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
