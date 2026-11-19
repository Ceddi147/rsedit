//! The settings page: a form view of the `settings.json` tab (like the Markdown
//! preview, it replaces the text of that tab while it is shown). Every control
//! changes one key of the file with `settings::edit_json`, saves it and so applies
//! it; everything else in the file stays as the user wrote it.

use egui::{Key, RichText};

use super::EditorApp;
use crate::settings::{self, Settings};

type Map = serde_json::Map<String, serde_json::Value>;

/// One change of one key, applied to the parsed file.
pub(super) enum SettingsEdit {
    Set(&'static str, serde_json::Value),
    SetCommand {
        old: Option<String>,
        ext: String,
        argv: Vec<String>,
    },
    RemoveCommand(String),
}

impl SettingsEdit {
    pub(super) fn apply(self, map: &mut Map) {
        match self {
            Self::Set(key, v) => {
                map.insert(key.to_owned(), v);
            }
            Self::SetCommand { old, ext, argv } => {
                settings::set_command(map, old.as_deref(), &ext, argv)
            }
            Self::RemoveCommand(ext) => settings::remove_command(map, &ext),
        }
    }
}

struct Row {
    ext: String,
    cmd: String,
    saved_ext: String,
    saved_cmd: String,
    error: Option<String>,
    /// The file holds something else than a list of text here: only removable.
    invalid: bool,
}

/// What the page shows, rebuilt whenever the document's text changes.
pub(super) struct SettingsForm {
    doc: u64,
    rev: u64,
    /// The text isn't a JSON object: nothing can be edited.
    parse_error: Option<String>,
    problems: Vec<String>,
    settings: Settings,
    notes_file: String,
    saved_notes_file: String,
    notes_shortcut: String,
    saved_notes_shortcut: String,
    /// `None` when "format.commands" isn't an object.
    rows: Option<Vec<Row>>,
    new_ext: String,
    new_cmd: String,
    new_error: Option<String>,
}

impl SettingsForm {
    fn from_text(doc: u64, rev: u64, text: &str) -> Self {
        let home = settings::home();
        let (settings, problems) = Settings::parse(text, &home);
        let json = serde_json::from_str::<serde_json::Value>(text);
        let obj = json.as_ref().ok().and_then(|v| v.as_object());
        let parse_error = match (&json, obj) {
            (Err(e), _) => Some(format!("settings.json is not valid JSON: {e}")),
            (Ok(_), None) => Some("settings.json must be a JSON object { ... }".to_owned()),
            _ => None,
        };
        let string = |key: &str, default: String| {
            obj.and_then(|o| o.get(key))
                .and_then(|v| v.as_str())
                .map_or(default, str::to_owned)
        };
        let notes_file = string("notes.file", "~/notes.md".into());
        let notes_shortcut = string(
            "notes.shortcut",
            crate::commands::format_shortcut(&Settings::default().notes_shortcut),
        );
        let rows = match obj.and_then(|o| o.get("format.commands")) {
            None => Some(Vec::new()),
            Some(v) => v.as_object().map(|m| {
                m.iter()
                    .map(|(ext, v)| {
                        let argv: Option<Vec<String>> = v.as_array().and_then(|a| {
                            a.iter().map(|x| x.as_str().map(str::to_owned)).collect()
                        });
                        let cmd = argv.as_deref().map(settings::join_args);
                        Row {
                            ext: ext.clone(),
                            saved_ext: ext.clone(),
                            invalid: cmd.is_none(),
                            cmd: cmd.clone().unwrap_or_else(|| v.to_string()),
                            saved_cmd: cmd.unwrap_or_default(),
                            error: None,
                        }
                    })
                    .collect()
            }),
        };
        Self {
            doc,
            rev,
            parse_error,
            problems,
            settings,
            saved_notes_file: notes_file.clone(),
            notes_file,
            saved_notes_shortcut: notes_shortcut.clone(),
            notes_shortcut,
            rows,
            new_ext: String::new(),
            new_cmd: String::new(),
            new_error: None,
        }
    }
}

/// Validates a formatter row: `(extension, argv)` or what is wrong.
fn check_command(
    ext: &str,
    cmd: &str,
    others: impl Iterator<Item = String>,
) -> Result<(String, Vec<String>), String> {
    let ext = ext.trim().trim_start_matches('.').to_owned();
    if ext.is_empty() {
        return Err("Enter a file extension or language, like cpp or rs".into());
    }
    let mut others = others;
    if others.any(|o| o == ext) {
        return Err(format!("\"{ext}\" is already in the list"));
    }
    let argv = settings::split_args(cmd).map_err(|e| format!("Command: {e}"))?;
    Ok((ext, argv))
}

/// Where the test finds a control to click.
pub(super) fn control_id(key: &str) -> egui::Id {
    egui::Id::unique(("settings-control", key))
}

fn remember(ui: &egui::Ui, key: &str, resp: &egui::Response) {
    ui.ctx()
        .data_mut(|d| d.insert_temp(control_id(key), resp.rect));
}

/// A single-line field that commits when it loses focus; Esc puts the saved text back.
/// Returns true when the text should be written.
fn field(ui: &mut egui::Ui, key: &str, text: &mut String, saved: &str, width: f32) -> bool {
    let resp = ui.add(egui::TextEdit::singleline(text).desired_width(width));
    remember(ui, key, &resp);
    if resp.lost_focus() {
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            *text = saved.to_owned();
            return false;
        }
        return text.trim() != saved;
    }
    false
}

