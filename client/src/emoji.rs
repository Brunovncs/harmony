//! The standard emoji: the picker's sections, `:name:` lookup and search.
//!
//! The data is the `emojis` crate's, which is Unicode's CLDR list. The Electron client generated
//! its own table from Python's unicodedata instead; the shape is the same (six sections, a
//! snake_case name per emoji, a short list of hand-picked aliases) but the names are CLDR's, so
//! `:thumbs_up:` rather than `:thumbs_up_sign:`.
//!
//! Custom emoji are not here. They belong to a server, and they win over everything in this file:
//! a server that calls something `:pizza:` means its picture.

use emojis::{Emoji, Group};
use std::collections::HashMap;
use std::sync::OnceLock;

/// One heading in the picker and what goes under it, in CLDR order.
pub struct Section {
    index: usize,
    pub items: Vec<&'static Emoji>,
}

impl Section {
    pub fn title(&self) -> &'static str {
        tr!(TITLES[self.index], TITLES_PT[self.index])
    }
}

/// The names people actually type.
///
/// CLDR's name for the thumbs up is "thumbs up", which is close, but nobody types `:thumbs_up:`
/// either. These are the short ones everybody learned somewhere else, kept exactly as the Electron
/// client had them so a message reads the same in both. `+1`, `-1` and `x` cannot be written as
/// a shortcode in a message (the syntax wants two or more of `[a-z0-9_]`); they are kept because
/// they were in the list.
pub const EMOJI_ALIASES: &[(&str, &str)] = &[
    ("thumbsup", "\u{1F44D}"),
    ("+1", "\u{1F44D}"),
    ("thumbsdown", "\u{1F44E}"),
    ("-1", "\u{1F44E}"),
    ("joy", "\u{1F602}"),
    ("rofl", "\u{1F923}"),
    ("sob", "\u{1F62D}"),
    ("cry", "\u{1F622}"),
    ("smile", "\u{1F604}"),
    ("grin", "\u{1F601}"),
    ("wink", "\u{1F609}"),
    ("heart", "\u{2764}\u{FE0F}"),
    ("fire", "\u{1F525}"),
    ("100", "\u{1F4AF}"),
    ("tada", "\u{1F389}"),
    ("party", "\u{1F389}"),
    ("eyes", "\u{1F440}"),
    ("ok", "\u{1F44C}"),
    ("pray", "\u{1F64F}"),
    ("clap", "\u{1F44F}"),
    ("wave", "\u{1F44B}"),
    ("rocket", "\u{1F680}"),
    ("skull", "\u{1F480}"),
    ("poop", "\u{1F4A9}"),
    ("shrug", "\u{1F937}"),
    ("facepalm", "\u{1F926}"),
    ("think", "\u{1F914}"),
    ("thinking", "\u{1F914}"),
    ("sunglasses", "\u{1F60E}"),
    ("cool", "\u{1F60E}"),
    ("sweat", "\u{1F605}"),
    ("pensive", "\u{1F614}"),
    ("rage", "\u{1F621}"),
    ("angry", "\u{1F620}"),
    ("sleepy", "\u{1F634}"),
    ("star", "\u{2B50}"),
    ("check", "\u{2705}"),
    ("x", "\u{274C}"),
    ("warning", "\u{26A0}\u{FE0F}"),
    ("brain", "\u{1F9E0}"),
    ("salt", "\u{1F9C2}"),
    ("beer", "\u{1F37A}"),
    ("pizza", "\u{1F355}"),
    ("cat", "\u{1F431}"),
    ("dog", "\u{1F436}"),
    ("frog", "\u{1F438}"),
    ("snake", "\u{1F40D}"),
    ("bug", "\u{1F41B}"),
    ("moon", "\u{1F319}"),
    ("sun", "\u{2600}\u{FE0F}"),
    ("zzz", "\u{1F4A4}"),
];

const TITLES: [&str; 6] = ["Smileys & people", "Food & drink", "Animals & nature", "Activities", "Travel & places", "Objects & symbols"];
const TITLES_PT: [&str; 6] =
    ["Carinhas e pessoas", "Comidas e bebidas", "Animais e natureza", "Atividades", "Viagens e lugares", "Objetos e símbolos"];

fn section_of(group: Group) -> usize {
    match group {
        Group::SmileysAndEmotion | Group::PeopleAndBody => 0,
        Group::FoodAndDrink => 1,
        Group::AnimalsAndNature => 2,
        Group::Activities => 3,
        Group::TravelAndPlaces => 4,
        Group::Objects | Group::Symbols | Group::Flags => 5,
    }
}

