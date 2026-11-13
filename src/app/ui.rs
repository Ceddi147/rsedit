//! Drawing of the application: menus, sidebar, editor groups, find bar, dialogs.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Color32, CursorIcon, Key, Modifiers, Rect, RichText, Sense, Stroke, pos2, vec2};

use super::{DISK_CHECK_EVERY, DropZone, EditorApp, Pending, Prompt, PromptKind, SidebarView};
use crate::commands::Command;
use crate::editor;
use crate::explorer::Action;
use crate::search::SearchJob;

/// Width of the draggable divider between editor groups.
const DIVIDER: f32 = 5.0;
/// Smallest width a group can be resized to.
const MIN_PANE_WIDTH: f32 = 120.0;
/// Wait this long after typing before re-running find-in-files.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Drag-and-drop payload of a tab being dragged.
struct TabDrag {
    pane: u64,
    doc: u64,
}

enum TabAction {
    Activate(usize),
    Close(usize),
    CloseOthers(usize),
    SplitRight(usize),
    Move(usize, isize),
    CopyPath(usize),
    TogglePreview,
}

/// The id of the editor widget for a document in a group.
pub(super) fn editor_id(pane: u64, doc: u64) -> egui::Id {
    egui::Id::unique(("editor", pane, doc))
}

impl EditorApp {
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Commands with Shift first: matching ignores extra Shift otherwise.
        let mut cmds: Vec<Command> = Command::ALL
            .iter()
            .copied()
            .filter(|c| c.shortcut().is_some())
            .collect();
        cmds.sort_by_key(|c| !c.shortcut().expect("filtered").modifiers.shift);
        for c in cmds {
            let s = c.shortcut().expect("filtered");
            if ctx.input_mut(|i| i.consume_shortcut(&s)) {
                self.run(c, ctx);
            }
        }
        // F3 / Shift+F3 step through find matches.
        if !self.find.query.text.is_empty() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::F3)) {
                self.find_step(false, false);
            } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F3)) {
                self.find_step(true, false);
            }
        }
        if self.find.open && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.find.open = false;
            self.focus_pending = true;
        }
    }

    // ------------------------------------------------------------------ menus

    fn menu_item(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, cmd: Command) {
        let label = cmd.label();
        let label = label.split_once(": ").map_or(label, |(_, l)| l);
        if ui
            .add(egui::Button::new(label).shortcut_text(cmd.shortcut_text()))
            .clicked()
        {
            self.run(cmd, ctx);
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        use Command::*;
        egui::MenuBar::new().ui(ui, |ui| {
            let groups: [(&str, &[Command]); 6] = [
                (
                    "File",
                    &[
                        NewFile,
                        OpenFile,
                        OpenFolder,
                        OpenNotes,
                        Save,
                        SaveAs,
                        SaveAll,
                        ReopenClosedEditor,
                        CloseEditor,
                        CloseAllEditors,
                        OpenSettings,
                        Quit,
                    ],
                ),
                (
                    "Edit",
                    &[
                        Undo,
                        Redo,
                        Cut,
                        Copy,
                        Paste,
                        Find,
                        Replace,
                        FindInFiles,
                        ToggleComment,
                        Indent,
                        Outdent,
                    ],
                ),
                (
                    "Selection",
                    &[
                        SelectAll,
                        SelectLine,
                        AddNextOccurrence,
                        SelectAllOccurrences,
                        CopyLineUp,
                        CopyLineDown,
                        MoveLineUp,
                        MoveLineDown,
                        DeleteLine,
                        InsertLineAbove,
                        InsertLineBelow,
                    ],
                ),
                (
                    "View",
                    &[
                        CommandPalette,
                        ShowExplorer,
                        ShowSearch,
                        ShowProblems,
                        ToggleSidebar,
                        SplitRight,
                        JoinGroup,
                        ZoomIn,
                        ZoomOut,
                        ZoomReset,
                        ToggleTheme,
                    ],
                ),
                (
                    "Go",
                    &[
                        GoToFile,
                        GoToLine,
                        GoToDefinition,
                        NavigateBack,
                        NavigateForward,
                        NextProblem,
                        PreviousProblem,
                        NextTab,
                        PreviousTab,
                        FocusGroup1,
                        FocusGroup2,
                        FocusGroup3,
                        RevealInExplorer,
                    ],
                ),
                (
                    "Git",
                    &[
                        GitShowSourceControl,
                        GitOpenChanges,
                        GitCommit,
                        GitStageFile,
                        GitStageAll,
                        GitUnstageAll,
                        GitPull,
                        GitPush,
                        GitFetch,
                        GitRefresh,
                        GitInit,
                    ],
                ),
            ];
            // Separators go before these.
            let breaks = [
                GoToDefinition,
                NextProblem,
                GitCommit,
                GitPull,
                GitRefresh,
                ReopenClosedEditor,
                OpenSettings,
                Quit,
                Cut,
                Find,
                ToggleComment,
                CopyLineUp,
                DeleteLine,
                ShowExplorer,
                SplitRight,
                ZoomIn,
                NextTab,
                FocusGroup1,
            ];
            for (title, cmds) in groups {
                ui.menu_button(title, |ui| {
                    for &c in cmds {
                        if breaks.contains(&c) && c != cmds[0] {
                            ui.separator();
                        }
                        self.menu_item(ui, ctx, c);
                    }
                });
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            self.branch_menu(ui, ctx);
            self.problem_counts_button(ui);
            let message = self
                .status
                .as_ref()
                .filter(|(_, at)| at.elapsed().as_secs() < 5)
                .map(|(m, _)| m.clone());
            if message.is_some() {
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.status_items(ui, ctx);
                // The message gets whatever room is left, cut off rather than overlapping.
                if let Some(m) = message {
                    ui.add(egui::Label::new(&m).truncate()).on_hover_text(&m);
                }
            });
        });
    }

    fn status_items(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let server = self.active_server_status();
        let Some((doc, view)) = self.active() else {
            return;
        };
        let mut run = None;
        ui.label(doc.language_name());
        if let Some((name, state)) = server {
            ui.separator();
            ui.label(RichText::new(format!("{name}: {state}")).weak());
        }
        ui.separator();
        ui.label("UTF-8");
        ui.separator();
        if ui
            .add(egui::Button::new(doc.line_ending.label()).frame(false))
            .on_hover_text("Change end of line sequence")
            .clicked()
        {
            run = Some(Command::ToggleLineEnding);
        }
        ui.separator();
        let indent = if doc.indent.tabs {
            "Tabs".to_owned()
        } else {
            format!("Spaces: {}", doc.indent.width)
        };
        ui.menu_button(indent, |ui| {
            for c in [
                Command::IndentWithSpaces2,
                Command::IndentWithSpaces4,
                Command::IndentWithTabs,
            ] {
                if ui.button(c.label()).clicked() {
                    run = Some(c);
                }
            }
        });
        ui.separator();
        let sels = doc.selections(view);
        let p = sels.primary();
        let (line, col) = crate::buffer::line_col(doc.rope(), p.head);
        let mut text = format!("Ln {}, Col {}", line + 1, col + 1);
        let selected: usize = sels.iter().map(|s| s.end() - s.start()).sum();
        if sels.len() > 1 {
            text += &format!(" ({} selections)", sels.len());
        } else if selected > 0 {
            text += &format!(" ({selected} selected)");
        }
        if ui
            .add(egui::Button::new(text).frame(false))
            .on_hover_text("Go to line")
            .clicked()
        {
            run = Some(Command::GoToLine);
        }
        if let Some(c) = run {
            self.run(c, ctx);
        }
    }

    /// "2 errors  5 warnings" in the status bar; toggles the Problems panel.
    fn problem_counts_button(&mut self, ui: &mut egui::Ui) {
        if self.lsp.is_empty() && self.diagnostics.is_empty() {
            return;
        }
        let (errors, warnings) = self.problem_counts();
        let weak = ui.visuals().weak_text_color();
        let mut job = egui::text::LayoutJob::default();
        let font = egui::TextStyle::Body.resolve(ui.style());
        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        let ec = if errors > 0 {
            editor::problem_color(1)
        } else {
            weak
        };
        let wc = if warnings > 0 {
            editor::problem_color(2)
        } else {
            weak
        };
        job.append(
            &plural(errors, "error"),
            0.0,
            egui::TextFormat::simple(font.clone(), ec),
        );
        job.append(
            &plural(warnings, "warning"),
            10.0,
            egui::TextFormat::simple(font, wc),
        );
        let hint = format!("Problems ({})", Command::ShowProblems.shortcut_text());
        if ui
            .add(egui::Button::new(job).frame(false))
            .on_hover_text(hint)
            .clicked()
        {
            self.show_problems = !self.show_problems;
        }
        ui.separator();
    }

    /// The Problems panel: every error/warning the language servers reported.
    fn problems_panel(&mut self, ui: &mut egui::Ui) {
        let (errors, warnings) = self.problem_counts();
        ui.horizontal(|ui| {
            ui.strong("Problems");
            ui.label(RichText::new(format!("{errors} errors, {warnings} warnings")).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("×").on_hover_text("Close").clicked() {
                    self.show_problems = false;
                }
            });
        });
        // Errors first, then by file and position.
        let mut rows: Vec<(&PathBuf, bool, &crate::lsp::Diagnostic)> = self
            .diagnostics
            .iter()
            .flat_map(|(p, (_, utf16, items))| {
                items
                    .iter()
                    .filter(|d| d.severity < 4)
                    .map(move |d| (p, *utf16, d))
            })
            .collect();
        rows.sort_by(|a, b| {
            a.2.severity
                .cmp(&b.2.severity)
                .then(a.0.cmp(b.0))
                .then(a.2.start.cmp(&b.2.start))
        });
        if rows.is_empty() {
            ui.label(RichText::new("No problems have been detected.").weak());
            return;
        }
        let root = self.workspace.as_ref().map(|w| w.path.clone());
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        let mut jump = None;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(
            ui,
            row_h,
            rows.len(),
            |ui, range| {
                for (path, utf16, d) in &rows[range] {
                    let shown = root
                        .as_ref()
                        .and_then(|r| path.strip_prefix(r).ok())
                        .unwrap_or(path);
                    let kind = match d.severity {
                        1 => "error",
                        2 => "warning",
                        _ => "info",
                    };
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        kind,
                        0.0,
                        egui::TextFormat::simple(font.clone(), editor::problem_color(d.severity)),
                    );
                    job.append(
                        d.message.lines().next().unwrap_or_default(),
                        10.0,
                        egui::TextFormat::simple(font.clone(), ui.visuals().text_color()),
                    );
                    let place = format!("{}:{}:{}", shown.display(), d.start.0 + 1, d.start.1 + 1);
                    job.append(
                        &place,
                        12.0,
                        egui::TextFormat::simple(font, ui.visuals().weak_text_color()),
                    );
                    let resp = ui
                        .add(
                            egui::Button::new(job)
                                .frame(false)
                                .truncate()
                                .min_size(vec2(ui.available_width(), row_h - 2.0)),
                        )
                        .on_hover_text(&d.message);
                    if resp.clicked() {
                        jump = Some(((*path).clone(), d.start, d.end, *utf16));
                    }
                }
            },
        );
        if let Some((path, start, end, utf16)) = jump {
            self.jump_to(&path, start, end, utf16, None);
        }
    }

    /// Branch name with ahead/behind counts; the menu switches branches and syncs.
    fn branch_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(repo) = &mut self.repo else { return };
        let Some(s) = &repo.status else { return };
        let mut text = s.branch.clone();
        if s.ahead > 0 {
            text += &format!(" ⬆{}", s.ahead);
        }
        if s.behind > 0 {
            text += &format!(" ⬇{}", s.behind);
        }
        if !s.entries.is_empty() {
            text += "*";
        }
        let current = s.branch.clone();
        let mut cmd = None;
        let mut switch = None;
        ui.menu_button(text, |ui| {
            for b in repo.branches() {
                if ui.add(egui::Button::selectable(*b == current, b)).clicked() && *b != current {
                    switch = Some(b.clone());
                }
            }
            ui.separator();
            for c in [
                Command::GitPull,
                Command::GitPush,
                Command::GitFetch,
                Command::GitShowSourceControl,
            ] {
                if ui.button(c.label()).clicked() {
                    cmd = Some(c);
                }
            }
        });
        ui.separator();
        if let Some(b) = switch {
            self.git_run(|r| r.run("Switch", vec!["switch".into(), b], true));
        }
        if let Some(c) = cmd {
            self.run(c, ctx);
        }
    }

    // ------------------------------------------------------------------ sidebar

    /// Shows a sidebar view, or hides the sidebar if that view is already showing
    /// (like VSCode's activity bar).
    pub(super) fn toggle_view(&mut self, view: SidebarView) {
        if self.show_sidebar && self.sidebar_view == view {
            self.show_sidebar = false;
            self.focus_pending = true;
            return;
        }
        self.show_sidebar = true;
        self.sidebar_view = view;
        match view {
            SidebarView::Search => self.search.focus_input = true,
            SidebarView::SourceControl => self.git_run(|r| r.refresh()),
            SidebarView::Explorer => {}
        }
    }

    fn git_change_count(&self) -> usize {
        self.repo
            .as_ref()
            .and_then(|r| r.status.as_ref())
            .map_or(0, |s| s.entries.len())
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut clicked = None;
        ui.horizontal(|ui| {
            let changes = self.git_change_count();
            let git = if changes > 0 {
                format!("Git ({changes})")
            } else {
                "Git".to_owned()
            };
            for (view, label) in [
                (SidebarView::Explorer, "Explorer".to_owned()),
                (SidebarView::Search, "Search".to_owned()),
                (SidebarView::SourceControl, git),
            ] {
                if ui
                    .selectable_label(self.sidebar_view == view, label)
                    .clicked()
                {
                    clicked = Some(view);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hint = format!("Close Sidebar ({})", Command::ToggleSidebar.shortcut_text());
                if ui.small_button("×").on_hover_text(hint).clicked() {
                    self.show_sidebar = false;
                    self.focus_pending = true;
                }
            });
        });
        if let Some(view) = clicked {
            self.toggle_view(view);
            if !self.show_sidebar {
                return;
            }
        }
        ui.separator();
        match self.sidebar_view {
            SidebarView::Explorer => self.explorer(ui, ctx),
            SidebarView::Search => self.search_panel(ui, ctx),
            SidebarView::SourceControl => self.source_control(ui, ctx),
        }
    }

    /// Shown while the sidebar is hidden: one button per sidebar view.
    fn activity_strip(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(4.0);
            let changes = self.git_change_count();
            let git_hint = if changes > 0 {
                format!("Source Control ({changes} changes)")
            } else {
                "Source Control".to_owned()
            };
            let views = [
                (
                    SidebarView::Explorer,
                    "📁",
                    format!("Explorer ({})", Command::ToggleSidebar.shortcut_text()),
                ),
                (
                    SidebarView::Search,
                    "🔍",
                    format!("Search ({})", Command::FindInFiles.shortcut_text()),
                ),
                (
                    SidebarView::SourceControl,
                    "🔀",
                    format!(
                        "{git_hint} ({})",
                        Command::GitShowSourceControl.shortcut_text()
                    ),
                ),
            ];
            for (view, icon, hint) in views {
                let resp = ui
                    .add(egui::Button::new(RichText::new(icon).size(16.0)).frame(false))
                    .on_hover_text(hint);
                if view == SidebarView::SourceControl && changes > 0 {
                    // A small dot when there are uncommitted changes.
                    ui.painter().circle_filled(
                        resp.rect.right_top() + vec2(-3.0, 3.0),
                        3.0,
                        status_color('M'),
                    );
                }
                if resp.clicked() {
                    self.toggle_view(view);
                }
                ui.add_space(6.0);
            }
        });
    }

    /// The Source Control view: commit box, staged and unstaged changes.
    fn source_control(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(repo) = &self.repo else {
            ui.label("The open folder isn't a git repository.");
            if self.workspace.is_some() && ui.button("Initialize Repository").clicked() {
                self.run(Command::GitInit, ctx);
            }
            return;
        };
        let Some(status) = repo.status.clone() else {
            ui.label(RichText::new("Reading status…").weak());
            return;
        };
        let busy = repo.busy.clone();
        let mut cmd = None;
        ui.horizontal(|ui| {
            ui.strong(format!("Branch: {}", status.branch));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    cmd = Some(Command::GitRefresh);
                }
                if ui.small_button("⬆").on_hover_text("Push").clicked() {
                    cmd = Some(Command::GitPush);
                }
                if ui.small_button("⬇").on_hover_text("Pull").clicked() {
                    cmd = Some(Command::GitPull);
                }
                if !busy.is_empty() {
                    ui.spinner().on_hover_text(busy.join(", "));
                }
            });
        });
        let staged: Vec<&crate::git::Entry> =
            status.entries.iter().filter(|e| e.staged()).collect();
        let unstaged: Vec<&crate::git::Entry> =
            status.entries.iter().filter(|e| e.unstaged()).collect();

        let edit = ui.add(
            egui::TextEdit::multiline(&mut self.commit_message)
                .hint_text("Message (Ctrl+Enter to commit)")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        if edit.has_focus() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
            cmd = Some(Command::GitCommit);
        }
        let label = if staged.is_empty() && !unstaged.is_empty() {
            "✔ Commit All"
        } else {
            "✔ Commit"
        };
        let can_commit = !self.commit_message.trim().is_empty() && !status.entries.is_empty();
        if ui
            .add_enabled(
                can_commit,
                egui::Button::new(label).min_size(vec2(ui.available_width(), 0.0)),
            )
            .clicked()
        {
            cmd = Some(Command::GitCommit);
        }
        ui.add_space(4.0);

        enum Act {
            Open(PathBuf),
            Changes(PathBuf),
            Stage(PathBuf),
            Unstage(PathBuf),
            Discard(crate::git::Entry),
        }
        let mut act = None;
        let root = repo.root.clone();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                let mut section = |ui: &mut egui::Ui,
                                   title: &str,
                                   list: &[&crate::git::Entry],
                                   staged: bool| {
                    if list.is_empty() {
                        return;
                    }
                    ui.horizontal(|ui| {
                        ui.strong(format!("{title} ({})", list.len()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if staged {
                                if ui.small_button("−").on_hover_text("Unstage All").clicked() {
                                    cmd = Some(Command::GitUnstageAll);
                                }
                            } else if ui.small_button("+").on_hover_text("Stage All").clicked() {
                                cmd = Some(Command::GitStageAll);
                            }
                        });
                    });
                    for e in list {
                        let rel = e.path.strip_prefix(&root).unwrap_or(&e.path);
                        let name = rel
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let dir = rel
                            .parent()
                            .map(|d| d.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let letter = if staged { e.index } else { e.letter() };
                        let color = status_color(letter);
                        ui.horizontal(|ui| {
                            let mut job = egui::text::LayoutJob::default();
                            let body = egui::TextStyle::Body.resolve(ui.style());
                            let small = egui::TextStyle::Small.resolve(ui.style());
                            job.append(&name, 0.0, egui::TextFormat::simple(body, color));
                            job.append(
                                &format!("  {dir}"),
                                0.0,
                                egui::TextFormat::simple(small, ui.visuals().weak_text_color()),
                            );
                            let resp = ui
                                .add(egui::Button::new(job).frame(false).truncate())
                                .on_hover_text(format!(
                                    "{} — click to see the changes",
                                    rel.display()
                                ));
                            if resp.clicked() {
                                act = Some(if e.untracked {
                                    Act::Open(e.path.clone())
                                } else {
                                    Act::Changes(e.path.clone())
                                });
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Open File").clicked() {
                                    act = Some(Act::Open(e.path.clone()));
                                }
                                if !e.untracked && ui.button("Open Changes").clicked() {
                                    act = Some(Act::Changes(e.path.clone()));
                                }
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        RichText::new(letter.to_string()).color(color).monospace(),
                                    );
                                    if staged {
                                        if ui.small_button("−").on_hover_text("Unstage").clicked()
                                        {
                                            act = Some(Act::Unstage(e.path.clone()));
                                        }
                                    } else {
                                        if ui.small_button("+").on_hover_text("Stage").clicked() {
                                            act = Some(Act::Stage(e.path.clone()));
                                        }
                                        if ui
                                            .small_button("⟲")
                                            .on_hover_text("Discard Changes")
                                            .clicked()
                                        {
                                            act = Some(Act::Discard((*e).clone()));
                                        }
                                    }
                                },
                            );
                        });
                    }
                    ui.add_space(6.0);
                };
                section(ui, "Staged Changes", &staged, true);
                section(ui, "Changes", &unstaged, false);
                if status.entries.is_empty() {
                    ui.label(RichText::new("No changes.").weak());
                }
            });

        match act {
            Some(Act::Open(p)) => {
                self.open_path(&p, false);
            }
            Some(Act::Changes(p)) => self.open_changes(&p),
            Some(Act::Stage(p)) => self.git_run(|r| r.stage(&[p])),
            Some(Act::Unstage(p)) => self.git_run(|r| r.unstage(&[p])),
            Some(Act::Discard(e)) => self.pending = Some(Pending::Discard(e)),
            None => {}
        }
        if let Some(c) = cmd {
            self.run(c, ctx);
        }
    }

    fn explorer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let active = self
            .active_doc_index()
            .and_then(|i| self.docs[i].path.clone());
        let mut action = None;
        let mut refresh = false;
        match &mut self.workspace {
            Some(root) => {
                ui.horizontal(|ui| {
                    ui.strong(root.name.to_uppercase());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                            refresh = true;
                        }
                        if ui.small_button("🗀+").on_hover_text("New Folder…").clicked() {
                            action = Some(Action::NewFolder(root.path.clone()));
                        }
                        if ui.small_button("🗋+").on_hover_text("New File…").clicked() {
                            action = Some(Action::NewFile(root.path.clone()));
                        }
                    });
                });
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let status = self.repo.as_ref().and_then(|r| r.status.as_ref());
                        let deco =
                            |path: &std::path::Path, is_dir: bool| -> Option<(String, Color32)> {
                                let s = status?;
                                if is_dir {
                                    return s.dirty_dirs.contains(path).then(|| {
                                        ("•".to_owned(), status_color('M').gamma_multiply(0.8))
                                    });
                                }
                                let e = s.entry(path)?;
                                Some((e.letter().to_string(), status_color(e.letter())))
                            };
                        if let Some(a) = root.ui(ui, active.as_deref(), 0, &deco) {
                            action = Some(a);
                        }
                    });
            }
            None => {
                ui.label("No folder open.");
                if ui.button("Open Folder…").clicked() {
                    self.run(Command::OpenFolder, ctx);
                }
            }
        }
        if refresh {
            self.refresh_workspace(ctx);
        }
        match action {
            Some(Action::Open(p, new_tab)) => {
                self.open_path(&p, new_tab);
            }
            Some(Action::NewFile(dir)) => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::NewFile(dir),
                    value: String::new(),
                    just_opened: true,
                })
            }
            Some(Action::NewFolder(dir)) => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::NewFolder(dir),
                    value: String::new(),
                    just_opened: true,
                })
            }
            Some(Action::Rename(p)) => {
                let value = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.prompt = Some(Prompt {
                    kind: PromptKind::Rename(p),
                    value,
                    just_opened: true,
                });
            }
            Some(Action::Delete(p)) => self.pending = Some(Pending::Delete(p)),
            Some(Action::CopyPath(p)) => ctx.copy_text(p.display().to_string()),
            None => {}
        }
    }

    fn search_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(root) = self.workspace.as_ref().map(|w| w.path.clone()) else {
            ui.label("Open a folder to search in it.");
            return;
        };
        let q = &mut self.search.query;
        let edit = ui.add(
            egui::TextEdit::singleline(&mut q.text)
                .hint_text("Search")
                .desired_width(f32::INFINITY),
        );
        if std::mem::take(&mut self.search.focus_input) {
            edit.request_focus();
        }
        let mut changed = edit.changed();
        ui.horizontal(|ui| {
            changed |= ui
                .toggle_value(&mut q.case_sensitive, "Aa")
                .on_hover_text("Match Case")
                .changed();
            changed |= ui
                .toggle_value(&mut q.whole_word, "ab")
                .on_hover_text("Match Whole Word")
                .changed();
            changed |= ui
                .toggle_value(&mut q.regex, ".*")
                .on_hover_text("Use Regular Expression")
                .changed();
        });
        if changed {
            self.search.edited_at = Some(Instant::now());
        }
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        // Search as you type, after a short pause.
        if let Some(at) = self.search.edited_at {
            let due = at.elapsed() >= SEARCH_DEBOUNCE;
            if due || enter {
                self.search.edited_at = None;
                self.search.job = Some(SearchJob::start(
                    &root,
                    self.search.query.clone(),
                    ctx.clone(),
                ));
            } else {
                ctx.request_repaint_after(SEARCH_DEBOUNCE);
            }
        }
        ui.separator();

        let Some(job) = &self.search.job else { return };
        let mut open = None;
        let r = job.results.lock().expect("results lock");
        if let Some(e) = &r.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return;
        }
        let summary = match (r.matches.len(), r.done) {
            (0, true) if !job.query.text.is_empty() => "No results".to_owned(),
            (0, true) => String::new(),
            (n, done) => format!(
                "{n}{} result{} in {} file{}{}",
                if r.truncated { "+" } else { "" },
                if n == 1 { "" } else { "s" },
                r.files_with_matches,
                if r.files_with_matches == 1 { "" } else { "s" },
                if done { "" } else { " — searching…" }
            ),
        };
        ui.label(RichText::new(summary).weak());

        // Flatten into rows (file headers + matches) so only visible rows are drawn.
        enum Row {
            File(PathBuf, usize),
            Match(usize),
        }
        let mut rows = Vec::new();
        let mut i = 0;
        while i < r.matches.len() {
            let path = &r.matches[i].path;
            let n = r.matches[i..]
                .iter()
                .take_while(|m| &m.path == path)
                .count();
            rows.push(Row::File(path.clone(), n));
            if !self.search.collapsed.contains(path) {
                rows.extend((i..i + n).map(Row::Match));
            }
            i += n;
        }
        let row_h =
            ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y + 2.0;
        let mut toggle = None;
        egui::ScrollArea::both().auto_shrink(false).show_rows(
            ui,
            row_h,
            rows.len(),
            |ui, range| {
                for row in &rows[range] {
                    match row {
                        Row::File(p, n) => {
                            let rel = p.strip_prefix(&root).unwrap_or(p).display().to_string();
                            let open = !self.search.collapsed.contains(p);
                            let resp = ui.add(
                                egui::Button::new(
                                    RichText::new(format!("    {rel}  ({n})")).strong(),
                                )
                                .frame(false)
                                .min_size(vec2(ui.available_width(), row_h - 2.0)),
                            );
                            let c = pos2(resp.rect.left() + 8.0, resp.rect.center().y);
                            crate::explorer::chevron(
                                ui.painter(),
                                c,
                                open,
                                ui.visuals().text_color(),
                            );
                            if resp.clicked() {
                                toggle = Some(p.clone());
                            }
                        }
                        Row::Match(i) => {
                            let m = &r.matches[*i];
                            let mut job = egui::text::LayoutJob::default();
                            let style = ui.style();
                            let font = egui::TextStyle::Monospace.resolve(style);
                            let weak = ui.visuals().weak_text_color();
                            let normal = ui.visuals().text_color();
                            job.append(
                                &format!("{:>5}  ", m.line + 1),
                                0.0,
                                egui::TextFormat::simple(font.clone(), weak),
                            );
                            job.append(
                                &m.preview[..m.preview_match.start],
                                0.0,
                                egui::TextFormat::simple(font.clone(), normal),
                            );
                            job.append(
                                &m.preview[m.preview_match.clone()],
                                0.0,
                                egui::TextFormat {
                                    background: Color32::from_rgba_unmultiplied(234, 92, 0, 90),
                                    ..egui::TextFormat::simple(font.clone(), normal)
                                },
                            );
                            job.append(
                                &m.preview[m.preview_match.end..],
                                0.0,
                                egui::TextFormat::simple(font, normal),
                            );
                            let resp = ui.add(
                                egui::Button::new(job)
                                    .frame(false)
                                    .min_size(vec2(ui.available_width(), row_h - 2.0)),
                            );
                            if resp.clicked() {
                                open = Some((m.path.clone(), m.line, m.cols.clone()));
                            }
                        }
                    }
                }
            },
        );
        drop(r);
        if let Some(p) = toggle
            && !self.search.collapsed.remove(&p)
        {
            self.search.collapsed.insert(p);
        }
        if let Some((path, line, cols)) = open {
            self.open_at(&path, line, cols);
        }
    }

    // ------------------------------------------------------------------ editor groups

    /// All editor groups side by side, with draggable dividers between them.
    /// Returns the screen rect of each group.
    fn editor_area(&mut self, ui: &mut egui::Ui) -> Vec<Rect> {
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());
        let n = self.panes.len();
        let content_w = (area.width() - DIVIDER * (n - 1) as f32).max(1.0);

        let mut action = None;
        let mut rects = Vec::with_capacity(n);
        let mut x = area.left();
        for i in 0..n {
            let w = if i + 1 == n {
                area.right() - x
            } else {
                self.panes[i].width * content_w
            };
            let rect = Rect::from_min_max(pos2(x, area.top()), pos2(x + w, area.bottom()));
            rects.push(rect);
            if let Some(a) = self.pane_ui(ui, i, rect) {
                action = Some((i, a));
            }
            x += w;

            if i + 1 < n {
                let divider =
                    Rect::from_min_max(pos2(x, area.top()), pos2(x + DIVIDER, area.bottom()));
                let resp = ui.interact(
                    divider,
                    ui.scope_id().with(("divider", self.panes[i].id)),
                    Sense::drag(),
                );
                let hot = resp.hovered() || resp.dragged();
                if hot {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                }
                if resp.dragged() {
                    let pair = self.panes[i].width + self.panes[i + 1].width;
                    let min = (MIN_PANE_WIDTH / content_w).min(pair / 2.0);
                    let left = (self.panes[i].width + resp.drag_delta().x / content_w)
                        .max(min)
                        .min(pair - min);
                    self.panes[i].width = left;
                    self.panes[i + 1].width = pair - left;
                }
                let visuals = ui.visuals();
                let stroke = if hot {
                    Stroke::new(2.0, visuals.selection.bg_fill)
                } else {
                    visuals.widgets.noninteractive.bg_stroke
                };
                ui.painter()
                    .vline(divider.center().x, area.y_range(), stroke);
                x += DIVIDER;
            }
        }

        self.tab_drop(ui, &rects);

        if let Some((i, action)) = action {
            match action {
                TabAction::Activate(t) => {
                    self.panes[i].active = t;
                    self.focus_pane(i);
                }
                TabAction::Close(t) => self.request_close(i, t),
                TabAction::CloseOthers(t) => {
                    let keep = self.panes[i].tabs[t];
                    let mut k = 0;
                    while k < self.panes[i].tabs.len() {
                        let doc = self.panes[i].tabs[k];
                        let dirty = self.doc_index(doc).is_some_and(|d| self.docs[d].is_dirty());
                        if doc == keep || dirty {
                            k += 1;
                        } else {
                            self.close_tab(i, k);
                        }
                    }
                }
                TabAction::SplitRight(t) => self.split_right(i, t),
                TabAction::TogglePreview => self.toggle_preview(i),
                TabAction::Move(t, dir) => self.move_tab(i, t, dir),
                TabAction::CopyPath(t) => {
                    if let Some(p) = self
                        .doc_index(self.panes[i].tabs[t])
                        .and_then(|d| self.docs[d].path.clone())
                    {
                        ui.ctx().copy_text(p.display().to_string());
                    }
                }
            }
        }
        rects
    }

    /// While a tab is dragged: shows where it would land, and moves it there on release.
    fn tab_drop(&mut self, ui: &egui::Ui, rects: &[Rect]) {
        let ctx = ui.ctx();
        let Some(drag) = egui::DragAndDrop::payload::<TabDrag>(ctx) else {
            return;
        };
        let from = self.panes.iter().position(|p| p.id == drag.pane);
        let tab = from.and_then(|f| self.panes[f].tabs.iter().position(|&d| d == drag.doc));
        let (Some(from), Some(tab)) = (from, tab) else {
            egui::DragAndDrop::clear_payload(ctx);
            return;
        };
        let Some(pos) = ctx.pointer_interact_pos() else {
            return;
        };
        ctx.set_cursor_icon(CursorIcon::Grabbing);

        if let Some(i) = self.doc_index(drag.doc) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::unique("tab-drag"),
            ));
            let galley = painter.layout_no_wrap(
                self.docs[i].title(),
                egui::TextStyle::Button.resolve(ui.style()),
                ui.visuals().strong_text_color(),
            );
            let label =
                Rect::from_min_size(pos + vec2(12.0, 8.0), galley.size()).expand2(vec2(6.0, 3.0));
            painter.rect(
                label,
                3.0,
                ui.visuals().window_fill,
                ui.visuals().window_stroke,
                egui::StrokeKind::Outside,
            );
            painter.galley(label.min + vec2(6.0, 3.0), galley, Color32::PLACEHOLDER);
        }

        let target = rects.iter().position(|r| r.contains(pos));
        let zone = target.and_then(|to| Some((to, self.drop_zone(from, to, rects[to], pos)?)));
        if let Some((to, zone)) = zone {
            let r = rects[to];
            let highlight = match zone {
                DropZone::Left => Rect::from_min_max(r.min, pos2(r.center().x, r.max.y)),
                DropZone::Right => Rect::from_min_max(pos2(r.center().x, r.min.y), r.max),
                DropZone::Center => r,
            };
            let color = ui.visuals().selection.bg_fill;
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("tab-drop"),
            ));
            painter.rect(
                highlight.shrink(2.0),
                4.0,
                color.gamma_multiply(0.25),
                Stroke::new(2.0, color),
                egui::StrokeKind::Inside,
            );
        }

        if ctx.input(|i| i.pointer.any_released()) {
            egui::DragAndDrop::clear_payload(ctx);
            if let Some((to, zone)) = zone {
                self.drop_tab(from, tab, to, zone);
            }
        }
    }

    /// One editor group: its tab bar plus the active document's editor.
    fn pane_ui(&mut self, ui: &mut egui::Ui, idx: usize, rect: Rect) -> Option<TabAction> {
        let pane_id = self.panes[idx].id;
        let mut ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .id_salt(("pane", pane_id)),
        );
        ui.set_clip_rect(rect.intersect(ui.clip_rect()));
        let is_focused = idx == self.focused;

        let mut action = None;
        let previewing = self.previewing(idx);
        if !self.panes[idx].tabs.is_empty() {
            let markdown = self.panes[idx]
                .active_doc()
                .and_then(|d| self.doc_index(d))
                .is_some_and(|d| Self::is_markdown(&self.docs[d]));
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(6, 4))
                .show(&mut ui, |ui| {
                    // One row: the tabs take what the button on the right leaves.
                    let row = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
                    let rtl = egui::Layout::right_to_left(egui::Align::Center);
                    ui.allocate_ui_with_layout(row, rtl, |ui| {
                        if markdown {
                            let hint = format!(
                                "{} ({})",
                                if previewing { "Edit the text" } else { "Show rendered Markdown" },
                                Command::ToggleMarkdownPreview.shortcut_text()
                            );
                            if ui
                                .selectable_label(previewing, "Preview")
                                .on_hover_text(hint)
                                .clicked()
                            {
                                action = Some(TabAction::TogglePreview);
                            }
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            if let Some(a) = self.tab_bar(ui, idx) {
                                action = Some(a);
                            }
                        });
                    });
                });
            ui.separator();
        }

        match self.panes[idx].active_doc().and_then(|d| self.doc_index(d)) {
            Some(di) if previewing => {
                if is_focused {
                    self.focus_pending = false; // nothing to type into here
                }
                let doc = &self.docs[di];
                let text = doc.rope().to_string();
                let resp = egui::ScrollArea::vertical()
                    .id_salt(("md-preview", pane_id, doc.id))
                    .auto_shrink(false)
                    .show(&mut ui, |ui| {
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::symmetric(24, 12))
                            .show(ui, |ui| {
                                egui_commonmark::CommonMarkViewer::new().show(
                                    ui,
                                    &mut self.md_cache,
                                    &text,
                                );
                            });
                    });
                // Clicking into the preview focuses its group, without covering
                // the links in it with a click-catching area.
                if ui.rect_contains_pointer(resp.inner_rect)
                    && ui.input(|i| i.pointer.any_pressed())
                {
                    self.focused = idx;
                }
            }
            Some(di) => {
                let doc = &mut self.docs[di];
                let id = editor_id(pane_id, doc.id);
                if is_focused && self.focus_pending {
                    ui.memory_mut(|m| m.request_focus(id));
                    self.focus_pending = false;
                }
                let find = (self.find.open && !self.find.query.text.is_empty())
                    .then_some(&self.find.query);
                let resp = editor::show(
                    &mut ui,
                    id,
                    doc,
                    pane_id,
                    editor::Options {
                        font_size: self.font_size,
                        find,
                    },
                );
                if resp.gained_focus() || resp.clicked() {
                    self.focused = idx;
                }
            }
            None => {
                if is_focused {
                    self.focus_pending = false;
                }
                let resp = ui.interact(
                    ui.available_rect_before_wrap(),
                    ui.scope_id().with("empty"),
                    Sense::click(),
                );
                if resp.clicked() {
                    self.focused = idx;
                }
                welcome(&mut ui);
            }
        }

        if is_focused && self.panes.len() > 1 {
            ui.painter().hline(
                rect.x_range(),
                rect.top() + 1.0,
                Stroke::new(2.0, ui.visuals().selection.bg_fill),
            );
        }
        action
    }

    fn tab_bar(&self, ui: &mut egui::Ui, idx: usize) -> Option<TabAction> {
        let pane = &self.panes[idx];
        let can_move_right = idx + 1 < self.panes.len() || pane.tabs.len() > 1;
        let mut action = None;
        egui::ScrollArea::horizontal()
            .id_salt(("tabs", pane.id))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (t, &doc_id) in pane.tabs.iter().enumerate() {
                        let Some(doc) = self.docs.iter().find(|d| d.id == doc_id) else {
                            continue;
                        };
                        let dirty = if doc.is_dirty() { " •" } else { "" };
                        let resp = ui
                            .add(egui::Button::selectable(
                                t == pane.active,
                                format!("{}{dirty}", doc.title()),
                            ))
                            .interact(Sense::drag());
                        if resp.drag_started() {
                            egui::DragAndDrop::set_payload(
                                ui.ctx(),
                                TabDrag {
                                    pane: pane.id,
                                    doc: doc_id,
                                },
                            );
                        }
                        if resp.clicked() {
                            action = Some(TabAction::Activate(t));
                        }
                        if resp.middle_clicked() {
                            action = Some(TabAction::Close(t));
                        }
                        let resp = match &doc.path {
                            Some(p) => resp.on_hover_text(p.display().to_string()),
                            None => resp,
                        };
                        resp.context_menu(|ui| {
                            if ui
                                .add(
                                    egui::Button::new("Split Right")
                                        .shortcut_text(Command::SplitRight.shortcut_text()),
                                )
                                .clicked()
                            {
                                action = Some(TabAction::SplitRight(t));
                            }
                            if ui
                                .add_enabled(idx > 0, egui::Button::new("Move to Left Group"))
                                .clicked()
                            {
                                action = Some(TabAction::Move(t, -1));
                            }
                            if ui
                                .add_enabled(
                                    can_move_right,
                                    egui::Button::new("Move to Right Group"),
                                )
                                .clicked()
                            {
                                action = Some(TabAction::Move(t, 1));
                            }
                            ui.separator();
                            if ui
                                .add_enabled(doc.path.is_some(), egui::Button::new("Copy Path"))
                                .clicked()
                            {
                                action = Some(TabAction::CopyPath(t));
                            }
                            ui.separator();
                            if ui.button("Close").clicked() {
                                action = Some(TabAction::Close(t));
                            }
                            if ui.button("Close Others").clicked() {
                                action = Some(TabAction::CloseOthers(t));
                            }
                        });
                        if ui.small_button("×").clicked() {
                            action = Some(TabAction::Close(t));
                        }
                        ui.add_space(6.0);
                    }
                });
            });
        action
    }

    // ------------------------------------------------------------------ find bar

    /// The find/replace bar, floating at the top right of the focused group.
    fn find_bar(&mut self, ctx: &egui::Context, group: Rect) {
        if !self.find.open {
            return;
        }
        let width = 360.0f32.min(group.width() - 20.0).max(160.0);
        let mut action = None;
        egui::Area::new(egui::Id::unique("find-bar"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(group.right() - width - 24.0, group.top() + 34.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(width);
                    // Read the matches before drawing the inputs (needs the doc).
                    let query = self.find.query.clone();
                    let (count, current) = match self.active() {
                        Some((doc, view)) => {
                            let p = doc.selections(view).primary();
                            match doc.find(&query) {
                                Ok(m) => (m.len(), m.iter().position(|r| *r == p.range())),
                                Err(_) => (0, None),
                            }
                        }
                        None => (0, None),
                    };
                    ui.horizontal(|ui| {
                        if crate::explorer::chevron_button(ui, self.find.replace)
                            .on_hover_text("Toggle Replace")
                            .clicked()
                        {
                            self.find.replace = !self.find.replace;
                        }
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut self.find.query.text)
                                .hint_text("Find")
                                .desired_width(width - 190.0),
                        );
                        if std::mem::take(&mut self.find.focus_input) {
                            edit.request_focus();
                            let mut st =
                                egui::TextEdit::load_state(ctx, edit.id).unwrap_or_default();
                            let all = egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(self.find.query.text.chars().count()),
                            );
                            st.cursor.set_char_range(Some(all));
                            st.store(ctx, edit.id);
                        }
                        if edit.changed() {
                            action = Some("incremental");
                        }
                        if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            action = Some(if ui.input(|i| i.modifiers.shift) {
                                "prev"
                            } else {
                                "next"
                            });
                            edit.request_focus();
                        }
                        let q = &mut self.find.query;
                        let mut toggled = ui
                            .toggle_value(&mut q.case_sensitive, "Aa")
                            .on_hover_text("Match Case")
                            .changed();
                        toggled |= ui
                            .toggle_value(&mut q.whole_word, "ab")
                            .on_hover_text("Match Whole Word")
                            .changed();
                        toggled |= ui
                            .toggle_value(&mut q.regex, ".*")
                            .on_hover_text("Use Regular Expression")
                            .changed();
                        if toggled {
                            action = Some("incremental");
                        }
                    });
                    ui.horizontal(|ui| {
                        let label = match (count, current) {
                            (0, _) if self.find.query.text.is_empty() => "".to_owned(),
                            (0, _) => "No results".to_owned(),
                            (n, Some(i)) => format!("{} of {n}", i + 1),
                            (n, None) => format!("? of {n}"),
                        };
                        ui.label(RichText::new(label).weak());
                        if let Some(e) = &self.find.error {
                            ui.colored_label(ui.visuals().error_fg_color, e)
                                .on_hover_text(e);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("×")
                                .on_hover_text("Close (Escape)")
                                .clicked()
                            {
                                action = Some("close");
                            }
                            if ui
                                .small_button("⬇")
                                .on_hover_text("Next Match (Enter, F3)")
                                .clicked()
                            {
                                action = Some("next");
                            }
                            if ui
                                .small_button("⬆")
                                .on_hover_text("Previous Match (Shift+Enter)")
                                .clicked()
                            {
                                action = Some("prev");
                            }
                        });
                    });
                    if self.find.replace {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.find.replacement)
                                    .hint_text("Replace")
                                    .desired_width(width - 150.0),
                            );
                            if ui.button("Replace").clicked() {
                                action = Some("replace");
                            }
                            if ui.button("All").on_hover_text("Replace All").clicked() {
                                action = Some("replace-all");
                            }
                        });
                    }
                });
            });
        match action {
            Some("incremental") => {
                self.find.error = None;
                self.find_step(true, true);
            }
            Some("next") => self.find_step(true, false),
            Some("prev") => self.find_step(false, false),
            Some("replace") => self.replace_one(),
            Some("replace-all") => self.replace_all(),
            Some("close") => {
                self.find.open = false;
                self.focus_pending = true;
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ dialogs

    /// "Save changes?" / "Delete?" confirmations.
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let destructive = match &pending {
            Pending::Delete(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let what = if path.is_dir() {
                    "folder and its contents"
                } else {
                    "file"
                };
                Some((format!("Delete the {what} '{name}'?"), "Delete"))
            }
            Pending::Discard(e) => {
                let name = e
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Some(if e.untracked {
                    (
                        format!("Delete the untracked file '{name}'?"),
                        "Delete File",
                    )
                } else {
                    (
                        format!("Discard your changes to '{name}'?"),
                        "Discard Changes",
                    )
                })
            }
            _ => None,
        };
        if let Some((question, button)) = destructive {
            let mut choice = None;
            egui::Modal::new(egui::Id::unique("confirm-destructive")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(question);
                ui.label(RichText::new("This can't be undone.").weak());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(button).clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        choice = Some(false);
                    }
                });
            });
            if let Some(yes) = choice {
                self.pending = None;
                if yes {
                    match pending {
                        Pending::Delete(path) => self.delete_path(&path, ctx),
                        Pending::Discard(e) => self.git_run(|r| r.discard(&e)),
                        _ => {}
                    }
                }
            }
            return;
        }

        let what = match &pending {
            Pending::CloseTab { doc, .. } => match self.doc_index(*doc) {
                Some(i) => format!(
                    "Do you want to save the changes you made to {}?",
                    self.docs[i].title()
                ),
                None => {
                    self.pending = None;
                    return;
                }
            },
            _ => "You have unsaved changes. Save them before quitting?".to_owned(),
        };
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(what);
            ui.label(RichText::new("Your changes will be lost if you don't save them.").weak());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some(true);
                }
                if ui.button("Don't Save").clicked() {
                    choice = Some(false);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.pending = None;
                }
            });
        });
        let Some(save) = choice else { return };
        self.pending = None;
        match pending {
            Pending::CloseTab { pane, doc } => {
                let Some(di) = self.doc_index(doc) else {
                    return;
                };
                if !save || self.save(di, false) {
                    let pane = self.panes.iter().position(|p| p.id == pane);
                    let tab = pane.and_then(|p| self.panes[p].tabs.iter().position(|&d| d == doc));
                    if let (Some(pane), Some(tab)) = (pane, tab) {
                        self.close_tab(pane, tab);
                    }
                }
            }
            Pending::Quit => {
                let all_saved = !save
                    || (0..self.docs.len())
                        .all(|i| !self.docs[i].is_dirty() || self.save(i, false));
                if all_saved {
                    self.allow_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Pending::Delete(_) | Pending::Discard(_) => {}
        }
    }

    fn prompt_dialog(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        let title = match &prompt.kind {
            PromptKind::NewFile(_) => "New file name",
            PromptKind::NewFolder(_) => "New folder name",
            PromptKind::Rename(_) => "Rename to",
        };
        let mut done = None;
        egui::Modal::new(egui::Id::unique("prompt")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(title);
            let edit =
                ui.add(egui::TextEdit::singleline(&mut prompt.value).desired_width(f32::INFINITY));
            if std::mem::take(&mut prompt.just_opened) {
                edit.request_focus();
            }
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() || enter {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    done = Some(false);
                }
            });
        });
        if let Some(ok) = done {
            let p = self.prompt.take().expect("checked above");
            if ok {
                self.finish_prompt(p.kind, &p.value, ctx);
            }
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let ws = self
            .workspace
            .as_ref()
            .map(|w| format!(" — {}", w.name))
            .unwrap_or_default();
        let title = match self.active_doc_index().map(|i| &self.docs[i]) {
            Some(d) => format!(
                "{}{}{ws} — rsedit",
                if d.is_dirty() { "● " } else { "" },
                d.title()
            ),
            None => format!("rsedit{ws}"),
        };
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
    }
}

