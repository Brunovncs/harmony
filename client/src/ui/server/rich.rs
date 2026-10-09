//! A message body, drawn: the parsed markdown (see `markdown`) as styled, wrapping text with
//! clickable links, mentions shown by display name, and emoji from shortcodes, custom ones as
//! pictures. A link shows where it goes on hover, and asks first when its text names elsewhere.

use crate::core::types::UserId;
use crate::emoji;
use crate::markdown::{self, Block, Kind, Line, Span};
use crate::session::{Picture, Session};
use crate::theme::{MONO, Theme, px, radius};
use crate::ui::overlay::Ask;
use gpui::{
    AnyElement, App, Context, ElementId, FontStyle, FontWeight, HighlightStyle, Hsla, InteractiveElement, InteractiveText, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, StrikethroughStyle, Styled, StyledText, UnderlineStyle, Window, div, img,
};
use std::ops::Range;

/// What a body needs from the session to draw mentions and custom emoji.
pub struct Lookup<'a> {
    pub session: &'a mut Session,
    pub me: UserId,
}

enum Piece {
    Text { text: String, highlights: Vec<(Range<usize>, HighlightStyle)>, mono: Vec<Range<usize>>, links: Vec<(Range<usize>, String)> },
    Emoji(Option<std::sync::Arc<gpui::RenderImage>>, String),
}

/// A body parsed once, to draw as often as needed.
pub struct Parsed {
    blocks: Vec<Block>,
    /// Only emoji, so drawn big.
    big: bool,
}

impl Parsed {
    pub fn new(text: &str, session: &Session) -> Parsed {
        Parsed { blocks: markdown::parse(text), big: markdown::is_emoji_only(text, |name| session.emoji(name).is_some()) }
    }
}

pub fn body(
    id: impl Into<SharedString>,
    parsed: &Parsed,
    size: f32,
    t: &Theme,
    look: &mut Lookup,
    cx: &mut Context<Session>,
) -> AnyElement {
    let id: SharedString = id.into();
    let size = if parsed.big { size * 2.2 } else { size };
    let mut out = div().flex().flex_col().gap(px(4.)).text_size(px(size)).line_height(px(size * 1.45));
    for (bi, block) in parsed.blocks.iter().enumerate() {
        let bid = SharedString::from(format!("{id}.{bi}"));
        out = out.child(match block {
            Block::Paragraph(lines) => {
                let mut d = div().flex().flex_col();
                for (li, line) in lines.iter().enumerate() {
                    d = d.child(line_el(format!("{bid}.{li}"), line, size, t, look, cx));
                }
                d.into_any_element()
            }
            Block::Heading { level, line } => {
                let s = match level {
                    1 => size * 1.45,
                    2 => size * 1.25,
                    _ => size * 1.1,
                };
                div()
                    .text_size(px(s))
                    .line_height(px(s * 1.35))
                    .font_weight(FontWeight::BOLD)
                    .child(line_el(format!("{bid}.0"), line, s, t, look, cx))
                    .into_any_element()
            }
            Block::Quote(lines) => {
                let mut d = div().flex().flex_col().pl(px(12.)).border_l_2().border_color(t.stroke_strong).text_color(t.text2);
                for (li, line) in lines.iter().enumerate() {
                    d = d.child(line_el(format!("{bid}.{li}"), line, size, t, look, cx));
                }
                d.into_any_element()
            }
            Block::List { ordered, items } => {
                let mut d = div().flex().flex_col().gap(px(2.));
                for (li, item) in items.iter().enumerate() {
                    let mark = if *ordered { format!("{}.", li + 1) } else { "•".into() };
                    d = d.child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(div().flex_none().min_w(px(16.)).text_color(t.text3).child(mark))
                            .child(div().flex_1().min_w(px(0.)).child(line_el(format!("{bid}.{li}"), item, size, t, look, cx))),
                    );
                }
                d.into_any_element()
            }
            Block::Code { text, .. } => div()
                .id(bid)
                .w_full()
                .px(px(12.))
                .py(px(10.))
                .rounded(px(radius::INNER + 1.))
                .bg(t.well)
                .border_1()
                .border_color(t.stroke)
                .font_family(MONO)
                .text_size(px(size * 0.9))
                .line_height(px(size * 1.4))
                .overflow_x_scroll()
                .child(text.clone())
                .into_any_element(),
        });
    }
    out.into_any_element()
}

