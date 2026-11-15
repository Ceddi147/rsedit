//! The "Quiet" look: warm neutrals, one accent, flat surfaces.
//!
//! Everything is flat fills and 1 px lines (no blur or large shadows), which
//! keeps frames cheap for the CPU rasterizer.

use egui::{Color32, CornerRadius, Shadow, Stroke, Theme, Visuals};

/// The colors the UI code paints with directly; the rest goes into egui's
/// [`Visuals`] (see [`apply`]).
#[derive(Clone, Copy)]
pub struct Palette {
    /// Editor and top bar background.
    pub bg: Color32,
    /// Sidebar background.
    pub side: Color32,
    /// Hairlines between areas.
    pub line: Color32,
    /// Fields and chips.
    pub raised: Color32,
    /// Selected and hovered rows, pressed buttons.
    pub raised2: Color32,
    pub text: Color32,
    pub strong: Color32,
    /// Secondary text; still 4.5:1 against `bg`.
    pub weak: Color32,
    /// Between `text` and `weak`: icons, inactive controls.
    pub mid: Color32,
    pub accent: Color32,
    /// The current line in the editor.
    pub current_line: Color32,
}

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x1B, 0x1A, 0x19),
    side: Color32::from_rgb(0x1F, 0x1E, 0x1C),
    line: Color32::from_rgb(0x2A, 0x28, 0x26),
    raised: Color32::from_rgb(0x24, 0x22, 0x20),
    raised2: Color32::from_rgb(0x2E, 0x2B, 0x28),
    text: Color32::from_rgb(0xE6, 0xE1, 0xDA),
    strong: Color32::from_rgb(0xF4, 0xF0, 0xEA),
    weak: Color32::from_rgb(0x90, 0x89, 0x80),
    mid: Color32::from_rgb(0xA3, 0x9C, 0x92),
    accent: Color32::from_rgb(0xE0, 0xA4, 0x58),
    current_line: Color32::from_rgb(0x22, 0x21, 0x1F),
};

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xFB, 0xFA, 0xF7),
    side: Color32::from_rgb(0xF3, 0xF1, 0xEC),
    line: Color32::from_rgb(0xE4, 0xE0, 0xD8),
    raised: Color32::from_rgb(0xEF, 0xEC, 0xE6),
    raised2: Color32::from_rgb(0xE5, 0xE1, 0xD9),
    text: Color32::from_rgb(0x1F, 0x1D, 0x1A),
    strong: Color32::from_rgb(0x00, 0x00, 0x00),
    weak: Color32::from_rgb(0x6B, 0x66, 0x5E),
    mid: Color32::from_rgb(0x55, 0x50, 0x49),
    accent: Color32::from_rgb(0x94, 0x50, 0x1A),
    current_line: Color32::from_rgb(0xF4, 0xF1, 0xEA),
};

/// The palette for the theme `ui` is drawn in.
pub fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// `over` at `alpha` (0–1) on top of `under`, as an opaque color.
fn mix(under: Color32, over: Color32, alpha: f32) -> Color32 {
    let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * alpha).round() as u8;
    Color32::from_rgb(m(under.r(), over.r()), m(under.g(), over.g()), m(under.b(), over.b()))
}

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.bg;
    v.text_edit_bg_color = Some(p.raised);
    v.faint_bg_color = p.raised;
    v.code_bg_color = p.raised;
    v.weak_text_color = Some(p.weak);
    v.hyperlink_color = p.accent;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    // Small, tight shadows: popups need some separation, and blurred ones are
    // expensive to rasterize on the CPU.
    let shadow = Shadow {
        offset: [0, 2],
        blur: 6,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 70 } else { 22 }),
    };
    v.window_shadow = shadow;
    v.popup_shadow = shadow;
    v.selection.bg_fill = mix(p.bg, p.accent, if dark { 0.28 } else { 0.22 });
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);

    let r = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = p.raised2;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = p.raised2;
    w.hovered.weak_bg_fill = p.raised2;
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.5, p.strong);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = mix(p.raised2, p.text, 0.08);
    w.active.weak_bg_fill = mix(p.raised2, p.text, 0.08);
    w.active.bg_stroke = Stroke::NONE;
    w.active.fg_stroke = Stroke::new(1.5, p.strong);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open.bg_fill = p.raised2;
    w.open.weak_bg_fill = p.raised2;
    w.open.bg_stroke = Stroke::NONE;
    w.open.fg_stroke = Stroke::new(1.0, p.strong);
    w.open.corner_radius = r;
    v
}

/// Installs the Quiet look for both themes.
pub fn apply(ctx: &egui::Context) {
    for (theme, p, dark) in [(Theme::Dark, &DARK, true), (Theme::Light, &LIGHT, false)] {
        ctx.set_visuals_of(theme, visuals(p, dark));
        ctx.style_mut_of(theme, |s| {
            s.spacing.button_padding = egui::vec2(8.0, 3.0);
            s.spacing.item_spacing = egui::vec2(8.0, 4.0);
            s.spacing.menu_margin = egui::Margin::same(6);
            // Show where a link goes before it's clicked (Markdown preview).
            s.url_in_tooltip = true;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance.
    fn lum(c: Color32) -> f32 {
        let f = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (a, b) = (lum(a), lum(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn text_is_readable() {
        for p in [&DARK, &LIGHT] {
            for bg in [p.bg, p.side, p.raised, p.current_line] {
                assert!(contrast(p.text, bg) >= 7.0);
                assert!(contrast(p.weak, bg) >= 4.5, "weak on {bg:?}");
                assert!(contrast(p.accent, bg) >= 4.5, "accent on {bg:?}");
            }
            assert!(contrast(p.weak, p.raised2) >= 4.0);
        }
    }
}