/// Hooks for the window runner (`crate::window`).
impl EditorApp {
    /// The window is closing for good: remember the open files for next time.
    pub fn on_exit(&mut self) {
        self.session().save();
    }

    /// Some input methods answer every IME cursor update with an empty "preedit"
    /// event, and egui-winit re-sends the cursor area on every frame that had input:
    /// together that's a loop redrawing at full frame rate while idle. The editor
    /// only uses committed IME text, so empty preedits can be dropped.
    pub fn raw_input_hook(&mut self, raw: &mut egui::RawInput) {
        raw.events.retain(|e| !matches!(e, egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) if text.is_empty()));
    }
}

impl EditorApp {
    /// Draws one frame of the whole application.
    pub(crate) fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_quit
            && self.docs.iter().any(|d| d.is_dirty())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending = Some(Pending::Quit);
        }

        // Files may have changed while we weren't looking.
        let refocused = ctx.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::WindowFocused(true)))
        });
        if refocused || self.last_disk_check.elapsed() >= DISK_CHECK_EVERY {
            self.check_disk();
        }

        self.update_git(&ctx);
        // Language servers: send edits, collect answers.
        self.sync_servers(&ctx);
        self.poll_servers();
        let typed: Vec<String> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| {
                    if let egui::Event::Text(t) = e {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
                .collect()
        });

        if self.pending.is_none() && self.prompt.is_none() {
            // The completion popup gets Up/Down/Enter/Tab/Escape before the editor.
            if self.editor_has_focus(&ctx) {
                match self.completion.handle_keys(&ctx) {
                    crate::completion::KeyResult::Accept => self.accept_completion(),
                    crate::completion::KeyResult::Close => self.completion.close(),
                    crate::completion::KeyResult::None => {}
                }
            }
            self.handle_shortcuts(&ctx);
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui, &ctx));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui, &ctx));
        if self.show_problems {
            egui::Panel::bottom("problems")
                .resizable(true)
                .default_size(170.0)
                .size_range(80.0..=600.0)
                .show(ui, |ui| self.problems_panel(ui));
        }
        if self.show_sidebar {
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(240.0)
                .size_range(160.0..=600.0)
                .show(ui, |ui| self.sidebar(ui, &ctx));
        } else {
            // A slim strip to bring the sidebar back.
            egui::Panel::left("activity")
                .resizable(false)
                .exact_size(34.0)
                .show(ui, |ui| self.activity_strip(ui));
        }
        let mut rects = Vec::new();
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(0))
            .show(ui, |ui| rects = self.editor_area(ui));

        // Completion popup at the caret of the focused editor.
        if self.completion.open {
            let caret = self
                .focused_editor_id()
                .and_then(|id| ctx.data(|d| d.get_temp::<Rect>(id.with("caret-rect"))));
            let loading = self.completion.request.is_some();
            if let Some(caret) = caret
                && self.completion.ui(&ctx, caret, loading)
            {
                self.accept_completion();
            }
        }
        let focused = self.editor_has_focus(&ctx);
        self.after_typing(&typed, focused);
        // Ctrl+click in an editor asks for the definition there.
        if let Some(id) = self.focused_editor_id()
            && ctx
                .data_mut(|d| d.remove_temp::<bool>(id.with("goto-definition")))
                .is_some()
        {
            self.go_to_definition();
        }

        if let Some(&group) = rects.get(self.focused) {
            self.find_bar(&ctx, group);
            let area = rects
                .iter()
                .copied()
                .reduce(|a, b| a.union(b))
                .unwrap_or(group);
            let files = self.file_index.as_ref();
            if let Some(pick) = self.palette.ui(&ctx, area, files) {
                self.pick(pick, &ctx);
            }
            if !self.palette.open {
                self.file_index = None;
            }
        }

        self.confirm_dialog(&ctx);
        self.prompt_dialog(&ctx);
        self.update_title(&ctx);
    }
}

fn welcome(ui: &mut egui::Ui) {
    let lines = [
        Command::CommandPalette,
        Command::GoToFile,
        Command::NewFile,
        Command::OpenFolder,
        Command::OpenNotes,
        Command::FindInFiles,
        Command::SplitRight,
        Command::ToggleSidebar,
    ];
    let text: Vec<String> = lines
        .iter()
        .map(|c| {
            let l = c.label();
            format!(
                "{:<16}{}",
                c.shortcut_text(),
                l.split_once(": ").map_or(l, |(_, x)| x)
            )
        })
        .collect();
    ui.centered_and_justified(|ui| {
        ui.label(
            RichText::new(format!("rsedit\n\n{}", text.join("\n")))
                .monospace()
                .size(14.0)
                .weak(),
        );
    });
}

/// Colour for a git status letter (VSCode-like).
fn status_color(letter: char) -> Color32 {
    match letter {
        'M' | 'T' => Color32::from_rgb(226, 192, 120),
        'A' | 'U' | '?' | 'R' | 'C' => Color32::from_rgb(115, 201, 145),
        'D' => Color32::from_rgb(220, 100, 90),
        '!' => Color32::from_rgb(230, 80, 80),
        _ => Color32::GRAY,
    }
}