fn line_el(id: impl Into<SharedString>, line: &Line, size: f32, t: &Theme, look: &mut Lookup, cx: &mut Context<Session>) -> AnyElement {
    let id: SharedString = id.into();
    let pieces = pieces(line, t, look, cx);
    if pieces.is_empty() {
        return div().h(px(size * 1.45)).into_any_element();
    }
    let only_text = matches!(pieces.as_slice(), [Piece::Text { .. }]);
    let mut els = pieces.into_iter().enumerate().map(|(i, p)| {
        match p {
            Piece::Text { text, highlights, mono, links } => {
                let targets: Vec<(String, String)> = links.iter().map(|(r, url)| (text[r.clone()].to_string(), url.clone())).collect();
                let ranges: Vec<Range<usize>> = links.iter().map(|l| l.0.clone()).collect();
                let theme = *t;
                let styled = StyledText::new(SharedString::from(text))
                    .with_highlights(highlights)
                    .with_font_family_overrides(mono.into_iter().map(|r| (r, SharedString::from(MONO))));
                InteractiveText::new(ElementId::Name(format!("{id}.{i}").into()), styled)
                    .on_click(ranges, move |ix, window, cx| {
                        if let Some((label, url)) = targets.get(ix) {
                            open_link(label, url, window, cx);
                        }
                    })
                    .tooltip(move |ix, window, cx| {
                        let (_, url) = links.iter().find(|(r, _)| r.contains(&ix))?;
                        Some(crate::widgets::tip(url.clone(), &theme)(window, cx))
                    })
                    .into_any_element()
            }
            Piece::Emoji(Some(image), _) => img(image).h(px(size * 1.3)).max_w(px(size * 4.)).into_any_element(),
            Piece::Emoji(None, raw) => div().child(raw).into_any_element(),
        }
    });
    // Text as a flex item is measured on one line and can't shrink below it, so a long message ran
    // past the edge. Alone it is a block, which wraps at the width it gets; among emoji, each run
    // may shrink, and wraps once it does.
    if only_text && let Some(el) = els.next() {
        return div().w_full().child(el).into_any_element();
    }
    div().flex().flex_wrap().items_center().children(els.map(|el| div().min_w(px(0.)).child(el))).into_any_element()
}

/// Opens a link, after asking when its text names somewhere other than where it goes.
fn open_link(label: &str, url: &str, window: &mut Window, cx: &mut App) {
    let Some(host) = misleading(label, url) else {
        cx.open_url(url);
        return;
    };
    let url = url.to_string();
    Ask::open(
        tr!("Open this link?", "Abrir este link?"),
        Some(trf!("It reads {} but goes to {}:\n{}", "O texto diz {} mas o link vai para {}:\n{}", label.trim(), host, url)),
        tr!("Open", "Abrir"),
        false,
        Vec::new(),
        window,
        cx,
        move |_, _, cx| {
            cx.open_url(&url);
            None
        },
    );
}

/// Where a link really goes, when its text reads like an address somewhere else: a sender picks
/// the text, the click opens the host.
fn misleading(label: &str, url: &str) -> Option<String> {
    let real = url::Url::parse(url).ok()?.host_str()?.to_string();
    let claimed = claimed_host(label)?;
    let bare = |h: &str| h.trim_end_matches('.').trim_start_matches("www.").to_string();
    (bare(&claimed) != bare(&real)).then_some(real)
}

/// The host a link's text names, if it reads like an address (`example.com`, `www.example.com/a`,
/// `https://example.com`), in the form a parsed URL has it, so `exämple.com` is compared as the
/// punycode it opens as.
fn claimed_host(label: &str) -> Option<String> {
    let s = label.trim();
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    let authority = s.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.chars().any(char::is_whitespace) {
        return None;
    }
    let parsed = url::Url::parse(&format!("http://{authority}/")).ok()?;
    match parsed.host()? {
        url::Host::Domain(d) => {
            let tld = d.trim_end_matches('.').rsplit('.').next()?;
            let named = d.contains('.') && (tld.starts_with("xn--") || tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()));
            named.then(|| d.to_string())
        }
        _ => parsed.host_str().map(str::to_string),
    }
}

fn style_of(span: &Span) -> HighlightStyle {
    HighlightStyle {
        font_weight: span.style.bold.then_some(FontWeight::BOLD),
        font_style: span.style.italic.then_some(FontStyle::Italic),
        strikethrough: span.style.strike.then_some(StrikethroughStyle { thickness: gpui::px(1.), color: None }),
        ..Default::default()
    }
}

