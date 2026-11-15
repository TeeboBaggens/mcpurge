//! The taupe theme, in a light and a dark variant.
//!
//! Both are built from one palette each, so the two stay in step: change a
//! surface in one and the same role changes in the other. Taupe is a warm
//! grey-brown, which means every neutral here carries a little red and yellow
//! and none of it is a true grey. The status colours (running, warning, error)
//! are warmed to match rather than left at egui's defaults, which are tuned for
//! a blue-grey theme and look like they belong to a different application.

use eframe::egui;
use egui::{Color32, CornerRadius, Stroke, Visuals};

/// Which theme the user asked for. `System` follows the desktop.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "Follow system",
            ThemeMode::Light => "Light taupe",
            ThemeMode::Dark => "Dark taupe",
        }
    }

    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark];
}

impl From<ThemeMode> for egui::ThemePreference {
    fn from(m: ThemeMode) -> Self {
        match m {
            ThemeMode::System => egui::ThemePreference::System,
            ThemeMode::Light => egui::ThemePreference::Light,
            ThemeMode::Dark => egui::ThemePreference::Dark,
        }
    }
}

/// The surfaces and inks of one variant.
struct Palette {
    /// Behind everything: the window itself.
    window: Color32,
    /// Panels (top bar, detail pane, log).
    panel: Color32,
    /// Sunken areas: text fields, the table's scroll body.
    sunken: Color32,
    /// Every other row in a striped table.
    stripe: Color32,
    /// A button or combo at rest.
    control: Color32,
    control_hovered: Color32,
    control_active: Color32,
    /// Hairlines: separators, group outlines, button edges.
    line: Color32,
    line_strong: Color32,
    /// Body text, and text that should recede.
    text: Color32,
    text_weak: Color32,
    /// The selection highlight, used for selected rows and tabs.
    accent: Color32,
    accent_text: Color32,
    /// Status inks, warmed towards the taupe neutrals.
    good: Color32,
    warn: Color32,
    danger: Color32,
    link: Color32,
}

/// Dark taupe: warm browns under a soft parchment ink.
const DARK: Palette = Palette {
    window: Color32::from_rgb(0x26, 0x22, 0x1F),
    panel: Color32::from_rgb(0x2E, 0x29, 0x25),
    sunken: Color32::from_rgb(0x1E, 0x1B, 0x18),
    stripe: Color32::from_rgb(0x33, 0x2D, 0x29),
    control: Color32::from_rgb(0x3C, 0x35, 0x30),
    control_hovered: Color32::from_rgb(0x4A, 0x42, 0x3B),
    control_active: Color32::from_rgb(0x59, 0x4F, 0x46),
    line: Color32::from_rgb(0x44, 0x3D, 0x37),
    line_strong: Color32::from_rgb(0x6B, 0x5F, 0x54),
    text: Color32::from_rgb(0xEA, 0xE2, 0xD8),
    text_weak: Color32::from_rgb(0xA3, 0x97, 0x8A),
    accent: Color32::from_rgb(0x7A, 0x66, 0x52),
    accent_text: Color32::from_rgb(0xF6, 0xEF, 0xE6),
    good: Color32::from_rgb(0x8F, 0xB5, 0x7E),
    warn: Color32::from_rgb(0xD9, 0xA5, 0x5C),
    danger: Color32::from_rgb(0xE0, 0x82, 0x6B),
    link: Color32::from_rgb(0xC2, 0xA2, 0x7E),
};

/// Light taupe: a greige paper with a dark bark ink.
const LIGHT: Palette = Palette {
    window: Color32::from_rgb(0xEF, 0xEA, 0xE2),
    panel: Color32::from_rgb(0xE5, 0xDE, 0xD4),
    sunken: Color32::from_rgb(0xF7, 0xF4, 0xEF),
    stripe: Color32::from_rgb(0xE7, 0xE1, 0xD8),
    control: Color32::from_rgb(0xDA, 0xD2, 0xC6),
    control_hovered: Color32::from_rgb(0xCD, 0xC3, 0xB5),
    control_active: Color32::from_rgb(0xBE, 0xB2, 0xA1),
    line: Color32::from_rgb(0xC4, 0xBA, 0xAC),
    line_strong: Color32::from_rgb(0x8E, 0x81, 0x70),
    text: Color32::from_rgb(0x2E, 0x28, 0x22),
    text_weak: Color32::from_rgb(0x6F, 0x65, 0x59),
    accent: Color32::from_rgb(0xB8, 0xA1, 0x89),
    accent_text: Color32::from_rgb(0x24, 0x1E, 0x19),
    good: Color32::from_rgb(0x4C, 0x75, 0x3C),
    warn: Color32::from_rgb(0x96, 0x60, 0x18),
    danger: Color32::from_rgb(0xA8, 0x3E, 0x28),
    link: Color32::from_rgb(0x7A, 0x5A, 0x33),
};

