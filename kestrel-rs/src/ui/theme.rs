//! The Kestrel palette, shared with the Roku channel and the PyQt client.
//!
//! Copper on ink: it suits the name and keeps the app visually clear of any
//! camera vendor's own branding.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

pub const INK: Color32 = Color32::from_rgb(0x0C, 0x11, 0x16);
pub const PANEL: Color32 = Color32::from_rgb(0x16, 0x1C, 0x23);
pub const PANEL_ALT: Color32 = Color32::from_rgb(0x21, 0x2A, 0x33);
pub const BORDER: Color32 = Color32::from_rgb(0x2E, 0x3A, 0x45);
pub const BORDER_SOFT: Color32 = Color32::from_rgb(0x22, 0x2C, 0x36);

pub const ACCENT: Color32 = Color32::from_rgb(0xE0, 0x79, 0x3A);
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(0xF5, 0x9E, 0x5B);
/// Copper knocked back over ink, for filled areas that carry text.
///
/// Full-strength copper is bright enough that small dark text on it reads
/// muddy; this is the same hue at 60% over the background, which takes white
/// text at 6.5:1. Opaque rather than translucent on purpose — these fills sit
/// over live video, and a see-through bar would take its contrast from
/// whatever the camera happens to be pointed at.
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0x8B, 0x4F, 0x2C);

pub const TEXT: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x93, 0xA1, 0xAF);
pub const PLACEHOLDER: Color32 = Color32::from_rgb(0x5F, 0x6D, 0x7A);

pub const OK: Color32 = Color32::from_rgb(0x3F, 0xBF, 0x7F);
pub const WARN: Color32 = Color32::from_rgb(0xF5, 0xC5, 0x42);
pub const ERROR: Color32 = Color32::from_rgb(0xE2, 0x45, 0x3C);

/// Detection colours. Copper stays the app accent, so the busiest category
/// takes it and the rest fan out into hues that hold up against ink.
pub fn event_color(kind: crate::api::EventKind) -> Color32 {
    use crate::api::EventKind::*;
    match kind {
        Motion => WARN,
        Person => ACCENT_BRIGHT,
        Vehicle => Color32::from_rgb(0x7E, 0xA6, 0xC4),
        Pet => OK,
        Face => Color32::from_rgb(0xC8, 0x8B, 0xD6),
        Package => Color32::from_rgb(0xD8, 0xB4, 0x7A),
    }
}

/// egui's bundled font covers Latin text and emoji, but not the arrows and
/// geometric shapes the PTZ pad and header use — those render as empty boxes.
/// DejaVu Sans is embedded as a fallback so a distributed binary does not
/// depend on whatever fonts happen to be installed.
fn install_fonts(ctx: &egui::Context) {
    const DEJAVU: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans.ttf");

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "dejavu".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(DEJAVU)),
    );
    // Appended rather than prepended: keep egui's own font for body text and
    // fall through to DejaVu only for glyphs it lacks.
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("dejavu".to_owned());
    }
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut style = (*ctx.style()).clone();
    let visuals = &mut style.visuals;

    visuals.dark_mode = true;
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = INK;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = INK;
    visuals.faint_bg_color = PANEL;
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER_SOFT);
    visuals.window_corner_radius = CornerRadius::same(10);

    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER_SOFT);

    w.inactive.bg_fill = PANEL_ALT;
    w.inactive.weak_bg_fill = PANEL_ALT;
    w.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.inactive.corner_radius = CornerRadius::same(8);

    w.hovered.bg_fill = BORDER;
    w.hovered.weak_bg_fill = BORDER;
    w.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.hovered.corner_radius = CornerRadius::same(8);

    w.active.bg_fill = ACCENT;
    w.active.weak_bg_fill = ACCENT;
    w.active.fg_stroke = Stroke::new(1.0_f32, INK);
    w.active.corner_radius = CornerRadius::same(8);

    w.open.bg_fill = PANEL_ALT;
    w.open.corner_radius = CornerRadius::same(8);

    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance.
    fn luminance(c: Color32) -> f32 {
        let channel = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// egui derives `RichText::strong()` from `widgets.active.fg_stroke`, which
    /// we deliberately set dark so button labels read against a copper fill.
    /// Any *unstyled* strong label therefore renders near-black on our dark
    /// panels — which is exactly what happened to the device name in the
    /// sidebar. Every colour we put on a panel must clear a readable ratio.
    #[test]
    fn text_is_readable_on_our_surfaces() {
        for (name, colour) in [("TEXT", TEXT), ("TEXT_DIM", TEXT_DIM)] {
            for (surface_name, surface) in [("INK", INK), ("PANEL", PANEL), ("PANEL_ALT", PANEL_ALT)]
            {
                let ratio = contrast(colour, surface);
                assert!(
                    ratio >= 4.5,
                    "{name} on {surface_name} is {ratio:.1}:1, below the 4.5:1 minimum"
                );
            }
        }
    }

    /// The dark-on-copper pairing that made the above necessary must itself
    /// stay readable, otherwise buttons become the problem instead.
    #[test]
    fn button_text_is_readable_on_the_accent() {
        let on_accent = Color32::from_rgb(0x1A, 0x0E, 0x06);
        let ratio = contrast(on_accent, ACCENT);
        assert!(ratio >= 4.5, "button text on copper is only {ratio:.1}:1");
    }
}