impl EditorApp {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui, idx: usize, di: usize) {
        let view = self.panes[idx].id;
        let (doc_id, rev) = (self.docs[di].id, self.docs[di].rev());
        let mut form = match self.settings_form.take() {
            Some(f) if f.doc == doc_id && f.rev == rev => f,
            _ => SettingsForm::from_text(doc_id, rev, &self.docs[di].rope().to_string()),
        };
        let dirty = self.docs[di].is_dirty();
        let mut edits: Vec<SettingsEdit> = Vec::new();
        let (mut save, mut to_json) = (false, false);
        let (weak, error_color, warn_color) = (
            crate::theme::palette(ui).weak,
            ui.visuals().error_fg_color,
            ui.visuals().warn_fg_color,
        );
        // A focused field would otherwise repaint twice a second, forever.
        ui.visuals_mut().text_cursor.blink = false;

        egui::ScrollArea::vertical()
            .id_salt(("settings-page", view, doc_id))
            .auto_shrink(false)
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(24, 12))
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width().min(720.0));
                        ui.horizontal(|ui| {
                            ui.heading("Settings");
                            if ui.button("Open settings.json").clicked() {
                                to_json = true;
                            }
                        });
                        if let Some(p) = &self.settings_file {
                            ui.label(RichText::new(p.display().to_string()).color(weak));
                        }
                        ui.add_space(6.0);
                        if let Some(e) = &form.parse_error {
                            ui.colored_label(error_color, e);
                            ui.label("Fix the file as text first; the controls are off until then.");
                        } else if dirty {
                            ui.horizontal(|ui| {
                                ui.colored_label(warn_color, "settings.json has unsaved changes.");
                                if ui.button("Save").clicked() {
                                    save = true;
                                }
                            });
                        }
                        for p in &form.problems {
                            ui.colored_label(warn_color, p);
                        }
                        let enabled = form.parse_error.is_none() && !dirty;
                        ui.add_enabled_ui(enabled, |ui| {
                            section(ui, "Editor");
                            let mut on = form.settings.cursor_blinking;
                            let r = ui.checkbox(&mut on, "Blinking text cursor");
                            remember(ui, "editor.cursorBlinking", &r);
                            if r.changed() {
                                edits.push(SettingsEdit::Set("editor.cursorBlinking", on.into()));
                            }

                            section(ui, "Notes");
                            egui::Grid::new("notes-grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                                ui.label("Notes file");
                                let saved = form.saved_notes_file.clone();
                                if field(ui, "notes.file", &mut form.notes_file, &saved, 360.0) {
                                    let v = form.notes_file.trim().to_owned();
                                    if !v.is_empty() {
                                        edits.push(SettingsEdit::Set("notes.file", v.into()));
                                    }
                                }
                                ui.end_row();
                                ui.label("Shortcut");
                                let saved = form.saved_notes_shortcut.clone();
                                if field(ui, "notes.shortcut", &mut form.notes_shortcut, &saved, 160.0)
                                {
                                    let v = form.notes_shortcut.trim().to_owned();
                                    if shortcut_problem(&v).is_none() {
                                        edits.push(SettingsEdit::Set("notes.shortcut", v.into()));
                                    }
                                }
                                ui.end_row();
                            });
                            if let Some(e) = shortcut_problem(form.notes_shortcut.trim()) {
                                ui.colored_label(error_color, e);
                            }

                            section(ui, "Formatting");
                            let mut on = form.settings.format_on_save;
                            let r = ui.checkbox(&mut on, "Format on save");
                            remember(ui, "format.onSave", &r);
                            if r.changed() {
                                edits.push(SettingsEdit::Set("format.onSave", on.into()));
                            }
                            ui.label(
                                RichText::new(
                                    "Formatter per file type. The file's text goes to the \
                                     command's stdin and the result is read from stdout; \
                                     {file} is the file's path, ~/ your home folder. \
                                     Leave the command empty to turn formatting off for \
                                     that type; without an entry the language server formats.",
                                )
                                .color(weak),
                            );
                            formatter_rows(ui, &mut form, &mut edits, error_color, weak);

                            section(ui, "C/C++");
                            let mut on = form.settings.clangd_background_index;
                            let r = ui.checkbox(
                                &mut on,
                                "clangd: index the whole project in the background (uses more memory)",
                            );
                            remember(ui, "clangd.backgroundIndex", &r);
                            if r.changed() {
                                edits.push(SettingsEdit::Set("clangd.backgroundIndex", on.into()));
                            }
                        });
                    });
            });

        if save {
            self.save(di, false);
        }
        if to_json {
            self.toggle_settings_view(idx);
        }
        let changed = !edits.is_empty();
        for e in edits {
            self.edit_settings(di, view, e);
        }
        if changed {
            ui.ctx().request_repaint(); // show the saved state
        }
        self.settings_form = Some(form);
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(22.0);
    ui.label(RichText::new(title).strong());
    ui.add_space(8.0);
}

