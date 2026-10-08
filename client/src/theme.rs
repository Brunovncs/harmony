//! Colours, type and corners. A dark or light base, one accent, and colour for state only where
//! something needs attention: speaking, live, muted, an error. Panes are rounded cards a step
//! above the page, with hairline edges instead of heavy shadows.
//!
//! The palettes are Harmony's own (Midnight, Dark, Onyx…, plus a custom one built from four
//! colours) and a System palette that follows Windows: light or dark, and its accent colour.

use gpui::{Hsla, Rgba, WindowAppearance, rgb};
use std::cell::Cell;

pub const FONT: &str = "Geist";
pub const MONO: &str = "Geist Mono";

/// The fonts the window draws with, so it looks the same everywhere.
pub fn fonts() -> Vec<std::borrow::Cow<'static, [u8]>> {
    vec![
        include_bytes!("../assets/fonts/Geist-Regular.ttf").as_slice().into(),
        include_bytes!("../assets/fonts/Geist-Medium.ttf").as_slice().into(),
        include_bytes!("../assets/fonts/Geist-SemiBold.ttf").as_slice().into(),
        include_bytes!("../assets/fonts/Geist-Bold.ttf").as_slice().into(),
        include_bytes!("../assets/fonts/GeistMono-Regular.ttf").as_slice().into(),
        include_bytes!("../assets/fonts/GeistMono-Medium.ttf").as_slice().into(),
        include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf").as_slice().into(),
    ]
}

thread_local! {
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
    static CURRENT: Cell<Option<Theme>> = const { Cell::new(None) };
}

/// The theme the window is drawn in now; the root view sets it before each frame, and views
/// deeper down (text fields, popovers) read it.
pub fn set_current(t: Theme) {
    CURRENT.with(|c| c.set(Some(t)));
}

pub fn current() -> Theme {
    CURRENT.with(|c| c.get()).unwrap_or_else(|| Theme::new("midnight", WindowAppearance::Dark, None))
}

/// The interface size the person picked, 0.7 to 1.8.
pub fn set_scale(scale: f32) {
    SCALE.with(|s| s.set(scale.clamp(0.7, 1.8)));
}

pub fn scale() -> f32 {
    SCALE.with(|s| s.get())
}

/// Logical pixels at the interface size. Every length in the UI goes through this, so the size
/// control scales layout and type together, as the old client's zoom did.
pub fn px(v: f32) -> gpui::Pixels {
    gpui::px(v * scale())
}

/// Type ramp, as (size, line height) in pixels.
pub mod text {
    pub const LABEL: (f32, f32) = (11., 14.);
    pub const CAPTION: (f32, f32) = (12., 16.);
    pub const BODY: (f32, f32) = (14., 20.);
    pub const TITLE: (f32, f32) = (16., 22.);
    pub const DISPLAY: (f32, f32) = (30., 38.);
}

/// Corner radii: the larger the surface, the rounder.
pub mod radius {
    pub const PANE: f32 = 14.;
    pub const CARD: f32 = 12.;
    pub const CONTROL: f32 = 10.;
    pub const INNER: f32 = 7.;
}

/// The gap between panes and around the window's edge.
pub const GUTTER: f32 = 10.;

/// A palette as Harmony defines one.
#[derive(Clone, Copy)]
struct Palette {
    bg: u32,
    surface: u32,
    surface2: u32,
    border: u32,
    border_strong: u32,
    text: u32,
    muted: u32,
    accent: u32,
    danger: u32,
    live: u32,
    warn: u32,
    video: u32,
    light: bool,
}

const fn pal(v: [u32; 12], light: bool) -> Palette {
    Palette {
        bg: v[0],
        surface: v[1],
        surface2: v[2],
        border: v[3],
        border_strong: v[4],
        text: v[5],
        muted: v[6],
        accent: v[7],
        danger: v[8],
        live: v[9],
        warn: v[10],
        video: v[11],
        light,
    }
}

