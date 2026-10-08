//! Message bodies: a small markdown dialect, `:shortcode:` emoji and `@nickname` mentions.
//!
//! This is the parse only. Drawing is the renderer's, and so is deciding whether a shortcode or a
//! mention names anything; a token that names nothing is drawn as the text it was.
//!
//! The rules are the Electron client's, including its quirks, because both clients draw the same
//! messages and a message should not change shape depending on who opens it. Its parser was a set
//! of regular expressions; this one scans by hand, and each scanner says which expression it
//! stands for.
//!
//! Block structure is decided line by line. Ordinary lines stay lines, with a break between them,
//! rather than becoming paragraphs: a two-line message is two lines, not two paragraphs with a
//! blank one between.

use crate::emoji::{self, is_js_space};

/// Emphasis in effect for a span. Nested runs add up, so `**_a_**` is bold and italic.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub style: Style,
    pub kind: Kind,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Kind {
    Text(String),
    /// Between backticks. Nothing else applies inside, which is the point of backticks.
    Code(String),
    /// http or https only. `url` is the normalised form, the one to open and to show on hover;
    /// `label` is what was written, drawn as plain text.
    Link {
        label: String,
        url: String,
    },
    /// A syntactically valid `:name:`. `name` is lowercased and has no colons; `raw` is the text
    /// as typed, for when the name resolves to nothing.
    Shortcode {
        name: String,
        raw: String,
    },
    /// `@nickname` or `@everyone`, with the same split: `name` folded, `raw` to fall back to.
    Mention {
        name: String,
        raw: String,
    },
}

pub type Line = Vec<Span>;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Block {
    /// A run of ordinary lines, drawn with a break between each and none after the last. An empty
    /// line is an empty `Line`.
    Paragraph(Vec<Line>),
    /// `#` to `###`.
    Heading {
        level: u8,
        line: Line,
    },
    Quote(Vec<Line>),
    /// Numbered or not according to its first item. The numbers typed are not kept: the Electron
    /// client counts from one whatever they were.
    List {
        ordered: bool,
        items: Vec<Line>,
    },
    /// A fenced block, verbatim. One that is never closed runs to the end of the message.
    Code {
        lang: Option<String>,
        text: String,
    },
}

impl Block {
    /// Every span in the block, in reading order.
    pub fn spans(&self) -> impl Iterator<Item = &Span> {
        let lines: &[Line] = match self {
            Block::Paragraph(lines) | Block::Quote(lines) | Block::List { items: lines, .. } => lines,
            Block::Heading { line, .. } => std::slice::from_ref(line),
            Block::Code { .. } => &[],
        };
        lines.iter().flatten()
    }
}

/// A message body as blocks, in order. An empty body is no blocks.
pub fn parse(body: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    if body.is_empty() {
        return blocks;
    }
    let lines: Vec<&str> = body.split('\n').collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if let Some(lang) = fence(line) {
            let mut end = i + 1;
            while end < lines.len() && !fence_end(lines[end]) {
                end += 1;
            }
            blocks.push(Block::Code { lang, text: lines[i + 1..end].join("\n") });
            i = end + 1;
            continue;
        }

        if let Some((level, text)) = heading(line) {
            blocks.push(Block::Heading { level, line: inline_line(text) });
            i += 1;
            continue;
        }

        if quote(line).is_some() {
            let mut parts = Vec::new();
            while let Some(part) = lines.get(i).and_then(|line| quote(line)) {
                parts.push(inline_line(part));
                i += 1;
            }
            blocks.push(Block::Quote(parts));
            continue;
        }

        if let Some((ordered, _)) = bullet(line) {
            let mut items = Vec::new();
            while let Some((_, item)) = lines.get(i).and_then(|line| bullet(line)) {
                items.push(inline_line(item));
                i += 1;
            }
            blocks.push(Block::List { ordered, items });
            continue;
        }

        // The same four tests that start a block end the run. The line at `i` just failed all of
        // them, so the run always takes at least one line and the loop always advances.
        let mut run = Vec::new();
        while let Some(&line) = lines.get(i)
            && fence(line).is_none()
            && heading(line).is_none()
            && quote(line).is_none()
            && bullet(line).is_none()
        {
            run.push(inline_line(line));
            i += 1;
        }
        blocks.push(Block::Paragraph(run));
    }
    blocks
}

/// Whether a message is nothing but emoji, to draw it large.
///
/// The Electron rule: the body has something besides space, is made only of space, pictographs,
/// joiners, variation selectors, skin-tone modifiers and `:name:`s, and actually shows at least
/// one emoji, either a pictograph or a `:name:` that resolved. A line of colons has the shape
/// and is not something to enlarge. `custom_exists` says whether this server has a custom emoji
/// of that name; the standard set is checked here.
///
/// A flag is regional indicators, which are not pictographs, so a lone flag is not enlarged; nor
/// is a keycap. That is the Electron client's behaviour and is kept.
pub fn is_emoji_only(body: &str, custom_exists: impl Fn(&str) -> bool) -> bool {
    if !body.chars().any(|c| !is_js_space(c)) || !emoji_shaped(body) {
        return false;
    }
    if body.chars().any(is_pictographic) {
        return true;
    }
    // Counted from the parse rather than the raw text, because only a shortcode the renderer will
    // draw as one counts: `:_smile_:` has the shape but is an italic word between colons.
    parse(body).iter().flat_map(Block::spans).any(|span| match &span.kind {
        Kind::Shortcode { name, .. } => custom_exists(name) || emoji::by_shortcode(name).is_some(),
        _ => false,
    })
}

