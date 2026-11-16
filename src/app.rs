//! Application state and actions. Drawing lives in [`ui`].

mod format;
mod ui;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

use crate::buffer::LspPos;
use crate::commands::Command;
use crate::completion::{Completion, is_ident, is_trigger, word_start};
use crate::document::{DiskStatus, Document, LineEnding, Query};
use crate::explorer::Node;
use crate::git::{Done, Repo};
use crate::lsp::{Server, ServerKey};
use crate::ops;
use crate::palette::{Palette, Pick};
use crate::search::{FileIndex, SearchJob};
use crate::session::{Group, Session};
use crate::settings::Settings;

const DEFAULT_FONT_SIZE: f32 = 14.0;
/// How often open files are checked for changes on disk (only while frames are drawn anyway).
const DISK_CHECK_EVERY: Duration = Duration::from_secs(2);
/// Closed files remembered for Ctrl+Shift+T.
const MAX_CLOSED: usize = 20;

/// Something that needs a confirmation before it can happen.
#[derive(Clone)]
enum Pending {
    CloseTab { pane: u64, doc: u64 },
    Quit,
    Delete(PathBuf),
    Discard(crate::git::Entry),
    /// May language servers run in this folder? (They run its code.)
    Trust(PathBuf),
    /// Open a file above [`LARGE_FILE`] bytes?
    OpenLarge { path: PathBuf, new_tab: bool },
}

/// Files above this size ask before opening: they load into memory whole (the
/// text plus the rope take about twice the file's size) and take a moment.
pub(super) const LARGE_FILE: u64 = 64 << 20;

/// A text prompt from the explorer (new file/folder, rename).
#[derive(Clone)]
enum PromptKind {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    /// Rename the symbol at `at` in document `doc` (which was at revision `rev`).
    RenameSymbol {
        doc: u64,
        at: LspPos,
        rev: u64,
    },
}

/// What a pending language server request (other than completion and definition) is for.
enum Ask {
    /// Find references of this word.
    References(String),
    /// Rename; the document must not have changed since `rev`.
    Rename { doc: u64, rev: u64, name: String },
}

struct Prompt {
    kind: PromptKind,
    value: String,
    just_opened: bool,
}

/// An editor group: a column with its own tabs, shown side by side with the others.
/// Groups refer to documents by id, so one buffer can be visible in several groups.
/// The pane id doubles as the view id inside documents (each group has its own cursors).
struct Pane {
    id: u64,
    tabs: Vec<u64>,
    active: usize,
    /// Share of the editor area's width; all panes sum to 1.
    width: f32,
}

impl Pane {
    fn new(id: u64, tabs: Vec<u64>, width: f32) -> Self {
        Self {
            id,
            tabs,
            active: 0,
            width,
        }
    }

    fn active_doc(&self) -> Option<u64> {
        self.tabs.get(self.active).copied()
    }
}

/// Where a dragged tab would land inside a group.
#[derive(Clone, Copy, PartialEq)]
enum DropZone {
    /// New group to the left of it.
    Left,
    /// New group to the right of it.
    Right,
    /// Into the group itself.
    Center,
}

#[derive(Clone, Copy, PartialEq)]
enum SidebarView {
    Explorer,
    Search,
    SourceControl,
}

/// The find/replace bar of the focused editor.
#[derive(Default)]
struct FindBar {
    open: bool,
    replace: bool,
    query: Query,
    replacement: String,
    focus_input: bool,
    error: Option<String>,
}

/// The find-in-files sidebar.
#[derive(Default)]
struct SearchPanel {
    query: Query,
    job: Option<SearchJob>,
    edited_at: Option<Instant>,
    focus_input: bool,
    collapsed: std::collections::HashSet<PathBuf>,
}

pub struct EditorApp {
    docs: Vec<Document>,
    panes: Vec<Pane>,
    /// Index of the group that receives opened files and shortcuts.
    focused: usize,
    /// Move keyboard focus into the focused group's editor on the next frame.
    focus_pending: bool,
    next_pane_id: u64,
    workspace: Option<Node>,
    file_index: Option<FileIndex>,
    show_sidebar: bool,
    sidebar_view: SidebarView,
    untitled_counter: usize,
    pending: Option<Pending>,
    prompt: Option<Prompt>,
    allow_quit: bool,
    status: Option<(String, Instant)>,
    window_title: String,
    palette: Palette,
    find: FindBar,
    search: SearchPanel,
    closed: Vec<PathBuf>,
    font_size: f32,
    last_disk_check: Instant,
    dark: bool,
    /// Line numbers count from the caret line (the caret line is 1).
    pub(super) relative_numbers: bool,
    completion: Completion,
    lsp_enabled: bool,
    lsp: HashMap<ServerKey, Server>,
    /// Servers that couldn't start (not installed, crashed): not retried.
    lsp_failed: HashSet<ServerKey>,
    /// Folders whose code language servers may run.
    pub(super) trust: crate::trust::Trust,
    /// Large files the user agreed to open.
    pub(super) large_files_ok: HashSet<PathBuf>,
    /// Copies of unsaved edits, for after a crash.
    pub(super) recovery: crate::recovery::Recovery,
    /// Which server each document belongs to (cached: finding the root touches the disk).
    doc_server: HashMap<u64, Option<(ServerKey, &'static str)>>,
    /// The git repository of the open folder.
    repo: Option<Repo>,
    /// Staged text of open files (the gutter compares against it); None = untracked.
    git_bases: HashMap<PathBuf, Option<String>>,
    /// Gutter marks of big files are diffed on a background thread:
    /// (document, revision, marks) come back here.
    marks_done: (
        std::sync::mpsc::Sender<(u64, u64, Vec<(std::ops::Range<usize>, crate::git::Mark)>)>,
        std::sync::mpsc::Receiver<(u64, u64, Vec<(std::ops::Range<usize>, crate::git::Mark)>)>,
    ),
    /// Documents with a diff in flight.
    marks_pending: HashSet<u64>,
    commit_message: String,
    /// Latest diagnostics per file: (batch number, UTF-16 positions?, items).
    diagnostics: HashMap<PathBuf, (u64, bool, Vec<crate::lsp::Diagnostic>)>,
    diagnostics_gen: u64,
    definition_request: Option<(ServerKey, i64)>,
    /// A pending find-references or rename request (a newer one replaces it).
    lsp_ask: Option<(ServerKey, i64, Ask)>,
    /// Hover docs of the word under the mouse pointer.
    hover: crate::editor::Hover,
    /// The parameter hints popup while typing a call.
    signature: crate::signature::Signature,
    /// Documents being formatted (see `format.rs`).
    format_jobs: Vec<format::FormatJob>,
    /// Places to go back/forward to (Alt+Left / Alt+Right).
    nav_back: Vec<NavPos>,
    nav_forward: Vec<NavPos>,
    show_problems: bool,
    /// From `settings.json`; reloaded whenever that file is saved.
    settings: Settings,
    /// (group id, document id) pairs showing the rendered Markdown instead of the text.
    previews: HashSet<(u64, u64)>,
    /// The text shown in a Markdown preview, per document, with its revision:
    /// copied once per edit instead of once per frame.
    pub(super) preview_text: HashMap<u64, (u64, String)>,
    md_cache: egui_commonmark::CommonMarkCache,
}

/// A remembered place for back/forward navigation.
#[derive(Clone)]
struct NavPos {
    doc: u64,
    path: Option<PathBuf>,
    pos: usize,
}

const MAX_NAV: usize = 50;

impl EditorApp {
    fn empty() -> Self {
        Self {
            docs: Vec::new(),
            panes: vec![Pane::new(0, Vec::new(), 1.0)],
            focused: 0,
            focus_pending: false,
            next_pane_id: 1,
            workspace: None,
            file_index: None,
            show_sidebar: true,
            sidebar_view: SidebarView::Explorer,
            untitled_counter: 0,
            pending: None,
            prompt: None,
            allow_quit: false,
            status: None,
            window_title: String::new(),
            palette: Palette::default(),
            find: FindBar::default(),
            search: SearchPanel::default(),
            closed: Vec::new(),
            font_size: DEFAULT_FONT_SIZE,
            last_disk_check: Instant::now(),
            dark: true,
            relative_numbers: false,
            completion: Completion::default(),
            lsp_enabled: true,
            lsp: HashMap::new(),
            lsp_failed: HashSet::new(),
            trust: crate::trust::Trust::everything(),
            large_files_ok: HashSet::new(),
            recovery: crate::recovery::Recovery::disabled(),
            doc_server: HashMap::new(),
            repo: None,
            git_bases: HashMap::new(),
            marks_done: std::sync::mpsc::channel(),
            marks_pending: HashSet::new(),
            commit_message: String::new(),
            diagnostics: HashMap::new(),
            diagnostics_gen: 0,
            definition_request: None,
            lsp_ask: None,
            hover: Default::default(),
            signature: Default::default(),
            format_jobs: Vec::new(),
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            show_problems: false,
            settings: Settings::default(),
            previews: HashSet::new(),
            preview_text: HashMap::new(),
            md_cache: Default::default(),
        }
    }

    pub fn new(ctx: &egui::Context, arg: Option<PathBuf>) -> Self {
        crate::highlight::preload();
        let ctx = ctx.clone();
        let mut app = Self::empty();
        app.trust = crate::trust::Trust::load();
        app.reload_settings();
        app.recovery = crate::recovery::Recovery::new();
        app.recovery.install_panic_hook();
        // Before the session, so restoring a tab finds the recovered text.
        let recovered = app.recover_unsaved();
        match arg {
            Some(p) if p.is_file() => {
                if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                    app.set_workspace(dir, &ctx);
                }
                app.open_path(&p, true);
            }
            Some(p) if p.is_dir() => app.set_workspace(&p, &ctx),
            Some(p) => app.set_status(format!("{} does not exist", p.display())),
            None => match Session::load() {
                Some(s) => app.restore(s, &ctx),
                None => {
                    if let Ok(cwd) = std::env::current_dir() {
                        app.set_workspace(&cwd, &ctx);
                    }
                }
            },
        }
        if recovered > 0 {
            // Recovered documents the session didn't show get tabs of their own.
            let shown: HashSet<u64> = app.panes.iter().flat_map(|p| p.tabs.clone()).collect();
            let hidden: Vec<u64> = app
                .docs
                .iter()
                .map(|d| d.id)
                .filter(|id| !shown.contains(id))
                .collect();
            for id in hidden {
                app.show_in_focused(id);
            }
            app.set_status(format!(
                "Recovered unsaved changes in {recovered} file{} (rsedit didn't quit normally)",
                if recovered == 1 { "" } else { "s" }
            ));
        }
        crate::theme::apply(&ctx);
        ctx.set_theme(if app.dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        app
    }

    /// Brings back unsaved edits from runs that ended without quitting, as
    /// unsaved documents. Returns how many.
    fn recover_unsaved(&mut self) -> usize {
        let found = self.recovery.take_stale();
        let n = found.len();
        for r in found {
            let path = r.path.map(|p| p.canonicalize().unwrap_or(p));
            let untitled = if path.is_none() {
                self.untitled_counter += 1;
                self.untitled_counter
            } else {
                0
            };
            self.docs
                .push(Document::recovered(path, &r.text, r.line_ending, untitled));
        }
        n
    }

    // ---------------------------------------------------------------- session

    fn session(&self) -> Session {
        Session {
            workspace: self.workspace.as_ref().map(|w| w.path.clone()),
            groups: self
                .panes
                .iter()
                .map(|p| {
                    let tabs: Vec<(PathBuf, usize)> = p
                        .tabs
                        .iter()
                        .filter_map(|id| self.doc_index(*id))
                        .filter_map(|i| {
                            let d = &self.docs[i];
                            Some((d.path.clone()?, d.selections(p.id).primary().head))
                        })
                        .collect();
                    Group {
                        width: p.width,
                        active: p.active.min(tabs.len().saturating_sub(1)),
                        tabs,
                    }
                })
                .collect(),
            focused: self.focused,
            sidebar: self.show_sidebar,
            sidebar_view: match self.sidebar_view {
                SidebarView::Explorer => "explorer",
                SidebarView::Search => "search",
                SidebarView::SourceControl => "git",
            }
            .to_owned(),
            dark: self.dark,
            relative_numbers: self.relative_numbers,
        }
    }

    fn restore(&mut self, s: Session, ctx: &egui::Context) {
        if let Some(w) = s.workspace.filter(|w| w.is_dir()) {
            self.set_workspace(&w, ctx);
        }
        self.show_sidebar = s.sidebar;
        self.sidebar_view = match s.sidebar_view.as_str() {
            "search" => SidebarView::Search,
            "git" => SidebarView::SourceControl,
            _ => SidebarView::Explorer,
        };
        self.dark = s.dark;
        self.relative_numbers = s.relative_numbers;
        let mut first = true;
        for g in s.groups {
            if !first {
                let id = self.next_pane_id;
                self.next_pane_id += 1;
                self.panes.push(Pane::new(id, Vec::new(), g.width));
                self.focused = self.panes.len() - 1;
            } else {
                self.panes[0].width = g.width;
            }
            first = false;
            for (path, caret) in &g.tabs {
                if path.is_file() {
                    self.open_path(path, true);
                    let view = self.panes[self.focused].id;
                    if let Some(i) = self.active_doc_index() {
                        self.docs[i].goto(view, *caret..*caret);
                    }
                }
            }
            let p = &mut self.panes[self.focused];
            p.active = g.active.min(p.tabs.len().saturating_sub(1));
        }
        // Drop groups whose files are gone, then make widths add up again.
        if self.panes.len() > 1 {
            self.panes.retain(|p| !p.tabs.is_empty());
            if self.panes.is_empty() {
                self.panes.push(Pane::new(0, Vec::new(), 1.0));
            }
        }
        let total: f32 = self.panes.iter().map(|p| p.width.max(0.05)).sum();
        for p in &mut self.panes {
            p.width = p.width.max(0.05) / total;
        }
        self.focused = s.focused.min(self.panes.len() - 1);
        self.focus_pending = true;
    }

    // ---------------------------------------------------------------- basics

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    // -------------------------------------------------------------- settings

    /// Reads `settings.json` again and applies it; problems go to the status bar.
    fn reload_settings(&mut self) -> bool {
        let (settings, problems) = Settings::load();
        self.apply_settings(settings);
        if !problems.is_empty() {
            self.set_status(format!("Settings: {}", problems.join("; ")));
        }
        problems.is_empty()
    }

    fn apply_settings(&mut self, settings: Settings) {
        crate::commands::configure_shortcut(Command::OpenNotes, settings.notes_shortcut);
        if settings.clangd_background_index != self.settings.clangd_background_index {
            self.lsp.retain(|k, _| k.command != "clangd"); // restarts with the new mode
        }
        self.settings = settings;
    }

    fn is_settings_file(path: &Path) -> bool {
        let Some(settings) = crate::settings::path() else {
            return false;
        };
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        canon(path) == canon(&settings)
    }

    // -------------------------------------------------------------- markdown