/// Every palette: the name settings store, and the one people see. The order is the one the
/// picker shows.
pub fn palettes() -> [(&'static str, &'static str); 10] {
    [
        ("system", tr!("System", "Sistema")),
        ("midnight", tr!("Midnight", "Meia-noite")),
        ("dark", tr!("Dark", "Escuro")),
        ("onyx", tr!("Onyx", "Ônix")),
        ("ocean", tr!("Ocean", "Oceano")),
        ("forest", tr!("Forest", "Floresta")),
        ("ember", tr!("Ember", "Brasa")),
        ("lavender", tr!("Lavender", "Lavanda")),
        ("daylight", tr!("Daylight", "Dia claro")),
        ("custom", tr!("Custom", "Personalizada")),
    ]
}

fn palette(name: &str) -> Option<Palette> {
    Some(match name {
        "midnight" => pal(
            [0x0f1116, 0x171a21, 0x1f232c, 0x2a2f3a, 0x3b4252, 0xe6e8ee, 0x8b93a7, 0x5b8cff, 0xe05561, 0x3fd07f, 0xe3a008, 0x07090d],
            false,
        ),
        "dark" => pal(
            [0x1e1f22, 0x2b2d31, 0x313338, 0x3f4147, 0x4e5058, 0xdbdee1, 0x949ba4, 0x5865f2, 0xda373c, 0x23a55a, 0xf0b132, 0x000000],
            false,
        ),
        "onyx" => pal(
            [0x000000, 0x0b0b0d, 0x141418, 0x26262c, 0x3a3a42, 0xe8e8ea, 0x8a8a93, 0x7c6cff, 0xe05561, 0x3fd07f, 0xe3a008, 0x000000],
            false,
        ),
        "forest" => pal(
            [0x0e1512, 0x15201b, 0x1c2a23, 0x273a30, 0x35503f, 0xe3ece6, 0x86a295, 0x3fb984, 0xe05561, 0x6ee7a8, 0xe3a008, 0x050a08],
            false,
        ),
        "ember" => pal(
            [0x17100e, 0x211714, 0x2c1f1a, 0x3d2a23, 0x553a30, 0xf0e5e0, 0xa89086, 0xff7a4d, 0xe05561, 0x4fd198, 0xf0b132, 0x0b0605],
            false,
        ),
        "lavender" => pal(
            [0x14111d, 0x1c1829, 0x252036, 0x342c4a, 0x493e66, 0xe9e4f5, 0x998fb5, 0xa78bfa, 0xe05561, 0x5fd3a6, 0xe3a008, 0x0a0810],
            false,
        ),
        "ocean" => pal(
            [0x0b141c, 0x111e29, 0x172836, 0x22394b, 0x2f4e67, 0xdfeaf2, 0x7f99ac, 0x38bdf8, 0xe05561, 0x3fd07f, 0xe3a008, 0x04090d],
            false,
        ),
        "daylight" => pal(
            [0xf2f3f5, 0xffffff, 0xebedef, 0xd4d7dc, 0xb6bbc4, 0x22262b, 0x5c6670, 0x3a6df0, 0xc4303a, 0x1a9550, 0x9a6a00, 0x16181d],
            true,
        ),
        _ => return None,
    })
}

