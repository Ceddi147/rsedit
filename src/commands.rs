//! Every user-facing command, with its name and shortcut. Menus, the command
//! palette and the keyboard handler all use this one list.

use egui::{Key, KeyboardShortcut, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    NewFile,
    OpenFile,
    OpenFolder,
    Save,
    SaveAs,
    SaveAll,
    CloseEditor,
    CloseAllEditors,
    ReopenClosedEditor,
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
    DeleteLine,
    MoveLineUp,
    MoveLineDown,
    CopyLineUp,
    CopyLineDown,
    InsertLineBelow,
    InsertLineAbove,
    SelectAll,
    SelectLine,
    AddNextOccurrence,
    SelectAllOccurrences,
    GoToFile,
    GoToLine,
    CommandPalette,
    NextTab,
    PreviousTab,
    ToggleSidebar,
    ShowExplorer,
    ShowSearch,
    SplitRight,
    JoinGroup,
    FocusGroup1,
    FocusGroup2,
    FocusGroup3,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleTheme,
    ToggleLineEnding,
    IndentWithSpaces2,
    IndentWithSpaces4,
    IndentWithTabs,
    RevealInExplorer,
    TriggerSuggest,
    ToggleLanguageServers,
    Quit,
}

use Command::*;

const CTRL: Modifiers = Modifiers::COMMAND;
const CTRL_SHIFT: Modifiers = Modifiers { shift: true, ..Modifiers::COMMAND };
const ALT: Modifiers = Modifiers::ALT;
const ALT_SHIFT: Modifiers = Modifiers { shift: true, ..Modifiers::ALT };

