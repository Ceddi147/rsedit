//! Signature help: while typing a call, a popup above the caret shows the
//! function's signature with the parameter being typed highlighted.

use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, Color32, FontId, Rect, RichText, pos2};

use crate::lsp::{ServerKey, SignatureHelp};

/// A signature help session in one editor.
#[derive(Default)]
pub struct Signature {
    /// The document and view it belongs to.
    pub doc: u64,
    pub view: u64,
    /// (revision, caret) of the latest request: an answer for another state is stale.
    pub at: (u64, usize),
    pub request: Option<(ServerKey, i64)>,
    /// What to show; `None` while closed or waiting for the first answer.
    pub help: Option<SignatureHelp>,
    /// A request was made and the session isn't closed.
    pub active: bool,
}

impl Signature {
    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// Draws the popup above the caret (below it when there's no room above).
    pub fn ui(&self, ctx: &egui::Context, caret: Rect, md: &mut egui_commonmark::CommonMarkCache) {
        let Some(help) = &self.help else { return };
        let above = caret.top() > 120.0;
        let (pivot, y) = if above {
            (Align2::LEFT_BOTTOM, caret.top() - 2.0)
        } else {
            (Align2::LEFT_TOP, caret.bottom() + 2.0)
        };
        let screen = ctx.content_rect();
        let x = caret.left().min(screen.right() - 620.0).max(screen.left());
        egui::Area::new(egui::Id::unique("signature-help"))
            .order(egui::Order::Middle) // below the completion popup
            .pivot(pivot)
            .fixed_pos(pos2(x, y))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(600.0);
                    let strong = ui.visuals().hyperlink_color;
                    let color = ui.visuals().text_color();
                    ui.horizontal_top(|ui| {
                        ui.label(label_job(&help.label, help.param.clone(), color, strong));
                        if help.count > 1 {
                            ui.label(
                                RichText::new(format!("{}/{}", help.index + 1, help.count)).weak(),
                            );
                        }
                    });
                    if !help.doc.is_empty() {
                        ui.separator();
                        crate::editor::hover_ui(ui, md, &help.doc);
                    }
                });
            });
    }
}

/// The signature as monospace text with the active parameter in `strong` and
/// underlined (there is no bold monospace font).
pub fn label_job(
    label: &str,
    param: Option<std::ops::Range<usize>>,
    color: Color32,
    strong: Color32,
) -> LayoutJob {
    let font = FontId::monospace(13.0);
    let plain = TextFormat::simple(font.clone(), color);
    let mut job = LayoutJob::default();
    job.wrap.max_width = 600.0;
    match param.filter(|r| {
        r.end <= label.len() && label.is_char_boundary(r.start) && label.is_char_boundary(r.end)
    }) {
        Some(r) => {
            job.append(&label[..r.start], 0.0, plain.clone());
            let mut active = TextFormat::simple(font, strong);
            active.underline = egui::Stroke::new(1.0, strong);
            job.append(&label[r.clone()], 0.0, active);
            job.append(&label[r.end..], 0.0, plain);
        }
        None => job.append(label, 0.0, plain),
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_job_marks_the_active_parameter() {
        let label = "add(int a, double b)";
        let job = label_job(label, Some(11..19), Color32::WHITE, Color32::BLUE);
        let parts: Vec<&str> = job
            .sections
            .iter()
            .map(|s| &job.text[s.byte_range.start.0..s.byte_range.end.0])
            .collect();
        assert_eq!(parts, ["add(int a, ", "double b", ")"]);
        assert_eq!(job.sections[1].format.color, Color32::BLUE);
        // No parameter, a bad range, or one inside a multi-byte char: one plain section.
        assert_eq!(
            label_job(label, None, Color32::WHITE, Color32::BLUE)
                .sections
                .len(),
            1
        );
        assert_eq!(
            label_job(label, Some(5..99), Color32::WHITE, Color32::BLUE)
                .sections
                .len(),
            1
        );
        let uni = "f(é, n)";
        assert_eq!(
            label_job(uni, Some(3..4), Color32::WHITE, Color32::BLUE)
                .sections
                .len(),
            1
        );
    }
}