/// The swatches drawn for a palette in the picker: background, surface, accent.
pub fn swatches(name: &str, window: WindowAppearance, custom: Option<&CustomColors>) -> [Hsla; 3] {
    let t = Theme::new(name, window, custom);
    [t.base, t.pane, t.accent]
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CustomColors {
    pub bg: u32,
    pub surface: u32,
    pub text: u32,
    pub accent: u32,
}

impl CustomColors {
    pub fn parse(bg: &str, surface: &str, text: &str, accent: &str) -> Option<CustomColors> {
        Some(CustomColors { bg: hex(bg)?, surface: hex(surface)?, text: hex(text)?, accent: hex(accent)? })
    }

    pub fn from_settings(c: &crate::core::settings::CustomTheme) -> Option<CustomColors> {
        CustomColors::parse(&c.bg, &c.surface, &c.text, &c.accent)
    }

    pub fn midnight() -> CustomColors {
        CustomColors { bg: 0x0f1116, surface: 0x171a21, text: 0xe6e8ee, accent: 0x5b8cff }
    }
}

pub fn hex(s: &str) -> Option<u32> {
    let s = s.trim().trim_start_matches('#');
    let s = if s.len() == 3 { s.chars().flat_map(|c| [c, c]).collect::<String>() } else { s.to_string() };
    (s.len() == 6).then(|| u32::from_str_radix(&s, 16).ok()).flatten()
}

pub fn to_hex(c: u32) -> String {
    format!("#{c:06x}")
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |v: u32, s: u32| ((v >> s) & 0xff) as f32;
    let m = |s: u32| ((ch(a, s) + (ch(b, s) - ch(a, s)) * t).round().clamp(0., 255.) as u32) << s;
    m(16) | m(8) | m(0)
}

fn luminance(c: u32) -> f32 {
    let ch = |s: u32| ((c >> s) & 0xff) as f32 / 255.;
    0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
}

/// The four custom colours turned into a whole palette, the same way the old client derived it.
fn derive(c: CustomColors) -> Palette {
    let light = luminance(c.bg) > 0.5;
    let status = palette(if light { "daylight" } else { "midnight" }).unwrap();
    Palette {
        bg: c.bg,
        surface: c.surface,
        surface2: mix(c.surface, c.text, 0.06),
        border: mix(c.surface, c.text, 0.13),
        border_strong: mix(c.surface, c.text, 0.24),
        text: c.text,
        muted: mix(c.text, c.bg, 0.4),
        accent: c.accent,
        danger: status.danger,
        live: status.live,
        warn: status.warn,
        video: mix(c.bg, 0x000000, if light { 0.88 } else { 0.5 }),
        light,
    }
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    /// The page behind the panes.
    pub base: Hsla,
    /// A pane: channels, chat, members.
    pub pane: Hsla,
    /// Something raised inside a pane: a card, a hovered row.
    pub layer: Hsla,
    pub layer_hover: Hsla,
    /// A control: button, field.
    pub control: Hsla,
    pub control_hover: Hsla,
    /// A sunken track, behind segmented choices and sliders.
    pub well: Hsla,
    pub stroke: Hsla,
    pub stroke_strong: Hsla,
    pub text: Hsla,
    pub text2: Hsla,
    pub text3: Hsla,
    pub accent: Hsla,
    pub accent_soft: Hsla,
    /// Text on an accent fill.
    pub on_accent: Hsla,
    pub critical: Hsla,
    /// Speaking, connected, good.
    pub success: Hsla,
    pub caution: Hsla,
    /// Behind video.
    pub stage: Hsla,
    pub scrim: Hsla,
    /// A floating surface: menus, popovers, tooltips.
    pub popover: Hsla,
}

fn c(v: u32) -> Hsla {
    rgb(v).into()
}

impl Theme {
    pub fn new(name: &str, window: WindowAppearance, custom: Option<&CustomColors>) -> Theme {
        let p = match name {
            "system" => system(window),
            "custom" => derive(custom.copied().unwrap_or_else(CustomColors::midnight)),
            other => palette(other).unwrap_or_else(|| palette("midnight").unwrap()),
        };
        Theme::from_palette(p)
    }

    fn from_palette(p: Palette) -> Theme {
        let dark = !p.light;
        let ink = if dark { gpui::white() } else { gpui::black() };
        let accent = c(p.accent);
        Theme {
            dark,
            base: c(p.bg),
            pane: c(p.surface),
            layer: ink.opacity(0.035),
            layer_hover: ink.opacity(0.06),
            control: c(p.surface2),
            control_hover: c(mix(p.surface2, p.text, 0.06)),
            well: if dark { gpui::black().opacity(0.25) } else { ink.opacity(0.05) },
            stroke: c(p.border),
            stroke_strong: c(p.border_strong),
            text: c(p.text),
            text2: c(p.muted),
            text3: c(mix(p.muted, p.surface, 0.3)),
            accent,
            accent_soft: accent.opacity(if dark { 0.16 } else { 0.12 }),
            on_accent: if luminance(p.accent) > 0.55 { c(0x0b0c10) } else { gpui::white() },
            critical: c(p.danger),
            success: c(p.live),
            caution: c(p.warn),
            stage: c(p.video),
            scrim: gpui::black().opacity(if dark { 0.55 } else { 0.35 }),
            popover: c(if dark { mix(p.surface2, p.bg, 0.25) } else { 0xffffff }),
        }
    }

    /// A status colour as a soft background behind its own text.
    pub fn tint(&self, color: Hsla) -> Hsla {
        color.opacity(if self.dark { 0.14 } else { 0.12 })
    }
}

/// Follows Windows: its light or dark mode, and its accent colour.
fn system(window: WindowAppearance) -> Palette {
    let dark = matches!(window, WindowAppearance::Dark | WindowAppearance::VibrantDark);
    let accent = windows_accent(dark).map(|a| {
        let to = |v: f32| (v.clamp(0., 1.) * 255.).round() as u32;
        to(a.r) << 16 | to(a.g) << 8 | to(a.b)
    });
    if dark {
        let mut p = pal(
            [0x111214, 0x1a1b1f, 0x26282d, 0x2c2e34, 0x3c3f46, 0xf3f4f6, 0x9da3ad, 0x60cdff, 0xff6b78, 0x6ccb5f, 0xfcc419, 0x08090b],
            false,
        );
        p.accent = accent.unwrap_or(p.accent);
        p
    } else {
        let mut p = pal(
            [0xf2f3f5, 0xffffff, 0xf1f2f4, 0xdfe1e5, 0xc4c7cd, 0x1b1c1f, 0x5f6570, 0x005fb8, 0xc42b1c, 0x0f7b0f, 0x9d5d00, 0x16181d],
            true,
        );
        p.accent = accent.unwrap_or(p.accent);
        p
    }
}

/// The Windows accent colour in the shade Windows itself uses on this background, or `None`
/// when it is a grey that would not read as an accent.
#[cfg(not(windows))]
fn windows_accent(_: bool) -> Option<Rgba> {
    None
}

#[cfg(windows)]
fn windows_accent(dark: bool) -> Option<Rgba> {
    let palette = registry_binary(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent", "AccentPalette")?;
    // Eight RGBA entries, lightest to darkest; the accent itself is the fourth.
    let i = if dark { 1 } else { 4 };
    let px = palette.get(i * 4..i * 4 + 3)?;
    let (r, g, b) = (px[0], px[1], px[2]);
    let chroma = r.max(g).max(b) - r.min(g).min(b);
    (chroma >= 48).then(|| rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)))
}

#[cfg(windows)]
fn registry_binary(key: &str, value: &str) -> Option<Vec<u8>> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let mut buf = vec![0u8; 64];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(key).as_ptr(),
            wide(value).as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (ok == 0).then(|| {
        buf.truncate(len as usize);
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_palettes_derive_like_the_old_client() {
        let p = derive(CustomColors { bg: 0x000000, surface: 0x101010, text: 0xffffff, accent: 0xff0000 });
        assert!(!p.light);
        assert_eq!(p.muted, mix(0xffffff, 0x000000, 0.4));
        assert_eq!(p.border, mix(0x101010, 0xffffff, 0.13));
        let light = derive(CustomColors { bg: 0xffffff, surface: 0xf0f0f0, text: 0x000000, accent: 0x0000ff });
        assert!(light.light);
        assert_eq!(light.danger, 0xc4303a);
    }

    #[test]
    fn hex_reads_short_and_long_forms() {
        assert_eq!(hex("#fff"), Some(0xffffff));
        assert_eq!(hex("5b8cff"), Some(0x5b8cff));
        assert_eq!(hex("nope"), None);
    }
}