/// What is wrong with a shortcut text, if anything.
fn shortcut_problem(text: &str) -> Option<String> {
    match settings::parse_shortcut(text) {
        Err(e) => Some(e),
        Ok(sc) => settings::conflict(sc).map(|c| {
            format!(
                "{} is already used by \"{}\"",
                crate::commands::format_shortcut(&sc),
                c.label()
            )
        }),
    }
}

fn formatter_rows(
    ui: &mut egui::Ui,
    form: &mut SettingsForm,
    edits: &mut Vec<SettingsEdit>,
    error_color: egui::Color32,
    weak: egui::Color32,
) {
    let Some(rows) = &mut form.rows else {
        ui.colored_label(
            error_color,
            "\"format.commands\" is not an object; fix it as text.",
        );
        return;
    };
    let exts: Vec<String> = rows.iter().map(|r| r.saved_ext.clone()).collect();
    let mut remove = None;
    // The command field takes what the type field and the button leave.
    let cmd_w = (ui.available_width() - 70.0 - 90.0 - 3.0 * 12.0).clamp(160.0, 560.0);
    // Plain rows, not a Grid: a grid sizes its columns from the first frame and
    // squeezes the fields.
    ui.spacing_mut().item_spacing = egui::vec2(12.0, 10.0);
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.add_sized(
                [70.0, 18.0],
                egui::Label::new(RichText::new("Type").color(weak)),
            );
            ui.label(RichText::new("Command").color(weak));
        });
        for (i, row) in rows.iter_mut().enumerate() {
            let key = format!("format.commands.{}", row.saved_ext);
            ui.horizontal(|ui| {
                if row.invalid {
                    ui.label(&row.ext);
                    ui.colored_label(error_color, format!("not a list of text: {}", row.cmd));
                } else {
                    let r1 = ui.add(egui::TextEdit::singleline(&mut row.ext).desired_width(70.0));
                    remember(ui, &format!("{key}.ext"), &r1);
                    let r2 = ui.add(egui::TextEdit::singleline(&mut row.cmd).desired_width(cmd_w));
                    remember(ui, &format!("{key}.cmd"), &r2);
                    let escaped = (r1.lost_focus() || r2.lost_focus())
                        && ui.input(|i| i.key_pressed(Key::Escape));
                    if escaped {
                        row.ext = row.saved_ext.clone();
                        row.cmd = row.saved_cmd.clone();
                        row.error = None;
                    } else if (r1.lost_focus() || r2.lost_focus())
                        && (row.ext != row.saved_ext || row.cmd != row.saved_cmd)
                    {
                        let others = exts
                            .iter()
                            .enumerate()
                            .filter(|(j, _)| *j != i)
                            .map(|(_, e)| e.clone());
                        match check_command(&row.ext, &row.cmd, others) {
                            Ok((ext, argv)) => {
                                row.error = None;
                                edits.push(SettingsEdit::SetCommand {
                                    old: Some(row.saved_ext.clone()),
                                    ext,
                                    argv,
                                });
                            }
                            Err(e) => row.error = Some(e),
                        }
                    }
                }
                if ui.button("Remove").clicked() {
                    remove = Some(row.saved_ext.clone());
                }
            });
            if let Some(e) = &row.error {
                ui.colored_label(error_color, e);
            }
        }
        ui.horizontal(|ui| {
            let r1 = ui.add(
                egui::TextEdit::singleline(&mut form.new_ext)
                    .hint_text("type")
                    .desired_width(70.0),
            );
            remember(ui, "format.commands.new.ext", &r1);
            let r2 = ui.add(
                egui::TextEdit::singleline(&mut form.new_cmd)
                    .hint_text("command")
                    .desired_width(cmd_w),
            );
            remember(ui, "format.commands.new.cmd", &r2);
            let add = ui.button("Add");
            remember(ui, "format.commands.new.add", &add);
            let enter = (r1.lost_focus() || r2.lost_focus())
                && ui.input(|i| i.key_pressed(Key::Enter))
                && !form.new_ext.trim().is_empty();
            if add.clicked() || enter {
                match check_command(&form.new_ext, &form.new_cmd, exts.iter().cloned()) {
                    Ok((ext, argv)) => {
                        form.new_error = None;
                        form.new_ext.clear();
                        form.new_cmd.clear();
                        edits.push(SettingsEdit::SetCommand {
                            old: None,
                            ext,
                            argv,
                        });
                    }
                    Err(e) => form.new_error = Some(e),
                }
            }
        });
    });
    if let Some(e) = &form.new_error {
        ui.colored_label(error_color, e);
    }
    if let Some(ext) = remove {
        edits.push(SettingsEdit::RemoveCommand(ext));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_rows_are_validated() {
        let none = || std::iter::empty::<String>();
        assert_eq!(
            check_command(" .cpp ", "u -q {file}", none()),
            Ok(("cpp".into(), vec!["u".into(), "-q".into(), "{file}".into()]))
        );
        assert_eq!(check_command("rs", "", none()), Ok(("rs".into(), vec![])));
        assert!(check_command("", "x", none()).is_err());
        assert!(check_command("c", "x 'y", none()).is_err());
        assert!(check_command("c", "x", ["c".to_owned()].into_iter()).is_err());
    }

    #[test]
    fn form_reads_the_users_file() {
        let text =
            r#"{"format.commands": {"cpp": ["~/u", "-c", "a b"], "x": 3}, "notes.file": "~/n.md"}"#;
        let f = SettingsForm::from_text(1, 1, text);
        assert_eq!(f.notes_file, "~/n.md");
        let rows = f.rows.unwrap();
        assert_eq!(rows[0].cmd, "~/u -c 'a b'");
        assert!(rows[1].invalid);
        assert!(
            SettingsForm::from_text(1, 1, "{ nope")
                .parse_error
                .is_some()
        );
    }
}