impl Command {
    pub const ALL: &[Command] = &[
        NewFile, OpenFile, OpenFolder, Save, SaveAs, SaveAll, CloseEditor, CloseAllEditors, ReopenClosedEditor,
        Undo, Redo, Cut, Copy, Paste, Find, Replace, FindInFiles, ToggleComment, Indent, Outdent, DeleteLine,
        MoveLineUp, MoveLineDown, CopyLineUp, CopyLineDown, InsertLineBelow, InsertLineAbove, SelectAll,
        SelectLine, AddNextOccurrence, SelectAllOccurrences, GoToFile, GoToLine, CommandPalette, NextTab,
        PreviousTab, ToggleSidebar, ShowExplorer, ShowSearch, SplitRight, JoinGroup, FocusGroup1, FocusGroup2,
        FocusGroup3, ZoomIn, ZoomOut, ZoomReset, ToggleTheme, ToggleLineEnding, IndentWithSpaces2,
        IndentWithSpaces4, IndentWithTabs, RevealInExplorer, TriggerSuggest, ToggleLanguageServers, Quit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NewFile => "File: New File",
            OpenFile => "File: Open File…",
            OpenFolder => "File: Open Folder…",
            Save => "File: Save",
            SaveAs => "File: Save As…",
            SaveAll => "File: Save All",
            CloseEditor => "View: Close Editor",
            CloseAllEditors => "View: Close All Editors",
            ReopenClosedEditor => "View: Reopen Closed Editor",
            Undo => "Edit: Undo",
            Redo => "Edit: Redo",
            Cut => "Edit: Cut",
            Copy => "Edit: Copy",
            Paste => "Edit: Paste",
            Find => "Edit: Find",
            Replace => "Edit: Replace",
            FindInFiles => "Search: Find in Files",
            ToggleComment => "Edit: Toggle Line Comment",
            Indent => "Edit: Indent Line",
            Outdent => "Edit: Outdent Line",
            DeleteLine => "Edit: Delete Line",
            MoveLineUp => "Edit: Move Line Up",
            MoveLineDown => "Edit: Move Line Down",
            CopyLineUp => "Edit: Copy Line Up",
            CopyLineDown => "Edit: Copy Line Down",
            InsertLineBelow => "Edit: Insert Line Below",
            InsertLineAbove => "Edit: Insert Line Above",
            SelectAll => "Selection: Select All",
            SelectLine => "Selection: Expand Line Selection",
            AddNextOccurrence => "Selection: Add Next Occurrence",
            SelectAllOccurrences => "Selection: Select All Occurrences",
            GoToFile => "Go: Go to File…",
            GoToLine => "Go: Go to Line…",
            CommandPalette => "View: Command Palette",
            NextTab => "View: Next Editor",
            PreviousTab => "View: Previous Editor",
            ToggleSidebar => "View: Toggle Sidebar",
            ShowExplorer => "View: Show Explorer",
            ShowSearch => "View: Show Search",
            SplitRight => "View: Split Editor Right",
            JoinGroup => "View: Join Editor Group",
            FocusGroup1 => "View: Focus First Editor Group",
            FocusGroup2 => "View: Focus Second Editor Group",
            FocusGroup3 => "View: Focus Third Editor Group",
            ZoomIn => "View: Zoom In",
            ZoomOut => "View: Zoom Out",
            ZoomReset => "View: Reset Zoom",
            ToggleTheme => "Preferences: Toggle Light/Dark Theme",
            ToggleLineEnding => "Change End of Line Sequence (LF/CRLF)",
            IndentWithSpaces2 => "Indent Using 2 Spaces",
            IndentWithSpaces4 => "Indent Using 4 Spaces",
            IndentWithTabs => "Indent Using Tabs",
            RevealInExplorer => "File: Reveal Active File in Explorer",
            TriggerSuggest => "Edit: Trigger Suggest",
            ToggleLanguageServers => "Preferences: Toggle Language Servers",
            Quit => "File: Quit",
        }
    }

    /// Global shortcut, handled by the app before the editor sees the keys.
    pub fn shortcut(self) -> Option<KeyboardShortcut> {
        let s = |m, k| Some(KeyboardShortcut::new(m, k));
        match self {
            NewFile => s(CTRL, Key::N),
            OpenFile => s(CTRL, Key::O),
            Save => s(CTRL, Key::S),
            SaveAs => s(CTRL_SHIFT, Key::S),
            CloseEditor => s(CTRL, Key::W),
            ReopenClosedEditor => s(CTRL_SHIFT, Key::T),
            TriggerSuggest => s(CTRL, Key::Space),
            Find => s(CTRL, Key::F),
            Replace => s(CTRL, Key::H),
            FindInFiles => s(CTRL_SHIFT, Key::F),
            GoToFile => s(CTRL, Key::P),
            GoToLine => s(CTRL, Key::G),
            CommandPalette => s(CTRL_SHIFT, Key::P),
            NextTab => s(CTRL, Key::PageDown),
            PreviousTab => s(CTRL, Key::PageUp),
            ToggleSidebar => s(CTRL, Key::B),
            ShowExplorer => s(CTRL_SHIFT, Key::E),
            SplitRight => s(CTRL, Key::Backslash),
            FocusGroup1 => s(CTRL, Key::Num1),
            FocusGroup2 => s(CTRL, Key::Num2),
            FocusGroup3 => s(CTRL, Key::Num3),
            Quit => s(CTRL, Key::Q),
            _ => None,
        }
    }

    /// Shortcut text shown in menus and the palette (includes editor-level keys).
    pub fn shortcut_text(self) -> String {
        if let Some(s) = self.shortcut() {
            return format_shortcut(&s);
        }
        let s = |m, k| format_shortcut(&KeyboardShortcut::new(m, k));
        match self {
            Undo => s(CTRL, Key::Z),
            Redo => s(CTRL, Key::Y),
            Cut => s(CTRL, Key::X),
            Copy => s(CTRL, Key::C),
            Paste => s(CTRL, Key::V),
            ToggleComment => s(CTRL, Key::Slash),
            Indent => s(CTRL, Key::CloseBracket),
            Outdent => s(CTRL, Key::OpenBracket),
            DeleteLine => s(CTRL_SHIFT, Key::K),
            MoveLineUp => s(ALT, Key::ArrowUp),
            MoveLineDown => s(ALT, Key::ArrowDown),
            CopyLineUp => s(ALT_SHIFT, Key::ArrowUp),
            CopyLineDown => s(ALT_SHIFT, Key::ArrowDown),
            InsertLineBelow => s(CTRL, Key::Enter),
            InsertLineAbove => s(CTRL_SHIFT, Key::Enter),
            SelectAll => s(CTRL, Key::A),
            SelectLine => s(CTRL, Key::L),
            AddNextOccurrence => s(CTRL, Key::D),
            SelectAllOccurrences => s(CTRL_SHIFT, Key::L),
            // Handled by egui itself (whole-window zoom, like VSCode).
            ZoomIn => s(CTRL, Key::Equals),
            ZoomOut => s(CTRL, Key::Minus),
            ZoomReset => s(CTRL, Key::Num0),
            _ => String::new(),
        }
    }
}

fn format_shortcut(s: &KeyboardShortcut) -> String {
    let mut out = String::new();
    if s.modifiers.command || s.modifiers.ctrl {
        out.push_str("Ctrl+");
    }
    if s.modifiers.shift {
        out.push_str("Shift+");
    }
    if s.modifiers.alt {
        out.push_str("Alt+");
    }
    let key = match s.logical_key {
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::Backslash => "\\",
        Key::Slash => "/",
        Key::OpenBracket => "[",
        Key::CloseBracket => "]",
        Key::Equals => "=",
        Key::Minus => "-",
        k => k.name(),
    };
    out.push_str(key);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_is_listed_once_and_shortcuts_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in Command::ALL {
            assert!(seen.insert(*c), "{c:?} listed twice");
        }
        let mut keys = std::collections::HashSet::new();
        for c in Command::ALL {
            let t = c.shortcut_text();
            if !t.is_empty() {
                assert!(keys.insert(t.clone()), "shortcut {t} used twice");
            }
        }
        assert_eq!(Command::Save.shortcut_text(), "Ctrl+S");
        assert_eq!(Command::CopyLineDown.shortcut_text(), "Shift+Alt+Down");
    }
}