/// Everyone a message mentions, folded, deduplicated, in order of first mention, with
/// `everyone` among them if it was used.
///
/// This is the server's reading, which is the one that decides who gets notified: it scans the
/// raw body, so a mention inside backticks or a code block still counts, though it is drawn as
/// code.
#[cfg_attr(not(test), allow(dead_code))]
pub fn mentions(body: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut p = 0;
    while p < body.len() {
        match mention_at(body, p) {
            Some((end, Kind::Mention { name, .. })) => {
                if !names.contains(&name) {
                    names.push(name);
                }
                p = end;
            }
            _ => p += 1,
        }
    }
    names
}

// --- blocks ------------------------------------------------------------------------------------

/// Up to three leading spaces off, as `^\s{0,3}`. A line indented further keeps a space in front,
/// so whatever is tested next fails, as the expression does.
fn indent(line: &str) -> &str {
    let mut rest = line;
    for _ in 0..3 {
        match rest.chars().next() {
            Some(c) if is_js_space(c) => rest = &rest[c.len_utf8()..],
            _ => break,
        }
    }
    rest
}

fn only_space(text: &str) -> bool {
    text.chars().all(is_js_space)
}

/// `^\s{0,3}```(\w*)\s*$`, giving the language if one was written.
fn fence(line: &str) -> Option<Option<String>> {
    let rest = indent(line).strip_prefix("```")?;
    let lang = rest.bytes().take_while(|&b| is_word(b)).count();
    only_space(&rest[lang..]).then(|| (lang > 0).then(|| rest[..lang].to_owned()))
}

/// `^\s{0,3}```\s*$`
fn fence_end(line: &str) -> bool {
    indent(line).strip_prefix("```").is_some_and(only_space)
}

/// `^(#{1,3})\s+(.+)$`
///
/// `.` stops at a line terminator, so a heading whose line ends in `\r` is not one. And `\s+`
/// gives a space back to `.+` when nothing else follows: `##  ` (two spaces) is a heading of one
/// space, `## ` is not a heading at all.
fn heading(line: &str) -> Option<(u8, &str)> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=3).contains(&level) {
        return None;
    }
    let after = &line[level..];
    let space = after.char_indices().find(|&(_, c)| !is_js_space(c)).map_or(after.len(), |(i, _)| i);
    if space == 0 {
        return None;
    }
    let mut text = &after[space..];
    if text.is_empty() {
        let (last, _) = after.char_indices().last()?;
        if last == 0 {
            return None;
        }
        text = &after[last..];
    }
    (!text.chars().any(|c| matches!(c, '\r' | '\u{2028}' | '\u{2029}'))).then_some((level as u8, text))
}

/// `^\s{0,3}>\s?`, giving what is left of the line.
fn quote(line: &str) -> Option<&str> {
    let rest = indent(line).strip_prefix('>')?;
    Some(match rest.chars().next() {
        Some(c) if is_js_space(c) => &rest[c.len_utf8()..],
        _ => rest,
    })
}

/// `^\s{0,3}(?:[-*+]|\d+[.)])\s+`, giving whether the marker was a number and the item's text.
fn bullet(line: &str) -> Option<(bool, &str)> {
    let rest = indent(line);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (ordered, after) =
        if digits > 0 { (true, rest[digits..].strip_prefix(['.', ')'])?) } else { (false, rest.strip_prefix(['-', '*', '+'])?) };
    let item = after.trim_start_matches(is_js_space);
    (item.len() < after.len()).then_some((ordered, item))
}

// --- inline ------------------------------------------------------------------------------------

fn inline_line(text: &str) -> Line {
    let mut out = Vec::new();
    inline(text, Style::default(), &mut out);
    out
}

enum Emphasis {
    Bold,
    Italic,
    Strike,
}

