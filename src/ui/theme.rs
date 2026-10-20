//! "Spinney Dark" palette, spacing and small styled building blocks, applied once at startup.
//! ponytail: dark only; a light theme can come later.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Stroke, TextStyle, Theme,
};

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const fn rgba(hex: u32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, a)
}

/// Window background (central area, active tab).
pub const BG: Color32 = rgb(0x1b1f2a);
/// Side panels.
pub const PANEL: Color32 = rgb(0x171b24);
/// Tab strip.
pub const TABS: Color32 = rgb(0x141821);
/// Text fields.
pub const FIELD: Color32 = rgb(0x11141b);
pub const BORDER: Color32 = rgb(0x2a3040);
pub const TEXT: Color32 = rgb(0xcfd6e4);
/// Text at ~55% opacity.
pub const MUTED: Color32 = rgba(0xcfd6e4, 140);
pub const ACCENT: Color32 = rgb(0x2ec4b6);
pub const ACCENT_TEXT: Color32 = rgb(0x5fe0d3);
/// Text on an accent fill.
pub const ON_ACCENT: Color32 = rgb(0x062b27);
/// Selected row background.
pub const SEL_BG: Color32 = rgba(0x2ec4b6, 0x1a);
/// Selected item background (sidebar, buttons).
pub const SEL_STRONG: Color32 = rgba(0x2ec4b6, 0x22);
pub const HOVER: Color32 = rgba(0xffffff, 0x0a);
pub const WARN: Color32 = rgb(0xf6a945);
pub const ADDED: Color32 = rgb(0x5fe0d3);
pub const DELETED: Color32 = rgb(0xff6b6b);
/// Diff line tints, normal and selected.
pub const DIFF_ADD: Color32 = rgba(0x2ec4b6, 0x1f);
pub const DIFF_DEL: Color32 = rgba(0xff6b6b, 0x1f);
pub const DIFF_ADD_SEL: Color32 = rgba(0x2ec4b6, 0x48);
pub const DIFF_DEL_SEL: Color32 = rgba(0xff6b6b, 0x48);
pub const RENAMED: Color32 = rgb(0xc084fc);
pub const UNTRACKED: Color32 = rgb(0x8b93a7);

/// Graph lane hues, in order.
pub const LANES: [Color32; 8] = [
    rgb(0x2ec4b6),
    rgb(0xf6a945),
    rgb(0x7c8cff),
    rgb(0xff6b9d),
    rgb(0x9be15d),
    rgb(0xc084fc),
    rgb(0x4fc3f7),
    rgb(0xffd166),
];

/// Corner radius of widgets and of panels/cards.
pub const WIDGET_R: u8 = 6;
pub const CARD_R: u8 = 8;

/// Make the dark style the Spinney Dark one and select it.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, style);
}

fn style(s: &mut egui::Style) {
    use FontFamily::{Monospace, Proportional};
    s.text_styles = [
        (TextStyle::Small, FontId::new(11.0, Proportional)),
        (TextStyle::Body, FontId::new(13.5, Proportional)),
        (TextStyle::Button, FontId::new(13.5, Proportional)),
        (TextStyle::Heading, FontId::new(18.0, Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, Monospace)),
    ]
    .into();
    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(6.0, 4.0);
    sp.button_padding = egui::vec2(8.0, 5.0);
    sp.interact_size.y = 22.0;
    sp.window_margin = Margin::same(14);
    sp.menu_margin = Margin::same(6);

    let v = &mut s.visuals;
    v.dark_mode = true;
    v.weak_text_alpha = 0.55;
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(CARD_R);
    v.menu_corner_radius = CornerRadius::same(CARD_R);
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = HOVER;
    v.code_bg_color = BORDER;
    v.hyperlink_color = ACCENT_TEXT;
    v.warn_fg_color = WARN;
    v.error_fg_color = DELETED;
    v.selection.bg_fill = SEL_STRONG;
    v.selection.stroke = Stroke::new(1.0, ACCENT_TEXT);

    let r = CornerRadius::same(WIDGET_R);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.weak_bg_fill = PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = r;
    w.inactive.weak_bg_fill = rgb(0x232838);
    w.inactive.bg_fill = BORDER;
    w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = r;
    for (wv, fill) in [
        (&mut w.hovered, rgb(0x2a3040)),
        (&mut w.active, rgb(0x323a4e)),
        (&mut w.open, rgb(0x2a3040)),
    ] {
        wv.weak_bg_fill = fill;
        wv.bg_fill = fill;
        wv.bg_stroke = Stroke::new(1.0, rgb(0x3a4256));
        wv.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        wv.corner_radius = r;
        wv.expansion = 0.0;
    }
}

/// Uppercase, small, letter-spaced, muted section header text.
pub fn section(text: &str) -> RichText {
    RichText::new(text.to_uppercase())
        .size(11.0)
        .extra_letter_spacing(0.8)
        .color(MUTED)
}

/// Muted form-field caption.
pub fn caption(text: &str) -> RichText {
    RichText::new(text).size(12.0).color(MUTED)
}

/// Teal call-to-action button.
pub fn primary(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.into()).color(ON_ACCENT).strong())
        .fill(ACCENT)
        .stroke(Stroke::NONE)
}

/// Rounded, bordered card.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(BG)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CARD_R)
        .inner_margin(10)
}

/// Frame of a side panel.
pub fn side_panel() -> egui::Frame {
    egui::Frame::new().fill(PANEL).inner_margin(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_selects_spinney_dark() {
        let ctx = egui::Context::default();
        ctx.set_theme(Theme::Light);
        apply(&ctx);
        assert_eq!(ctx.theme(), Theme::Dark);
        let st = ctx.style_of(Theme::Dark);
        assert_eq!(st.visuals.panel_fill, BG);
        assert_eq!(st.visuals.selection.bg_fill, SEL_STRONG);
        assert_eq!(st.spacing.item_spacing, egui::vec2(6.0, 4.0));
        assert_eq!(st.text_styles[&TextStyle::Body].size, 13.5);
    }

    #[test]
    fn palette_matches_mockup() {
        assert_eq!(BG, Color32::from_rgb(0x1b, 0x1f, 0x2a));
        assert_eq!(LANES[0], ACCENT);
        assert_eq!(LANES[7], Color32::from_rgb(0xff, 0xd1, 0x66));
        assert_eq!(MUTED.a(), 140);
        assert_eq!(SEL_BG.a(), 0x1a);
    }
}
