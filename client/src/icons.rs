//! The interface's icons, drawn as line art on a 24 by 24 grid with one light stroke, so they
//! look the same on every system. The shapes are Lucide's (ISC licence), except the brand mark,
//! the soundboard and the blur. Each is turned into an SVG image in the colour asked for, once,
//! and kept.

use gpui::{Hsla, Image, ImageFormat};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

fn shapes(name: &str) -> &'static str {
    match name {
        // Pads that play sounds.
        "soundboard" => {
            r#"<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><circle cx="15.5" cy="19.5" r="1.5"/><path d="M17 19.5V14l3.5 1.5"/>"#
        }
        "blur" => {
            r#"<circle cx="12" cy="12" r="3"/><circle cx="12" cy="12" r="7" stroke-dasharray="2 3"/><circle cx="12" cy="12" r="10.5" stroke-opacity=".4" stroke-dasharray="1 4"/>"#
        }
        // Nothing, to keep a menu's labels in line where an item has no icon.
        "blank" => "",
        "hash" => r#"<path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18"/>"#,
        "volume" => {
            r#"<path d="M11 4.7a.7.7 0 0 0-1.2-.5L6.4 7.6a1.4 1.4 0 0 1-1 .4H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.4a1.4 1.4 0 0 1 1 .4l3.4 3.4a.7.7 0 0 0 1.2-.5ZM16 9a5 5 0 0 1 0 6M19.4 18.4a9 9 0 0 0 0-12.8"/>"#
        }
        "volume-off" => {
            r#"<path d="M11 4.7a.7.7 0 0 0-1.2-.5L6.4 7.6a1.4 1.4 0 0 1-1 .4H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.4a1.4 1.4 0 0 1 1 .4l3.4 3.4a.7.7 0 0 0 1.2-.5ZM22 9l-6 6M16 9l6 6"/>"#
        }
        "bell" => {
            r#"<path d="M10.3 21a2 2 0 0 0 3.4 0M3.3 15.3A1 1 0 0 0 4 17h16a1 1 0 0 0 .7-1.7C19.4 14 18 12.5 18 8A6 6 0 0 0 6 8c0 4.5-1.4 6-2.7 7.3"/>"#
        }
        "bell-off" => {
            r#"<path d="M10.3 21a2 2 0 0 0 3.4 0M17 17H4a1 1 0 0 1-.7-1.7C4.6 14 6 12.5 6 8a6 6 0 0 1 .3-1.7M2 2l20 20M8.7 3A6 6 0 0 1 18 8c0 2.7.8 4.7 1.7 6"/>"#
        }
        "lock" => r#"<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>"#,
        "mic" => r#"<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2M12 19v3"/>"#,
        "mic-off" => {
            r#"<path d="M2 2l20 20M18.9 13.2A7.1 7.1 0 0 0 19 12v-2M5 10v2a7 7 0 0 0 12 5M15 9.3V5a3 3 0 0 0-5.7-1.3M9 9v3a3 3 0 0 0 5.1 2.1M12 19v3"/>"#
        }
        "headphones" => {
            r#"<path d="M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 18 0v7a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3"/>"#
        }
        "headphones-off" => {
            r#"<path d="M21 14h-1.3M9.1 3.5A9 9 0 0 1 21 12v3.3M2 2l20 20M20.4 20.4A2 2 0 0 1 19 21h-1a2 2 0 0 1-2-2v-3M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 2.6-6.4"/>"#
        }
        "phone-off" => {
            r#"<path d="M10.7 13.3a16 16 0 0 0 3.4 2.6l1.3-1.3a2 2 0 0 1 2.1-.4 12.8 12.8 0 0 0 2.8.7 2 2 0 0 1 1.7 2v3a2 2 0 0 1-2.2 2 19.8 19.8 0 0 1-8.6-3.1 19.4 19.4 0 0 1-3.3-2.7m-2.7-3.3a19.8 19.8 0 0 1-3.1-8.6A2 2 0 0 1 4.1 2h3a2 2 0 0 1 2 1.7 12.8 12.8 0 0 0 .7 2.8 2 2 0 0 1-.4 2.1L8.1 9.9M22 2 2 22"/>"#
        }
        "monitor" => r#"<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>"#,
        "monitor-off" => r#"<path d="M17 17H4a2 2 0 0 1-2-2V5c0-1.5 1-2 1-2M22 15V5a2 2 0 0 0-2-2H9M8 21h8M12 17v4M2 2l20 20"/>"#,
        "screen-share" => r#"<path d="M13 3H4a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-3M8 21h8M12 17v4M17 8l5-5M17 3h5v5"/>"#,
        "screen-share-off" => r#"<path d="M13 3H4a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-3M8 21h8M12 17v4M22 3l-5 5M17 3l5 5"/>"#,
        "camera" => {
            r#"<path d="m16 13 5.2 3.5a.5.5 0 0 0 .8-.4V7.9a.5.5 0 0 0-.8-.4L16 10.5"/><rect x="2" y="6" width="14" height="12" rx="2"/>"#
        }
        "camera-off" => {
            r#"<path d="M10.7 6H14a2 2 0 0 1 2 2v2.5l5.2-3.1a.5.5 0 0 1 .8.5v8.2M16 16a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h2M2 2l20 20"/>"#
        }
        "sparkles" => {
            r#"<path d="M9.9 15.5A2 2 0 0 0 8.5 14.1l-6.1-1.6a.5.5 0 0 1 0-1l6.1-1.6a2 2 0 0 0 1.4-1.4l1.6-6.1a.5.5 0 0 1 1 0l1.6 6.1a2 2 0 0 0 1.4 1.4l6.1 1.6a.5.5 0 0 1 0 1l-6.1 1.6a2 2 0 0 0-1.4 1.4l-1.6 6.1a.5.5 0 0 1-1 0ZM20 3v4M22 5h-4M4 17v2M5 18H3"/>"#
        }
        "image" => {
            r#"<rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21"/>"#
        }
        "ban" => r#"<circle cx="12" cy="12" r="10"/><path d="m4.9 4.9 14.2 14.2"/>"#,
        "settings" => {
            r#"<path d="M12.2 2h-.4a2 2 0 0 0-2 2v.2a2 2 0 0 1-1 1.7l-.4.3a2 2 0 0 1-2 0l-.2-.1a2 2 0 0 0-2.7.7l-.2.4a2 2 0 0 0 .7 2.7l.2.1a2 2 0 0 1 1 1.7v.5a2 2 0 0 1-1 1.7l-.2.1a2 2 0 0 0-.7 2.7l.2.4a2 2 0 0 0 2.7.7l.2-.1a2 2 0 0 1 2 0l.4.3a2 2 0 0 1 1 1.7v.2a2 2 0 0 0 2 2h.4a2 2 0 0 0 2-2v-.2a2 2 0 0 1 1-1.7l.4-.3a2 2 0 0 1 2 0l.2.1a2 2 0 0 0 2.7-.7l.2-.4a2 2 0 0 0-.7-2.7l-.2-.1a2 2 0 0 1-1-1.7v-.5a2 2 0 0 1 1-1.7l.2-.1a2 2 0 0 0 .7-2.7l-.2-.4a2 2 0 0 0-2.7-.7l-.2.1a2 2 0 0 1-2 0l-.4-.3a2 2 0 0 1-1-1.7V4a2 2 0 0 0-2-2Z"/><circle cx="12" cy="12" r="3"/>"#
        }
        "plus" => r#"<path d="M5 12h14M12 5v14"/>"#,
        "minus" => r#"<path d="M5 12h14"/>"#,
        "search" => r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>"#,
        "send" => {
            r#"<path d="M14.5 21.7a.5.5 0 0 0 .9 0l6.5-19a.5.5 0 0 0-.6-.6l-19 6.5a.5.5 0 0 0 0 .9l7.9 3.2a2 2 0 0 1 1.1 1.1ZM21.9 2.1 10.9 13.1"/>"#
        }
        "smile" => r#"<circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01"/>"#,
        "smile-plus" => r#"<path d="M22 11v1a10 10 0 1 1-9-10M8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01M16 5h6M19 2v6"/>"#,
        "pin" => {
            r#"<path d="M12 17v5M9 10.8a2 2 0 0 1-1.1 1.8l-1.8.9A2 2 0 0 0 5 15.2V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.8a2 2 0 0 0-1.1-1.8l-1.8-.9A2 2 0 0 1 15 10.8V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1Z"/>"#
        }
        "edit" => r#"<path d="M21.2 6.8a1 1 0 0 0-4-4L3.8 16.2a2 2 0 0 0-.5.8L2 21.4a.5.5 0 0 0 .6.6l4.4-1.3a2 2 0 0 0 .8-.5ZM15 5l4 4"/>"#,
        "delete" => r#"<path d="M3 6h18M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2M10 11v6M14 11v6"/>"#,
        "reply" => r#"<path d="m9 17-5-5 5-5M20 18v-2a4 4 0 0 0-4-4H4"/>"#,
        "paperclip" => r#"<path d="m21.4 11.1-9.2 9.2a6 6 0 0 1-8.5-8.5l8.6-8.6A4 4 0 1 1 18 8.8l-8.6 8.6a2 2 0 0 1-2.8-2.8l8.5-8.5"/>"#,
        "users" => {
            r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M22 21v-2a4 4 0 0 0-3-3.9M16 3.1a4 4 0 0 1 0 7.8"/><circle cx="9" cy="7" r="4"/>"#
        }
        "crown" => {
            r#"<path d="M11.6 3.3a.5.5 0 0 1 .9 0l2.9 5.6a1 1 0 0 0 1.5.3l4.3-3.7a.5.5 0 0 1 .8.5l-2.8 10.2a1 1 0 0 1-1 .7H5.8a1 1 0 0 1-1-.7L2 6a.5.5 0 0 1 .8-.5l4.3 3.7a1 1 0 0 0 1.5-.3ZM5 21h14"/>"#
        }
        "shield" => {
            r#"<path d="M20 13c0 5-3.5 7.5-7.7 9a1 1 0 0 1-.7 0C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.2-2.7a1.2 1.2 0 0 1 1.5 0C14.5 3.8 17 5 19 5a1 1 0 0 1 1 1Z"/>"#
        }
        "shield-off" => {
            r#"<path d="m2 2 20 20M5 5a1 1 0 0 0-1 1v7c0 5 3.5 7.5 7.7 8.9a1 1 0 0 0 .7 0c2.3-.8 4.5-2 5.9-3.7M9.3 3.7a12.3 12.3 0 0 0 1.9-1.4 1.2 1.2 0 0 1 1.5 0C14.5 3.8 17 5 19 5a1 1 0 0 1 1 1v7a9.8 9.8 0 0 1-.1 1.3"/>"#
        }
        "close" => r#"<path d="M18 6 6 18M6 6l12 12"/>"#,
        "check" => r#"<path d="M20 6 9 17l-5-5"/>"#,
        "chevron-right" => r#"<path d="m9 18 6-6-6-6"/>"#,
        "chevron-down" => r#"<path d="m6 9 6 6 6-6"/>"#,
        "arrow-left" => r#"<path d="m12 19-7-7 7-7M19 12H5"/>"#,
        "arrow-right" => r#"<path d="M5 12h14M12 5l7 7-7 7"/>"#,
        "info" => r#"<circle cx="12" cy="12" r="10"/><path d="M12 16v-4M12 8h.01"/>"#,
        "warning" => r#"<path d="m21.7 18-8-14a2 2 0 0 0-3.5 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.7-3M12 9v4M12 17h.01"/>"#,
        "clock" => r#"<circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>"#,
        "log-out" => r#"<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4M16 17l5-5-5-5M21 12H9"/>"#,
        "server" => {
            r#"<rect x="2" y="2" width="20" height="8" rx="2"/><rect x="2" y="14" width="20" height="8" rx="2"/><path d="M6 6h.01M6 18h.01"/>"#
        }
        "maximize" => r#"<path d="M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M3 16v3a2 2 0 0 0 2 2h3M16 21h3a2 2 0 0 0 2-2v-3"/>"#,
        "minimize" => r#"<path d="M8 3v3a2 2 0 0 1-2 2H3M21 8h-3a2 2 0 0 1-2-2V3M3 16h3a2 2 0 0 1 2 2v3M16 21v-3a2 2 0 0 1 2-2h3"/>"#,
        "expand" => r#"<path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"/>"#,
        "shrink" => r#"<path d="M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7"/>"#,
        "fullscreen" => {
            r#"<path d="M3 7V5a2 2 0 0 1 2-2h2M17 3h2a2 2 0 0 1 2 2v2M21 17v2a2 2 0 0 1-2 2h-2M7 21H5a2 2 0 0 1-2-2v-2"/><rect x="7" y="8" width="10" height="8" rx="1"/>"#
        }
        "eye" => {
            r#"<path d="M2.1 12.3a1 1 0 0 1 0-.7 10.8 10.8 0 0 1 19.8 0 1 1 0 0 1 0 .7 10.8 10.8 0 0 1-19.8 0"/><circle cx="12" cy="12" r="3"/>"#
        }
        "music" => r#"<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>"#,
        "download" => r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3"/>"#,
        "restart" => r#"<path d="M21 12a9 9 0 1 1-9-9c2.5 0 4.9 1 6.7 2.7L21 8"/><path d="M21 3v5h-5"/>"#,
        "radio" => {
            r#"<circle cx="12" cy="12" r="2"/><path d="M16.2 7.8c2.3 2.3 2.3 6.1 0 8.5M7.8 16.2c-2.3-2.3-2.3-6.1 0-8.5M19.1 4.9C23 8.8 23 15.1 19.1 19M4.9 19.1C1 15.2 1 8.8 4.9 4.9"/>"#
        }
        "keyboard" => {
            r#"<rect x="2" y="4" width="20" height="16" rx="2"/><path d="M6 8h.01M10 8h.01M14 8h.01M18 8h.01M8 12h.01M12 12h.01M16 12h.01M7 16h10"/>"#
        }
        "palette" => {
            r#"<path d="M12 2C6.5 2 2 6.5 2 12s4.5 10 10 10c.9 0 1.6-.7 1.6-1.7 0-.4-.2-.8-.4-1.1-.3-.3-.4-.7-.4-1.1a1.6 1.6 0 0 1 1.7-1.7h2c3 0 5.5-2.5 5.5-5.6C22 6 17.5 2 12 2Z"/><circle cx="13.5" cy="6.5" r=".5"/><circle cx="17.5" cy="10.5" r=".5"/><circle cx="8.5" cy="7.5" r=".5"/><circle cx="6.5" cy="12.5" r=".5"/>"#
        }
        "chevron-up" => r#"<path d="M6 15l6-6 6 6"/>"#,
        "play" => r#"<path d="M7 4.5v15a.8.8 0 0 0 1.2.7l12-7.5a.8.8 0 0 0 0-1.4l-12-7.5A.8.8 0 0 0 7 4.5Z"/>"#,
        _ => r#"<circle cx="12" cy="12" r="2"/>"#,
    }
}

fn svg(name: &str, color: Hsla) -> String {
    let c = color.to_rgb();
    let hex = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="48" height="48" fill="none" stroke="#{:02x}{:02x}{:02x}" stroke-opacity="{:.3}" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">{}</svg>"##,
        hex(c.r),
        hex(c.g),
        hex(c.b),
        c.a,
        shapes(name)
    )
}

thread_local! {
    static CACHE: RefCell<HashMap<(&'static str, u32), Arc<Image>>> = RefCell::new(HashMap::new());
}

/// The icon in that colour, as an image to draw at any size.
pub fn image(name: &'static str, color: Hsla) -> Arc<Image> {
    let c = color.to_rgb();
    let key = u32::from_be_bytes([c.r, c.g, c.b, c.a].map(|v| (v.clamp(0., 1.) * 255.).round() as u8));
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry((name, key))
            .or_insert_with(|| Arc::new(Image::from_bytes(ImageFormat::Svg, svg(name, color).into_bytes())))
            .clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_names_fall_back_to_a_dot() {
        assert_eq!(shapes("nope"), r#"<circle cx="12" cy="12" r="2"/>"#);
        assert!(svg("check", gpui::hsla(0., 0., 1., 1.)).contains("stroke=\"#ffffff\""));
    }
}
