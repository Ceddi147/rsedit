//! User settings: `settings.json` in rsedit's config directory, edited in a
//! normal editor tab ("Preferences: Open Settings") and applied when saved.
//! Unknown keys are ignored and bad values fall back to the defaults, with a
//! message saying what was wrong.

use std::path::{Path, PathBuf};

use egui::{Key, KeyboardShortcut, Modifiers};

use crate::commands::Command;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// The notes file opened by "Notes: Open Notes".
    pub notes_file: PathBuf,
    /// Shortcut for "Notes: Open Notes".
    pub notes_shortcut: KeyboardShortcut,
    /// Let clangd index the whole project in the background. That makes references
    /// and rename see files that aren't open, but costs memory (default: off).
    pub clangd_background_index: bool,
    /// Format a file when saving it.
    pub format_on_save: bool,
    /// External formatters: file extension or language id → command (reads the text
    /// on stdin, writes the result on stdout). An empty command turns formatting off.
    pub format_commands: std::collections::HashMap<String, Vec<String>>,
    /// Let the text cursor fade in and out (it stays solid while typing and
    /// after a few seconds without input).
    pub cursor_blinking: bool,
}

/// Written when the settings file is first opened, so every option is visible.
const DEFAULT_TEXT: &str = r#"{
    "notes.file": "~/notes.md",
    "notes.shortcut": "Ctrl+Shift+N",
    "clangd.backgroundIndex": false,
    "format.onSave": false,
    "format.commands": {},
    "editor.cursorBlinking": true
}
"#;

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rsedit"))
}

pub fn path() -> Option<PathBuf> {
    Some(config_dir()?.join("settings.json"))
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notes_file: home().join("notes.md"),
            notes_shortcut: KeyboardShortcut::new(
                Modifiers {
                    shift: true,
                    ..Modifiers::COMMAND
                },
                Key::N,
            ),
            clangd_background_index: false,
            format_on_save: false,
            format_commands: Default::default(),
            cursor_blinking: true,
        }
    }
}

impl Settings {
    /// Reads the settings file. A missing file means defaults; problems are
    /// returned as messages next to the (partly default) settings.
    pub fn load() -> (Self, Vec<String>) {
        match path().map(std::fs::read_to_string) {
            Some(Ok(text)) => Self::parse(&text, &home()),
            _ => (Self::default(), Vec::new()),
        }
    }

    pub fn parse(text: &str, home: &Path) -> (Self, Vec<String>) {
        let mut s = Self {
            notes_file: home.join("notes.md"),
            ..Self::default()
        };
        let mut problems = Vec::new();
        let json: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return (s, vec![format!("settings.json is not valid JSON: {e}")]),
        };
        let Some(obj) = json.as_object() else {
            return (s, vec!["settings.json must be a JSON object { … }".into()]);
        };
        if let Some(v) = obj.get("notes.file") {
            match v.as_str().map(str::trim) {
                Some(p) if !p.is_empty() => s.notes_file = expand_path(p, home),
                _ => problems.push("\"notes.file\" must be a path (text)".into()),
            }
        }
        if let Some(v) = obj.get("notes.shortcut") {
            match v
                .as_str()
                .ok_or("must be text like \"Ctrl+Shift+N\"".to_owned())
                .and_then(parse_shortcut)
            {
                Ok(sc) => match conflict(sc) {
                    Some(other) => problems.push(format!(
                        "\"notes.shortcut\": {} is already used by \"{}\"",
                        crate::commands::format_shortcut(&sc),
                        other.label()
                    )),
                    None => s.notes_shortcut = sc,
                },
                Err(e) => problems.push(format!("\"notes.shortcut\": {e}")),
            }
        }
        if let Some(v) = obj.get("clangd.backgroundIndex") {
            match v.as_bool() {
                Some(b) => s.clangd_background_index = b,
                None => problems.push("\"clangd.backgroundIndex\" must be true or false".into()),
            }
        }
        if let Some(v) = obj.get("format.onSave") {
            match v.as_bool() {
                Some(b) => s.format_on_save = b,
                None => problems.push("\"format.onSave\" must be true or false".into()),
            }
        }
        if let Some(v) = obj.get("editor.cursorBlinking") {
            match v.as_bool() {
                Some(b) => s.cursor_blinking = b,
                None => problems.push("\"editor.cursorBlinking\" must be true or false".into()),
            }
        }
        if let Some(v) = obj.get("format.commands") {
            match v.as_object() {
                None => problems.push("\"format.commands\" must be an object".into()),
                Some(map) => {
                    for (key, cmd) in map {
                        let argv: Option<Vec<String>> = cmd.as_array().and_then(|a| {
                            a.iter().map(|x| x.as_str().map(str::to_owned)).collect()
                        });
                        match argv {
                            Some(argv) => {
                                s.format_commands.insert(key.clone(), argv);
                            }
                            None => problems
                                .push(format!("\"format.commands\".{key} must be a list of text")),
                        }
                    }
                }
            }
        }
        (s, problems)
    }

    /// Creates the settings file with all defaults spelled out, if it is missing.
    pub fn ensure_file() -> std::io::Result<PathBuf> {
        let p = path().ok_or_else(|| std::io::Error::other("no home directory"))?;
        if !p.exists() {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&p, DEFAULT_TEXT)?;
        }
        Ok(p)
    }
}

/// `~` and `~/…` mean the home directory; relative paths are taken from there too.
fn expand_path(p: &str, home: &Path) -> PathBuf {
    if p == "~" {
        return home.to_path_buf();
    }
    let p = p.strip_prefix("~/").unwrap_or(p);
    home.join(p) // an absolute `p` replaces `home`
}