    fn is_markdown(doc: &Document) -> bool {
        doc.path
            .as_deref()
            .and_then(Path::extension)
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                ["md", "markdown", "mdown", "mkd"]
                    .iter()
                    .any(|m| e.eq_ignore_ascii_case(m))
            })
    }

    /// Whether group `idx` shows its active document as rendered Markdown.
    fn previewing(&self, idx: usize) -> bool {
        let pane = &self.panes[idx];
        pane.active_doc()
            .is_some_and(|d| self.previews.contains(&(pane.id, d)))
    }

    /// Switches group `idx` between the text and the rendered Markdown.
    fn toggle_preview(&mut self, idx: usize) {
        let pane_id = self.panes[idx].id;
        let Some(di) = self.panes[idx].active_doc().and_then(|d| self.doc_index(d)) else {
            return;
        };
        if !Self::is_markdown(&self.docs[di]) {
            self.set_status("Preview is only available for Markdown (.md) files");
            return;
        }
        let key = (pane_id, self.docs[di].id);
        if !self.previews.remove(&key) {
            self.previews.insert(key);
        }
        self.focused = idx;
        self.focus_pending = true; // back to the editor when leaving the preview
    }

    /// Opens the notes file in a tab, creating it (and its folder) if needed.
    fn open_notes(&mut self) {
        let path = self.settings.notes_file.clone();
        let mut created = false;
        if !path.exists() {
            let made = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| {
                    // `create_new`: never truncate a file that appeared meanwhile.
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                });
            match made {
                Ok(_) => created = true,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    self.set_status(format!(
                        "Could not create notes file {}: {e}",
                        path.display()
                    ));
                    return;
                }
            }
        }
        if self.open_path(&path, true).is_some() {
            self.focus_pending = true;
            if created {
                self.set_status(format!("Created notes file {}", path.display()));
            }
        }
    }

    fn set_workspace(&mut self, dir: &Path, ctx: &egui::Context) {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        self.workspace = Some(Node::root(&dir));
        // The quick-open index is built when Ctrl+P is used, not here.
        self.file_index = None;
        self.search.job = None;
        self.repo = crate::git::find_root(&dir).map(|root| Repo::open(root, ctx.clone()));
        self.git_bases.clear();
    }

    /// Re-reads the explorer tree and file index after files changed.
    fn refresh_workspace(&mut self, ctx: &egui::Context) {
        if let Some(repo) = &mut self.repo {
            repo.refresh(); // files were created, renamed or deleted
        }
        if let Some(ws) = &mut self.workspace {
            ws.refresh();
            if self.file_index.is_some() {
                self.file_index = Some(FileIndex::build(&ws.path, ctx.clone()));
            }
        }
    }

    fn doc_index(&self, id: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    /// Index into `docs` of the document shown in the focused group.
    fn active_doc_index(&self) -> Option<usize> {
        self.panes[self.focused]
            .active_doc()
            .and_then(|id| self.doc_index(id))
    }

    /// The focused document and its view id (the focused group's id).
    fn active(&mut self) -> Option<(&mut Document, u64)> {
        let view = self.panes[self.focused].id;
        let i = self.active_doc_index()?;
        Some((&mut self.docs[i], view))
    }

    fn focus_pane(&mut self, idx: usize) {
        if idx < self.panes.len() {
            self.focused = idx;
            self.focus_pending = true;
        }
    }

    // ---------------------------------------------------------------- tabs & groups

    /// Shows a document in the focused group, adding a tab next to the active one if needed.
    fn show_in_focused(&mut self, doc: u64) {
        let pane = &mut self.panes[self.focused];
        match pane.tabs.iter().position(|&d| d == doc) {
            Some(t) => pane.active = t,
            None => {
                let at = (pane.active + 1).min(pane.tabs.len());
                pane.tabs.insert(at, doc);
                pane.active = at;
            }
        }
        self.focus_pending = true;
    }

    fn new_file(&mut self) {
        self.untitled_counter += 1;
        let doc = Document::new_untitled(self.untitled_counter);
        let id = doc.id;
        self.docs.push(doc);
        self.show_in_focused(id);
    }

    /// Opens a file in the focused group. With `new_tab` it gets a tab of its own,
    /// otherwise it takes the place of the active tab. Returns the document id.
    fn open_path(&mut self, path: &Path, new_tab: bool) -> Option<u64> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let id = match self.docs.iter().find(|d| d.path.as_deref() == Some(&path)) {
            Some(doc) => doc.id,
            None if std::fs::metadata(&path).is_ok_and(|m| m.len() > LARGE_FILE)
                && !self.large_files_ok.contains(&path) =>
            {
                if self.pending.is_none() {
                    self.pending = Some(Pending::OpenLarge { path, new_tab });
                }
                return None;
            }
            None => match Document::open(&path) {
                Ok(doc) => {
                    let id = doc.id;
                    self.docs.push(doc);
                    id
                }
                Err(e) => {
                    self.set_status(format!("Could not open {}: {e}", path.display()));
                    return None;
                }
            },
        };
        let already_here = self.panes[self.focused].tabs.contains(&id);
        if new_tab || already_here {
            self.show_in_focused(id);
        } else {
            self.replace_active_tab(id);
        }
        Some(id)
    }

    /// Opens a file and selects a range given as (line, column range), 0-based.
    fn open_at(&mut self, path: &Path, line: usize, cols: std::ops::Range<usize>) {
        if self.open_path(path, false).is_none() {
            return;
        }
        if let Some((doc, view)) = self.active() {
            let rope = doc.rope();
            let line = line.min(rope.len_lines() - 1);
            let start = rope.line_to_char(line);
            let len = crate::buffer::line_len(rope, line);
            doc.goto(view, start + cols.start.min(len)..start + cols.end.min(len));
        }
    }

    /// Shows `doc` in the focused group's active tab instead of what was there.
    /// Never hides unsaved changes: a dirty document that's not visible in another
    /// group keeps its tab, and `doc` gets a new one.
    fn replace_active_tab(&mut self, doc: u64) {
        let focused = self.focused;
        let Some(old) = self.panes[focused].active_doc() else {
            self.show_in_focused(doc);
            return;
        };
        let shown_elsewhere = |app: &Self| {
            app.panes
                .iter()
                .enumerate()
                .any(|(i, p)| i != focused && p.tabs.contains(&old))
        };
        let dirty = self.doc_index(old).is_some_and(|i| self.docs[i].is_dirty());
        if dirty && !shown_elsewhere(self) {
            self.show_in_focused(doc);
            return;
        }
        let pane_id = self.panes[focused].id;
        let pane = &mut self.panes[focused];
        pane.tabs[pane.active] = doc;
        self.focus_pending = true;
        if let Some(i) = self.doc_index(old) {
            self.docs[i].drop_view(pane_id);
        }
        if !shown_elsewhere(self) {
            self.forget_doc(old);
        }
    }

    /// Removes a document that no group shows any more.
    fn forget_doc(&mut self, doc: u64) {
        if let Some(i) = self.doc_index(doc) {
            let d = self.docs.remove(i);
            if let Some(p) = d.path {
                self.closed.retain(|c| *c != p);
                self.closed.push(p);
                if self.closed.len() > MAX_CLOSED {
                    self.closed.remove(0);
                }
            }
        }
    }

    /// Opens the given tab in a new group directly to the right of `from`.
    fn split_right(&mut self, from: usize, tab: usize) {
        let Some(&doc) = self.panes[from].tabs.get(tab) else {
            return;
        };
        self.insert_pane(from + 1, from, doc);
        let (src, dst) = (self.panes[from].id, self.panes[from + 1].id);
        if let Some(i) = self.doc_index(doc) {
            self.docs[i].clone_view(src, dst);
        }
    }

    /// Inserts a new group showing `doc` at index `at`, taking half of `donor`'s width,
    /// and focuses it.
    fn insert_pane(&mut self, at: usize, donor: usize, doc: u64) {
        let half = self.panes[donor].width / 2.0;
        self.panes[donor].width = half;
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        self.panes.insert(at, Pane::new(id, vec![doc], half));
        self.focus_pane(at);
    }

    /// Where dropping a tab dragged out of group `from` would go, for a pointer at `pos`
    /// over group `to`. `None` if the drop wouldn't change anything.
    fn drop_zone(
        &self,
        from: usize,
        to: usize,
        rect: egui::Rect,
        pos: egui::Pos2,
    ) -> Option<DropZone> {
        let edge = rect.width() * 0.25;
        let zone = if pos.x < rect.left() + edge {
            DropZone::Left
        } else if pos.x > rect.right() - edge {
            DropZone::Right
        } else {
            DropZone::Center
        };
        let alone = self.panes[from].tabs.len() == 1;
        match zone {
            DropZone::Center if from == to => None,
            // Splitting a group's only tab off next to itself would just recreate it.
            DropZone::Left | DropZone::Right if from == to && alone => None,
            _ => Some(zone),
        }
    }

    /// Moves a dragged tab from group `from` according to `zone` of group `to`.
    fn drop_tab(&mut self, from: usize, tab: usize, to: usize, zone: DropZone) {
        let Some(&doc) = self.panes[from].tabs.get(tab) else {
            return;
        };
        let src_view = self.panes[from].id;
        let from = match zone {
            DropZone::Center => {
                self.focused = to;
                self.show_in_focused(doc);
                from
            }
            DropZone::Left | DropZone::Right => {
                let at = if zone == DropZone::Left { to } else { to + 1 };
                self.insert_pane(at, to, doc);
                if at <= from { from + 1 } else { from }
            }
        };
        let dst_view = self.panes[self.focused].id;
        if let Some(i) = self.doc_index(doc) {
            self.docs[i].clone_view(src_view, dst_view);
        }
        // May remove the source group; `remove_pane` keeps `focused` pointing at the target.
        self.remove_tab(from, tab);
    }

    /// Moves a tab to the neighbouring group (`dir` = -1 left, +1 right).
    /// Moving right out of the last group creates a new one.
    fn move_tab(&mut self, from: usize, tab: usize, dir: isize) {
        let Some(to) = from.checked_add_signed(dir) else {
            return;
        };
        if to >= self.panes.len() {
            self.drop_tab(from, tab, from, DropZone::Right);
        } else {
            self.drop_tab(from, tab, to, DropZone::Center);
        }
    }

    /// Removes a tab from a group without closing the document. Empty groups collapse.
    fn remove_tab(&mut self, pane: usize, tab: usize) {
        let p = &mut self.panes[pane];
        let doc = p.tabs.remove(tab);
        let view = p.id;
        if p.active > tab || p.active >= p.tabs.len() {
            p.active = p.active.saturating_sub(1);
        }
        if let Some(i) = self.doc_index(doc) {
            self.docs[i].drop_view(view);
        }
        if self.panes[pane].tabs.is_empty() && self.panes.len() > 1 {
            self.remove_pane(pane);
        }
    }

    fn remove_pane(&mut self, idx: usize) {
        if self.panes.len() <= 1 {
            return;
        }
        let width = self.panes.remove(idx).width;
        // Give the freed space to the neighbour on the left (or right for the first group).
        self.panes[idx.saturating_sub(1)].width += width;
        if self.focused > idx || self.focused >= self.panes.len() {
            self.focused = self.focused.saturating_sub(1);
        }
        self.focus_pending = true;
    }

    /// Moves all tabs of a group into its left neighbour (right for the first group) and removes it.
    fn join_group(&mut self, idx: usize) {
        if self.panes.len() <= 1 {
            return;
        }
        let target = if idx == 0 { 1 } else { idx - 1 };
        while !self.panes[idx].tabs.is_empty() {
            let alive = self.panes.len();
            self.drop_tab(idx, 0, target, DropZone::Center);
            if self.panes.len() < alive {
                break; // the group collapsed after its last tab left
            }
        }
    }

    /// Asks before closing a tab if that would discard unsaved changes.
    fn request_close(&mut self, pane: usize, tab: usize) {
        let Some(&doc) = self.panes.get(pane).and_then(|p| p.tabs.get(tab)) else {
            return;
        };
        let other_view = self
            .panes
            .iter()
            .enumerate()
            .any(|(i, p)| i != pane && p.tabs.contains(&doc));
        let dirty = self.doc_index(doc).is_some_and(|i| self.docs[i].is_dirty());
        if dirty && !other_view {
            self.pending = Some(Pending::CloseTab {
                pane: self.panes[pane].id,
                doc,
            });
        } else {
            self.close_tab(pane, tab);
        }
    }

    /// Closes a tab and drops its document once no group shows it any more.
    fn close_tab(&mut self, pane: usize, tab: usize) {
        let Some(&doc) = self.panes.get(pane).and_then(|p| p.tabs.get(tab)) else {
            return;
        };
        self.remove_tab(pane, tab);
        if !self.panes.iter().any(|p| p.tabs.contains(&doc)) {
            self.forget_doc(doc);
        }
    }

    /// Ctrl+W: close the focused tab, or the focused group if it's already empty.
    fn close_active(&mut self) {
        let pane = &self.panes[self.focused];
        if pane.tabs.is_empty() {
            self.remove_pane(self.focused);
        } else {
            self.request_close(self.focused, pane.active);
        }
    }

    /// Closes every tab that has no unsaved changes.
    fn close_all_clean(&mut self) {
        loop {
            let target = self.panes.iter().enumerate().find_map(|(p, pane)| {
                pane.tabs
                    .iter()
                    .position(|d| self.doc_index(*d).is_some_and(|i| !self.docs[i].is_dirty()))
                    .map(|t| (p, t))
            });
            match target {
                Some((p, t)) => self.close_tab(p, t),
                None => break,
            }
        }
        if self.docs.iter().any(|d| d.is_dirty()) {
            self.set_status("Kept editors with unsaved changes open");
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        let p = &mut self.panes[self.focused];
        if !p.tabs.is_empty() {
            p.active = (p.active as isize + delta).rem_euclid(p.tabs.len() as isize) as usize;
            self.focus_pending = true;
        }
    }

    // ---------------------------------------------------------------- files

    /// Saves the doc at `idx`. Returns false if it wasn't saved (error or cancelled).
    fn save(&mut self, idx: usize, force_dialog: bool) -> bool {
        let Some(doc) = self.docs.get_mut(idx) else {
            return false;
        };
        let result = if doc.path.is_none() || force_dialog {
            let mut dialog = rfd::FileDialog::new().set_file_name(doc.title());
            if let Some(ws) = &self.workspace {
                dialog = dialog.set_directory(&ws.path);
            }
            match dialog.save_file() {
                Some(p) => doc.save_as(p),
                None => return false,
            }
        } else {
            doc.save()
        };
        match result {
            Ok(()) => {
                let msg = format!("Saved {}", doc.title());
                let doc_id = doc.id;
                let saved_settings = doc.path.as_deref().is_some_and(Self::is_settings_file);
                let saved_build_config = doc
                    .path
                    .as_deref()
                    .and_then(|p| p.file_name())
                    .is_some_and(|n| n == "c_cpp_properties.json" || n == "CMakeLists.txt");
                self.set_status(msg);
                if saved_settings && self.reload_settings() {
                    self.set_status("Settings saved and applied");
                }
                // New include paths: forget the guessed flags and restart clangd on them.
                if saved_build_config {
                    crate::cppflags::invalidate();
                    self.lsp.retain(|k, _| k.command != "clangd");
                }
                // rust-analyzer runs `cargo check` on save. Edits go out first.
                self.sync_doc(idx, None);
                if let Some(Some((key, _))) = self.doc_server.get(&doc_id).cloned()
                    && let Some(server) = self.lsp.get_mut(&key)
                {
                    server.did_save(doc_id);
                }
                if let Some(repo) = &mut self.repo {
                    repo.refresh();
                }
                if let Some(ws) = &mut self.workspace {
                    ws.refresh();
                }
                true
            }
            Err(e) => {
                self.set_status(format!("Save failed: {e}"));
                false
            }
        }
    }

    fn save_all(&mut self, ctx: &egui::Context) {
        for i in 0..self.docs.len() {
            if !self.docs[i].is_dirty() {
                continue;
            }
            // Formatting finishes (and saves) later; its failure doesn't stop the rest.
            if self.settings.format_on_save && self.start_format(i, true, ctx) {
                continue;
            }
            if !self.save(i, false) {
                break;
            }
        }
    }

    /// Reloads files changed on disk (and warns about ones with unsaved edits).
    fn check_disk(&mut self) {
        self.last_disk_check = Instant::now();
        let mut reloaded = false;
        let mut msgs = Vec::new();
        for d in &mut self.docs {
            match d.check_disk() {
                DiskStatus::Unchanged => {}
                DiskStatus::Reloaded => {
                    reloaded = true;
                    msgs.push(format!("Reloaded {} (changed on disk)", d.title()))
                }
                DiskStatus::Conflict => msgs.push(format!(
                    "{} changed on disk; saving will overwrite it",
                    d.title()
                )),
            }
        }
        if let Some(repo) = &mut self.repo {
            if reloaded {
                repo.refresh();
            } else {
                repo.refresh_if_stale();
            }
        }
        if let Some(m) = msgs.pop() {
            self.set_status(m);
        }
    }

    /// Carries out an explorer prompt (new file/folder, rename).
    fn finish_prompt(&mut self, kind: PromptKind, name: &str, ctx: &egui::Context) {
        let name = name.trim();
        if let PromptKind::RenameSymbol { doc, at, rev } = kind {
            self.start_rename(doc, at, rev, name);
            return;
        }
        if name.is_empty() || name.contains('/') && matches!(kind, PromptKind::Rename(_)) {
            self.set_status("Invalid name");
            return;
        }
        let result = match &kind {
            PromptKind::NewFile(dir) => {
                let p = dir.join(name);
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&p)
                    .map(|_| Some(p))
            }
            PromptKind::NewFolder(dir) => std::fs::create_dir_all(dir.join(name)).map(|_| None),
            PromptKind::RenameSymbol { .. } => unreachable!("handled above"),
            PromptKind::Rename(old) => {
                let new = old.with_file_name(name);
                std::fs::rename(old, &new).map(|_| {
                    // Keep open documents pointing at the renamed files.
                    for d in &mut self.docs {
                        if let Some(p) = &d.path
                            && let Ok(rest) = p.strip_prefix(old)
                        {
                            d.path = Some(if rest.as_os_str().is_empty() {
                                new.clone()
                            } else {
                                new.join(rest)
                            });
                        }
                    }
                    None
                })
            }
        };
        match result {
            Ok(Some(file)) => {
                self.refresh_workspace(ctx);
                self.open_path(&file, true);
            }
            Ok(None) => self.refresh_workspace(ctx),
            Err(e) => self.set_status(format!("Failed: {e}")),
        }
    }

    fn delete_path(&mut self, path: &Path, ctx: &egui::Context) {
        let result = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match result {
            Ok(()) => {
                self.set_status(format!("Deleted {}", path.display()));
                self.refresh_workspace(ctx);
            }
            Err(e) => self.set_status(format!("Delete failed: {e}")),
        }
    }

    // ---------------------------------------------------------------- find

    fn open_find(&mut self, replace: bool) {
        // Start from the selected text, like VSCode.
        if let Some((doc, view)) = self.active() {
            let p = doc.selections(view).primary();
            if !p.is_empty() && p.end() - p.start() < 200 {
                let text = doc.rope().slice(p.range()).to_string();
                if !text.contains('\n') {
                    self.find.query.text = text;
                }
            }
        }
        self.find.open = true;
        self.find.replace = replace;
        self.find.focus_input = true;
    }

    /// Selects the next (or previous) match from the caret, wrapping around.
    fn find_step(&mut self, forward: bool, from_start_of_selection: bool) {
        let query = self.find.query.clone();
        let Some((doc, view)) = self.active() else {
            return;
        };
        let p = doc.selections(view).primary();
        let matches = match doc.find(&query) {
            Ok(m) => m,
            Err(e) => {
                self.find.error = Some(e);
                return;
            }
        };
        if matches.is_empty() {
            return;
        }
        let i = if forward {
            let from = if from_start_of_selection {
                p.start()
            } else {
                p.end()
            };
            let i = matches.partition_point(|m| m.start < from);
            if i == matches.len() { 0 } else { i }
        } else {
            let i = matches.partition_point(|m| m.start < p.start());
            if i == 0 { matches.len() - 1 } else { i - 1 }
        };
        let m = matches[i].clone();
        doc.goto(view, m);
        self.find.error = None;
    }

    fn replace_one(&mut self) {
        let (query, with) = (self.find.query.clone(), self.find.replacement.clone());
        let Some((doc, view)) = self.active() else {
            return;
        };
        let p = doc.selections(view).primary();
        let is_match = doc.find(&query).is_ok_and(|m| {
            m.binary_search_by_key(&p.start(), |r| r.start)
                .is_ok_and(|i| m[i] == p.range())
        });
        if is_match {
            doc.replace(view, &query, p.range(), &with);
        }
        self.find_step(true, false);
    }

    fn replace_all(&mut self) {
        let (query, with) = (self.find.query.clone(), self.find.replacement.clone());
        let Some((doc, view)) = self.active() else {
            return;
        };
        let n = doc.replace_all(view, &query, &with);
        self.set_status(format!(
            "Replaced {n} occurrence{}",
            if n == 1 { "" } else { "s" }
        ));
    }

    // ---------------------------------------------------------------- commands

    fn pick(&mut self, pick: Pick, ctx: &egui::Context) {
        match pick {
            Pick::File(rel) => {
                if let Some(root) = self.file_index.as_ref().map(|i| i.root.clone()) {
                    self.open_path(&root.join(rel), false);
                }
            }
            Pick::Command(c) => self.run(c, ctx),
            Pick::Line(line, col) => {
                if let Some((doc, view)) = self.active() {
                    let rope = doc.rope();
                    let l = (line - 1).min(rope.len_lines() - 1);
                    let c = col
                        .unwrap_or(1)
                        .saturating_sub(1)
                        .min(crate::buffer::line_len(rope, l));
                    let pos = rope.line_to_char(l) + c;
                    doc.goto(view, pos..pos);
                }
                self.focus_pending = true;
            }
        }
    }

    /// Runs a command from a shortcut, menu or the palette.
    fn run(&mut self, cmd: Command, ctx: &egui::Context) {
        use Command::*;
        // Editor commands work on the focused editor; give it the keyboard back afterwards.
        let edit = |app: &mut Self, f: &dyn Fn(&mut Document, u64)| {
            if let Some((doc, view)) = app.active() {
                f(doc, view);
            }
            app.focus_pending = true;
        };
        match cmd {
            NewFile => self.new_file(),
            OpenFile => {
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    self.open_path(&p, true);
                }
            }
            OpenFolder => {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.set_workspace(&p, ctx);
                    self.show_sidebar = true;
                    self.sidebar_view = SidebarView::Explorer;
                }
            }
            Save => {
                if let Some(i) = self.active_doc_index() {
                    self.save_or_format(i, ctx);
                }
            }
            FormatDocument => self.format_document(ctx),
            SaveAs => {
                if let Some(i) = self.active_doc_index() {
                    self.save(i, true);
                }
            }
            SaveAll => self.save_all(ctx),
            CloseEditor => self.close_active(),
            CloseAllEditors => self.close_all_clean(),
            ReopenClosedEditor => {
                if let Some(p) = self.closed.pop() {
                    self.open_path(&p, true);
                }
            }
            Undo => edit(self, &|d, v| d.undo(v)),
            Redo => edit(self, &|d, v| d.redo(v)),
            Copy | Cut => {
                if let Some((doc, view)) = self.active() {
                    let sels = doc.selections(view);
                    if cmd == Cut {
                        let (text, _, change) = ops::cut(doc.rope(), &sels);
                        doc.commit(view, change);
                        ctx.copy_text(text);
                    } else {
                        ctx.copy_text(ops::copy_text(doc.rope(), &sels).0);
                    }
                }
                self.focus_pending = true;
            }
            Paste => {
                // The clipboard arrives as a Paste event, which the focused editor handles.
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                self.focus_pending = true;
            }
            Find => self.open_find(false),
            Replace => self.open_find(true),
            FindInFiles => {
                self.show_sidebar = true;
                self.sidebar_view = SidebarView::Search;
                self.search.focus_input = true;
                if let Some((doc, view)) = self.active() {
                    let p = doc.selections(view).primary();
                    if !p.is_empty() && p.end() - p.start() < 200 {
                        let t = doc.rope().slice(p.range()).to_string();
                        if !t.contains('\n') {
                            self.search.query.text = t;
                            self.search.edited_at = Some(Instant::now());
                        }
                    }
                }
            }
            ToggleComment => edit(self, &|d, v| d.toggle_comment(v)),
            Indent => edit(self, &|d, v| d.apply(v, ops::indent_lines)),
            Outdent => edit(self, &|d, v| d.apply(v, ops::outdent_lines)),
            DeleteLine => edit(self, &|d, v| d.apply(v, |r, s, _| ops::delete_lines(r, s))),
            MoveLineUp => edit(self, &|d, v| {
                d.run(v, |r, s, _| ops::move_lines(r, s, true))
            }),
            MoveLineDown => edit(self, &|d, v| {
                d.run(v, |r, s, _| ops::move_lines(r, s, false))
            }),
            CopyLineUp => edit(self, &|d, v| {
                d.apply(v, |r, s, _| ops::copy_lines(r, s, true))
            }),
            CopyLineDown => edit(self, &|d, v| {
                d.apply(v, |r, s, _| ops::copy_lines(r, s, false))
            }),
            InsertLineBelow => edit(self, &|d, v| {
                d.apply(v, |r, s, _| ops::insert_line(r, s, false))
            }),
            InsertLineAbove => edit(self, &|d, v| {
                d.apply(v, |r, s, _| ops::insert_line(r, s, true))
            }),
            SelectAll => edit(self, &|d, v| d.motion(v, |r, _| ops::select_all(r))),
            SelectLine => edit(self, &|d, v| d.motion(v, ops::select_line)),
            AddNextOccurrence => edit(self, &|d, v| d.motion(v, ops::add_next_occurrence)),
            SelectAllOccurrences => edit(self, &|d, v| d.motion(v, ops::select_all_occurrences)),
            GoToFile => {
                self.palette.show_with("");
                self.build_index(ctx);
            }
            GoToLine => self.palette.show_with(":"),
            CommandPalette => self.palette.show_with(">"),
            NextTab => self.cycle_tab(1),
            PreviousTab => self.cycle_tab(-1),
            ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            ShowExplorer => {
                self.show_sidebar = true;
                self.sidebar_view = SidebarView::Explorer;
            }
            ShowSearch => {
                self.show_sidebar = true;
                self.sidebar_view = SidebarView::Search;
                self.search.focus_input = true;
            }
            SplitRight => {
                let pane = &self.panes[self.focused];
                if !pane.tabs.is_empty() {
                    self.split_right(self.focused, pane.active);
                }
            }
            JoinGroup => self.join_group(self.focused),
            FocusGroup1 => self.focus_pane(0),
            FocusGroup2 => self.focus_pane(1),
            FocusGroup3 => self.focus_pane(2),
            ZoomIn => egui::gui_zoom::zoom_in(ctx),
            ZoomOut => egui::gui_zoom::zoom_out(ctx),
            ZoomReset => ctx.set_zoom_factor(1.0),
            ToggleRelativeLineNumbers => self.relative_numbers = !self.relative_numbers,
            ToggleTheme => {
                self.dark = !self.dark;
                ctx.set_theme(if self.dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                });
            }
            ToggleLineEnding => {
                if let Some((doc, _)) = self.active() {
                    doc.line_ending = match doc.line_ending {
                        LineEnding::Lf => LineEnding::CrLf,
                        LineEnding::CrLf => LineEnding::Lf,
                    };
                }
            }
            IndentWithSpaces2 => self.set_indent(ops::Indent {
                tabs: false,
                width: 2,
            }),
            IndentWithSpaces4 => self.set_indent(ops::Indent {
                tabs: false,
                width: 4,
            }),
            IndentWithTabs => self.set_indent(ops::Indent {
                tabs: true,
                width: 4,
            }),
            GitShowSourceControl => {
                self.show_sidebar = true;
                self.sidebar_view = SidebarView::SourceControl;
                self.git_run(|r| r.refresh());
            }
            GitOpenChanges => {
                if let Some(p) = self
                    .active_doc_index()
                    .and_then(|i| self.docs[i].path.clone())
                {
                    self.open_changes(&p);
                }
            }
            GitCommit => self.commit(),
            GitStageAll => self.git_run(|r| r.stage_all()),
            GitUnstageAll => self.git_run(|r| r.unstage_all()),
            GitStageFile => {
                if let Some(p) = self
                    .active_doc_index()
                    .and_then(|i| self.docs[i].path.clone())
                {
                    self.git_run(|r| r.stage(&[p]));
                }
            }
            GitRefresh => self.git_run(|r| r.refresh()),
            GitPull => self.git_run(|r| r.run("Pull", vec!["pull".into()], true)),
            GitPush => self.git_run(|r| r.push()),
            GitFetch => self.git_run(|r| r.run("Fetch", vec!["fetch".into()], true)),
            GitInit => {
                if let Some(root) = self.workspace.as_ref().map(|w| w.path.clone()) {
                    match crate::git::git(&root, &["init"]) {
                        Ok(_) => {
                            self.repo = Some(Repo::open(root, ctx.clone()));
                            self.set_status("Initialized git repository");
                        }
                        Err(e) => self.set_status(format!("git init failed: {e}")),
                    }
                }
            }
            FindAllReferences => self.find_references(),
            RenameSymbol => self.rename_symbol(),
            GoToDefinition => {
                self.go_to_definition();
                self.focus_pending = true;
            }
            NavigateBack => self.navigate(true),
            NavigateForward => self.navigate(false),
            ShowProblems => self.show_problems = !self.show_problems,
            NextProblem => self.step_problem(true),
            PreviousProblem => self.step_problem(false),
            TriggerSuggest => {
                self.open_completion(None);
                self.focus_pending = true;
            }
            ShowSignatureHelp => {
                if self.lsp_target("Parameter Hints").is_some() {
                    self.request_signature(1, None, false);
                }
                self.focus_pending = true;
            }
            TrustFolder => {
                // Ask again about folders answered with "Not Now".
                self.trust.ask_again();
                self.lsp_failed.clear();
            }
            ToggleLanguageServers => {
                self.lsp_enabled = !self.lsp_enabled;
                if !self.lsp_enabled {
                    self.lsp.clear(); // shuts them down
                    for d in &mut self.docs {
                        d.set_change_log(false);
                    }
                }
                self.lsp_failed.clear();
                crate::cppflags::invalidate();
                self.set_status(if self.lsp_enabled {
                    "Language servers on"
                } else {
                    "Language servers off (word completion only)"
                });
            }
            RevealInExplorer => {
                let path = self
                    .active_doc_index()
                    .and_then(|i| self.docs[i].path.clone());
                if let (Some(p), Some(ws)) = (path, &mut self.workspace) {
                    ws.reveal(&p);
                    self.show_sidebar = true;
                    self.sidebar_view = SidebarView::Explorer;
                }
            }
            OpenNotes => self.open_notes(),
            ToggleMarkdownPreview => self.toggle_preview(self.focused),
            OpenSettings => match Settings::ensure_file() {
                Ok(p) => {
                    if self.open_path(&p, true).is_some() {
                        self.focus_pending = true;
                    }
                }
                Err(e) => self.set_status(format!("Could not create settings file: {e}")),
            },
            Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    // ---------------------------------------------------------------- language servers

    fn server_key(&mut self, doc_idx: usize) -> Option<(ServerKey, &'static str)> {
        let d = &self.docs[doc_idx];
        if let Some(k) = self.doc_server.get(&d.id) {
            return k.clone();
        }
        let key = d.path.as_deref().and_then(|p| {
            let (command, lang) = crate::lsp::server_for(p)?;
            let ws = self.workspace.as_ref().map(|w| w.path.as_path());
            Some((
                ServerKey {
                    command,
                    root: crate::lsp::root_for(p, command, ws),
                },
                lang,
            ))
        });
        self.doc_server.insert(d.id, key.clone());
        key
    }

    /// Starts servers for open Rust/C++ files and sends them the latest edits.
    fn sync_servers(&mut self, ctx: &egui::Context) {
        if !self.lsp_enabled {
            return;
        }
        self.doc_server
            .retain(|id, _| self.docs.iter().any(|d| d.id == *id));
        for i in 0..self.docs.len() {
            self.sync_doc(i, Some(ctx));
        }
        // Tell servers about closed files; stop servers nobody needs (frees their memory).
        let open: HashSet<u64> = self.docs.iter().map(|d| d.id).collect();
        for server in self.lsp.values_mut() {
            let gone: Vec<u64> = server
                .opened
                .keys()
                .copied()
                .filter(|id| !open.contains(id))
                .collect();
            for id in gone {
                server.did_close(id);
            }
        }
        self.lsp.retain(|_, s| !s.opened.is_empty());
    }

    /// Sends one document's edits to its server; starts the server if `ctx` is given.
    fn sync_doc(&mut self, i: usize, ctx: Option<&egui::Context>) {
        let Some((key, lang)) = self.server_key(i) else {
            return;
        };
        if self.lsp_failed.contains(&key) {
            return;
        }
        if !self.lsp.contains_key(&key) {
            let Some(ctx) = ctx else { return };
            // Servers run the project's code (build scripts, macros, cargo
            // config): only in folders the user trusts.
            if !self.trust.is_trusted(&key.root) {
                let folder = self.trust_folder(&key.root);
                if self.pending.is_none()
                    && self.prompt.is_none()
                    && !self.trust.is_declined(&folder)
                {
                    self.pending = Some(Pending::Trust(folder));
                }
                return;
            }
            match Server::start(
                key.clone(),
                ctx.clone(),
                self.workspace.as_ref().map(|w| w.path.clone()),
                self.settings.clangd_background_index,
            ) {
                Ok(s) => {
                    self.lsp.insert(key.clone(), s);
                }
                Err(e) => {
                    self.set_status(format!(
                        "Couldn't start {}: {e}. Using word completion.",
                        key.command
                    ));
                    self.lsp_failed.insert(key);
                    return;
                }
            }
        }
        let server = self.lsp.get_mut(&key).expect("inserted above");
        let doc = &mut self.docs[i];
        let Some(path) = doc.path.clone() else { return };
        if !server.opened.contains_key(&doc.id) {
            doc.set_change_log(true);
            server.did_open(doc.id, &path, lang, doc.rope().to_string());
        } else {
            let changes = doc.take_changes();
            if changes.as_ref().is_none_or(|c| !c.is_empty()) {
                server.did_change(doc.id, changes, || doc.rope().to_string());
            }
        }
    }

    /// The folder to ask about trusting for a server rooted at `root`: the open
    /// workspace when the root is inside it (one question per project).
    pub(super) fn trust_folder(&self, root: &Path) -> PathBuf {
        match &self.workspace {
            Some(w) if root.starts_with(&w.path) => w.path.clone(),
            _ => root.to_path_buf(),
        }
    }

    /// Handles server answers; notices servers that died.
    fn poll_servers(&mut self) {
        let mut dead = Vec::new();
        let mut definitions = Vec::new();
        let mut answers = Vec::new();
        let mut formatted = Vec::new();
        for (key, server) in self.lsp.iter_mut() {
            for ev in server.poll() {
                match ev {
                    crate::lsp::Event::Completion {
                        id,
                        items,
                        incomplete,
                    } => {
                        if self.completion.request.as_ref() == Some(&(key.clone(), id)) {
                            self.completion.request = None;
                            if let Some(d) = self.docs.iter().find(|d| d.id == self.completion.doc)
                            {
                                self.completion.set_server_items(
                                    items,
                                    incomplete,
                                    d.rope(),
                                    server.utf16,
                                );
                            }
                        }
                    }
                    crate::lsp::Event::Diagnostics { path, items } => {
                        let path = path.canonicalize().unwrap_or(path);
                        self.diagnostics_gen += 1;
                        if items.is_empty() {
                            self.diagnostics.remove(&path);
                        } else {
                            self.diagnostics
                                .insert(path, (self.diagnostics_gen, server.utf16, items));
                        }
                    }
                    crate::lsp::Event::Definition { id, locations } => {
                        if self.definition_request.as_ref() == Some(&(key.clone(), id)) {
                            self.definition_request = None;
                            definitions.push((key.clone(), server.utf16, locations));
                        }
                    }
                    crate::lsp::Event::SignatureHelp { id, help } => {
                        if self.signature.request == Some((key.clone(), id)) {
                            self.signature.request = None;
                            // An answer for an older revision or caret is dropped; the next
                            // frame asks again.
                            let current = self.docs.iter().find(|d| d.id == self.signature.doc);
                            let still_there = current.is_some_and(|d| {
                                self.signature.at
                                    == (d.rev(), d.selections(self.signature.view).primary().head)
                            });
                            if still_there {
                                match help {
                                    Some(h) => self.signature.help = Some(h),
                                    None => self.signature.close(),
                                }
                            }
                        }
                    }
                    crate::lsp::Event::Formatting { id, result } => {
                        formatted.push((key.clone(), server.utf16, id, result));
                    }
                    crate::lsp::Event::Hover { id, text } => {
                        if self
                            .hover
                            .asked
                            .as_ref()
                            .is_some_and(|a| a.1 == *key && a.2 == id)
                            && let Some((at, ..)) = self.hover.asked.take()
                        {
                            self.hover.answer = Some((at, text));
                        }
                    }
                    ev @ (crate::lsp::Event::References { .. }
                    | crate::lsp::Event::Rename { .. }) => {
                        let id = match &ev {
                            crate::lsp::Event::References { id, .. }
                            | crate::lsp::Event::Rename { id, .. } => *id,
                            _ => unreachable!(),
                        };
                        if self
                            .lsp_ask
                            .as_ref()
                            .is_some_and(|a| a.0 == *key && a.1 == id)
                            && let Some((_, _, ask)) = self.lsp_ask.take()
                        {
                            answers.push((key.clone(), server.utf16, ev, ask));
                        }
                    }
                }
            }
            if !server.alive {
                dead.push((
                    key.clone(),
                    server.stderr.lock().map(|s| s.clone()).unwrap_or_default(),
                ));
            }
        }
        for (key, err) in dead {
            self.lsp.remove(&key);
            for d in &mut self.docs {
                if self
                    .doc_server
                    .get(&d.id)
                    .and_then(|k| k.as_ref())
                    .is_some_and(|(k, _)| *k == key)
                {
                    d.set_change_log(false);
                }
            }
            let hint = if key.command == "rust-analyzer" && err.contains("Unknown binary") {
                " Install it with: rustup component add rust-analyzer".to_owned()
            } else {
                String::new()
            };
            self.set_status(format!(
                "{} stopped ({}). Using word completion.{hint}",
                key.command,
                err.trim()
            ));
            self.lsp_failed.insert(key);
        }
        for (key, utf16, locations) in definitions {
            match locations.first() {
                Some(loc) => {
                    if locations.len() > 1 {
                        self.set_status(format!(
                            "{} definitions; showing the first",
                            locations.len()
                        ));
                    }
                    self.jump_to(&loc.path.clone(), loc.start, loc.end, utf16, Some(key));
                }
                None => self.set_status("No definition found"),
            }
        }
        for (key, utf16, ev, ask) in answers {
            self.handle_answer(key, utf16, ev, ask);
        }
        for (key, utf16, id, result) in formatted {
            self.lsp_formatted(&key, id, utf16, result);
        }
        self.poll_format();
        self.apply_diagnostics();
    }

    /// Converts new server diagnostics into problems of the open documents.
    fn apply_diagnostics(&mut self) {
        for d in &mut self.docs {
            let Some(path) = &d.path else { continue };
            match self.diagnostics.get(path) {
                Some((generation, utf16, items)) if d.problems_gen != *generation => {
                    let rope = d.rope();
                    let mut problems: Vec<crate::document::Problem> = items
                        .iter()
                        .map(|it| {
                            let start = crate::completion::char_pos(rope, it.start, *utf16);
                            let end = crate::completion::char_pos(rope, it.end, *utf16).max(start);
                            crate::document::Problem {
                                range: start..end,
                                severity: it.severity,
                                message: it.message.clone(),
                            }
                        })
                        .collect();
                    problems.sort_by_key(|p| (p.range.start, p.severity));
                    d.problems = problems;
                    d.problems_gen = *generation;
                }
                None if d.problems_gen != 0 => {
                    d.problems.clear();
                    d.problems_gen = 0;
                }
                _ => {}
            }
        }
    }

    /// Counts of (errors, warnings) across all files.
    fn problem_counts(&self) -> (usize, usize) {
        let mut counts = (0, 0);
        for (_, _, items) in self.diagnostics.values() {
            counts.0 += items.iter().filter(|d| d.severity == 1).count();
            counts.1 += items.iter().filter(|d| d.severity == 2).count();
        }
        counts
    }

    /// Prepares a language server request about the caret: the document's index, its
    /// server and the caret position. Says why in the status bar when it can't.
    fn lsp_target(&mut self, what: &str) -> Option<(usize, ServerKey, LspPos)> {
        let i = self.active_doc_index()?;
        let Some((key, _)) = self.server_key(i) else {
            self.set_status(format!(
                "{what} needs a language server (Rust or C/C++ files)"
            ));
            return None;
        };
        if !self.lsp.get(&key).is_some_and(|s| s.ready) {
            self.set_status(format!("{} isn't ready yet", key.command));
            return None;
        }
        self.sync_doc(i, None);
        let view = self.panes[self.focused].id;
        let doc = &self.docs[i];
        let at = LspPos::of(doc.rope(), doc.selections(view).primary().head);
        Some((i, key, at))
    }

    /// F12 / Ctrl+click: asks the language server where the symbol at the caret is defined.
    fn go_to_definition(&mut self) {
        let Some((i, key, at)) = self.lsp_target("Go to Definition") else {
            return;
        };
        let doc_id = self.docs[i].id;
        let id = self
            .lsp
            .get_mut(&key)
            .and_then(|s| s.definition(doc_id, at));
        self.definition_request = id.map(|id| (key, id));
    }

    /// The pointer rested on a word: asks the language server for its docs.
    /// Silent when there is no server or it isn't ready: hover must never nag.
    fn request_hover(&mut self, target: crate::editor::HoverAt) {
        let Some(i) = self.doc_index(target.doc) else {
            return;
        };
        if self.docs[i].rev() != target.rev {
            return;
        }
        let Some((key, _)) = self.server_key(i) else {
            return;
        };
        if !self.lsp.get(&key).is_some_and(|s| s.ready) {
            return;
        }
        self.sync_doc(i, None);
        let doc = &self.docs[i];
        let at = LspPos::of(doc.rope(), target.word.start);
        let id = self.lsp.get_mut(&key).and_then(|s| s.hover(doc.id, at));
        self.hover.asked = id.map(|id| (target, key, id));
    }

    /// The word at the caret of document `i`.
    fn word_at_caret(&self, i: usize) -> String {
        let doc = &self.docs[i];
        let head = doc.selections(self.panes[self.focused].id).primary().head;
        let range = crate::buffer::word_at(doc.rope(), head);
        doc.rope().slice(range).to_string()
    }

    /// Shift+F12: lists every use of the symbol at the caret.
    fn find_references(&mut self) {
        let Some((i, key, at)) = self.lsp_target("Find All References") else {
            return;
        };
        let word = self.word_at_caret(i);
        let doc_id = self.docs[i].id;
        let id = self
            .lsp
            .get_mut(&key)
            .and_then(|s| s.references(doc_id, at));
        self.lsp_ask = id.map(|id| (key, id, Ask::References(word)));
    }

    /// F2: asks for the new name, then renames the symbol at the caret everywhere.
    fn rename_symbol(&mut self) {
        let Some((i, _, at)) = self.lsp_target("Rename Symbol") else {
            return;
        };
        let value = self.word_at_caret(i);
        if value.trim().is_empty() {
            self.set_status("Put the caret on a symbol to rename it");
            return;
        }
        self.prompt = Some(Prompt {
            kind: PromptKind::RenameSymbol {
                doc: self.docs[i].id,
                at,
                rev: self.docs[i].rev(),
            },
            value,
            just_opened: true,
        });
    }

    /// The prompt was confirmed: sends the rename to the server.
    fn start_rename(&mut self, doc: u64, at: LspPos, rev: u64, name: &str) {
        let Some(i) = self.doc_index(doc) else { return };
        if name.is_empty() {
            self.set_status("Invalid name");
            return;
        }
        if self.docs[i].rev() != rev {
            self.set_status("Document changed; rename cancelled");
            return;
        }
        let Some((key, _)) = self.server_key(i) else {
            return;
        };
        let id = self.lsp.get_mut(&key).and_then(|s| s.rename(doc, at, name));
        self.lsp_ask = id.map(|id| {
            (
                key,
                id,
                Ask::Rename {
                    doc,
                    rev,
                    name: name.to_owned(),
                },
            )
        });
    }

    fn handle_answer(&mut self, key: ServerKey, utf16: bool, ev: crate::lsp::Event, ask: Ask) {
        match (ev, ask) {
            (crate::lsp::Event::References { locations, .. }, Ask::References(word)) => {
                match locations.as_slice() {
                    [] => self.set_status("No references found"),
                    [one] => {
                        self.jump_to(&one.path.clone(), one.start, one.end, utf16, Some(key));
                        self.set_status("1 reference");
                    }
                    _ => {
                        let matches = self.reference_matches(&locations, utf16);
                        let files = matches
                            .iter()
                            .map(|m| &m.path)
                            .collect::<std::collections::HashSet<_>>()
                            .len();
                        let hint = if key.command == "clangd"
                            && !self.settings.clangd_background_index
                        {
                            " (clangd sees open files and their headers; see \"clangd.backgroundIndex\")"
                        } else {
                            ""
                        };
                        self.set_status(format!(
                            "{} references in {files} files{hint}",
                            matches.len()
                        ));
                        let query = Query {
                            text: word,
                            case_sensitive: true,
                            whole_word: true,
                            regex: false,
                        };
                        self.search.query = query.clone();
                        self.search.edited_at = None;
                        self.search.collapsed.clear();
                        self.search.job = Some(SearchJob::finished(query, matches));
                        self.show_sidebar = true;
                        self.sidebar_view = SidebarView::Search;
                    }
                }
            }
            (crate::lsp::Event::Rename { result, .. }, Ask::Rename { doc, rev, name }) => {
                if self
                    .doc_index(doc)
                    .is_none_or(|i| self.docs[i].rev() != rev)
                {
                    self.set_status("Document changed; rename cancelled");
                    return;
                }
                match result {
                    Err(message) => self.set_status(message),
                    Ok(files) if files.is_empty() => self.set_status("Nothing to rename"),
                    Ok(files) => match self.apply_workspace_edit(files, utf16, &key) {
                        Ok((edits, files)) => self.set_status(format!(
                            "Renamed to `{name}`: {edits} edits in {files} files"
                        )),
                        Err(message) => self.set_status(message),
                    },
                }
            }
            _ => {}
        }
    }

    /// Applies a language server's edits to open documents (undoable, left unsaved)
    /// and to closed files (saved right away, without opening a tab). Checks
    /// everything first, so a failure changes nothing. Returns (edits, files).
    fn apply_workspace_edit(
        &mut self,
        files: Vec<crate::lsp::FileEdit>,
        utf16: bool,
        key: &ServerKey,
    ) -> Result<(usize, usize), String> {
        enum Target {
            Open(usize),
            Closed(Document),
        }
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let mut targets = Vec::new();
        for f in &files {
            let path = canon(&f.path);
            let open = self
                .docs
                .iter()
                .position(|d| d.path.as_deref().is_some_and(|p| canon(p) == path));
            targets.push(match open {
                Some(i) => {
                    let known = self
                        .lsp
                        .get(key)
                        .and_then(|s| s.opened.get(&self.docs[i].id))
                        .map(|(_, v)| *v);
                    // The server's positions refer to the text it knows about.
                    if known.is_none() && self.docs[i].is_dirty()
                        || f.version.is_some() && f.version != known
                    {
                        return Err("A file changed meanwhile; rename cancelled".into());
                    }
                    Target::Open(i)
                }
                None => Target::Closed(
                    Document::open(&path)
                        .map_err(|e| format!("Can't read {}: {e}", path.display()))?,
                ),
            });
        }
        let (mut edits, mut failed) = (0, Vec::new());
        for (f, target) in files.iter().zip(targets) {
            match target {
                Target::Open(i) => edits += self.docs[i].apply_lsp_edits(&f.edits, utf16),
                Target::Closed(mut doc) => {
                    edits += doc.apply_lsp_edits(&f.edits, utf16);
                    if let Err(e) = doc.save() {
                        failed.push(format!("{}: {e}", f.path.display()));
                    }
                }
            }
        }
        if let Some(repo) = &mut self.repo {
            repo.refresh();
        }
        if failed.is_empty() {
            Ok((edits, files.len()))
        } else {
            Err(format!("Couldn't save {}", failed.join(", ")))
        }
    }

    /// Find-in-files style hits (with a line preview) for server locations.
    fn reference_matches(
        &self,
        locations: &[crate::lsp::Location],
        utf16: bool,
    ) -> Vec<crate::search::FileMatch> {
        let mut locs: Vec<&crate::lsp::Location> = locations.iter().collect();
        locs.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
        let mut out = Vec::new();
        let mut current: Option<(PathBuf, Vec<String>)> = None;
        for loc in locs {
            if current.as_ref().is_none_or(|(p, _)| *p != loc.path) {
                let open = self
                    .docs
                    .iter()
                    .find(|d| d.path.as_deref() == Some(&loc.path));
                let lines: Vec<String> = match open {
                    Some(d) => d
                        .rope()
                        .lines()
                        .map(|l| l.to_string().trim_end_matches(['\r', '\n']).to_owned())
                        .collect(),
                    None => match std::fs::read_to_string(&loc.path) {
                        Ok(t) => t
                            .split('\n')
                            .map(|l| l.trim_end_matches('\r').to_owned())
                            .collect(),
                        Err(_) => continue,
                    },
                };
                current = Some((loc.path.clone(), lines));
            }
            let Some((_, lines)) = &current else { continue };
            let Some(line) = lines.get(loc.start.0) else {
                continue;
            };
            let byte_of = |col: usize| {
                let n = crate::completion::col_to_char(line.chars(), col, utf16);
                line.char_indices().nth(n).map_or(line.len(), |(b, _)| b)
            };
            let start = byte_of(loc.start.1);
            let end = if loc.end.0 == loc.start.0 {
                byte_of(loc.end.1)
            } else {
                line.len()
            };
            out.push(crate::search::file_match(
                &loc.path,
                loc.start.0,
                line,
                start..end.max(start),
            ));
        }
        out
    }

    /// Where the focused editor is now, for back/forward navigation.
    fn here(&self) -> Option<NavPos> {
        let i = self.active_doc_index()?;
        let d = &self.docs[i];
        Some(NavPos {
            doc: d.id,
            path: d.path.clone(),
            pos: d.selections(self.panes[self.focused].id).primary().head,
        })
    }

    fn remember_here(&mut self) {
        if let Some(h) = self.here() {
            self.nav_back.push(h);
            if self.nav_back.len() > MAX_NAV {
                self.nav_back.remove(0);
            }
            self.nav_forward.clear();
        }
    }

    /// Opens a file at a server range (remembering where we came from). Files opened
    /// this way use the server they were found by, so jumping into a library doesn't
    /// start another language server for it.
    fn jump_to(
        &mut self,
        path: &Path,
        start: (usize, usize),
        end: (usize, usize),
        utf16: bool,
        server: Option<ServerKey>,
    ) {
        self.remember_here();
        let fresh = !self.docs.iter().any(|d| d.path.as_deref() == Some(path));
        let Some(id) = self.open_path(path, true) else {
            return;
        };
        if fresh
            && let Some(key) = server
            && let Some((_, lang)) = crate::lsp::server_for(path)
        {
            self.doc_server.insert(id, Some((key, lang)));
        }
        if let Some((doc, view)) = self.active() {
            let a = crate::completion::char_pos(doc.rope(), start, utf16);
            let b = crate::completion::char_pos(doc.rope(), end, utf16).max(a);
            doc.goto(view, a..b);
        }
    }

    /// Alt+Left / Alt+Right.
    fn navigate(&mut self, back: bool) {
        let target = if back {
            self.nav_back.pop()
        } else {
            self.nav_forward.pop()
        };
        let Some(target) = target else { return };
        if let Some(h) = self.here() {
            if back {
                self.nav_forward.push(h)
            } else {
                self.nav_back.push(h)
            }
        }
        let id = if self.doc_index(target.doc).is_some() {
            self.show_in_focused(target.doc);
            Some(target.doc)
        } else {
            target.path.as_deref().and_then(|p| self.open_path(p, true))
        };
        if id.is_some()
            && let Some((doc, view)) = self.active()
        {
            doc.goto(view, target.pos..target.pos);
        }
    }

    /// F8 / Shift+F8: the next/previous problem in the active file, wrapping around.
    fn step_problem(&mut self, forward: bool) {
        let Some((doc, view)) = self.active() else {
            return;
        };
        let caret = doc.selections(view).primary().head;
        let shown: Vec<&crate::document::Problem> =
            doc.problems.iter().filter(|p| p.severity < 4).collect();
        if shown.is_empty() {
            self.set_status("No problems in this file");
            return;
        }
        let target = if forward {
            shown
                .iter()
                .find(|p| p.range.start > caret)
                .or(shown.first())
        } else {
            shown
                .iter()
                .rev()
                .find(|p| p.range.start < caret)
                .or(shown.last())
        };
        let Some(p) = target.map(|p| (*p).clone()) else {
            return;
        };
        doc.goto(view, p.range.start..p.range.start);
        self.set_status(p.message.lines().next().unwrap_or_default().to_owned());
        self.focus_pending = true;
    }

    /// "rust-analyzer: ready" etc. for the status bar.
    fn active_server_status(&mut self) -> Option<(&'static str, &'static str)> {
        let i = self.active_doc_index()?;
        let (key, _) = self.server_key(i)?;
        let state = match self.lsp.get(&key) {
            _ if !self.lsp_enabled => "off",
            Some(s) if s.ready => "ready",
            Some(_) => "starting…",
            None if self.lsp_failed.contains(&key) => "unavailable",
            None => "off",
        };
        Some((key.command, state))
    }

    fn focused_editor_id(&self) -> Option<egui::Id> {
        let pane = &self.panes[self.focused];
        Some(ui::editor_id(pane.id, pane.active_doc()?))
    }

    fn editor_has_focus(&self, ctx: &egui::Context) -> bool {
        self.focused_editor_id()
            .is_some_and(|id| ctx.memory(|m| m.has_focus(id)))
    }

    /// Opens the completion popup at the caret. `trigger` is a member-access
    /// character just typed (`.`, `:`, `>`), which completes from the caret.
    fn open_completion(&mut self, trigger: Option<char>) {
        let Some((doc, view)) = self.active() else {
            return;
        };
        let caret = doc.selections(view).primary().head;
        let rope = doc.rope();
        let start = if trigger.is_some() {
            caret
        } else {
            word_start(rope, caret)
        };
        let (id, lang) = (doc.id, doc.syntax_token());
        let rope = rope.clone(); // cheap: ropes share their chunks
        self.completion
            .begin(id, view, start, &rope, caret, &lang, trigger.is_none());
        self.request_completion(trigger);
    }

    fn request_completion(&mut self, trigger: Option<char>) {
        let Some(i) = self.active_doc_index() else {
            return;
        };
        let Some((key, _)) = self.server_key(i) else {
            return;
        };
        // The server must have the latest text before we ask.
        self.sync_doc(i, None);
        let view = self.panes[self.focused].id;
        let doc = &self.docs[i];
        let at = LspPos::of(doc.rope(), doc.selections(view).primary().head);
        let id = self
            .lsp
            .get_mut(&key)
            .and_then(|s| s.completion(doc.id, at, trigger));
        self.completion.request = id.map(|id| (key, id));
    }

    /// Reacts to this frame's typing: completion and signature help.
    fn after_typing(&mut self, typed: &[String], editor_focused: bool) {
        self.update_completion(typed, editor_focused);
        self.update_signature(typed.last().and_then(|t| t.chars().last()), editor_focused);
    }

    /// Asks the server for the signature of the call at the primary caret.
    fn request_signature(&mut self, kind: u8, trigger: Option<char>, retrigger: bool) {
        let Some(i) = self.active_doc_index() else {
            return;
        };
        let Some((key, _)) = self.server_key(i) else {
            return;
        };
        if !self.lsp.get(&key).is_some_and(|s| s.ready) {
            return;
        }
        self.sync_doc(i, None);
        let view = self.panes[self.focused].id;
        let doc = &self.docs[i];
        let caret = doc.selections(view).primary().head;
        let at = LspPos::of(doc.rope(), caret);
        let id = self
            .lsp
            .get_mut(&key)
            .and_then(|s| s.signature_help(doc.id, at, kind, trigger, retrigger));
        let sig = &mut self.signature;
        if sig.doc != doc.id || sig.view != view {
            sig.close(); // another editor: its hints don't carry over
        }
        sig.doc = doc.id;
        sig.view = view;
        sig.at = (doc.rev(), caret);
        sig.request = id.map(|id| (key, id));
        sig.active = id.is_some();
    }

    /// Opens, refreshes or closes parameter hints after this frame's typing. Asks
    /// only on trigger characters, or (while open) when the text or caret changed.
    fn update_signature(&mut self, last: Option<char>, editor_focused: bool) {
        let view = self.panes[self.focused].id;
        let Some(i) = self.active_doc_index() else {
            self.signature.close();
            return;
        };
        let doc_id = self.docs[i].id;
        let key = self.server_key(i).map(|(key, _)| key);
        let server = key.as_ref().and_then(|k| self.lsp.get(k));
        let usable = editor_focused
            && self.lsp_enabled
            && server.is_some_and(|s| s.ready && !s.sig_triggers.is_empty());
        let sig = &self.signature;
        if !usable || (sig.active && (sig.doc != doc_id || sig.view != view)) {
            self.signature.close();
            return;
        }
        let (triggers, retriggers) = server
            .map(|s| (s.sig_triggers.clone(), s.sig_retriggers.clone()))
            .unwrap_or_default();
        let doc = &self.docs[i];
        let state = (doc.rev(), doc.selections(view).primary().head);
        if let Some(c) = last
            && triggers.contains(&c)
        {
            let retrigger = self.signature.help.is_some();
            self.request_signature(2, Some(c), retrigger);
        } else if self.signature.help.is_some()
            && self.signature.request.is_none()
            && self.signature.at != state
        {
            match last {
                Some(c) if retriggers.contains(&c) => self.request_signature(2, Some(c), true),
                _ => self.request_signature(3, None, true),
            }
        }
    }

    /// Opens, narrows or closes completion after this frame's typing.
    fn update_completion(&mut self, typed: &[String], editor_focused: bool) {
        let view = self.panes[self.focused].id;
        let Some(i) = self.active_doc_index() else {
            self.completion.close();
            return;
        };
        let doc = &self.docs[i];
        let c = &self.completion;
        if !editor_focused || (c.open && (c.doc != doc.id || c.view != view)) {
            self.completion.close();
            return;
        }
        let rope = doc.rope().clone(); // cheap: ropes share their chunks
        let caret = doc.selections(view).primary().head;
        let last = typed.last().and_then(|t| t.chars().last());
        match last {
            Some(c) if is_ident(c) && !self.completion.open => self.open_completion(None),
            Some(c) if is_ident(c) => {
                if !self.completion.update_prefix(&rope, caret) {
                    self.completion.close();
                } else if self.completion.incomplete && self.completion.request.is_none() {
                    self.request_completion(None);
                }
            }
            Some(c) => {
                let prev = caret.checked_sub(2).map(|p| rope.char(p));
                if is_trigger(c, prev) {
                    self.open_completion(Some(c));
                } else {
                    self.completion.close();
                }
            }
            None if self.completion.open => {
                if !self.completion.update_prefix(&rope, caret) {
                    self.completion.close();
                }
            }
            None => {}
        }
        if self.completion.open && !self.completion.has_items() && self.completion.request.is_none()
        {
            self.completion.close();
        }
    }

    fn accept_completion(&mut self) {
        let view = self.panes[self.focused].id;
        let Some(i) = self.active_doc_index() else {
            return;
        };
        let doc = &mut self.docs[i];
        if let Some(change) = self.completion.accept(doc.rope(), &doc.selections(view)) {
            doc.commit(view, change);
        }
        self.completion.close();
        self.focus_pending = true;
        // Accepting a function usually leaves the caret after `(`: show its parameters.
        let caret = self.docs[i].selections(view).primary().head;
        if caret > 0 && self.docs[i].rope().char(caret - 1) == '(' {
            self.request_signature(1, None, false);
        }
    }

    // ---------------------------------------------------------------- git

    /// Handles finished git jobs and keeps gutter marks current.
    fn update_git(&mut self, ctx: &egui::Context) {
        let Some(repo) = &mut self.repo else { return };
        let mut status_changed = false;
        let mut files_changed = false;
        let mut messages = Vec::new();
        for done in repo.poll() {
            match done {
                Done::Status(Ok(_)) => status_changed = true,
                Done::Status(Err(e)) => messages.push(format!("git status failed: {e}")),
                Done::StatusUnchanged => {}
                Done::Base { path, text } => {
                    self.git_bases.insert(path.clone(), text);
                    for d in self
                        .docs
                        .iter_mut()
                        .filter(|d| d.path.as_ref() == Some(&path))
                    {
                        d.git_marks_rev = None; // recompute
                    }
                }
                Done::Ran { label, result, .. } => match result {
                    Ok(_) => {
                        if label == "Commit" {
                            self.commit_message.clear();
                        }
                        files_changed |= matches!(label.as_str(), "Discard" | "Pull" | "Switch");
                        messages.push(format!("{label}: done"));
                    }
                    Err(e) => messages.push(format!("{label} failed: {e}")),
                },
            }
        }
        // The index may have changed: reload the staged text of open files.
        if status_changed {
            let paths: Vec<PathBuf> = self
                .docs
                .iter()
                .filter_map(|d| d.path.clone())
                .filter(|p| p.starts_with(&repo.root))
                .collect();
            for p in &paths {
                repo.load_base(p);
            }
            self.git_bases.retain(|p, _| paths.contains(p));
        }
        // Files opened since the last status get their base too.
        for d in &self.docs {
            if let Some(p) = &d.path
                && p.starts_with(&repo.root)
                && !self.git_bases.contains_key(p)
            {
                self.git_bases.insert(p.clone(), None);
                repo.load_base(p);
            }
        }
        // Gutter marks follow the text. Diffing a big file costs a few ms, so for
        // those wait until typing pauses and diff on a background thread; small
        // files update right away.
        const PAUSE: Duration = Duration::from_millis(120);
        const BIG: usize = 2000;
        while let Ok((id, rev, marks)) = self.marks_done.1.try_recv() {
            self.marks_pending.remove(&id);
            if let Some(d) = self.docs.iter_mut().find(|d| d.id == id && d.rev() == rev) {
                d.git_marks = marks;
                d.git_marks_rev = Some(rev);
            } // else: edited meanwhile; the loop below starts another diff
        }
        for d in &mut self.docs {
            let rev = d.rev();
            if d.git_marks_rev == Some(rev) || self.marks_pending.contains(&d.id) {
                continue;
            }
            let big = d.rope().len_lines() > BIG;
            if d.git_marks_rev.is_some() && big {
                match d.git_marks_stale_since {
                    Some((r, at)) if r == rev && at.elapsed() >= PAUSE => {}
                    Some((r, at)) if r == rev => {
                        ctx.request_repaint_after(
                            PAUSE.saturating_sub(at.elapsed()) + Duration::from_millis(5),
                        );
                        continue;
                    }
                    _ => {
                        d.git_marks_stale_since = Some((rev, Instant::now()));
                        ctx.request_repaint_after(PAUSE);
                        continue;
                    }
                }
            }
            match d.path.as_ref().and_then(|p| self.git_bases.get(p)) {
                Some(Some(base)) if big => {
                    // Ropes are cheap to clone; the text is copied on the thread.
                    let (base, rope, id) = (base.clone(), d.rope().clone(), d.id);
                    let (tx, ctx) = (self.marks_done.0.clone(), ctx.clone());
                    self.marks_pending.insert(id);
                    std::thread::spawn(move || {
                        let marks = crate::git::line_marks(&base, &rope.to_string());
                        let _ = tx.send((id, rev, marks));
                        ctx.request_repaint();
                    });
                }
                Some(Some(base)) => {
                    d.git_marks_rev = Some(rev);
                    d.git_marks = crate::git::line_marks(base, &d.rope().to_string());
                }
                _ => {
                    d.git_marks_rev = Some(rev);
                    d.git_marks = Vec::new();
                }
            }
        }
        if files_changed {
            self.check_disk();
        }
        if let Some(m) = messages.pop() {
            self.set_status(m);
        }
    }

    /// Shows the changes of the active file against its staged version, as a diff.
    fn open_changes(&mut self, path: &Path) {
        let Some(repo) = &self.repo else { return };
        let rel = repo.rel(path);
        // Prefer the open buffer (it may have unsaved edits) over the file on disk.
        let current = match self.docs.iter().find(|d| d.path.as_deref() == Some(path)) {
            Some(d) => d.rope().to_string(),
            None => std::fs::read_to_string(path).unwrap_or_default(),
        };
        let base = crate::git::git(&repo.root, &["show", &format!(":{rel}")]).unwrap_or_default();
        let text = crate::git::unified(&base, &current, &rel);
        let title = format!(
            "{} (changes)",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        );
        match self
            .docs
            .iter_mut()
            .find(|d| d.virtual_title() == Some(title.as_str()))
        {
            Some(d) => {
                let id = d.id;
                d.reset_text(&text);
                self.show_in_focused(id);
            }
            None => {
                let d = crate::document::Document::virtual_doc(&title, &text, "diff");
                let id = d.id;
                self.docs.push(d);
                self.show_in_focused(id);
            }
        }
    }

    fn commit(&mut self) {
        let msg = self.commit_message.trim().to_owned();
        let Some(repo) = &mut self.repo else { return };
        if msg.is_empty() {
            self.show_sidebar = true;
            self.sidebar_view = SidebarView::SourceControl;
            self.set_status("Type a commit message first");
            return;
        }
        // Like VSCode's "stage all and commit" when nothing is staged.
        let nothing_staged = repo
            .status
            .as_ref()
            .is_some_and(|s| !s.entries.iter().any(|e| e.staged()));
        repo.commit(&msg, nothing_staged);
    }

    fn git_run(&mut self, f: impl FnOnce(&mut Repo)) {
        match &mut self.repo {
            Some(repo) => f(repo),
            None => self.set_status("Not a git repository"),
        }
    }

    fn set_indent(&mut self, indent: ops::Indent) {
        if let Some((doc, _)) = self.active() {
            doc.indent = indent;
        }
    }

    /// Lists the workspace's files for quick open (in the background). The list
    /// only lives while the palette is open, so it never holds memory otherwise.
    fn build_index(&mut self, ctx: &egui::Context) {
        if let Some(ws) = &self.workspace {
            self.file_index = Some(FileIndex::build(&ws.path, ctx.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Selection, Selections};

    /// Big files get their gutter marks from a background diff.
    #[test]
    fn big_files_get_git_marks_in_the_background() {
        let dir = std::env::temp_dir().join(format!("rsedit-bigmarks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| crate::git::git(&dir, args).unwrap();
        g(&["init", "-q"]);
        let file = dir.join("big.txt");
        let text: String = (0..3000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, &text).unwrap();
        g(&["add", "big.txt"]);
        g(&["-c", "user.email=t@example.com", "-c", "user.name=T", "commit", "-qm", "x"]);

        let ctx = egui::Context::default();
        let mut app = EditorApp::empty();
        app.set_workspace(&dir, &ctx);
        let id = app.open_path(&file, true).unwrap();
        let pane = app.panes[app.focused].id;
        let i = app.doc_index(id).unwrap();
        app.docs[i].apply(pane, |r, s, c| ops::type_text(r, s, "changed ", c));
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            app.update_git(&ctx);
            let d = &app.docs[i];
            if d.git_marks_rev == Some(d.rev()) && !d.git_marks.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let d = &app.docs[i];
        assert_eq!(d.git_marks, vec![(0..1, crate::git::Mark::Modified)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Edits left behind by a crashed run come back unsaved, with their path.
    #[test]
    fn crashed_edits_come_back_unsaved() {
        let dir = std::env::temp_dir().join(format!("rsedit-recover-app-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, "on disk\n").unwrap();
        let root = dir.join("state");
        crate::recovery::Recovery::leave_behind(&root, u32::MAX, Some(file.clone()), "edited\n");
        crate::recovery::Recovery::leave_behind(&root, u32::MAX - 1, None, "scratch\n");

        let mut app = EditorApp::empty();
        app.recovery = crate::recovery::Recovery::at(&root);
        assert_eq!(app.recover_unsaved(), 2);
        let doc = app.docs.iter().find(|d| d.path.is_some()).unwrap();
        assert_eq!(doc.rope().to_string(), "edited\n");
        assert_eq!(doc.path.as_deref(), Some(file.canonicalize().unwrap().as_path()));
        assert!(doc.is_dirty());
        let scratch = app.docs.iter().find(|d| d.path.is_none()).unwrap();
        assert_eq!(scratch.rope().to_string(), "scratch\n");
        assert!(scratch.is_dirty());
        // Opening the file again shows the recovered text, not the disk's.
        let id = app.open_path(&file, true).unwrap();
        let shown = app.docs.iter().find(|d| d.id == id).unwrap();
        assert_eq!(shown.rope().to_string(), "edited\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file above the size limit asks before it is read.
    #[test]
    fn large_files_ask_first() {
        let dir = std::env::temp_dir().join(format!("rsedit-large-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.log");
        // Sparse: no actual 64 MiB written.
        std::fs::File::create(&big).unwrap().set_len(LARGE_FILE + 1).unwrap();
        let mut app = EditorApp::empty();
        assert!(app.open_path(&big, true).is_none());
        assert!(app.docs.is_empty(), "not read yet");
        assert!(matches!(app.pending, Some(Pending::OpenLarge { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Opening a file in a folder that isn't trusted asks first and starts no
    /// language server; "Not Now" isn't asked again until requested.
    #[test]
    fn language_servers_wait_for_trust() {
        let dir = std::env::temp_dir().join(format!("rsedit-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = EditorApp::empty();
        app.trust = crate::trust::Trust::default();
        app.set_workspace(&dir, &ctx);
        app.open_path(&dir.join("src/main.rs"), true);
        app.sync_servers(&ctx);
        assert!(app.lsp.is_empty(), "no server before the folder is trusted");
        let Some(Pending::Trust(folder)) = app.pending.clone() else {
            panic!("expected the trust question");
        };
        assert_eq!(folder, app.workspace.as_ref().unwrap().path);

        app.pending = None;
        app.trust.decline(&folder);
        app.sync_servers(&ctx);
        assert!(app.pending.is_none() && app.lsp.is_empty(), "not asked again");

        app.run(Command::TrustFolder, &ctx);
        app.sync_servers(&ctx);
        assert!(matches!(app.pending, Some(Pending::Trust(_))), "asked again on request");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// App with one group per entry, each holding that many new documents.
    /// Returns the app and the document ids per group.
    fn app_with(groups: &[usize]) -> (EditorApp, Vec<Vec<u64>>) {
        let mut app = EditorApp::empty();
        let mut ids = Vec::new();
        for (g, &n) in groups.iter().enumerate() {
            if g > 0 {
                app.panes.push(Pane::new(app.next_pane_id, Vec::new(), 0.0));
                app.next_pane_id += 1;
            }
            app.focused = g;
            for _ in 0..n {
                app.new_file();
            }
            ids.push(app.panes[g].tabs.clone());
        }
        let w = 1.0 / groups.len() as f32;
        app.panes.iter_mut().for_each(|p| p.width = w);
        (app, ids)
    }

    fn layout(app: &EditorApp) -> Vec<Vec<u64>> {
        app.panes.iter().map(|p| p.tabs.clone()).collect()
    }

    fn check_invariants(app: &EditorApp) {
        let total: f32 = app.panes.iter().map(|p| p.width).sum();
        assert!((total - 1.0).abs() < 1e-5, "widths sum to {total}");
        assert!(app.focused < app.panes.len());
        assert!(
            app.panes.iter().all(|p| !p.tabs.is_empty()),
            "no empty groups left behind"
        );
    }

    #[test]
    fn drag_tab_to_right_edge_of_own_group_splits() {
        let (mut app, ids) = app_with(&[2]);
        app.drop_tab(0, 1, 0, DropZone::Right);
        assert_eq!(layout(&app), vec![vec![ids[0][0]], vec![ids[0][1]]]);
        assert_eq!(app.focused, 1);
        check_invariants(&app);
    }

    #[test]
    fn drag_tab_to_left_edge_of_own_group_splits() {
        let (mut app, ids) = app_with(&[2]);
        app.drop_tab(0, 0, 0, DropZone::Left);
        assert_eq!(layout(&app), vec![vec![ids[0][0]], vec![ids[0][1]]]);
        assert_eq!(app.focused, 0);
        check_invariants(&app);
    }

    #[test]
    fn drag_last_tab_into_other_group_removes_source() {
        let (mut app, ids) = app_with(&[1, 2]);
        app.drop_tab(0, 0, 1, DropZone::Center);
        // Lands right after the target's active tab (its last one here).
        assert_eq!(layout(&app), vec![vec![ids[1][0], ids[1][1], ids[0][0]]]);
        assert_eq!(app.panes[0].active_doc(), Some(ids[0][0]));
        assert_eq!(app.focused, 0);
        check_invariants(&app);
    }

    #[test]
    fn drag_last_tab_to_far_edge_of_other_group() {
        let (mut app, ids) = app_with(&[1, 1, 1]);
        app.drop_tab(0, 0, 2, DropZone::Right);
        assert_eq!(
            layout(&app),
            vec![vec![ids[1][0]], vec![ids[2][0]], vec![ids[0][0]]]
        );
        assert_eq!(app.focused, 2);
        check_invariants(&app);
        app.drop_tab(2, 0, 0, DropZone::Left);
        assert_eq!(
            layout(&app),
            vec![vec![ids[0][0]], vec![ids[1][0]], vec![ids[2][0]]]
        );
        assert_eq!(app.focused, 0);
        check_invariants(&app);
    }

    #[test]
    fn no_op_drops_are_rejected() {
        let (app, _) = app_with(&[1, 2]);
        let r = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 300.0));
        let (left, mid, right) = (
            egui::pos2(10.0, 50.0),
            egui::pos2(200.0, 50.0),
            egui::pos2(390.0, 50.0),
        );
        assert!(app.drop_zone(0, 0, r, left).is_none());
        assert!(app.drop_zone(0, 0, r, right).is_none());
        assert!(app.drop_zone(1, 1, r, mid).is_none());
        assert!(app.drop_zone(1, 1, r, right) == Some(DropZone::Right));
        assert!(app.drop_zone(0, 1, r, mid) == Some(DropZone::Center));
        assert!(app.drop_zone(1, 0, r, left) == Some(DropZone::Left));
    }

    #[test]
    fn split_keeps_cursor_and_views_are_cleaned_up() {
        let (mut app, ids) = app_with(&[1]);
        app.run_edit(|d, v| d.apply(v, |r, s, c| ops::type_text(r, s, "hello world", c)));
        app.set_caret(6);
        app.split_right(0, 0);
        let doc = app.doc_index(ids[0][0]).unwrap();
        assert_eq!(
            app.docs[doc].selections(app.panes[1].id).primary().head,
            6,
            "new group starts at the same spot"
        );
        app.close_tab(1, 0);
        assert_eq!(app.panes.len(), 1);
        check_invariants(&app);
    }

    #[test]
    fn join_group_moves_all_tabs() {
        let (mut app, ids) = app_with(&[1, 3]);
        app.join_group(1);
        assert_eq!(app.panes.len(), 1);
        let mut tabs = layout(&app)[0].clone();
        tabs.sort();
        let mut all: Vec<u64> = ids.concat();
        all.sort();
        assert_eq!(tabs, all);
        check_invariants(&app);
    }

    #[test]
    fn find_step_wraps_around() {
        let (mut app, _) = app_with(&[1]);
        app.run_edit(|d, v| d.apply(v, |r, s, c| ops::type_text(r, s, "ab ab ab", c)));
        app.find.query.text = "ab".into();
        app.set_caret(4);
        app.find_step(true, false);
        let sel = |app: &mut EditorApp| {
            app.active()
                .map(|(d, v)| d.selections(v).primary().range())
                .unwrap()
        };
        assert_eq!(sel(&mut app), 6..8);
        app.find_step(true, false);
        assert_eq!(sel(&mut app), 0..2, "wraps to the first match");
        app.find_step(false, false);
        assert_eq!(sel(&mut app), 6..8, "and back");
    }

    /// Drives the whole app headlessly, one frame per call.
    struct Harness {
        ctx: egui::Context,
        app: EditorApp,
    }

    impl Harness {
        fn new(app: EditorApp) -> Self {
            Self {
                ctx: egui::Context::default(),
                app,
            }
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = self.ctx.run_ui(input, |ui| self.app.frame(ui));
            out.textures_delta.clear();
        }

        fn key(&mut self, key: egui::Key, modifiers: egui::Modifiers) {
            let ev = egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            };
            self.frame(vec![ev]);
            self.frame(vec![]);
        }

        fn type_text(&mut self, text: &str) {
            self.frame(
                text.chars()
                    .map(|c| egui::Event::Text(c.to_string()))
                    .collect(),
            );
            self.frame(vec![]);
        }

        fn active_title(&self) -> Option<String> {
            self.app
                .active_doc_index()
                .map(|i| self.app.docs[i].title())
        }

        fn caret(&mut self) -> (usize, std::ops::Range<usize>) {
            let (doc, view) = self.app.active().unwrap();
            let p = doc.selections(view).primary();
            (doc.rope().char_to_line(p.head), p.range())
        }
    }

    const CTRL: egui::Modifiers = egui::Modifiers::COMMAND;
    const CTRL_SHIFT: egui::Modifiers = egui::Modifiers {
        shift: true,
        ..egui::Modifiers::COMMAND
    };

    #[test]
    fn markdown_preview_toggles_per_group_and_only_for_markdown() {
        let dir = std::env::temp_dir().join(format!("rsedit-mdprev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let md = dir.join("notes.md");
        let txt = dir.join("plain.txt");
        std::fs::write(&md, "# Title\n\n- [x] done\n- [ ] todo\n").unwrap();
        std::fs::write(&txt, "just text").unwrap();
        let mut app = EditorApp::empty();
        app.open_path(&txt, true);
        app.open_path(&md, true);
        let mut h = Harness::new(app);
        h.frame(vec![]);

        h.key(egui::Key::V, CTRL_SHIFT);
        assert!(h.app.previewing(0), "Ctrl+Shift+V shows the preview");
        // Typing goes nowhere while previewing: the text stays as it is.
        h.type_text("zzz");
        assert_eq!(
            h.app.docs[h.app.active_doc_index().unwrap()]
                .rope()
                .to_string(),
            "# Title\n\n- [x] done\n- [ ] todo\n"
        );

        // Another group shows the same file as text, the preview stays where it was.
        h.key(egui::Key::Backslash, CTRL);
        assert_eq!(h.app.focused, 1);
        assert!(!h.app.previewing(1));
        assert!(h.app.previewing(0));

        h.app.focused = 0;
        h.key(egui::Key::V, CTRL_SHIFT);
        assert!(!h.app.previewing(0), "toggles back to the text");

        // Not for other files.
        h.key(egui::Key::PageUp, CTRL);
        assert_eq!(h.active_title().as_deref(), Some("plain.txt"));
        h.key(egui::Key::V, CTRL_SHIFT);
        assert!(!h.app.previewing(0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn notes_shortcut_creates_opens_and_never_overwrites_the_notes_file() {
        let dir = std::env::temp_dir().join(format!("rsedit-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let notes = dir.join("sub").join("notes.md");
        let mut app = EditorApp::empty();
        app.settings.notes_file = notes.clone();
        let mut h = Harness::new(app);
        h.frame(vec![]);

        // Default shortcut: creates the file (and its folder) and opens it focused.
        h.key(egui::Key::N, CTRL_SHIFT);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "");
        assert_eq!(h.active_title().as_deref(), Some("notes.md"));
        h.type_text("buy milk");
        h.key(egui::Key::S, CTRL);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "buy milk");

        // Again from another tab: same tab, nothing truncated.
        h.key(egui::Key::N, CTRL);
        assert_eq!(h.active_title().as_deref(), Some("Untitled-1"));
        h.key(egui::Key::N, CTRL_SHIFT);
        assert_eq!(h.active_title().as_deref(), Some("notes.md"));
        let tabs = h.app.panes[h.app.focused].tabs.len();
        assert_eq!(tabs, 2, "the notes tab is reused");
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "buy milk");

        // An existing file elsewhere is opened as it is.
        let other = dir.join("existing.txt");
        std::fs::write(&other, "keep me").unwrap();
        let (mut settings, problems) = crate::settings::Settings::parse(
            &format!(
                r#"{{"notes.file": "{}", "notes.shortcut": "Alt+N"}}"#,
                other.display()
            ),
            &dir,
        );
        assert!(problems.is_empty(), "{problems:?}");
        settings.notes_file = other.clone();
        h.app.apply_settings(settings);
        assert_eq!(Command::OpenNotes.shortcut_text(), "Alt+N");
        h.key(egui::Key::N, egui::Modifiers::ALT);
        assert_eq!(h.active_title().as_deref(), Some("existing.txt"));
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "keep me");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn palette_quick_open_go_to_line_and_commands() {
        let mut app = EditorApp::empty();
        app.set_workspace(
            Path::new(env!("CARGO_MANIFEST_DIR")),
            &egui::Context::default(),
        );
        let mut h = Harness::new(app);
        h.frame(vec![]);
        assert!(
            h.app.file_index.is_none(),
            "no file list until quick open is used"
        );

        // Ctrl+P, type a fuzzy name, Enter: opens the file.
        h.key(egui::Key::P, CTRL);
        assert!(h.app.palette.open);
        for _ in 0..200 {
            if h.app.file_index.as_ref().is_some_and(|i| i.is_done()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        h.type_text("sesrs");
        h.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert!(!h.app.palette.open);
        assert_eq!(h.active_title().as_deref(), Some("session.rs"));
        assert!(
            h.app.file_index.is_none(),
            "the file list is freed when the palette closes"
        );

        // Ctrl+G 12 Enter: caret on line 12.
        h.key(egui::Key::G, CTRL);
        h.type_text("12");
        h.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(h.caret().0, 11);

        // Ctrl+Shift+P "split right" Enter: a second group.
        h.key(egui::Key::P, CTRL_SHIFT);
        h.type_text("split right");
        h.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(h.app.panes.len(), 2);
        assert_eq!(h.active_title().as_deref(), Some("session.rs"));
        assert_eq!(h.caret().0, 11, "the new group starts at the same line");

        // Escape closes the palette without doing anything.
        h.key(egui::Key::P, CTRL);
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!h.app.palette.open);
    }

    #[test]
    fn find_bar_selects_matches() {
        let (app, _) = app_with(&[1]);
        let mut h = Harness::new(app);
        h.frame(vec![]);
        h.type_text("alpha beta\nbeta gamma\nbeta");
        h.app.set_caret(0);
        h.key(egui::Key::F, CTRL);
        assert!(h.app.find.open);
        h.type_text("beta");
        assert_eq!(
            h.caret().1,
            6..10,
            "incremental search selects the first match"
        );
        h.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(h.caret().1, 11..15, "Enter goes to the next one");
        h.key(egui::Key::F3, egui::Modifiers::SHIFT);
        assert_eq!(h.caret().1, 6..10, "Shift+F3 goes back");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!h.app.find.open);
        // Typing goes back into the editor afterwards.
        h.type_text("!");
        let (doc, _) = h.app.active().unwrap();
        assert_eq!(
            doc.rope().to_string(),
            "alpha !\nbeta gamma\nbeta",
            "replaces the selected match"
        );
    }

    /// Waits (up to `secs`) for the completion popup to have suggestions from a server.
    fn wait_for_server_items(h: &mut Harness, secs: u64) -> Vec<String> {
        let until = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < until {
            h.frame(vec![]);
            if h.app.completion.open
                && h.app.completion.request.is_none()
                && h.app.completion.has_items()
            {
                return h.app.completion.labels();
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        h.app.completion.labels()
    }

    #[test]
    #[ignore = "needs clangd installed; run with --ignored"]
    fn clangd_member_completion_end_to_end() {
        let dir = std::env::temp_dir().join(format!("rsedit-clangd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("compile_flags.txt"), "-std=c++17\n").unwrap();
        let file = dir.join("main.cpp");
        std::fs::write(
            &file,
            "struct Point { int xcoord; int ycoord; };\nint main() {\n  Point p;\n  p\n}\n",
        )
        .unwrap();

        let mut app = EditorApp::empty();
        app.set_workspace(&dir, &egui::Context::default());
        let mut h = Harness::new(app);
        h.app.open_path(&file, true);
        h.frame(vec![]);
        let pos = h
            .app
            .active()
            .map(|(d, _)| d.rope().to_string().find("  p\n").unwrap() + 3)
            .unwrap();
        h.app.set_caret(pos);
        h.frame(vec![]);
        h.type_text(".");
        let labels = wait_for_server_items(&mut h, 20);
        assert!(
            labels.iter().any(|l| l == "xcoord"),
            "clangd offered {labels:?}"
        );

        // Narrow it down and accept with Enter.
        h.type_text("yc");
        h.key(egui::Key::Enter, egui::Modifiers::NONE);
        let (doc, _) = h.app.active().unwrap();
        assert!(
            doc.rope().to_string().contains("  p.ycoord\n"),
            "{}",
            doc.rope()
        );
        assert_eq!(h.app.active_server_status(), Some(("clangd", "ready")));
        drop(h);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_workflow_end_to_end() {
        use crate::git::{Mark, git};
        let dir = std::env::temp_dir().join(format!("rsedit-gitapp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| git(&dir, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.com"]);
        g(&["config", "user.name", "T"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "init"]);

        let mut app = EditorApp::empty();
        app.set_workspace(&dir, &egui::Context::default());
        let mut h = Harness::new(app);
        let file = dir.join("a.txt").canonicalize().unwrap();
        h.app.open_path(&file, true);
        let settle = |h: &mut Harness, ok: &dyn Fn(&mut EditorApp) -> bool| {
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                h.frame(vec![]);
                if ok(&mut h.app) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        assert!(settle(&mut h, &|a| a
            .repo
            .as_ref()
            .is_some_and(|r| r.status.is_some())
            && a.git_bases.get(&file).is_some_and(|b| b.is_some())));
        assert_eq!(
            h.app.repo.as_ref().unwrap().status.as_ref().unwrap().branch,
            "main"
        );

        // Edit line 2 in the buffer: a "modified" mark, before even saving.
        let pos = h
            .app
            .active()
            .map(|(d, _)| d.rope().line_to_char(1))
            .unwrap();
        h.app.set_caret(pos);
        h.type_text("2:");
        assert!(settle(&mut h, &|a| a.active().is_some_and(|(d, _)| d
            .git_marks
            == vec![(1..2, Mark::Modified)])));

        // Open Changes shows a diff of the unsaved buffer.
        h.app.run(Command::GitOpenChanges, &h.ctx.clone());
        let (diff, _) = h.app.active().unwrap();
        assert_eq!(diff.title(), "a.txt (changes)");
        assert!(
            diff.rope().to_string().contains("-two\n+2:two"),
            "{}",
            diff.rope()
        );
        h.app.close_active();

        // Save, then commit from the panel (nothing staged: stages all).
        let i = h.app.doc_index(h.app.panes[0].tabs[0]).unwrap();
        assert!(h.app.save(i, false));
        assert!(settle(&mut h, &|a| a
            .repo
            .as_ref()
            .unwrap()
            .status
            .as_ref()
            .is_some_and(|s| s.entries.len() == 1)));
        h.app.commit_message = "change two".into();
        h.app.run(Command::GitCommit, &h.ctx.clone());
        assert!(settle(&mut h, &|a| a
            .repo
            .as_ref()
            .unwrap()
            .status
            .as_ref()
            .is_some_and(|s| s.entries.is_empty())));
        assert!(
            h.app.commit_message.is_empty(),
            "message cleared after a successful commit"
        );
        assert_eq!(g(&["log", "--format=%s", "-1"]).trim(), "change two");
        // The base is now the committed text, so the marks go away.
        assert!(settle(&mut h, &|a| a
            .active()
            .is_some_and(|(d, _)| d.git_marks.is_empty())));
        drop(h);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sidebar_closes_and_reopens() {
        let (app, _) = app_with(&[1]);
        let mut h = Harness::new(app);
        h.frame(vec![]);
        assert!(h.app.show_sidebar && h.app.sidebar_view == SidebarView::Explorer);
        // Clicking the active view's tab hides the sidebar; the strip is drawn instead.
        h.app.toggle_view(SidebarView::Explorer);
        assert!(!h.app.show_sidebar);
        h.frame(vec![]);
        // A strip button (or another tab) shows that view.
        h.app.toggle_view(SidebarView::Search);
        assert!(h.app.show_sidebar && h.app.sidebar_view == SidebarView::Search);
        h.frame(vec![]);
        // Ctrl+B still toggles.
        h.key(egui::Key::B, CTRL);
        assert!(!h.app.show_sidebar);
        h.key(egui::Key::B, CTRL);
        assert!(h.app.show_sidebar);
    }

    /// Every non-ASCII character in UI code must exist in the bundled fonts, or it
    /// shows up as an empty box (we don't load system fonts).
    #[test]
    fn ui_glyphs_exist_in_bundled_fonts() {
        let sources = [
            include_str!("app/ui.rs"),
            include_str!("app.rs"),
            include_str!("explorer.rs"),
            include_str!("palette.rs"),
            include_str!("completion.rs"),
            include_str!("editor.rs"),
            include_str!("commands.rs"),
        ];
        // The window title is drawn by the desktop, not by egui.
        let allowed_elsewhere = ['●'];
        let ctx = egui::Context::default();
        let mut missing = Vec::new();
        let mut out = ctx.run_ui(Default::default(), |ui| {
            for src in sources {
                // Only string literals outside tests matter.
                let code = src.split("#[cfg(test)]").next().unwrap_or(src);
                for lit in code.split('"').skip(1).step_by(2) {
                    for c in lit
                        .chars()
                        .filter(|c| !c.is_ascii() && !allowed_elsewhere.contains(c))
                    {
                        let font = egui::FontId::proportional(14.0);
                        if !ui.fonts_mut(|f| f.has_glyph(&font, c)) && !missing.contains(&c) {
                            missing.push(c);
                        }
                    }
                }
            }
        });
        out.textures_delta.clear();
        assert!(
            missing.is_empty(),
            "glyphs missing from the bundled fonts: {missing:?}"
        );
    }

    #[test]
    #[ignore = "needs clangd installed; run with --ignored"]
    fn clangd_diagnostics_and_definition_end_to_end() {
        let dir = std::env::temp_dir().join(format!("rsedit-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("compile_flags.txt"), "-std=c++17\n").unwrap();
        std::fs::write(
            dir.join("util.hpp"),
            "#pragma once\ninline int helper(int x) { return x * 2; }\n",
        )
        .unwrap();
        let main = dir.join("main.cpp");
        std::fs::write(&main, "#include \"util.hpp\"\nint main() {\n  int y = helper(1);\n  return undefined_name;\n}\n").unwrap();

        let mut app = EditorApp::empty();
        app.set_workspace(&dir, &egui::Context::default());
        let mut h = Harness::new(app);
        let main = main.canonicalize().unwrap();
        h.app.open_path(&main, true);
        let settle = |h: &mut Harness, ok: &dyn Fn(&mut EditorApp) -> bool| {
            let until = Instant::now() + Duration::from_secs(20);
            while Instant::now() < until {
                h.frame(vec![]);
                if ok(&mut h.app) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        };

        // The error arrives and sits exactly on `undefined_name`.
        assert!(settle(&mut h, &|a| a.active().is_some_and(|(d, _)| d
            .problems
            .iter()
            .any(|p| p.severity == 1))));
        let text = h.app.active().map(|(d, _)| d.rope().to_string()).unwrap();
        let (doc, _) = h.app.active().unwrap();
        let err = doc
            .problems
            .iter()
            .find(|p| p.severity == 1)
            .unwrap()
            .clone();
        assert_eq!(&text[err.range.clone()], "undefined_name", "{err:?}");
        assert_eq!(h.app.problem_counts().0, 1);

        // Typing before the error moves the squiggle with the text.
        h.app.set_caret(0);
        h.type_text("// x\n");
        let (doc, _) = h.app.active().unwrap();
        let moved = doc.problems.iter().find(|p| p.severity == 1).unwrap();
        assert_eq!(
            doc.rope().slice(moved.range.clone()).to_string(),
            "undefined_name"
        );

        // F8 jumps to it.
        h.app.set_caret(0);
        h.key(egui::Key::F8, egui::Modifiers::NONE);
        let (line, _) = h.caret();
        assert_eq!(line, 4);

        // F12 on `helper` opens util.hpp at the definition; Alt+Left comes back.
        let call = h
            .app
            .active()
            .map(|(d, _)| d.rope().to_string().find("helper(1)").unwrap() + 2)
            .unwrap();
        h.app.set_caret(call);
        h.key(egui::Key::F12, egui::Modifiers::NONE);
        assert!(
            settle(&mut h, &|a| a
                .active()
                .is_some_and(|(d, _)| d.title() == "util.hpp")),
            "no jump"
        );
        let (doc, view) = h.app.active().unwrap();
        let sel = doc.selections(view).primary();
        assert_eq!(doc.rope().slice(sel.range()).to_string(), "helper");
        let util_id = doc.id;
        let main_key = h
            .app
            .server_key(h.app.doc_index(h.app.panes[0].tabs[0]).unwrap())
            .unwrap()
            .0;
        assert_eq!(
            h.app
                .doc_server
                .get(&util_id)
                .cloned()
                .flatten()
                .map(|k| k.0),
            Some(main_key),
            "same server"
        );
        h.key(egui::Key::ArrowLeft, egui::Modifiers::ALT);
        assert_eq!(h.active_title().as_deref(), Some("main.cpp"));
        assert_eq!(h.caret().1, call..call);
        h.key(egui::Key::ArrowRight, egui::Modifiers::ALT);
        assert_eq!(h.active_title().as_deref(), Some("util.hpp"));
        drop(h);
        std::fs::remove_dir_all(&dir).ok();
    }

    impl EditorApp {
        fn set_caret(&mut self, pos: usize) {
            if let Some((doc, view)) = self.active() {
                doc.set_selections(view, Selections::single(Selection::caret(pos)), true);
            }
        }

        fn run_edit(&mut self, f: impl FnOnce(&mut Document, u64)) {
            let (doc, view) = self.active().unwrap();
            f(doc, view);
        }
    }

    #[test]
    fn shift_alt_f_formats_without_typing_the_letter() {
        let dir = std::env::temp_dir().join(format!("rsedit-fmtkey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let mut app = EditorApp::empty();
        app.settings
            .format_commands
            .insert("txt".into(), vec!["tr".into(), "a-z".into(), "A-Z".into()]);
        app.open_path(&file, true);
        let mut h = Harness::new(app);
        h.frame(vec![]);
        // The window system sends the shortcut's letter as text too.
        let shortcut = egui::Event::Key {
            key: egui::Key::F,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::ALT | egui::Modifiers::SHIFT,
        };
        h.frame(vec![shortcut, egui::Event::Text("F".into())]);
        for _ in 0..200 {
            h.frame(vec![]);
            if h.app.format_jobs.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            h.app.docs[0].rope().to_string(),
            "HELLO\n",
            "formatted, and no stray F"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Whole frames as the window draws them (UI + tessellation + software
/// rasterization) at real window sizes, in a highlighted file.
#[cfg(test)]
mod render_bench {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};

    use super::EditorApp;

    /// Saves a frame of the Markdown preview to `$RSEDIT_SNAPSHOT/markdown.ppm`.
    #[test]
    #[ignore = "writes an image to look at, run with --ignored"]
    fn markdown_preview_snapshot() {
        let Some(out_dir) = std::env::var_os("RSEDIT_SNAPSHOT") else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("rsedit-mdsnap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let md = dir.join("notes.md");
        std::fs::write(
            &md,
            "# Notes\n\nSome *emphasis*, **strong**, `code` and a [link](https://example.com).\n\n\
             ## Todo\n\n- [x] done thing\n- [ ] open thing\n  - nested item\n\n\
             1. first\n2. second\n\n> A quote\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n\
             | Name | Value |\n|------|-------|\n| a    | 1     |\n| b    | 2     |\n\n---\n\nThe end.\n",
        )
        .unwrap();
        let (w, h) = (1000usize, 760usize);
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = EditorApp::empty();
        app.open_path(&md, true);
        app.toggle_preview(0);
        let mut renderer = EguiSoftwareRender::new(ColorFieldOrder::Bgra).with_caching(false);
        let mut buf = vec![[0u8; 4]; w * h];
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(w as f32, h as f32),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.frame(ui));
            let mut textures = std::mem::take(&mut out.textures_delta);
            let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
            buf.fill([0, 0, 0, 255]);
            renderer.render(
                &mut BufferMutRef::new(&mut buf, w, h),
                &prims,
                &textures,
                out.pixels_per_point,
            );
            textures.clear();
        }
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        ppm.extend(buf.iter().flat_map(|p| [p[2], p[1], p[0]]));
        std::fs::write(Path::new(&out_dir).join("markdown.ppm"), ppm).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `RSEDIT_SNAPSHOT=dir` also saves the last frame of each size there as PPM.
    #[test]
    #[ignore = "timing, run with --release --ignored --nocapture"]
    fn software_frame_times() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for (w, h) in [(1200usize, 780usize), (2560, 1440)] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx);
            let mut app = EditorApp::empty();
            app.lsp_enabled = false;
            app.set_workspace(root, &ctx);
            app.open_path(&root.join("src/app/ui.rs"), true);
            app.focus_pending = true;
            let mut renderer = EguiSoftwareRender::new(ColorFieldOrder::Bgra).with_caching(false);
            let mut buf = vec![[0u8; 4]; w * h];
            let mut frame = |app: &mut EditorApp, events: Vec<egui::Event>| {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(w as f32, h as f32),
                    )),
                    events,
                    ..Default::default()
                };
                let t = Instant::now();
                let mut out = ctx.run_ui(input, |ui| app.frame(ui));
                let mut textures = std::mem::take(&mut out.textures_delta);
                let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
                let ui = t.elapsed();
                let t = Instant::now();
                buf.fill([0, 0, 0, 255]);
                renderer.render(
                    &mut BufferMutRef::new(&mut buf, w, h),
                    &prims,
                    &textures,
                    out.pixels_per_point,
                );
                textures.clear();
                (ui, t.elapsed())
            };
            let key = |key| egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            for _ in 0..30 {
                frame(&mut app, vec![]);
                std::thread::sleep(Duration::from_millis(5)); // background highlighting/git
            }
            let mut run =
                |name: &str, app: &mut EditorApp, events: &dyn Fn(usize) -> Vec<egui::Event>| {
                    let (mut ui, mut raster, mut worst) =
                        (Duration::ZERO, Duration::ZERO, Duration::ZERO);
                    let n = 40;
                    for i in 0..n {
                        let (u, r) = frame(app, events(i));
                        ui += u;
                        raster += r;
                        worst = worst.max(u + r);
                    }
                    println!(
                        "{w}x{h} {name}: ui {:?} + raster {:?} per frame, worst total {worst:?}",
                        ui / n as u32,
                        raster / n as u32
                    );
                };
            run("idle", &mut app, &|_| vec![]);
            run("page down", &mut app, &|i| {
                if i % 2 == 0 {
                    vec![key(egui::Key::PageDown)]
                } else {
                    vec![]
                }
            });
            run("typing", &mut app, &|i| {
                vec![egui::Event::Text(
                    ((b'a' + (i % 26) as u8) as char).to_string(),
                )]
            });
            if let Some(dir) = std::env::var_os("RSEDIT_SNAPSHOT") {
                let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
                ppm.extend(buf.iter().flat_map(|p| [p[2], p[1], p[0]]));
                std::fs::write(Path::new(&dir).join(format!("frame-{w}x{h}.ppm")), ppm).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod workspace_edit_tests {
    use super::*;
    use crate::lsp::{FileEdit, Location};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rsedit-wsedit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn key() -> ServerKey {
        ServerKey {
            command: "clangd",
            root: PathBuf::new(),
        }
    }

    #[test]
    fn rename_edits_open_and_closed_files() {
        let d = dir("apply");
        let (a, b) = (d.join("a.rs"), d.join("b.rs"));
        std::fs::write(&a, "foo();\nfoo();\n").unwrap();
        std::fs::write(&b, "foo();\r\nfoo();\r\n").unwrap();
        let mut app = EditorApp::empty();
        app.open_path(&a, true);
        let edit = |path: &Path, lines: &[usize]| FileEdit {
            path: path.to_path_buf(),
            version: None,
            edits: lines
                .iter()
                .map(|l| ((*l, 0), (*l, 3), "bar".to_owned()))
                .collect(),
        };
        let (edits, files) = app
            .apply_workspace_edit(vec![edit(&a, &[0, 1]), edit(&b, &[0, 1])], true, &key())
            .unwrap();
        assert_eq!((edits, files), (4, 2));
        // The open file changed in memory (dirty, undoable); the closed one was saved.
        let doc = app
            .docs
            .iter_mut()
            .find(|x| x.path.as_deref() == Some(a.as_path()))
            .unwrap();
        assert_eq!(doc.rope().to_string(), "bar();\nbar();\n");
        assert!(doc.is_dirty());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "foo();\nfoo();\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "bar();\r\nbar();\r\n");
        assert_eq!(app.docs.len(), 1, "closed files don't open a tab");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rename_with_a_stale_version_changes_nothing() {
        let d = dir("stale");
        let b = d.join("b.rs");
        std::fs::write(&b, "foo();\n").unwrap();
        let mut app = EditorApp::empty();
        let bad = FileEdit {
            path: d.join("missing.rs"),
            version: None,
            edits: vec![((0, 0), (0, 3), "bar".to_owned())],
        };
        let ok = FileEdit {
            path: b.clone(),
            version: None,
            edits: vec![((0, 0), (0, 3), "bar".to_owned())],
        };
        assert!(
            app.apply_workspace_edit(vec![ok, bad], true, &key())
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "foo();\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reference_matches_have_columns_and_previews() {
        let d = dir("refs");
        let (a, b) = (d.join("a.rs"), d.join("b.rs"));
        std::fs::write(&a, "let é😀 = foo(foo);\n").unwrap();
        std::fs::write(&b, "    foo();\n").unwrap();
        let mut app = EditorApp::empty();
        app.open_path(&a, true);
        let loc = |p: &Path, line: usize, c: usize| Location {
            path: p.to_path_buf(),
            start: (line, c),
            end: (line, c + 3),
        };
        // UTF-16 columns: "let é😀 = " is 10 units but 9 chars.
        let m = app.reference_matches(&[loc(&b, 0, 4), loc(&a, 0, 14), loc(&a, 0, 10)], true);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].path, a);
        assert_eq!(m[0].cols, 9..12);
        assert_eq!(m[1].cols, 13..16);
        assert_eq!(m[2].path, b);
        assert_eq!(&m[2].preview[m[2].preview_match.clone()], "foo");
        let _ = std::fs::remove_dir_all(&d);
    }
}
