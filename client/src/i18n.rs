//! English and Brazilian Portuguese. Strings live where they are used, both languages side by
//! side, so a missing translation is a compile error rather than a blank label:
//!
//! ```ignore
//! tr!("Mute", "Silenciar")
//! trf!("{} joined", "{} entrou", name)
//! ```

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Pt,
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn lang() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::Pt,
        _ => Lang::En,
    }
}

pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// The `language` setting: "en", "pt", or empty to follow the system.
pub fn resolve(setting: &str) -> Lang {
    match setting {
        "en" => Lang::En,
        "pt" => Lang::Pt,
        _ => system(),
    }
}

pub fn code(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "en",
        Lang::Pt => "pt",
    }
}

fn system() -> Lang {
    let locale = system_locale().unwrap_or_default().to_ascii_lowercase();
    if locale.starts_with("pt") { Lang::Pt } else { Lang::En }
}

#[cfg(windows)]
fn system_locale() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
    let mut buf = [0u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    (n > 1).then(|| String::from_utf16_lossy(&buf[..n as usize - 1]))
}

#[cfg(not(windows))]
fn system_locale() -> Option<String> {
    std::env::var("LC_ALL").or_else(|_| std::env::var("LANG")).ok()
}

/// The string for the current language.
#[macro_export]
macro_rules! tr {
    ($en:expr, $pt:expr $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => $en,
            $crate::i18n::Lang::Pt => $pt,
        }
    };
}

/// `format!` with a pattern per language; both take the same arguments.
#[macro_export]
macro_rules! trf {
    ($en:literal, $pt:literal $(, $arg:expr)* $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => format!($en $(, $arg)*),
            $crate::i18n::Lang::Pt => format!($pt $(, $arg)*),
        }
    };
}

const MONTHS_EN: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const MONTHS_PT: [&str; 12] =
    ["janeiro", "fevereiro", "março", "abril", "maio", "junho", "julho", "agosto", "setembro", "outubro", "novembro", "dezembro"];
const DAYS_EN: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const DAYS_PT: [&str; 7] = ["segunda-feira", "terça-feira", "quarta-feira", "quinta-feira", "sexta-feira", "sábado", "domingo"];

fn month(d: &impl chrono::Datelike) -> &'static str {
    let m = d.month0() as usize;
    tr!(MONTHS_EN[m], MONTHS_PT[m])
}

/// "8 October 2026", "8 de outubro de 2026".
pub fn date(d: &impl chrono::Datelike) -> String {
    trf!("{} {} {}", "{} de {} de {}", d.day(), month(d), d.year())
}

/// "Thursday, 8 October 2026 · 14:05", "quinta-feira, 8 de outubro de 2026 · 14:05".
pub fn date_time(d: &(impl chrono::Datelike + chrono::Timelike)) -> String {
    let w = d.weekday().num_days_from_monday() as usize;
    format!("{}, {} · {:02}:{:02}", tr!(DAYS_EN[w], DAYS_PT[w]), date(d), d.hour(), d.minute())
}

/// A message's time, as short as it can be next to `now`: "14:05" today, "Yesterday 14:05",
/// "8 Oct 14:05" this year, "8 Oct 2025" before.
pub fn message_time(d: &chrono::DateTime<chrono::Local>, now: &chrono::DateTime<chrono::Local>) -> String {
    use chrono::{Datelike, Timelike};
    let hhmm = format!("{:02}:{:02}", d.hour(), d.minute());
    let days = (now.date_naive() - d.date_naive()).num_days();
    let short_month = || month(d).chars().take(3).collect::<String>();
    match days {
        ..=0 => hhmm,
        1 => trf!("Yesterday {}", "Ontem, {}", hhmm),
        _ if d.year() == now.year() => trf!("{} {} {}", "{} {} {}", d.day(), short_month(), hhmm),
        _ => trf!("{} {} {}", "{} {} {}", d.day(), short_month(), d.year()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The language is global; tests that switch it take turns.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn picks_the_language() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        set(Lang::Pt);
        assert_eq!(tr!("Mute", "Silenciar"), "Silenciar");
        assert_eq!(trf!("{} joined", "{} entrou", "lis"), "lis entrou");
        set(Lang::En);
        assert_eq!(trf!("{} joined", "{} entrou", "lis"), "lis joined");
        assert_eq!(resolve("pt"), Lang::Pt);
    }

    #[test]
    fn dates_follow_the_language() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        use chrono::TimeZone;
        let at = |y, m, d, h| chrono::Local.with_ymd_and_hms(y, m, d, h, 5, 0).unwrap();
        let now = at(2026, 10, 8, 18);
        set(Lang::Pt);
        assert_eq!(date_time(&at(2026, 10, 8, 14)), "quinta-feira, 8 de outubro de 2026 · 14:05");
        assert_eq!(message_time(&at(2026, 10, 7, 9), &now), "Ontem, 09:05");
        assert_eq!(message_time(&at(2026, 3, 2, 9), &now), "2 mar 09:05");
        set(Lang::En);
        assert_eq!(message_time(&at(2026, 10, 8, 14), &now), "14:05");
        assert_eq!(message_time(&at(2026, 10, 7, 9), &now), "Yesterday 09:05");
        assert_eq!(message_time(&at(2025, 3, 2, 9), &now), "2 Mar 2025");
        assert_eq!(date(&at(2026, 10, 8, 14)), "8 October 2026");
    }
}
