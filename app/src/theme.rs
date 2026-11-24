//! Near-monochrome tokens (Wada Sanzo #221: carmine, neutral gray, black), fonts, motion curves.
//! The accent marks only what needs a human: changes requested, blocked, hot spots, an
//! over-length draft, errors. Participants differ by glyph and lane, never by colour.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Tokens {
    pub bg: Color32,
    pub surface: Color32,
    pub sunk: Color32,
    pub ink: Color32,
    pub ink2: Color32,
    pub ink3: Color32,
    pub line: Color32,
    pub line2: Color32,
    pub accent: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const LIGHT: Tokens = Tokens {
    bg: hex(0xf6f7f7),
    surface: hex(0xffffff),
    sunk: hex(0xeef0f0),
    ink: hex(0x111314),
    ink2: hex(0x4a5053),
    ink3: hex(0x788083),
    line: hex(0xe3e6e7),
    line2: hex(0xc9cfd1),
    accent: hex(0xa62c37),
};

pub const DARK: Tokens = Tokens {
    bg: hex(0x0e1011),
    surface: hex(0x151819),
    sunk: hex(0x1a1d1f),
    ink: hex(0xeceeee),
    ink2: hex(0xb6bfc1),
    ink3: hex(0x7f878a),
    line: hex(0x232729),
    line2: hex(0x363c3e),
    accent: hex(0xe0646f),
};

pub fn tokens(ctx: &egui::Context) -> Tokens {
    if ctx.global_style().visuals.dark_mode { DARK } else { LIGHT }
}

/// CSS `color-mix(in srgb, a p%, b)`.
pub fn mix(a: Color32, p: f32, b: Color32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 * p + y as f32 * (1.0 - p)).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

pub fn fade(c: Color32, alpha: f32) -> Color32 {
    c.gamma_multiply(alpha.clamp(0.0, 1.0))
}

pub fn sans(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

/// Segoe UI and Cascadia Mono on Windows (the web UI's system-ui and mono stacks), egui's
/// built-in fonts as the fallback for anything they lack.
pub fn install(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    let fonts_dir = std::env::var_os("WINDIR").map(|w| std::path::PathBuf::from(w).join("Fonts"));
    let mut load = |key: &str, files: &[&str]| -> bool {
        let Some(dir) = &fonts_dir else { return false };
        for f in files {
            if let Ok(bytes) = std::fs::read(dir.join(f)) {
                defs.font_data.insert(key.into(), Arc::new(FontData::from_owned(bytes)));
                return true;
            }
        }
        false
    };
    let sans_ok = load("ui", &["segoeui.ttf"]);
    let bold_ok = load("ui-bold", &["seguisb.ttf", "segoeuib.ttf"]);
    let mono_ok = load("ui-mono", &["CascadiaMono.ttf", "consola.ttf"]);
    let fallback_sans = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let fallback_mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let chain = |first: Option<&str>, rest: &[String]| -> Vec<String> { first.map(str::to_string).into_iter().chain(rest.iter().cloned()).collect() };
    defs.families.insert(FontFamily::Proportional, chain(sans_ok.then_some("ui"), &fallback_sans));
    defs.families.insert(FontFamily::Monospace, chain(mono_ok.then_some("ui-mono"), &fallback_mono));
    let bold_chain = chain(bold_ok.then_some("ui-bold"), &defs.families[&FontFamily::Proportional].clone());
    defs.families.insert(FontFamily::Name("bold".into()), bold_chain);
    ctx.set_fonts(defs);
    ctx.set_theme(egui::ThemePreference::System);
}

/// Widget visuals from the tokens: flat, hairline borders, no default fills.
pub fn apply(ctx: &egui::Context, t: &Tokens) {
    ctx.global_style_mut(|s| {
        let v = &mut s.visuals;
        v.panel_fill = t.bg;
        v.window_fill = t.surface;
        v.extreme_bg_color = t.surface;
        v.faint_bg_color = t.sunk;
        v.override_text_color = Some(t.ink);
        v.hyperlink_color = t.ink;
        v.selection.bg_fill = fade(t.ink3, 0.35);
        v.selection.stroke = Stroke::new(1.0, t.ink);
        v.text_cursor.stroke = Stroke::new(1.5, t.ink);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.bg_fill = Color32::TRANSPARENT;
            w.weak_bg_fill = Color32::TRANSPARENT;
            w.bg_stroke = Stroke::NONE;
            w.fg_stroke = Stroke::new(1.0, t.ink);
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.line);
        s.spacing.scroll.bar_width = 6.0;
        s.spacing.scroll.floating = true;
        s.spacing.item_spacing = egui::vec2(0.0, 0.0);
        s.interaction.selectable_labels = false;
    });
}

/// The web UI's spring, `linear(0, .22 7%, .6 17%, .88 28%, 1.02 40%, 1.05 49%, 1.02 62%, 1 76%, 1)`:
/// slight overshoot, settles by 76%.
pub fn spring(t: f32) -> f32 {
    const P: [(f32, f32); 9] = [(0.0, 0.0), (0.07, 0.22), (0.17, 0.6), (0.28, 0.88), (0.40, 1.02), (0.49, 1.05), (0.62, 1.02), (0.76, 1.0), (1.0, 1.0)];
    let t = t.clamp(0.0, 1.0);
    for w in P.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if t <= x1 {
            return y0 + (y1 - y0) * (t - x0) / (x1 - x0);
        }
    }
    1.0
}

/// True when Windows has "Animation effects" turned off (the reduced-motion preference).
pub fn reduced_motion() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};
        let mut on: i32 = 1;
        // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer we pass.
        let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut on).cast(), 0) };
        ok != 0 && on == 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spring_overshoots_then_settles() {
        assert_eq!(spring(0.0), 0.0);
        assert!((spring(0.49) - 1.05).abs() < 1e-6);
        assert_eq!(spring(0.9), 1.0);
        assert_eq!(mix(Color32::WHITE, 0.5, Color32::BLACK), Color32::from_rgb(128, 128, 128));
    }
}