/// The newest emoji the picker offers. Windows 10 and older Windows 11 builds draw anything
/// newer as an empty box, so those are left out of the grid (they still show when typed).
const NEWEST: emojis::UnicodeVersion = emojis::UnicodeVersion::new(14, 0);

fn shown(e: &Emoji) -> bool {
    e.unicode_version() <= NEWEST
}

/// Harmony's six sections, in picker order. Default skin tones only: the variants are reachable
/// by name but would multiply the grid by six.
pub fn sections() -> &'static [Section] {
    static SECTIONS: OnceLock<Vec<Section>> = OnceLock::new();
    SECTIONS.get_or_init(|| {
        let mut sections: Vec<Section> = (0..TITLES.len()).map(|index| Section { index, items: Vec::new() }).collect();
        for emoji in emojis::iter().filter(|e| shown(e)) {
            sections[section_of(emoji.group())].items.push(emoji);
        }
        sections
    })
}

/// Every name a `:name:` can resolve to through this file, aliases first. Built once: the lookup
/// runs for every shortcode in every message drawn.
fn names() -> &'static HashMap<String, &'static str> {
    static NAMES: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: HashMap<String, &'static str> = EMOJI_ALIASES.iter().map(|&(name, emoji)| (name.to_owned(), emoji)).collect();
        for emoji in emojis::iter() {
            let variants = emoji.skin_tones().into_iter().flatten().chain(emoji.skin_tones().is_none().then_some(emoji));
            for variant in variants {
                names.entry(snake(variant.name())).or_insert(variant.as_str());
            }
        }
        names
    })
}

/// The emoji a typed `:name:` stands for, given the name without its colons and already
/// lowercased, as the markdown parser hands it over.
///
/// Aliases first, then the full snake_case names (skin tones included, so
/// `:thumbs_up_medium_skin_tone:` works), then GitHub's shortcodes, which cover most of what
/// people bring from elsewhere.
pub fn by_shortcode(name: &str) -> Option<&'static str> {
    names().get(name).copied().or_else(|| emojis::get_by_shortcode(name).map(Emoji::as_str))
}

/// The snake_case name of an emoji, for the `:name:` shown on hover. Accepts unqualified forms
/// (a heart without its U+FE0F) and skin-tone variants.
pub fn name_of(emoji: &str) -> Option<String> {
    emojis::get(emoji).map(|emoji| snake(emoji.name()))
}

/// The picker's search box text as the search sees it: trimmed, lowercased, and every run of
/// anything outside `[a-z0-9_]` turned into one underscore, with none left at either end. Custom
/// emoji names are matched against the same string.
pub fn normalize_query(query: &str) -> String {
    let lower = query.trim_matches(is_js_space).to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut gap = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            if gap {
                out.push('_');
                gap = false;
            }
            out.push(c);
        } else {
            gap = true;
        }
    }
    out.trim_matches('_').to_owned()
}

/// Standard emoji whose name contains the query, in picker order, at most `limit` of them.
///
/// The Electron picker lists the server's custom matches first and stops the whole grid at 300,
/// so a caller doing the same passes what is left of the 300 after its own. An empty query (after
/// normalising) is not a search and returns nothing.
pub fn search(query: &str, limit: usize) -> Vec<&'static Emoji> {
    static INDEX: OnceLock<Vec<(&'static Emoji, String)>> = OnceLock::new();
    let query = normalize_query(query);
    if query.is_empty() {
        return Vec::new();
    }
    let index = INDEX.get_or_init(|| sections().iter().flat_map(|s| &s.items).map(|&e| (e, snake(e.name()))).collect());
    index.iter().filter(|(_, name)| name.contains(&query)).map(|&(emoji, _)| emoji).take(limit).collect()
}

/// A CLDR name as a shortcode: "woman’s hat" becomes `womans_hat`, "flag: Côte d’Ivoire"
/// `flag_cote_divoire`. Apostrophes vanish rather than splitting a word, accents fold to their
/// letter, and the two keycaps whose names end in a bare symbol get a word for it, or they would
/// both be `keycap`.
fn snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut gap = false;
    for c in name.chars() {
        let symbol = match c {
            '\'' | '\u{2019}' => continue,
            '#' => Some("hash"),
            '*' => Some("asterisk"),
            _ => None,
        };
        let letter = fold_accent(c).to_ascii_lowercase();
        if symbol.is_none() && !letter.is_ascii_alphanumeric() {
            gap = true;
            continue;
        }
        if (gap || symbol.is_some()) && !out.is_empty() {
            out.push('_');
        }
        gap = symbol.is_some();
        match symbol {
            Some(word) => out.push_str(word),
            None => out.push(letter),
        }
    }
    out
}

