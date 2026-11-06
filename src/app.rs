//! Application state and actions. Drawing lives in [`ui`].

mod ui;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

use crate::buffer::LspPos;
use crate::commands::Command;
use crate::completion::{Completion, is_ident, is_trigger, word_start};
use crate::lsp::{Server, ServerKey};
use crate::document::{DiskStatus, Document, LineEnding, Query};
use crate::explorer::Node;
use crate::git::{Done, Repo};
use crate::ops;
use crate::palette::{Palette, Pick};
use crate::search::{FileIndex, SearchJob};
use crate::session::{Group, Session};

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
}

/// A text prompt from the explorer (new file/folder, rename).
#[derive(Clone)]
enum PromptKind {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
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
        Self { id, tabs, active: 0, width }
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
    completion: Completion,
    lsp_enabled: bool,
    lsp: HashMap<ServerKey, Server>,
    /// Servers that couldn't start (not installed, crashed): not retried.
    lsp_failed: HashSet<ServerKey>,
    /// Which server each document belongs to (cached: finding the root touches the disk).
    doc_server: HashMap<u64, Option<(ServerKey, &'static str)>>,
    /// The git repository of the open folder.
    repo: Option<Repo>,
    /// Staged text of open files (the gutter compares against it); None = untracked.
    git_bases: HashMap<PathBuf, Option<String>>,
    commit_message: String,
}

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
            completion: Completion::default(),
            lsp_enabled: true,
            lsp: HashMap::new(),
            lsp_failed: HashSet::new(),
            doc_server: HashMap::new(),
            repo: None,
            git_bases: HashMap::new(),
            commit_message: String::new(),
        }
    }

    pub fn new(cc: &eframe::CreationContext<'_>, arg: Option<PathBuf>) -> Self {
        crate::highlight::preload();
        let ctx = cc.egui_ctx.clone();
        let mut app = Self::empty();
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
        ctx.set_theme(if app.dark { egui::Theme::Dark } else { egui::Theme::Light });
        app
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
                    Group { width: p.width, active: p.active.min(tabs.len().saturating_sub(1)), tabs }
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
        self.panes[self.focused].active_doc().and_then(|id| self.doc_index(id))
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
        let shown_elsewhere = |app: &Self| app.panes.iter().enumerate().any(|(i, p)| i != focused && p.tabs.contains(&old));
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
        let Some(&doc) = self.panes[from].tabs.get(tab) else { return };
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
    fn drop_zone(&self, from: usize, to: usize, rect: egui::Rect, pos: egui::Pos2) -> Option<DropZone> {
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
        let Some(&doc) = self.panes[from].tabs.get(tab) else { return };
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
        let Some(to) = from.checked_add_signed(dir) else { return };
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
        let Some(&doc) = self.panes.get(pane).and_then(|p| p.tabs.get(tab)) else { return };
        let other_view = self.panes.iter().enumerate().any(|(i, p)| i != pane && p.tabs.contains(&doc));
        let dirty = self.doc_index(doc).is_some_and(|i| self.docs[i].is_dirty());
        if dirty && !other_view {
            self.pending = Some(Pending::CloseTab { pane: self.panes[pane].id, doc });
        } else {
            self.close_tab(pane, tab);
        }
    }

    /// Closes a tab and drops its document once no group shows it any more.
    fn close_tab(&mut self, pane: usize, tab: usize) {
        let Some(&doc) = self.panes.get(pane).and_then(|p| p.tabs.get(tab)) else { return };
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
                pane.tabs.iter().position(|d| self.doc_index(*d).is_some_and(|i| !self.docs[i].is_dirty())).map(|t| (p, t))
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
        let Some(doc) = self.docs.get_mut(idx) else { return false };
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
                self.set_status(msg);
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

    fn save_all(&mut self) {
        for i in 0..self.docs.len() {
            if self.docs[i].is_dirty() && !self.save(i, false) {
                break;
            }
        }
    }

    /// Reloads files changed on disk (and warns about ones with unsaved edits).
    fn check_disk(&mut self) {
        self.last_disk_check = Instant::now();
        if let Some(repo) = &mut self.repo {
            repo.refresh();
        }
        let mut msgs = Vec::new();
        for d in &mut self.docs {
            match d.check_disk() {
                DiskStatus::Unchanged => {}
                DiskStatus::Reloaded => msgs.push(format!("Reloaded {} (changed on disk)", d.title())),
                DiskStatus::Conflict => {
                    msgs.push(format!("{} changed on disk; saving will overwrite it", d.title()))
                }
            }
        }
        if let Some(m) = msgs.pop() {
            self.set_status(m);
        }
    }

    /// Carries out an explorer prompt (new file/folder, rename).
    fn finish_prompt(&mut self, kind: PromptKind, name: &str, ctx: &egui::Context) {
        let name = name.trim();
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
                std::fs::OpenOptions::new().write(true).create_new(true).open(&p).map(|_| Some(p))
            }
            PromptKind::NewFolder(dir) => std::fs::create_dir_all(dir.join(name)).map(|_| None),
            PromptKind::Rename(old) => {
                let new = old.with_file_name(name);
                std::fs::rename(old, &new).map(|_| {
                    // Keep open documents pointing at the renamed files.
                    for d in &mut self.docs {
                        if let Some(p) = &d.path
                            && let Ok(rest) = p.strip_prefix(old)
                        {
                            d.path = Some(if rest.as_os_str().is_empty() { new.clone() } else { new.join(rest) });
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
        let result = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
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
        let Some((doc, view)) = self.active() else { return };
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
            let from = if from_start_of_selection { p.start() } else { p.end() };
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
        let Some((doc, view)) = self.active() else { return };
        let p = doc.selections(view).primary();
        let is_match = doc.find(&query).is_ok_and(|m| m.binary_search_by_key(&p.start(), |r| r.start).is_ok_and(|i| m[i] == p.range()));
        if is_match {
            doc.replace(view, &query, p.range(), &with);
        }
        self.find_step(true, false);
    }

    fn replace_all(&mut self) {
        let (query, with) = (self.find.query.clone(), self.find.replacement.clone());
        let Some((doc, view)) = self.active() else { return };
        let n = doc.replace_all(view, &query, &with);
        self.set_status(format!("Replaced {n} occurrence{}", if n == 1 { "" } else { "s" }));
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
                    let c = col.unwrap_or(1).saturating_sub(1).min(crate::buffer::line_len(rope, l));
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
                    self.save(i, false);
                }
            }
            SaveAs => {
                if let Some(i) = self.active_doc_index() {
                    self.save(i, true);
                }
            }
            SaveAll => self.save_all(),
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
            MoveLineUp => edit(self, &|d, v| d.run(v, |r, s, _| ops::move_lines(r, s, true))),
            MoveLineDown => edit(self, &|d, v| d.run(v, |r, s, _| ops::move_lines(r, s, false))),
            CopyLineUp => edit(self, &|d, v| d.apply(v, |r, s, _| ops::copy_lines(r, s, true))),
            CopyLineDown => edit(self, &|d, v| d.apply(v, |r, s, _| ops::copy_lines(r, s, false))),
            InsertLineBelow => edit(self, &|d, v| d.apply(v, |r, s, _| ops::insert_line(r, s, false))),
            InsertLineAbove => edit(self, &|d, v| d.apply(v, |r, s, _| ops::insert_line(r, s, true))),
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
            ToggleTheme => {
                self.dark = !self.dark;
                ctx.set_theme(if self.dark { egui::Theme::Dark } else { egui::Theme::Light });
            }
            ToggleLineEnding => {
                if let Some((doc, _)) = self.active() {
                    doc.line_ending = match doc.line_ending {
                        LineEnding::Lf => LineEnding::CrLf,
                        LineEnding::CrLf => LineEnding::Lf,
                    };
                }
            }
            IndentWithSpaces2 => self.set_indent(ops::Indent { tabs: false, width: 2 }),
            IndentWithSpaces4 => self.set_indent(ops::Indent { tabs: false, width: 4 }),
            IndentWithTabs => self.set_indent(ops::Indent { tabs: true, width: 4 }),
            GitShowSourceControl => {
                self.show_sidebar = true;
                self.sidebar_view = SidebarView::SourceControl;
                self.git_run(|r| r.refresh());
            }
            GitOpenChanges => {
                if let Some(p) = self.active_doc_index().and_then(|i| self.docs[i].path.clone()) {
                    self.open_changes(&p);
                }
            }
            GitCommit => self.commit(),
            GitStageAll => self.git_run(|r| r.stage_all()),
            GitUnstageAll => self.git_run(|r| r.unstage_all()),
            GitStageFile => {
                if let Some(p) = self.active_doc_index().and_then(|i| self.docs[i].path.clone()) {
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
            TriggerSuggest => {
                self.open_completion(None);
                self.focus_pending = true;
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
                self.set_status(if self.lsp_enabled { "Language servers on" } else { "Language servers off (word completion only)" });
            }
            RevealInExplorer => {
                let path = self.active_doc_index().and_then(|i| self.docs[i].path.clone());
                if let (Some(p), Some(ws)) = (path, &mut self.workspace) {
                    ws.reveal(&p);
                    self.show_sidebar = true;
                    self.sidebar_view = SidebarView::Explorer;
                }
            }
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
            Some((ServerKey { command, root: crate::lsp::root_for(p, command, ws) }, lang))
        });
        self.doc_server.insert(d.id, key.clone());
        key
    }

    /// Starts servers for open Rust/C++ files and sends them the latest edits.
    fn sync_servers(&mut self, ctx: &egui::Context) {
        if !self.lsp_enabled {
            return;
        }
        self.doc_server.retain(|id, _| self.docs.iter().any(|d| d.id == *id));
        for i in 0..self.docs.len() {
            self.sync_doc(i, Some(ctx));
        }
        // Tell servers about closed files; stop servers nobody needs (frees their memory).
        let open: HashSet<u64> = self.docs.iter().map(|d| d.id).collect();
        for server in self.lsp.values_mut() {
            let gone: Vec<u64> = server.opened.keys().copied().filter(|id| !open.contains(id)).collect();
            for id in gone {
                server.did_close(id);
            }
        }
        self.lsp.retain(|_, s| !s.opened.is_empty());
    }

    /// Sends one document's edits to its server; starts the server if `ctx` is given.
    fn sync_doc(&mut self, i: usize, ctx: Option<&egui::Context>) {
        let Some((key, lang)) = self.server_key(i) else { return };
        if self.lsp_failed.contains(&key) {
            return;
        }
        if !self.lsp.contains_key(&key) {
            let Some(ctx) = ctx else { return };
            match Server::start(key.clone(), ctx.clone()) {
                Ok(s) => {
                    self.lsp.insert(key.clone(), s);
                }
                Err(e) => {
                    self.set_status(format!("Couldn't start {}: {e}. Using word completion.", key.command));
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

    /// Handles server answers; notices servers that died.
    fn poll_servers(&mut self) {
        let mut dead = Vec::new();
        for (key, server) in self.lsp.iter_mut() {
            for ev in server.poll() {
                let crate::lsp::Event::Completion { id, items, incomplete } = ev;
                if self.completion.request.as_ref() == Some(&(key.clone(), id)) {
                    self.completion.request = None;
                    if let Some(d) = self.docs.iter().find(|d| d.id == self.completion.doc) {
                        self.completion.set_server_items(items, incomplete, d.rope(), server.utf16);
                    }
                }
            }
            if !server.alive {
                dead.push((key.clone(), server.stderr.lock().map(|s| s.clone()).unwrap_or_default()));
            }
        }
        for (key, err) in dead {
            self.lsp.remove(&key);
            for d in &mut self.docs {
                if self.doc_server.get(&d.id).and_then(|k| k.as_ref()).is_some_and(|(k, _)| *k == key) {
                    d.set_change_log(false);
                }
            }
            let hint = if key.command == "rust-analyzer" && err.contains("Unknown binary") {
                " Install it with: rustup component add rust-analyzer".to_owned()
            } else {
                String::new()
            };
            self.set_status(format!("{} stopped ({}). Using word completion.{hint}", key.command, err.trim()));
            self.lsp_failed.insert(key);
        }
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
        self.focused_editor_id().is_some_and(|id| ctx.memory(|m| m.has_focus(id)))
    }

    /// Opens the completion popup at the caret. `trigger` is a member-access
    /// character just typed (`.`, `:`, `>`), which completes from the caret.
    fn open_completion(&mut self, trigger: Option<char>) {
        let Some((doc, view)) = self.active() else { return };
        let caret = doc.selections(view).primary().head;
        let rope = doc.rope();
        let start = if trigger.is_some() { caret } else { word_start(rope, caret) };
        let (id, lang) = (doc.id, doc.syntax_token());
        let rope = rope.clone(); // cheap: ropes share their chunks
        self.completion.begin(id, view, start, &rope, caret, &lang, trigger.is_none());
        self.request_completion(trigger);
    }

    fn request_completion(&mut self, trigger: Option<char>) {
        let Some(i) = self.active_doc_index() else { return };
        let Some((key, _)) = self.server_key(i) else { return };
        // The server must have the latest text before we ask.
        self.sync_doc(i, None);
        let view = self.panes[self.focused].id;
        let doc = &self.docs[i];
        let at = LspPos::of(doc.rope(), doc.selections(view).primary().head);
        let id = self.lsp.get_mut(&key).and_then(|s| s.completion(doc.id, at, trigger));
        self.completion.request = id.map(|id| (key, id));
    }

    /// Opens, narrows or closes completion after this frame's typing.
    fn after_typing(&mut self, typed: &[String], editor_focused: bool) {
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
        if self.completion.open && !self.completion.has_items() && self.completion.request.is_none() {
            self.completion.close();
        }
    }

    fn accept_completion(&mut self) {
        let view = self.panes[self.focused].id;
        let Some(i) = self.active_doc_index() else { return };
        let doc = &mut self.docs[i];
        if let Some(change) = self.completion.accept(doc.rope(), &doc.selections(view)) {
            doc.commit(view, change);
        }
        self.completion.close();
        self.focus_pending = true;
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
                    for d in self.docs.iter_mut().filter(|d| d.path.as_ref() == Some(&path)) {
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
            let paths: Vec<PathBuf> = self.docs.iter().filter_map(|d| d.path.clone()).filter(|p| p.starts_with(&repo.root)).collect();
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
        // those wait until typing pauses; small files update right away.
        const PAUSE: Duration = Duration::from_millis(120);
        for d in &mut self.docs {
            let rev = d.rev();
            if d.git_marks_rev == Some(rev) {
                continue;
            }
            if d.git_marks_rev.is_some() && d.rope().len_lines() > 2000 {
                match d.git_marks_stale_since {
                    Some((r, at)) if r == rev && at.elapsed() >= PAUSE => {}
                    Some((r, at)) if r == rev => {
                        ctx.request_repaint_after(PAUSE.saturating_sub(at.elapsed()) + Duration::from_millis(5));
                        continue;
                    }
                    _ => {
                        d.git_marks_stale_since = Some((rev, Instant::now()));
                        ctx.request_repaint_after(PAUSE);
                        continue;
                    }
                }
            }
            d.git_marks_rev = Some(rev);
            d.git_marks = match d.path.as_ref().and_then(|p| self.git_bases.get(p)) {
                Some(Some(base)) => crate::git::line_marks(base, &d.rope().to_string()),
                _ => Vec::new(),
            };
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
        let title = format!("{} (changes)", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
        match self.docs.iter_mut().find(|d| d.virtual_title() == Some(title.as_str())) {
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
        let nothing_staged = repo.status.as_ref().is_some_and(|s| !s.entries.iter().any(|e| e.staged()));
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
        assert!(app.panes.iter().all(|p| !p.tabs.is_empty()), "no empty groups left behind");
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
        assert_eq!(layout(&app), vec![vec![ids[1][0]], vec![ids[2][0]], vec![ids[0][0]]]);
        assert_eq!(app.focused, 2);
        check_invariants(&app);
        app.drop_tab(2, 0, 0, DropZone::Left);
        assert_eq!(layout(&app), vec![vec![ids[0][0]], vec![ids[1][0]], vec![ids[2][0]]]);
        assert_eq!(app.focused, 0);
        check_invariants(&app);
    }

    #[test]
    fn no_op_drops_are_rejected() {
        let (app, _) = app_with(&[1, 2]);
        let r = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 300.0));
        let (left, mid, right) = (egui::pos2(10.0, 50.0), egui::pos2(200.0, 50.0), egui::pos2(390.0, 50.0));
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
        assert_eq!(app.docs[doc].selections(app.panes[1].id).primary().head, 6, "new group starts at the same spot");
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
        let sel = |app: &mut EditorApp| app.active().map(|(d, v)| d.selections(v).primary().range()).unwrap();
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
            Self { ctx: egui::Context::default(), app }
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
                events,
                ..Default::default()
            };
            let mut out = self.ctx.run_ui(input, |ui| self.app.frame(ui));
            out.textures_delta.clear();
        }

        fn key(&mut self, key: egui::Key, modifiers: egui::Modifiers) {
            let ev = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
            self.frame(vec![ev]);
            self.frame(vec![]);
        }

        fn type_text(&mut self, text: &str) {
            self.frame(text.chars().map(|c| egui::Event::Text(c.to_string())).collect());
            self.frame(vec![]);
        }

        fn active_title(&self) -> Option<String> {
            self.app.active_doc_index().map(|i| self.app.docs[i].title())
        }

        fn caret(&mut self) -> (usize, std::ops::Range<usize>) {
            let (doc, view) = self.app.active().unwrap();
            let p = doc.selections(view).primary();
            (doc.rope().char_to_line(p.head), p.range())
        }
    }

    const CTRL: egui::Modifiers = egui::Modifiers::COMMAND;
    const CTRL_SHIFT: egui::Modifiers = egui::Modifiers { shift: true, ..egui::Modifiers::COMMAND };

    #[test]
    fn palette_quick_open_go_to_line_and_commands() {
        let mut app = EditorApp::empty();
        app.set_workspace(Path::new(env!("CARGO_MANIFEST_DIR")), &egui::Context::default());
        let mut h = Harness::new(app);
        h.frame(vec![]);
        assert!(h.app.file_index.is_none(), "no file list until quick open is used");

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
        assert!(h.app.file_index.is_none(), "the file list is freed when the palette closes");

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
        assert_eq!(h.caret().1, 6..10, "incremental search selects the first match");
        h.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(h.caret().1, 11..15, "Enter goes to the next one");
        h.key(egui::Key::F3, egui::Modifiers::SHIFT);
        assert_eq!(h.caret().1, 6..10, "Shift+F3 goes back");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!h.app.find.open);
        // Typing goes back into the editor afterwards.
        h.type_text("!");
        let (doc, _) = h.app.active().unwrap();
        assert_eq!(doc.rope().to_string(), "alpha !\nbeta gamma\nbeta", "replaces the selected match");
    }

    /// Waits (up to `secs`) for the completion popup to have suggestions from a server.
    fn wait_for_server_items(h: &mut Harness, secs: u64) -> Vec<String> {
        let until = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < until {
            h.frame(vec![]);
            if h.app.completion.open && h.app.completion.request.is_none() && h.app.completion.has_items() {
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
        std::fs::write(&file, "struct Point { int xcoord; int ycoord; };\nint main() {\n  Point p;\n  p\n}\n").unwrap();

        let mut app = EditorApp::empty();
        app.set_workspace(&dir, &egui::Context::default());
        let mut h = Harness::new(app);
        h.app.open_path(&file, true);
        h.frame(vec![]);
        let pos = h.app.active().map(|(d, _)| d.rope().to_string().find("  p\n").unwrap() + 3).unwrap();
        h.app.set_caret(pos);
        h.frame(vec![]);
        h.type_text(".");
        let labels = wait_for_server_items(&mut h, 20);
        assert!(labels.iter().any(|l| l == "xcoord"), "clangd offered {labels:?}");

        // Narrow it down and accept with Enter.
        h.type_text("yc");
        h.key(egui::Key::Enter, egui::Modifiers::NONE);
        let (doc, _) = h.app.active().unwrap();
        assert!(doc.rope().to_string().contains("  p.ycoord\n"), "{}", doc.rope());
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
        assert!(settle(&mut h, &|a| a.repo.as_ref().is_some_and(|r| r.status.is_some()) && a.git_bases.get(&file).is_some_and(|b| b.is_some())));
        assert_eq!(h.app.repo.as_ref().unwrap().status.as_ref().unwrap().branch, "main");

        // Edit line 2 in the buffer: a "modified" mark, before even saving.
        let pos = h.app.active().map(|(d, _)| d.rope().line_to_char(1)).unwrap();
        h.app.set_caret(pos);
        h.type_text("2:");
        assert!(settle(&mut h, &|a| a.active().is_some_and(|(d, _)| d.git_marks == vec![(1..2, Mark::Modified)])));

        // Open Changes shows a diff of the unsaved buffer.
        h.app.run(Command::GitOpenChanges, &h.ctx.clone());
        let (diff, _) = h.app.active().unwrap();
        assert_eq!(diff.title(), "a.txt (changes)");
        assert!(diff.rope().to_string().contains("-two\n+2:two"), "{}", diff.rope());
        h.app.close_active();

        // Save, then commit from the panel (nothing staged: stages all).
        let i = h.app.doc_index(h.app.panes[0].tabs[0]).unwrap();
        assert!(h.app.save(i, false));
        assert!(settle(&mut h, &|a| a.repo.as_ref().unwrap().status.as_ref().is_some_and(|s| s.entries.len() == 1)));
        h.app.commit_message = "change two".into();
        h.app.run(Command::GitCommit, &h.ctx.clone());
        assert!(settle(&mut h, &|a| a.repo.as_ref().unwrap().status.as_ref().is_some_and(|s| s.entries.is_empty())));
        assert!(h.app.commit_message.is_empty(), "message cleared after a successful commit");
        assert_eq!(g(&["log", "--format=%s", "-1"]).trim(), "change two");
        // The base is now the committed text, so the marks go away.
        assert!(settle(&mut h, &|a| a.active().is_some_and(|(d, _)| d.git_marks.is_empty())));
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
}