enum Node<'a> {
    Code(&'a str),
    Run(&'a str, Emphasis),
    Link { label: &'a str, url: String },
}

/// Inline markdown, then shortcodes and mentions in the text between.
///
/// The Electron expression, in its order of preference at any one position:
///
/// ```text
/// (`[^`\n]+`)
/// (\*\*[^\n]+?\*\*)
/// ((?<![\w_])__[^\n]+?__(?![\w_]))
/// (~~[^\n]+?~~)
/// (\*[^*\n]+?\*)
/// ((?<![\w_])_[^_\n]+?_(?![\w_]))
/// (\[[^\]\n]+\]\(https?://[^\s)]+\))
/// (https?://[^\s<]+)
/// ```
///
/// Code comes first so that whatever is inside backticks wins. The underscore forms refuse to
/// start or end inside a word, so snake_case and @big_tuna are left alone. Emphasis recurses
/// into its own text, where the lookarounds see only that text. A link that will not open stays
/// text, and its characters are not looked at again.
fn inline(text: &str, style: Style, out: &mut Line) {
    let (mut last, mut p) = (0, 0);
    while p < text.len() {
        let Some((end, node)) = inline_at(text, p) else {
            p += 1;
            continue;
        };
        if let Some(node) = node {
            plain(&text[last..p], style, out);
            match node {
                Node::Code(code) => out.push(Span { style, kind: Kind::Code(code.to_owned()) }),
                Node::Run(inner, emphasis) => {
                    let mut style = style;
                    match emphasis {
                        Emphasis::Bold => style.bold = true,
                        Emphasis::Italic => style.italic = true,
                        Emphasis::Strike => style.strike = true,
                    }
                    inline(inner, style, out);
                }
                Node::Link { label, url } => out.push(Span { style, kind: Kind::Link { label: label.to_owned(), url } }),
            }
            last = end;
        }
        p = end;
    }
    plain(&text[last..], style, out);
}

/// The match starting at `p`, if any, as where it ends and what it makes. A match that makes
/// nothing (a link that will not open) still ends somewhere: the scan resumes after it.
fn inline_at(text: &str, p: usize) -> Option<(usize, Option<Node<'_>>)> {
    let b = text.as_bytes();
    let run = |open: usize, close: usize, emphasis| Some((close + open, Some(Node::Run(&text[p + open..close], emphasis))));
    match b[p] {
        b'`' => {
            let q = find(b, p + 1, b"`")?;
            (q > p + 1).then(|| (q + 1, Some(Node::Code(&text[p + 1..q]))))
        }
        b'*' => {
            if b.get(p + 1) == Some(&b'*')
                && let Some(q) = find(b, p + 3, b"**")
            {
                return run(2, q, Emphasis::Bold);
            }
            let q = find(b, p + 1, b"*")?;
            if q >= p + 2 { run(1, q, Emphasis::Italic) } else { None }
        }
        b'_' if !word_at(b, p.checked_sub(1)) => {
            if b.get(p + 1) == Some(&b'_') {
                let mut from = p + 3;
                while let Some(q) = find(b, from, b"__") {
                    if !word_at(b, Some(q + 2)) {
                        return run(2, q, Emphasis::Bold);
                    }
                    from = q + 1;
                }
            }
            let q = find(b, p + 1, b"_")?;
            if q >= p + 2 && !word_at(b, Some(q + 1)) { run(1, q, Emphasis::Italic) } else { None }
        }
        b'~' if b.get(p + 1) == Some(&b'~') => run(2, find(b, p + 3, b"~~")?, Emphasis::Strike),
        b'[' => {
            let q = find(b, p + 1, b"]")?;
            if q < p + 2 || b.get(q + 1) != Some(&b'(') {
                return None;
            }
            let href = &text[q + 2..];
            let scheme = scheme_len(href)?;
            let stop = href[scheme..].find(|c: char| c == ')' || is_js_space(c))? + scheme;
            if stop == scheme || !href[stop..].starts_with(')') {
                return None;
            }
            let label = &text[p + 1..q];
            Some((q + 2 + stop + 1, http_url(&href[..stop]).map(|url| Node::Link { label, url })))
        }
        b'h' => {
            let rest = &text[p..];
            let scheme = scheme_len(rest)?;
            let stop = rest[scheme..].find(|c: char| c == '<' || is_js_space(c)).map_or(rest.len(), |i| i + scheme);
            if stop == scheme {
                return None;
            }
            let href = &rest[..stop];
            Some((p + stop, http_url(href).map(|url| Node::Link { label: href, url })))
        }
        _ => None,
    }
}

/// `https?://`, case-sensitive as the expression is.
fn scheme_len(text: &str) -> Option<usize> {
    if text.starts_with("https://") {
        Some(8)
    } else if text.starts_with("http://") {
        Some(7)
    } else {
        None
    }
}

/// The URL as a browser would hold it, or nothing if it does not parse or is not http(s). The
/// window would otherwise be asked to navigate itself to somebody's link.
fn http_url(href: &str) -> Option<String> {
    let url = url::Url::parse(href).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.into())
}

/// Shortcodes and mentions in a run of text that markdown has finished with.
///
/// One scan for both, `:([a-z0-9_]{2,32}):|(?<![\w@])@([a-z0-9][a-z0-9_-]{0,23})`, so neither
/// kind can cut the other in half. The lookbehind sees only this run, which can start a mention
/// right after markdown that ends in a word character: in `_a_@bob`, `@bob` is a mention.
fn plain(text: &str, style: Style, out: &mut Line) {
    let b = text.as_bytes();
    let (mut last, mut p) = (0, 0);
    while p < b.len() {
        let token = match b[p] {
            b':' => shortcode_at(text, p),
            b'@' => mention_at(text, p),
            _ => None,
        };
        let Some((end, kind)) = token else {
            p += 1;
            continue;
        };
        push_text(out, style, &text[last..p]);
        out.push(Span { style, kind });
        last = end;
        p = end;
    }
    push_text(out, style, &text[last..]);
}

/// `:([a-z0-9_]{2,32}):`, case-insensitive. No `-` or `+`: the server folds both into `_` when
/// it stores a name, so a shortcode holding one could never match anything.
fn shortcode_at(text: &str, p: usize) -> Option<(usize, Kind)> {
    let b = text.as_bytes();
    let len = b[p + 1..].iter().take_while(|&&c| is_word(c)).count();
    let end = p + 1 + len;
    ((2..=32).contains(&len) && b.get(end) == Some(&b':'))
        .then(|| (end + 1, Kind::Shortcode { name: text[p + 1..end].to_ascii_lowercase(), raw: text[p..=end].to_owned() }))
}

/// `(?<![\w@])@([a-z0-9][a-z0-9_-]{0,23})`, case-insensitive: the nickname rule. Not after a word
/// character or another `@`, so an email address mentions nobody.
fn mention_at(text: &str, p: usize) -> Option<(usize, Kind)> {
    let b = text.as_bytes();
    if b[p] != b'@' || p.checked_sub(1).is_some_and(|q| is_word(b[q]) || b[q] == b'@') {
        return None;
    }
    if !b.get(p + 1).is_some_and(u8::is_ascii_alphanumeric) {
        return None;
    }
    let len = 1 + b[p + 2..].iter().take_while(|&&c| is_word(c) || c == b'-').take(23).count();
    let end = p + 1 + len;
    Some((end, Kind::Mention { name: text[p + 1..end].to_ascii_lowercase(), raw: text[p..end].to_owned() }))
}

fn push_text(out: &mut Line, style: Style, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Span { style: last, kind: Kind::Text(prev) }) = out.last_mut()
        && *last == style
    {
        prev.push_str(text);
        return;
    }
    out.push(Span { style, kind: Kind::Text(text.to_owned()) });
}

