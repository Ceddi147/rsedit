//! User settings: `settings.json` in rsedit's config directory, shown as a
//! settings page ("Preferences: Open Settings") or edited as text, and applied
//! when saved. Unknown keys are ignored and bad values fall back to the
//! defaults, with a message saying what was wrong. The page changes single keys
//! with `edit_json`, so everything else in the file stays as the user wrote it.

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
    pub fn load_from(path: Option<&Path>) -> (Self, Vec<String>) {
        match path.map(std::fs::read_to_string) {
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
    pub fn ensure_file_at(p: &Path) -> std::io::Result<()> {
        if !p.exists() {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(p, DEFAULT_TEXT)?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------ writing single keys

type Map = serde_json::Map<String, serde_json::Value>;

/// Parses `text`, lets `f` change the top-level object and returns the new text,
/// or `None` if nothing changed. Key order and all other values are kept.
pub fn edit_json(text: &str, f: impl FnOnce(&mut Map)) -> Result<Option<String>, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("settings.json is not valid JSON: {e}"))?;
    let Some(map) = value.as_object_mut() else {
        return Err("settings.json must be a JSON object { … }".into());
    };
    let before = map.clone();
    f(map);
    if *map == before {
        return Ok(None);
    }
    Ok(Some(to_text(&value)))
}

/// The house style: 4 spaces, one key per line, lists of plain values on one line.
pub fn to_text(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out.push('\n');
    out
}

fn write_value(out: &mut String, v: &serde_json::Value, depth: usize) {
    use serde_json::Value;
    let pad = |out: &mut String, d: usize| out.push_str(&"    ".repeat(d));
    match v {
        Value::Object(m) if m.is_empty() => out.push_str("{}"),
        Value::Object(m) => {
            out.push_str("{\n");
            for (i, (k, v)) in m.iter().enumerate() {
                pad(out, depth + 1);
                out.push_str(&Value::String(k.clone()).to_string());
                out.push_str(": ");
                write_value(out, v, depth + 1);
                out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
            }
            pad(out, depth);
            out.push('}');
        }
        Value::Array(a) if a.is_empty() => out.push_str("[]"),
        Value::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&x.to_string());
            }
            out.push(']');
        }
        Value::Array(a) => {
            out.push_str("[\n");
            for (i, x) in a.iter().enumerate() {
                pad(out, depth + 1);
                write_value(out, x, depth + 1);
                out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
            }
            pad(out, depth);
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Sets the formatter for `ext` (renaming it from `old`, keeping its position).
pub fn set_command(map: &mut Map, old: Option<&str>, ext: &str, argv: Vec<String>) {
    let list = serde_json::Value::from(argv);
    let entry = map
        .entry("format.commands")
        .or_insert_with(|| serde_json::Value::Object(Map::new()));
    let Some(cmds) = entry.as_object_mut() else {
        return;
    };
    match old {
        Some(old) if old != ext => {
            let rebuilt: Map = std::mem::take(cmds)
                .into_iter()
                .map(|(k, v)| {
                    if k == old {
                        (ext.to_owned(), list.clone())
                    } else {
                        (k, v)
                    }
                })
                .collect();
            *cmds = rebuilt;
        }
        _ => {
            cmds.insert(ext.to_owned(), list);
        }
    }
}

pub fn remove_command(map: &mut Map, ext: &str) {
    if let Some(cmds) = map
        .get_mut("format.commands")
        .and_then(|v| v.as_object_mut())
    {
        cmds.shift_remove(ext);
    }
}

/// A command line as one line of text, quoted so `split_args` gives it back.
pub fn join_args(args: &[String]) -> String {
    let plain = |a: &str| {
        !a.is_empty()
            && !a
                .chars()
                .any(|c| c.is_whitespace() || "'\"\\$`".contains(c))
    };
    args.iter()
        .map(|a| {
            if plain(a) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Splits a command line: whitespace separates, `'…'` is literal, `"…"` allows
/// `\"` and `\\`, a backslash outside quotes escapes the next character.
pub fn split_args(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("a ' is not closed".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => cur.push(c),
                            Some(c) => {
                                cur.push('\\');
                                cur.push(c);
                            }
                            None => return Err("a \" is not closed".into()),
                        },
                        Some(c) => cur.push(c),
                        None => return Err("a \" is not closed".into()),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some(c) => cur.push(c),
                    None => cur.push('\\'),
                }
            }
            c => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        out.push(cur);
    }
    Ok(out)
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
pub(crate) fn conflict(sc: KeyboardShortcut) -> Option<Command> {
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

    #[test]
    fn writer_reproduces_files_byte_for_byte() {
        let v: serde_json::Value = serde_json::from_str(DEFAULT_TEXT).unwrap();
        assert_eq!(to_text(&v), DEFAULT_TEXT);
        let mine = "{\n    \"format.onSave\": true,\n    \"format.commands\": {\n        \"cpp\": [\"~/u/uncrustify\", \"-c\", \"x.cfg\", \"--assume\", \"{file}\"],\n        \"c\": []\n    },\n    \"my.custom\": [1, 2, \"x\"],\n    \"notes.file\": \"~/n.md\"\n}\n";
        let v: serde_json::Value = serde_json::from_str(mine).unwrap();
        assert_eq!(to_text(&v), mine);
    }

    #[test]
    fn edit_json_changes_only_the_key() {
        let text = "{\n    \"z\": 1,\n    \"my.custom\": [1, 2, \"x\"],\n    \"a\": 2\n}\n";
        let set = |m: &mut Map| {
            m.insert("a".into(), 3.into());
        };
        let out = edit_json(text, set).unwrap().unwrap();
        assert_eq!(out, text.replace("\"a\": 2\n", "\"a\": 3\n"));
        assert_eq!(edit_json(text, |_| {}), Ok(None));
        let new = edit_json(text, |m| {
            m.insert("b".into(), true.into());
        })
        .unwrap()
        .unwrap();
        assert!(new.ends_with("\"a\": 2,\n    \"b\": true\n}\n"));
        assert!(edit_json("{ nope", |_| {}).is_err());
        assert!(edit_json("[1]", |_| {}).is_err());
    }

    #[test]
    fn commands_rename_keeps_position() {
        let text = r#"{"format.commands": {"a": ["x"], "b": ["y"], "c": ["z"]}}"#;
        let out = edit_json(text, |m| set_command(m, Some("b"), "rs", vec![]))
            .unwrap()
            .unwrap();
        let (s, _) = Settings::parse(&out, Path::new("/h"));
        assert!(s.format_commands["rs"].is_empty() && !s.format_commands.contains_key("b"));
        let pos = |k: &str| out.find(&format!("\"{k}\"")).unwrap();
        assert!(pos("a") < pos("rs") && pos("rs") < pos("c"));
        let out = edit_json(&out, |m| remove_command(m, "a"))
            .unwrap()
            .unwrap();
        assert!(!out.contains("\"a\""));
    }

    #[test]
    fn args_round_trip() {
        for args in [
            vec![],
            vec!["".to_owned()],
            vec!["a b".to_owned(), "it's".into(), "\\".into(), "\"q\"".into()],
            vec!["{file}".to_owned(), "--assume={file}".into(), "~/x".into()],
        ] {
            assert_eq!(split_args(&join_args(&args)).unwrap(), args, "{args:?}");
        }
        assert!(split_args("a 'b").is_err());
        assert!(split_args("a \"b").is_err());
        let u: Vec<String> = ["~/.u/uncrustify", "-c", "~/.u/u.cfg", "-q", "{file}"]
            .map(String::from)
            .to_vec();
        assert_eq!(join_args(&u), "~/.u/uncrustify -c ~/.u/u.cfg -q {file}");
    }
}