/// Parses "Ctrl+Shift+N", "Alt+F2", "F9", … (case-insensitive modifiers).
pub fn parse_shortcut(text: &str) -> Result<KeyboardShortcut, String> {
    let mut modifiers = Modifiers::NONE;
    let mut key = None;
    // Split on '+', but "Ctrl++" means the plus key.
    let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
    if text.trim_end().ends_with("++") {
        parts.truncate(parts.len() - 2);
        parts.push("+");
    }
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "cmd" | "command" => modifiers |= Modifiers::COMMAND,
            "shift" => modifiers.shift = true,
            "alt" | "option" => modifiers.alt = true,
            "" => return Err(format!("\"{text}\" has an empty part")),
            _ if key.is_some() => return Err(format!("\"{text}\" has more than one key")),
            _ => {
                let k = Key::from_name(part)
                    .or_else(|| Key::from_name(&part.to_ascii_uppercase()))
                    .ok_or_else(|| format!("unknown key \"{part}\""))?;
                key = Some(k);
            }
        }
    }
    let key = key.ok_or_else(|| format!("\"{text}\" has no key, only modifiers"))?;
    let is_function_key = key.name().starts_with('F') && key.name().len() > 1;
    if !modifiers.command && !modifiers.alt && !is_function_key {
        return Err(format!(
            "\"{text}\" needs Ctrl or Alt, or it would block typing"
        ));
    }
    Ok(KeyboardShortcut::new(modifiers, key))
}

/// Another command that already has this shortcut (global or in the editor).
fn conflict(sc: KeyboardShortcut) -> Option<Command> {
    let text = crate::commands::format_shortcut(&sc);
    Command::ALL
        .iter()
        .copied()
        .filter(|c| *c != Command::OpenNotes)
        .find(|c| c.builtin_shortcut_text() == text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL_SHIFT: Modifiers = Modifiers {
        shift: true,
        ..Modifiers::COMMAND
    };

    #[test]
    fn defaults_when_empty_or_broken() {
        let home = Path::new("/home/me");
        let (s, problems) = Settings::parse("{}", home);
        assert_eq!(s.notes_file, Path::new("/home/me/notes.md"));
        assert_eq!(s.notes_shortcut, KeyboardShortcut::new(CTRL_SHIFT, Key::N));
        assert!(problems.is_empty());
        let (s, problems) = Settings::parse("{ nope", home);
        assert_eq!(s.notes_file, Path::new("/home/me/notes.md"));
        assert_eq!(problems.len(), 1);
        // The file written for new users parses to exactly the defaults.
        assert_eq!(
            Settings::parse(DEFAULT_TEXT, home),
            Settings::parse("{}", home)
        );
    }

    #[test]
    fn notes_path_and_shortcut() {
        let home = Path::new("/home/me");
        let (s, problems) = Settings::parse(
            r#"{"notes.file": "~/docs/todo.txt", "notes.shortcut": "alt+shift+n"}"#,
            home,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(s.notes_file, Path::new("/home/me/docs/todo.txt"));
        let alt_shift = Modifiers {
            shift: true,
            ..Modifiers::ALT
        };
        assert_eq!(s.notes_shortcut, KeyboardShortcut::new(alt_shift, Key::N));
        let (s, _) = Settings::parse(r#"{"notes.file": "/tmp/n.md"}"#, home);
        assert_eq!(s.notes_file, Path::new("/tmp/n.md"));
        let (s, _) = Settings::parse(r#"{"notes.file": "notes/today.md"}"#, home);
        assert_eq!(s.notes_file, Path::new("/home/me/notes/today.md"));
    }

    #[test]
    fn bad_shortcuts_are_reported_and_ignored() {
        let home = Path::new("/home/me");
        for bad in [
            "N",
            "Ctrl+",
            "Ctrl+Shift",
            "Ctrl+Foo",
            "Ctrl+A+B",
            "Ctrl+S",
            "Ctrl+Shift+K",
        ] {
            let (s, problems) = Settings::parse(&format!(r#"{{"notes.shortcut": "{bad}"}}"#), home);
            assert_eq!(problems.len(), 1, "{bad} should be rejected");
            assert_eq!(s.notes_shortcut, Settings::default().notes_shortcut);
        }
        assert!(parse_shortcut("F9").is_ok());
        assert_eq!(
            parse_shortcut("Ctrl+,"),
            Ok(KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma))
        );
        assert_eq!(
            parse_shortcut("Ctrl++"),
            Ok(KeyboardShortcut::new(Modifiers::COMMAND, Key::Plus))
        );
    }

    #[test]
    fn format_settings() {
        let text = r#"{"format.onSave": true, "format.commands": {"cpp": ["uncrustify", "-q"], "rs": [], "bad": "x"}}"#;
        let (s, problems) = Settings::parse(text, Path::new("/h"));
        assert!(s.format_on_save);
        assert_eq!(s.format_commands["cpp"], vec!["uncrustify", "-q"]);
        assert!(
            s.format_commands["rs"].is_empty(),
            "an empty command is kept: it disables formatting"
        );
        assert!(!s.format_commands.contains_key("bad"));
        assert_eq!(
            problems,
            vec!["\"format.commands\".bad must be a list of text"]
        );
        let (s, problems) = Settings::parse(
            r#"{"format.onSave": 1, "format.commands": []}"#,
            Path::new("/h"),
        );
        assert!(!s.format_on_save && s.format_commands.is_empty());
        assert_eq!(problems.len(), 2);
    }
}