/// `\w` without the `u` flag: ASCII letters, digits and underscore.
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn word_at(b: &[u8], i: Option<usize>) -> bool {
    i.and_then(|i| b.get(i)).is_some_and(|&c| is_word(c))
}

fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

// --- emoji shape -------------------------------------------------------------------------------

/// `^(?:\s|\p{Extended_Pictographic}|[‍️\u{1F3FB}-\u{1F3FF}]|:[a-z0-9_]{2,32}:)+$`
/// with the `i` and `u` flags. Under those two, `[a-z]` also takes the long s and the Kelvin
/// sign, which fold to `s` and `k`.
fn emoji_shaped(text: &str) -> bool {
    let shortcode_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '\u{17F}' || c == '\u{212A}';
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == ':' {
            let name = &rest[1..];
            let len = name.find(|c: char| !shortcode_char(c)).unwrap_or(name.len());
            if !(2..=32).contains(&name[..len].chars().count()) || !name[len..].starts_with(':') {
                return false;
            }
            rest = &name[len + 1..];
        } else if is_js_space(c) || is_pictographic(c) || matches!(c, '\u{200D}' | '\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}') {
            rest = &rest[c.len_utf8()..];
        } else {
            return false;
        }
    }
    true
}

/// Unicode's Extended_Pictographic property (Unicode 16).
fn is_pictographic(c: char) -> bool {
    let c = c as u32;
    EXTENDED_PICTOGRAPHIC
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

#[rustfmt::skip]
const EXTENDED_PICTOGRAPHIC: &[(u32, u32)] = &[
    (0xA9, 0xA9), (0xAE, 0xAE), (0x203C, 0x203C), (0x2049, 0x2049), (0x2122, 0x2122), (0x2139, 0x2139), (0x2194, 0x2199),
    (0x21A9, 0x21AA), (0x231A, 0x231B), (0x2328, 0x2328), (0x2388, 0x2388), (0x23CF, 0x23CF), (0x23E9, 0x23F3), (0x23F8, 0x23FA),
    (0x24C2, 0x24C2), (0x25AA, 0x25AB), (0x25B6, 0x25B6), (0x25C0, 0x25C0), (0x25FB, 0x25FE), (0x2600, 0x2605), (0x2607, 0x2612),
    (0x2614, 0x2685), (0x2690, 0x2705), (0x2708, 0x2712), (0x2714, 0x2714), (0x2716, 0x2716), (0x271D, 0x271D), (0x2721, 0x2721),
    (0x2728, 0x2728), (0x2733, 0x2734), (0x2744, 0x2744), (0x2747, 0x2747), (0x274C, 0x274C), (0x274E, 0x274E), (0x2753, 0x2755),
    (0x2757, 0x2757), (0x2763, 0x2767), (0x2795, 0x2797), (0x27A1, 0x27A1), (0x27B0, 0x27B0), (0x27BF, 0x27BF), (0x2934, 0x2935),
    (0x2B05, 0x2B07), (0x2B1B, 0x2B1C), (0x2B50, 0x2B50), (0x2B55, 0x2B55), (0x3030, 0x3030), (0x303D, 0x303D), (0x3297, 0x3297),
    (0x3299, 0x3299), (0x1F000, 0x1F0FF), (0x1F10D, 0x1F10F), (0x1F12F, 0x1F12F), (0x1F16C, 0x1F171), (0x1F17E, 0x1F17F),
    (0x1F18E, 0x1F18E), (0x1F191, 0x1F19A), (0x1F1AD, 0x1F1E5), (0x1F201, 0x1F20F), (0x1F21A, 0x1F21A), (0x1F22F, 0x1F22F),
    (0x1F232, 0x1F23A), (0x1F23C, 0x1F23F), (0x1F249, 0x1F3FA), (0x1F400, 0x1F53D), (0x1F546, 0x1F64F), (0x1F680, 0x1F6FF),
    (0x1F774, 0x1F77F), (0x1F7D5, 0x1F7FF), (0x1F80C, 0x1F80F), (0x1F848, 0x1F84F), (0x1F85A, 0x1F85F), (0x1F888, 0x1F88F),
    (0x1F8AE, 0x1F8FF), (0x1F90C, 0x1F93A), (0x1F93C, 0x1F945), (0x1F947, 0x1FAFF), (0x1FC00, 0x1FFFD),
];

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Style = Style { bold: false, italic: false, strike: false };
    const BOLD: Style = Style { bold: true, italic: false, strike: false };
    const ITALIC: Style = Style { bold: false, italic: true, strike: false };
    const STRIKE: Style = Style { bold: false, italic: false, strike: true };
    const BOLD_ITALIC: Style = Style { bold: true, italic: true, strike: false };

    fn text(s: &str) -> Span {
        styled(s, PLAIN)
    }

    fn styled(s: &str, style: Style) -> Span {
        Span { style, kind: Kind::Text(s.into()) }
    }

    fn code(s: &str) -> Span {
        Span { style: PLAIN, kind: Kind::Code(s.into()) }
    }

    fn link(label: &str, url: &str) -> Span {
        Span { style: PLAIN, kind: Kind::Link { label: label.into(), url: url.into() } }
    }

    fn shortcode(name: &str, raw: &str) -> Span {
        Span { style: PLAIN, kind: Kind::Shortcode { name: name.into(), raw: raw.into() } }
    }

    fn mention(name: &str, raw: &str) -> Span {
        Span { style: PLAIN, kind: Kind::Mention { name: name.into(), raw: raw.into() } }
    }

    fn para(lines: &[&[Span]]) -> Block {
        Block::Paragraph(lines.iter().map(|l| l.to_vec()).collect())
    }

    /// The spans of a body that parses as one ordinary line.
    fn line(body: &str) -> Line {
        match parse(body).as_slice() {
            [Block::Paragraph(lines)] if lines.len() == 1 => lines[0].clone(),
            other => panic!("{body:?} is not one plain line: {other:?}"),
        }
    }

    fn no_custom(_: &str) -> bool {
        false
    }

    // --- blocks ----------------------------------------------------------------------------------

    #[test]
    fn empty_body_is_nothing() {
        assert_eq!(parse(""), []);
    }

    #[test]
    fn plain_lines_stay_lines() {
        assert_eq!(parse("hello"), [para(&[&[text("hello")]])]);
        assert_eq!(parse("a\nb"), [para(&[&[text("a")], &[text("b")]])]);
        assert_eq!(parse("a\n\nb"), [para(&[&[text("a")], &[], &[text("b")]])]);
        assert_eq!(parse("\n"), [para(&[&[], &[]])]);
        assert_eq!(parse("   "), [para(&[&[text("   ")]])]);
    }

    #[test]
    fn headings() {
        assert_eq!(parse("# One"), [Block::Heading { level: 1, line: vec![text("One")] }]);
        assert_eq!(parse("## Two"), [Block::Heading { level: 2, line: vec![text("Two")] }]);
        assert_eq!(parse("###   Three "), [Block::Heading { level: 3, line: vec![text("Three ")] }]);
        assert_eq!(parse("#\tTab"), [Block::Heading { level: 1, line: vec![text("Tab")] }]);
        assert_eq!(parse("# **big** deal"), [Block::Heading { level: 1, line: vec![styled("big", BOLD), text(" deal")] }]);
    }

    #[test]
    fn not_headings() {
        assert_eq!(line("#### Four"), [text("#### Four")]);
        assert_eq!(line("#hashtag"), [text("#hashtag")]);
        assert_eq!(line(" # indented"), [text(" # indented")]);
        assert_eq!(line("#"), [text("#")]);
        // A marker and one space: the case that once made the Electron plain run stall.
        assert_eq!(line("## "), [text("## ")]);
        // `.` does not match a carriage return.
        assert_eq!(line("# title\r"), [text("# title\r")]);
    }

    #[test]
    fn heading_of_spaces_gives_one_back() {
        assert_eq!(parse("##  "), [Block::Heading { level: 2, line: vec![text(" ")] }]);
        assert_eq!(parse("#   "), [Block::Heading { level: 1, line: vec![text(" ")] }]);
        assert_eq!(line("# \r"), [text("# \r")]);
    }

    #[test]
    fn quotes() {
        assert_eq!(parse("> a\n>b\n   >  c"), [Block::Quote(vec![vec![text("a")], vec![text("b")], vec![text(" c")]])]);
        assert_eq!(parse(">"), [Block::Quote(vec![vec![]])]);
        assert_eq!(parse("> *it*"), [Block::Quote(vec![vec![styled("it", ITALIC)]])]);
        assert_eq!(line("    > deep"), [text("    > deep")]);
        assert_eq!(parse("> q\nplain"), [Block::Quote(vec![vec![text("q")]]), para(&[&[text("plain")]])]);
    }

    #[test]
    fn quote_wins_over_list() {
        assert_eq!(parse("> - x"), [Block::Quote(vec![vec![text("- x")]])]);
    }

    #[test]
    fn unordered_lists() {
        assert_eq!(
            parse("- a\n* b\n+ c"),
            [Block::List { ordered: false, items: vec![vec![text("a")], vec![text("b")], vec![text("c")]] }]
        );
        assert_eq!(parse("  -   spaced"), [Block::List { ordered: false, items: vec![vec![text("spaced")]] }]);
        assert_eq!(parse("- "), [Block::List { ordered: false, items: vec![vec![]] }]);
        assert_eq!(parse("* *"), [Block::List { ordered: false, items: vec![vec![text("*")]] }]);
    }

    #[test]
    fn ordered_lists_count_from_one() {
        assert_eq!(parse("1. a\n2) b"), [Block::List { ordered: true, items: vec![vec![text("a")], vec![text("b")]] }]);
        assert_eq!(parse("7. seven"), [Block::List { ordered: true, items: vec![vec![text("seven")]] }]);
        assert_eq!(parse("10) ten"), [Block::List { ordered: true, items: vec![vec![text("ten")]] }]);
    }

    #[test]
    fn list_kind_comes_from_its_first_item() {
        assert_eq!(parse("1. a\n- b"), [Block::List { ordered: true, items: vec![vec![text("a")], vec![text("b")]] }]);
        assert_eq!(parse("- a\n1. b"), [Block::List { ordered: false, items: vec![vec![text("a")], vec![text("b")]] }]);
    }

    #[test]
    fn not_lists() {
        assert_eq!(line("-a"), [text("-a")]);
        assert_eq!(line("1.a"), [text("1.a")]);
        assert_eq!(line("1 a"), [text("1 a")]);
        assert_eq!(line("-"), [text("-")]);
        assert_eq!(line("    - deep"), [text("    - deep")]);
        assert_eq!(line("**bold** start"), [styled("bold", BOLD), text(" start")]);
        assert_eq!(line("*it* start"), [styled("it", ITALIC), text(" start")]);
    }

    #[test]
    fn list_items_are_inline_markdown() {
        assert_eq!(parse("* *it*"), [Block::List { ordered: false, items: vec![vec![styled("it", ITALIC)]] }]);
    }

    #[test]
    fn fences() {
        assert_eq!(
            parse("```rust\nlet a = **b**;\n  x\n```"),
            [Block::Code { lang: Some("rust".into()), text: "let a = **b**;\n  x".into() }]
        );
        assert_eq!(parse("```\n```"), [Block::Code { lang: None, text: String::new() }]);
        assert_eq!(parse("   ```js  \nx\n   ```  "), [Block::Code { lang: Some("js".into()), text: "x".into() }]);
    }

    #[test]
    fn unclosed_fence_runs_to_the_end() {
        assert_eq!(parse("```\na\n# b"), [Block::Code { lang: None, text: "a\n# b".into() }]);
        assert_eq!(parse("```"), [Block::Code { lang: None, text: String::new() }]);
    }

    #[test]
    fn parsing_resumes_after_a_fence() {
        assert_eq!(
            parse("before\n```\ncode\n```\nafter\n- item"),
            [
                para(&[&[text("before")]]),
                Block::Code { lang: None, text: "code".into() },
                para(&[&[text("after")]]),
                Block::List { ordered: false, items: vec![vec![text("item")]] },
            ]
        );
    }

    #[test]
    fn closing_fence_takes_no_language() {
        assert_eq!(parse("```\na\n```js\nb"), [Block::Code { lang: None, text: "a\n```js\nb".into() }]);
    }

    #[test]
    fn not_fences() {
        assert_eq!(line("```rust code"), [text("```rust code")]);
        assert_eq!(line("````"), [text("````")]);
        assert_eq!(line("    ```"), [text("    ```")]);
        assert_eq!(line("``` c++"), [text("``` c++")]);
    }

    #[test]
    fn a_plain_run_stops_at_any_block() {
        assert_eq!(
            parse("a\n# h\nb\n> q\nc"),
            [
                para(&[&[text("a")]]),
                Block::Heading { level: 1, line: vec![text("h")] },
                para(&[&[text("b")]]),
                Block::Quote(vec![vec![text("q")]]),
                para(&[&[text("c")]]),
            ]
        );
    }

    #[test]
    fn carriage_returns_are_ordinary_characters() {
        assert_eq!(parse("a\r\nb"), [para(&[&[text("a\r")], &[text("b")]])]);
        assert_eq!(parse("- a\r"), [Block::List { ordered: false, items: vec![vec![text("a\r")]] }]);
    }

    // --- inline ----------------------------------------------------------------------------------

    #[test]
    fn code_wins() {
        assert_eq!(line("`**not bold**`"), [code("**not bold**")]);
        assert_eq!(line("a `:smile: @bob` b"), [text("a "), code(":smile: @bob"), text(" b")]);
        assert_eq!(line("``"), [text("``")]);
        assert_eq!(line("`open"), [text("`open")]);
        assert_eq!(line("**`x`**"), [Span { style: BOLD, kind: Kind::Code("x".into()) }]);
    }

    #[test]
    fn bold_and_strike() {
        assert_eq!(line("**b**"), [styled("b", BOLD)]);
        assert_eq!(line("a **b c** d"), [text("a "), styled("b c", BOLD), text(" d")]);
        assert_eq!(line("~~gone~~"), [styled("gone", STRIKE)]);
        assert_eq!(line("**unclosed"), [text("**unclosed")]);
        assert_eq!(line("****"), [text("****")]);
        assert_eq!(line("~~~~"), [text("~~~~")]);
        assert_eq!(line("~single~"), [text("~single~")]);
    }

    #[test]
    fn bold_is_lazy() {
        assert_eq!(line("**a** and **b**"), [styled("a", BOLD), text(" and "), styled("b", BOLD)]);
        // The closer is the first `**` after at least one character, so the inside keeps a star.
        assert_eq!(line("***a***"), [styled("*a", BOLD), text("*")]);
        assert_eq!(line("*****"), [styled("*", BOLD)]);
    }

    #[test]
    fn italic_star() {
        assert_eq!(line("*it*"), [styled("it", ITALIC)]);
        assert_eq!(line("2*3*4"), [text("2"), styled("3", ITALIC), text("4")]);
        assert_eq!(line("**a*"), [text("*"), styled("a", ITALIC)]);
        assert_eq!(line("a* *"), [text("a"), styled(" ", ITALIC)]);
    }

    #[test]
    fn underscores_do_not_start_or_end_inside_words() {
        assert_eq!(line("__b__"), [styled("b", BOLD)]);
        assert_eq!(line("_i_"), [styled("i", ITALIC)]);
        assert_eq!(line("a _i_ b"), [text("a "), styled("i", ITALIC), text(" b")]);
        assert_eq!(line("snake_case_name"), [text("snake_case_name")]);
        assert_eq!(line("snake__case__name"), [text("snake__case__name")]);
        assert_eq!(line("_a_b"), [text("_a_b")]);
        assert_eq!(line("a_b_"), [text("a_b_")]);
        assert_eq!(line("__a__b"), [text("__a__b")]);
        assert_eq!(line("(_i_)"), [text("("), styled("i", ITALIC), text(")")]);
    }

    #[test]
    fn double_underscore_closer_skips_ones_inside_words() {
        assert_eq!(line("__a__b__"), [styled("a__b", BOLD)]);
        assert_eq!(line("__a___"), [styled("a_", BOLD)]);
    }

    #[test]
    fn nicknames_with_underscores_survive() {
        assert_eq!(line("hi @big_tuna_ ok"), [text("hi "), mention("big_tuna_", "@big_tuna_"), text(" ok")]);
        assert_eq!(line("@big_tuna and _x_"), [mention("big_tuna", "@big_tuna"), text(" and "), styled("x", ITALIC)]);
    }

    #[test]
    fn emphasis_nests() {
        assert_eq!(line("**bold *both* bold**"), [styled("bold ", BOLD), styled("both", BOLD_ITALIC), styled(" bold", BOLD)]);
        assert_eq!(line("~~**x**~~"), [styled("x", Style { bold: true, italic: false, strike: true })]);
        assert_eq!(line("_**x**_"), [styled("x", BOLD_ITALIC)]);
    }

    #[test]
    fn lookarounds_see_only_the_text_they_are_in() {
        // In the whole line the inner `_` follows a `_`, a word character; inside the bold run it
        // starts the string.
        assert_eq!(line("___i___"), [styled("i", BOLD_ITALIC)]);
    }

    #[test]
    fn links() {
        assert_eq!(line("[docs](https://example.com)"), [link("docs", "https://example.com/")]);
        assert_eq!(line("see [a](http://x.org/p?q=1#f)."), [text("see "), link("a", "http://x.org/p?q=1#f"), text(".")]);
        assert_eq!(line("[a [b](http://x.org)"), [link("a [b", "http://x.org/")]);
    }

    #[test]
    fn link_labels_are_plain_text() {
        assert_eq!(line("[**b** :smile: @bob](http://x.org)"), [link("**b** :smile: @bob", "http://x.org/")]);
    }

    #[test]
    fn links_inherit_emphasis() {
        assert_eq!(
            line("**[a](http://x.org)**"),
            [Span { style: BOLD, kind: Kind::Link { label: "a".into(), url: "http://x.org/".into() } }]
        );
    }

    #[test]
    fn only_http_links() {
        assert_eq!(line("[x](ftp://a.org)"), [text("[x](ftp://a.org)")]);
        assert_eq!(line("[x](javascript:alert(1))"), [text("[x](javascript:alert(1))")]);
        assert_eq!(line("[x](HTTP://a.org)"), [text("[x](HTTP://a.org)")]);
        // An empty label is no link, but the URL inside is still a bare one, parenthesis and all.
        assert_eq!(line("[](http://a.org)"), [text("[]("), link("http://a.org)", "http://a.org)/")]);
    }

    #[test]
    fn a_link_that_will_not_open_stays_text_and_is_not_rescanned() {
        assert_eq!(line("[**b**](http://[)"), [text("[**b**](http://[)")]);
        assert_eq!(line("http://[ :smile:"), [text("http://[ "), shortcode("smile", ":smile:")]);
        assert_eq!(line("<http://x.org>"), [text("<http://x.org>")]);
    }

    #[test]
    fn a_broken_link_can_still_hold_a_bare_url() {
        assert_eq!(line("[a](http://x y)"), [text("[a]("), link("http://x", "http://x/"), text(" y)")]);
    }

    #[test]
    fn bare_urls() {
        assert_eq!(
            line("go to https://example.com/a now"),
            [text("go to "), link("https://example.com/a", "https://example.com/a"), text(" now")]
        );
        // Greedy to the next space or `<`, trailing punctuation included.
        assert_eq!(line("(http://x.org/a)."), [text("("), link("http://x.org/a).", "http://x.org/a).")]);
        assert_eq!(line("http://a.org<b"), [link("http://a.org", "http://a.org/"), text("<b")]);
        assert_eq!(line("http://"), [text("http://")]);
        assert_eq!(line("HTTPS://x.org"), [text("HTTPS://x.org")]);
        assert_eq!(line("xhttp://a.org"), [text("x"), link("http://a.org", "http://a.org/")]);
    }

    #[test]
    fn bare_urls_inside_code_are_code() {
        assert_eq!(line("`http://a.org`"), [code("http://a.org")]);
    }

    // --- shortcodes and mentions ---------------------------------------------------------------

    #[test]
    fn shortcodes() {
        assert_eq!(line(":smile:"), [shortcode("smile", ":smile:")]);
        assert_eq!(line("hi :Party_Parrot:!"), [text("hi "), shortcode("party_parrot", ":Party_Parrot:"), text("!")]);
        assert_eq!(line(":ab:"), [shortcode("ab", ":ab:")]);
        assert_eq!(line(":100:"), [shortcode("100", ":100:")]);
        let long = "a".repeat(32);
        assert_eq!(line(&format!(":{long}:")), [shortcode(&long, &format!(":{long}:"))]);
    }

    #[test]
    fn not_shortcodes() {
        assert_eq!(line(":a:"), [text(":a:")]);
        assert_eq!(line(&format!(":{}:", "a".repeat(33))), [text(&format!(":{}:", "a".repeat(33)))]);
        assert_eq!(line(":smile-face:"), [text(":smile-face:")]);
        assert_eq!(line(":+1:"), [text(":+1:")]);
        assert_eq!(line("::"), [text("::")]);
        assert_eq!(line("12:30:00"), [text("12"), shortcode("30", ":30:"), text("00")]);
    }

    #[test]
    fn shortcodes_do_not_overlap() {
        assert_eq!(line(":foo:smile:"), [shortcode("foo", ":foo:"), text("smile:")]);
        assert_eq!(line(":a:bc:"), [text(":a"), shortcode("bc", ":bc:")]);
        assert_eq!(line(":a::b:"), [text(":a::b:")]);
    }

    #[test]
    fn emphasis_carries_into_shortcodes_and_mentions() {
        assert_eq!(line("**:smile:**"), [Span { style: BOLD, kind: Kind::Shortcode { name: "smile".into(), raw: ":smile:".into() } }]);
        assert_eq!(line("*@bob*"), [Span { style: ITALIC, kind: Kind::Mention { name: "bob".into(), raw: "@bob".into() } }]);
    }

    #[test]
    fn underscores_can_eat_a_shortcode() {
        assert_eq!(line(":_smile_:"), [text(":"), styled("smile", ITALIC), text(":")]);
    }

    #[test]
    fn mentions_in_text() {
        assert_eq!(line("@bob"), [mention("bob", "@bob")]);
        assert_eq!(line("hey @Bob!"), [text("hey "), mention("bob", "@Bob"), text("!")]);
        assert_eq!(line("@everyone look"), [mention("everyone", "@everyone"), text(" look")]);
        assert_eq!(line("@a-b_c"), [mention("a-b_c", "@a-b_c")]);
        assert_eq!(line("(@bob)"), [text("("), mention("bob", "@bob"), text(")")]);
        assert_eq!(line(":@bob"), [text(":"), mention("bob", "@bob")]);
    }

    #[test]
    fn not_mentions() {
        assert_eq!(line("me@bob.com"), [text("me@bob.com")]);
        assert_eq!(line("@@bob"), [text("@@bob")]);
        assert_eq!(line("@-bob"), [text("@-bob")]);
        assert_eq!(line("@_bob"), [text("@_bob")]);
        assert_eq!(line("@"), [text("@")]);
        assert_eq!(line("@ bob"), [text("@ bob")]);
    }

    #[test]
    fn mentions_stop_at_24_characters() {
        let name = "a".repeat(30);
        assert_eq!(line(&format!("@{name}")), [mention(&name[..24], &format!("@{}", &name[..24])), text(&name[24..])]);
    }

    #[test]
    fn mention_lookbehind_sees_only_the_run_of_text() {
        // The Electron client scanned each piece between markdown on its own, so the `_` that
        // closes the italic does not count as a word character before the `@`.
        assert_eq!(line("_a_@bob"), [styled("a", ITALIC), mention("bob", "@bob")]);
        assert_eq!(line("**x@bob**"), [styled("x@bob", BOLD)]);
        assert_eq!(line("a_@bob"), [text("a_@bob")]);
    }

    #[test]
    fn non_ascii_text_passes_through() {
        assert_eq!(line("ção **é** 日本 @bob"), [text("ção "), styled("é", BOLD), text(" 日本 "), mention("bob", "@bob")]);
        assert_eq!(line("é@bob"), [text("é"), mention("bob", "@bob")]);
        assert_eq!(line("*😀*"), [styled("😀", ITALIC)]);
    }

    // --- emoji only ------------------------------------------------------------------------------

    #[test]
    fn emoji_only() {
        assert!(is_emoji_only("😀", no_custom));
        assert!(is_emoji_only(" 😀 🎉\n😀 ", no_custom));
        assert!(is_emoji_only("👍🏽", no_custom));
        assert!(is_emoji_only("👨‍👩‍👧", no_custom));
        assert!(is_emoji_only("❤️", no_custom));
        assert!(is_emoji_only("©", no_custom));
        assert!(is_emoji_only(":smile:", no_custom));
        assert!(is_emoji_only(":Smile: :thumbs_up:", no_custom));
        assert!(is_emoji_only(":smile: :nope:", no_custom));
        assert!(is_emoji_only(":nope: 😀", no_custom));
    }

    #[test]
    fn custom_emoji_count() {
        assert!(!is_emoji_only(":hrmny_logo:", no_custom));
        assert!(is_emoji_only(":hrmny_logo:", |name| name == "hrmny_logo"));
        assert!(is_emoji_only(":Hrmny_Logo:", |name| name == "hrmny_logo"));
    }

    #[test]
    fn not_emoji_only() {
        assert!(!is_emoji_only("", no_custom));
        assert!(!is_emoji_only("  \n ", no_custom));
        assert!(!is_emoji_only("hi 😀", no_custom));
        assert!(!is_emoji_only("::", no_custom));
        assert!(!is_emoji_only(":nope:", no_custom));
        assert!(!is_emoji_only(":a:", no_custom));
        assert!(!is_emoji_only("`:smile:`", no_custom));
        assert!(!is_emoji_only("> 😀", no_custom));
        assert!(!is_emoji_only("\u{200D}\u{FE0F}", no_custom));
    }

    #[test]
    fn flags_and_keycaps_are_not_pictographs() {
        assert!(!is_emoji_only("🇧🇷", no_custom));
        assert!(!is_emoji_only("🇧🇷 😀", no_custom));
        assert!(!is_emoji_only("1️⃣", no_custom));
    }

    #[test]
    fn a_shortcode_markdown_swallowed_does_not_count() {
        assert!(!is_emoji_only(":_smile_:", no_custom));
        assert!(!is_emoji_only(":__smile__:", no_custom));
    }

    #[test]
    fn shape_uses_unicode_case_folding() {
        assert!(is_emoji_only("😀 :\u{17F}mile:", no_custom));
        assert!(!is_emoji_only(":\u{17F}mile:", no_custom));
    }

    // --- mention list ----------------------------------------------------------------------------

    #[test]
    fn mention_list() {
        assert_eq!(mentions("@Bob hi @alice and @bob, @everyone"), ["bob", "alice", "everyone"]);
        assert_eq!(mentions("no one"), Vec::<String>::new());
        assert_eq!(mentions("mail me@bob.com or @@x"), Vec::<String>::new());
        assert_eq!(mentions("@EVERYONE @everyone"), ["everyone"]);
    }

    #[test]
    fn mention_list_reads_the_raw_body() {
        assert_eq!(mentions("`@bob`\n```\n@carol\n```"), ["bob", "carol"]);
        // Unlike the drawing, the whole body is one string to the server: `_` is a word character.
        assert_eq!(mentions("_a_@bob"), Vec::<String>::new());
        assert_eq!(mentions("olá@bob"), ["bob"]);
    }

    #[test]
    fn spans_walks_every_line() {
        let blocks = parse("a\n# b\n> c\n- d\n```\ne\n```");
        let all: Vec<_> = blocks.iter().flat_map(Block::spans).cloned().collect();
        assert_eq!(all, [text("a"), text("b"), text("c"), text("d")]);
    }
}