/// Status colours, pulled from whichever variant is live so a caller does not
/// have to know which one that is. Reading them from `Visuals` is what keeps a
/// green "running" label legible on parchment as well as on bark.
pub struct Status {
    pub good: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub weak: Color32,
}

pub fn status(visuals: &Visuals) -> Status {
    let p = if visuals.dark_mode { &DARK } else { &LIGHT };
    Status {
        good: p.good,
        warn: p.warn,
        danger: p.danger,
        weak: p.text_weak,
    }
}

fn visuals(p: &Palette, dark_mode: bool) -> Visuals {
    let mut v = if dark_mode {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.dark_mode = dark_mode;
    v.override_text_color = Some(p.text);
    v.weak_text_color = Some(p.text_weak);

    v.window_fill = p.window;
    v.panel_fill = p.panel;
    v.faint_bg_color = p.stripe;
    v.extreme_bg_color = p.sunken;
    v.code_bg_color = p.sunken;
    v.window_stroke = Stroke::new(1.0, p.line);

    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.0, p.accent_text);

    v.hyperlink_color = p.link;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;

    let radius = CornerRadius::same(4);

    // Non-interactive: labels, separators, group outlines.
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.weak_bg_fill = p.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.noninteractive.corner_radius = radius;

    // At rest.
    v.widgets.inactive.bg_fill = p.control;
    v.widgets.inactive.weak_bg_fill = p.control;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.corner_radius = radius;

    // Under the pointer.
    v.widgets.hovered.bg_fill = p.control_hovered;
    v.widgets.hovered.weak_bg_fill = p.control_hovered;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.line_strong);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, p.text);
    v.widgets.hovered.corner_radius = radius;

    // Being pressed.
    v.widgets.active.bg_fill = p.control_active;
    v.widgets.active.weak_bg_fill = p.control_active;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.line_strong);
    v.widgets.active.fg_stroke = Stroke::new(1.5, p.accent_text);
    v.widgets.active.corner_radius = radius;

    // Open menus and combo boxes.
    v.widgets.open.bg_fill = p.control_active;
    v.widgets.open.weak_bg_fill = p.control_active;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.line_strong);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.open.corner_radius = radius;

    v
}

/// Install both variants and set which one is used.
///
/// Both are registered every time, so the system can flip between light and
/// dark underneath a `System` preference without the app having to notice.
pub fn apply(ctx: &egui::Context, mode: ThemeMode) {
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&DARK, true));
    ctx.set_visuals_of(egui::Theme::Light, visuals(&LIGHT, false));
    ctx.set_theme(mode);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Readability is not a matter of taste, so it is checked. WCAG AA for
    /// body text is 4.5:1; these are large-ish UI strings on their own
    /// surface, and anything under 4.5 here would be a real complaint.
    fn luminance(c: Color32) -> f32 {
        let f = |v: u8| {
            let s = v as f32 / 255.0;
            if s <= 0.040_45 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn body_text_is_readable_on_every_surface() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for (surface_name, surface) in [
                ("window", p.window),
                ("panel", p.panel),
                ("sunken", p.sunken),
                ("stripe", p.stripe),
                ("control", p.control),
            ] {
                let c = contrast(p.text, surface);
                assert!(
                    c >= 4.5,
                    "{name} text on {surface_name}: contrast {c:.2} is below 4.5"
                );
            }
        }
    }

    #[test]
    fn status_colours_and_weak_text_stay_legible() {
        // 3:1 is the WCAG floor for large text and for meaningful graphics,
        // which is what these are: short coloured labels and markers.
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            for (ink_name, ink) in [
                ("good", p.good),
                ("warn", p.warn),
                ("danger", p.danger),
                ("weak text", p.text_weak),
                ("link", p.link),
            ] {
                for (surface_name, surface) in [
                    ("window", p.window),
                    ("panel", p.panel),
                    ("sunken", p.sunken),
                ] {
                    let c = contrast(ink, surface);
                    assert!(
                        c >= 3.0,
                        "{name} {ink_name} on {surface_name}: contrast {c:.2} is below 3.0"
                    );
                }
            }
        }
    }

    #[test]
    fn selected_rows_keep_their_text_readable() {
        for (name, p) in [("dark", &DARK), ("light", &LIGHT)] {
            let c = contrast(p.accent_text, p.accent);
            assert!(
                c >= 4.5,
                "{name} selection text on the accent: contrast {c:.2} is below 4.5"
            );
        }
    }

    #[test]
    fn both_variants_build() {
        let d = visuals(&DARK, true);
        let l = visuals(&LIGHT, false);
        assert!(d.dark_mode);
        assert!(!l.dark_mode);
        assert_ne!(d.panel_fill, l.panel_fill);
    }
}