fn fold_accent(c: char) -> char {
    match c {
        'À'..='Å' | 'à'..='å' => 'a',
        'Ç' | 'ç' => 'c',
        'È'..='Ë' | 'è'..='ë' => 'e',
        'Ì'..='Ï' | 'ì'..='ï' => 'i',
        'Ñ' | 'ñ' => 'n',
        'Ò'..='Ö' | 'ò'..='ö' => 'o',
        'Ù'..='Ü' | 'ù'..='ü' => 'u',
        'Ý' | 'ý' | 'ÿ' => 'y',
        _ => c,
    }
}

/// What JavaScript's `trim` and `\s` count as space, which is not quite Rust's `is_whitespace`:
/// JavaScript adds U+FEFF and leaves out U+0085.
pub(crate) fn is_js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2028}' | '\u{2029}')
        || matches!(c, '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use emojis::SkinTone;

    fn section_index(emoji: &str) -> usize {
        sections().iter().position(|s| s.items.iter().any(|e| e.as_str() == emoji)).unwrap()
    }

    #[test]
    fn six_sections_in_order() {
        let order: Vec<_> = sections().iter().map(|s| s.index).collect();
        assert_eq!(order, (0..TITLES.len()).collect::<Vec<_>>());
        assert!(sections().iter().all(|s| !s.items.is_empty()));
    }

    #[test]
    fn sections_hold_every_default_emoji_once() {
        let total: usize = sections().iter().map(|s| s.items.len()).sum();
        assert_eq!(total, emojis::iter().filter(|e| shown(e)).count());
        for section in sections() {
            assert!(section.items.iter().all(|e| matches!(e.skin_tone(), None | Some(SkinTone::Default))));
        }
        assert!(!sections().iter().flat_map(|s| &s.items).any(|e| e.as_str() == "\u{1F44D}\u{1F3FD}"));
    }

    #[test]
    fn groups_map_onto_harmony_sections() {
        assert_eq!(section_index("\u{1F600}"), 0);
        assert_eq!(section_index("\u{1F44D}"), 0);
        assert_eq!(section_index("\u{1F355}"), 1);
        assert_eq!(section_index("\u{1F436}"), 2);
        assert_eq!(section_index("\u{26BD}"), 3);
        assert_eq!(section_index("\u{1F680}"), 4);
        assert_eq!(section_index("\u{1F4A1}"), 5);
        assert_eq!(section_index("\u{2764}\u{FE0F}"), 0);
        assert_eq!(section_index("\u{267B}\u{FE0F}"), 5);
        assert_eq!(section_index("\u{1F1E7}\u{1F1F7}"), 5);
    }

    #[test]
    fn aliases_are_the_electron_list() {
        assert_eq!(EMOJI_ALIASES.len(), 51);
        for &(name, emoji) in EMOJI_ALIASES {
            assert_eq!(by_shortcode(name), Some(emoji), "{name}");
        }
        assert_eq!(by_shortcode("heart"), Some("\u{2764}\u{FE0F}"));
        assert_eq!(by_shortcode("sleepy"), Some("\u{1F634}"));
    }

    #[test]
    fn aliases_win_over_names_and_github() {
        // GitHub's :sleepy: is the sleepy face; the alias says sleeping face.
        assert_eq!(emojis::get_by_shortcode("sleepy").unwrap().as_str(), "\u{1F62A}");
        assert_eq!(by_shortcode("sleepy"), Some("\u{1F634}"));
        assert_eq!(by_shortcode("star"), Some("\u{2B50}"));
    }

    #[test]
    fn full_names_resolve() {
        assert_eq!(by_shortcode("thumbs_up"), Some("\u{1F44D}"));
        assert_eq!(by_shortcode("grinning_face"), Some("\u{1F600}"));
        assert_eq!(by_shortcode("womans_hat"), Some("\u{1F452}"));
        assert_eq!(by_shortcode("t_shirt"), Some("\u{1F455}"));
        assert_eq!(by_shortcode("eight_oclock"), Some("\u{1F557}"));
        assert_eq!(by_shortcode("pinata"), Some("\u{1FA85}"));
        assert_eq!(by_shortcode("flag_brazil"), Some("\u{1F1E7}\u{1F1F7}"));
        assert_eq!(by_shortcode("flag_cote_divoire"), Some("\u{1F1E8}\u{1F1EE}"));
        assert_eq!(by_shortcode("japanese_here_button"), Some("\u{1F201}"));
        assert_eq!(by_shortcode("keycap_hash"), Some("#\u{FE0F}\u{20E3}"));
        assert_eq!(by_shortcode("keycap_asterisk"), Some("*\u{FE0F}\u{20E3}"));
        assert_eq!(by_shortcode("thumbs_up_medium_skin_tone"), Some("\u{1F44D}\u{1F3FD}"));
    }

    #[test]
    fn github_shortcodes_are_the_fallback() {
        assert_eq!(by_shortcode("smiley"), Some("\u{1F603}"));
        assert_eq!(by_shortcode("tshirt"), Some("\u{1F455}"));
        assert_eq!(by_shortcode("not_an_emoji_at_all"), None);
        assert_eq!(by_shortcode(""), None);
    }

    #[test]
    fn names_are_unique_and_round_trip() {
        let mut seen = HashMap::new();
        for emoji in emojis::iter() {
            for variant in emoji.skin_tones().into_iter().flatten().chain(emoji.skin_tones().is_none().then_some(emoji)) {
                let name = name_of(variant.as_str()).unwrap();
                assert!(!name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'), "{name}");
                assert!(!name.starts_with('_') && !name.ends_with('_') && !name.contains("__"), "{name}");
                if let Some(other) = seen.insert(name.clone(), variant.as_str()) {
                    panic!("{name} names both {other} and {}", variant.as_str());
                }
                if !EMOJI_ALIASES.iter().any(|&(alias, _)| alias == name) {
                    assert_eq!(by_shortcode(&name), Some(variant.as_str()), "{name}");
                }
            }
        }
    }

    #[test]
    fn name_of_handles_unqualified_and_unknown() {
        assert_eq!(name_of("\u{1F44D}").as_deref(), Some("thumbs_up"));
        assert_eq!(name_of("\u{2764}").as_deref(), Some("red_heart"));
        assert_eq!(name_of("\u{2764}\u{FE0F}").as_deref(), Some("red_heart"));
        assert_eq!(name_of("\u{1F44D}\u{1F3FF}").as_deref(), Some("thumbs_up_dark_skin_tone"));
        assert_eq!(name_of("a"), None);
        assert_eq!(name_of(""), None);
    }

    #[test]
    fn query_is_normalised_like_the_picker() {
        assert_eq!(normalize_query("  Thumbs Up "), "thumbs_up");
        assert_eq!(normalize_query("thumbs-up!!"), "thumbs_up");
        assert_eq!(normalize_query("__cat__"), "cat");
        assert_eq!(normalize_query("a__b"), "a__b");
        assert_eq!(normalize_query("a _b"), "a__b");
        assert_eq!(normalize_query("!!!"), "");
        assert_eq!(normalize_query("\u{FEFF} x \u{3000}"), "x");
        assert_eq!(normalize_query("caf\u{E9}"), "caf");
    }

    #[test]
    fn search_matches_names_by_substring_in_picker_order() {
        let found: Vec<_> = search("thumbs", 300).iter().map(|e| e.as_str()).collect();
        assert_eq!(found, ["\u{1F44D}", "\u{1F44E}"]);
        assert_eq!(search("Thumbs Up", 300).first().map(|e| e.as_str()), Some("\u{1F44D}"));
        let faces = search("face", 300);
        assert!(faces.len() > 50);
        let order: Vec<_> = faces.iter().map(|e| section_index(e.as_str())).collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn search_ignores_aliases_and_skin_tones() {
        assert!(search("thumbsup", 300).is_empty());
        assert!(search("skin_tone", 300).is_empty());
    }

    #[test]
    fn search_is_capped_and_empty_queries_find_nothing() {
        assert_eq!(search("a", 300).len(), 300);
        assert_eq!(search("a", 7).len(), 7);
        assert_eq!(search("a", 0).len(), 0);
        assert!(search("", 300).is_empty());
        assert!(search("  - ", 300).is_empty());
        assert!(search("zzzzqqq", 300).is_empty());
    }
}