fn pieces(line: &Line, t: &Theme, look: &mut Lookup, cx: &mut Context<Session>) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut mono = Vec::new();
    let mut links = Vec::new();
    let flush = |out: &mut Vec<Piece>, text: &mut String, h: &mut Vec<_>, m: &mut Vec<_>, l: &mut Vec<_>| {
        if !text.is_empty() {
            out.push(Piece::Text {
                text: std::mem::take(text),
                highlights: std::mem::take(h),
                mono: std::mem::take(m),
                links: std::mem::take(l),
            });
        }
    };
    for span in line {
        let base = style_of(span);
        let start = text.len();
        let push = |text: &mut String, h: &mut Vec<(Range<usize>, HighlightStyle)>, s: &str, style: HighlightStyle| {
            let a = text.len();
            text.push_str(s);
            h.push((a..text.len(), style));
        };
        match &span.kind {
            Kind::Text(s) => push(&mut text, &mut highlights, s, base),
            Kind::Code(s) => {
                push(&mut text, &mut highlights, s, HighlightStyle { color: Some(t.text), background_color: Some(t.well), ..base });
                mono.push(start..text.len());
            }
            Kind::Link { label, url } => {
                let style = HighlightStyle {
                    color: Some(t.accent),
                    underline: Some(UnderlineStyle { thickness: gpui::px(1.), color: Some(t.accent.opacity(0.5)), wavy: false }),
                    ..base
                };
                push(&mut text, &mut highlights, label, style);
                links.push((start..text.len(), url.clone()));
            }
            Kind::Shortcode { name, raw } => {
                if let Some(custom) = look.session.emoji(name).cloned() {
                    flush(&mut out, &mut text, &mut highlights, &mut mono, &mut links);
                    let pic = match look.session.picture(&custom.hash, cx) {
                        Picture::Ready(img) => Some(img),
                        _ => None,
                    };
                    out.push(Piece::Emoji(pic, raw.clone()));
                } else if let Some(e) = emoji::by_shortcode(name) {
                    push(&mut text, &mut highlights, e, base);
                } else {
                    push(&mut text, &mut highlights, raw, base);
                }
            }
            Kind::Mention { name, raw } => {
                let me = look.session.users.get(&look.me).map(|u| u.nickname.clone()).unwrap_or_default();
                let (shown, mine) = if name == "everyone" {
                    (Some("@everyone".to_string()), true)
                } else {
                    (look.session.user_by_nick(name).map(|u| format!("@{}", u.name())), *name == me)
                };
                match shown {
                    Some(s) => {
                        let color: Hsla = if mine { t.caution } else { t.accent };
                        push(
                            &mut text,
                            &mut highlights,
                            &s,
                            HighlightStyle {
                                color: Some(color),
                                background_color: Some(color.opacity(0.16)),
                                font_weight: Some(FontWeight::MEDIUM),
                                ..base
                            },
                        )
                    }
                    None => push(&mut text, &mut highlights, raw, base),
                }
            }
        }
    }
    flush(&mut out, &mut text, &mut highlights, &mut mono, &mut links);
    out
}

/// A one-line preview of a body: no markdown marks, mentions by name.
pub fn plain(body: &str, session: &Session) -> String {
    let mut out = String::new();
    for block in markdown::parse(body) {
        for span in block.spans() {
            match &span.kind {
                Kind::Text(s) | Kind::Code(s) => out.push_str(s),
                Kind::Link { label, .. } => out.push_str(label),
                Kind::Shortcode { name, raw } => out.push_str(emoji::by_shortcode(name).unwrap_or(raw)),
                Kind::Mention { name, raw } => match session.user_by_nick(name) {
                    Some(u) => {
                        out.push('@');
                        out.push_str(u.name());
                    }
                    None => out.push_str(raw),
                },
            }
        }
        out.push(' ');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_asks_first_only_when_its_text_names_another_host() {
        // The text is the address, or no address at all: straight through.
        assert_eq!(misleading("https://example.com/a", "https://example.com/a"), None);
        assert_eq!(misleading("example.com", "https://www.example.com/"), None);
        assert_eq!(misleading("EXAMPLE.com/docs", "https://example.com/docs"), None);
        assert_eq!(misleading("the docs", "https://example.com/"), None);
        assert_eq!(misleading("**b** :smile: @bob", "http://x.org/"), None);
        assert_eq!(misleading("exämple.com", "https://exämple.com/"), None);
        // The text claims somewhere else.
        assert_eq!(misleading("paypal.com", "https://evil.example/"), Some("evil.example".into()));
        assert_eq!(misleading("https://bank.com/login", "https://bank.com.evil.example/"), Some("bank.com.evil.example".into()));
        assert_eq!(misleading("pаypal.com", "https://paypal.com/"), Some("paypal.com".into()));
        assert_eq!(misleading("10.0.0.1", "http://203.0.113.9/"), Some("203.0.113.9".into()));
    }
}
