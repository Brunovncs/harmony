//! "Update" at the top of the window when GitHub has a newer release. A click downloads the new
//! program, checks it and swaps it in, then Harmony starts again on its own. In a voice call it
//! waits: the new version is put in place at once, and the restart happens when the call ends,
//! or now if the person says so. A development build only opens the release's page, so it never
//! replaces the program cargo built.

use crate::core::update::{self, Release};
use crate::prefs::{prefs, set_prefs};
use crate::theme::{MONO, Theme, current, px, radius};
use crate::ui::overlay::{self, Ask};
use crate::widgets::*;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, Global, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Task, Window, div, relative,
};
use std::path::PathBuf;
use std::time::Duration;

const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// Builds from `cargo run` keep their program; only release builds replace themselves.
const SELF_UPDATE: bool = !cfg!(debug_assertions);

#[derive(Clone, PartialEq)]
enum State {
    Idle,
    /// Asked, and this is the latest.
    Current,
    Available(Release),
    Downloading(Release, f32),
    /// In place, waiting for the call to end before restarting.
    Ready(Release),
    Restarting(Release),
    Failed(Release, String),
}

pub struct Updater {
    state: State,
    checking: bool,
    /// Why the last check failed, for Settings.
    check_error: Option<String>,
    in_call: bool,
    /// The program as it was started; after a swap it holds the new version.
    exe: Option<PathBuf>,
    _poll: Task<()>,
}

struct Handle(Entity<Updater>);

impl Global for Handle {}

/// Starts checking, at once and every few hours while the setting is on.
pub fn init(cx: &mut App) {
    let updater = cx.new(Updater::new);
    cx.set_global(Handle(updater));
}

fn handle(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<Handle>().map(|h| h.0.clone())
}

/// Every view that shows the button or the setting reads the state while drawing, so a change
/// redraws the windows.
fn changed(cx: &mut Context<Updater>) {
    cx.notify();
    cx.refresh_windows();
}

impl Updater {
    fn new(cx: &mut Context<Self>) -> Updater {
        let exe = std::env::current_exe().ok();
        if let Some(exe) = exe.clone() {
            cx.spawn(async move |_, cx| {
                if cx.background_executor().spawn(async move { update::clean_up(&exe) }).await {
                    cx.update(|cx| {
                        overlay::toast(
                            trf!(
                                "Harmony was updated to version {}.",
                                "O Harmony foi atualizado para a versão {}.",
                                env!("CARGO_PKG_VERSION")
                            ),
                            cx,
                        )
                    });
                }
            })
            .detach();
        }
        let poll = cx.spawn(async move |this, cx| {
            let check = |u: &mut Updater, cx: &mut Context<Updater>| {
                if prefs(cx).check_updates {
                    u.check(cx)
                }
            };
            while this.update(cx, check).is_ok() {
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        });
        Updater { state: State::Idle, checking: false, check_error: None, in_call: false, exe, _poll: poll }
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        if self.checking || matches!(self.state, State::Downloading(..) | State::Ready(_) | State::Restarting(_)) {
            return;
        }
        self.checking = true;
        changed(cx);
        let found = crate::core::run(update::check());
        cx.spawn(async move |this, cx| {
            let found = found.await;
            let _ = this.update(cx, |u, cx| {
                u.checking = false;
                let settled = matches!(u.state, State::Idle | State::Current | State::Available(_));
                match found {
                    Ok(Some(r)) if settled => u.state = State::Available(r),
                    Ok(None) if settled => u.state = State::Current,
                    Ok(_) => {}
                    Err(e) => {
                        log::info!("update check failed: {e}");
                        u.check_error = Some(e);
                        return changed(cx);
                    }
                }
                u.check_error = None;
                changed(cx);
            });
        })
        .detach();
    }

    fn click(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.state.clone() {
            State::Available(r) | State::Failed(r, _) => self.update_now(r, cx),
            State::Ready(r) if !self.in_call => self.restart(r, cx),
            State::Ready(r) => {
                let this = cx.entity();
                Ask::confirm_action(
                    tr!("Restart Harmony now?", "Reiniciar o Harmony agora?"),
                    trf!(
                        "Harmony {} is ready. Restarting now takes you out of the voice call; otherwise it restarts by itself when you leave.",
                        "O Harmony {} está pronto. Reiniciar agora tira você da chamada de voz; senão ele reinicia sozinho quando você sair.",
                        r.version
                    ),
                    tr!("Leave the call and restart", "Sair da chamada e reiniciar"),
                    window,
                    cx,
                    move |_, cx| {
                        this.update(cx, |u, cx| {
                            if let State::Ready(r) = u.state.clone() {
                                u.restart(r, cx);
                            }
                        })
                    },
                );
            }
            _ => {}
        }
    }

    fn update_now(&mut self, r: Release, cx: &mut Context<Self>) {
        let Some(exe) = self.exe.clone().filter(|_| SELF_UPDATE && r.exe.is_some()) else {
            cx.open_url(&r.page);
            return;
        };
        self.state = State::Downloading(r.clone(), 0.);
        changed(cx);
        let (tx, rx) = async_channel::unbounded::<f32>();
        let job = crate::core::run({
            let r = r.clone();
            async move {
                let new = update::beside(&exe, "new");
                let mut shown = 0.;
                update::download(&r, &new, move |p| {
                    if p - shown >= 0.01 {
                        shown = p;
                        let _ = tx.try_send(p);
                    }
                })
                .await?;
                update::swap(&exe, &new)
            }
        });
        cx.spawn(async move |this, cx| {
            while let Ok(p) = rx.recv().await {
                let _ = this.update(cx, |u, cx| {
                    if let State::Downloading(_, done) = &mut u.state {
                        *done = p;
                        changed(cx);
                    }
                });
            }
            let result = job.await;
            let _ = this.update(cx, |u, cx| match result {
                Ok(()) if u.in_call => {
                    u.state = State::Ready(r);
                    changed(cx);
                }
                Ok(()) => u.restart(r, cx),
                Err(e) => {
                    log::warn!("update to {} failed: {e}", r.version);
                    u.state = State::Failed(r, e);
                    changed(cx);
                }
            });
        })
        .detach();
    }

    fn restart(&mut self, r: Release, cx: &mut Context<Self>) {
        self.state = State::Restarting(r.clone());
        changed(cx);
        let exe = self.exe.clone();
        cx.spawn(async move |this, cx| {
            // Long enough to read "Restarting".
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let started = exe
                .ok_or_else(|| tr!("the program's path is unknown", "o caminho do programa é desconhecido").to_string())
                .and_then(|exe| update::relaunch(&exe));
            let _ = this.update(cx, |u, cx| match started {
                Ok(()) => cx.quit(),
                Err(e) => {
                    u.state = State::Failed(r, e);
                    changed(cx);
                }
            });
        })
        .detach();
    }

    fn set_in_call(&mut self, on: bool, cx: &mut Context<Self>) {
        self.in_call = on;
        if on || !matches!(self.state, State::Ready(_)) {
            return;
        }
        // Moving to another channel leaves one call for the next; restart only if none follows.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            let _ = this.update(cx, |u, cx| {
                if let (false, State::Ready(r)) = (u.in_call, u.state.clone()) {
                    u.restart(r, cx);
                }
            });
        })
        .detach();
    }
}

/// The voice call started or ended.
pub fn call_changed(in_call: bool, cx: &mut App) {
    if let Some(u) = handle(cx) {
        u.update(cx, |u, cx| u.set_in_call(in_call, cx));
    }
}

/// The first lines of the release notes as plain text, for the tooltip: no Markdown marks, and
/// without the "Full Changelog" link GitHub adds.
fn excerpt(notes: &str) -> String {
    let lines: Vec<String> = notes
        .lines()
        .map(|l| {
            let l = l.trim().trim_start_matches('#').trim().replace("**", "");
            match l.strip_prefix("* ").or_else(|| l.strip_prefix("- ")) {
                Some(item) => format!("• {item}"),
                None => l,
            }
        })
        .filter(|l| !l.is_empty() && !l.starts_with("Full Changelog"))
        .take(6)
        .collect();
    let text = lines.join("\n");
    if text.chars().count() > 400 { text.chars().take(400).collect::<String>() + "…" } else { text }
}

fn about(r: &Release) -> String {
    let head = trf!("Harmony {} is out.", "O Harmony {} já está disponível.", r.version);
    match excerpt(&r.notes) {
        notes if notes.is_empty() => head,
        notes => format!("{head}\n\n{notes}"),
    }
}

fn pill(
    id: &'static str,
    glyph: &'static str,
    text: impl Into<SharedString>,
    fg: Hsla,
    bg: Hsla,
    border: Hsla,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .h(px(28.))
        .px(px(11.))
        .rounded_full()
        .overflow_hidden()
        .bg(bg)
        .border_1()
        .border_color(border)
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(fg)
        .child(icon(glyph, 14., fg))
        .child(text.into())
}

/// The button for the top bar, while there is an update to act on.
pub fn button(cx: &App) -> Option<AnyElement> {
    let updater = handle(cx)?;
    let t = current();
    let u = updater.read(cx);
    let clickable = |p: gpui::Stateful<gpui::Div>| {
        let updater = updater.clone();
        p.cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .active(|s| s.opacity(0.75))
            .on_click(move |_, window, cx| updater.update(cx, |u, cx| u.click(window, cx)))
    };
    let el = match &u.state {
        State::Available(r) => {
            let installs = SELF_UPDATE && r.exe.is_some();
            let hint = if installs {
                tr!(
                    "Click to update. Harmony restarts by itself, after your call if you're in one.",
                    "Clique para atualizar. O Harmony reinicia sozinho, depois da sua chamada se você estiver em uma."
                )
            } else {
                tr!(
                    "This build can't replace itself; the click opens the release's page.",
                    "Esta cópia não se atualiza sozinha; clicar abre a página da versão."
                )
            };
            clickable(pill("update-available", "download", tr!("Update", "Atualizar"), t.on_accent, t.accent, t.accent))
                .tooltip(tip(format!("{}\n\n{hint}", about(r)), &t))
                .into_any_element()
        }
        State::Downloading(r, done) => div()
            .id("update-progress")
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .px(px(11.))
            .rounded_full()
            .overflow_hidden()
            .bg(t.accent_soft)
            .border_1()
            .border_color(t.accent.opacity(0.35))
            .child(div().absolute().left_0().top_0().bottom_0().w(relative(*done)).bg(t.accent.opacity(0.28)))
            .child(icon("download", 14., t.accent))
            .child(div().font_family(MONO).text_size(px(11.)).text_color(t.accent).child(trf!(
                "Updating {}%",
                "Atualizando {}%",
                (done * 100.).round() as u32
            )))
            .tooltip(tip(trf!("Downloading Harmony {}…", "Baixando o Harmony {}…", r.version), &t))
            .into_any_element(),
        State::Ready(r) => clickable(pill("update-ready", "restart", tr!("Restart", "Reiniciar"), t.on_accent, t.accent, t.accent))
            .tooltip(tip(
                trf!(
                    "Harmony {} is ready. It restarts by itself when you leave the call, or click to restart now.",
                    "O Harmony {} está pronto. Ele reinicia sozinho quando você sair da chamada, ou clique para reiniciar agora.",
                    r.version
                ),
                &t,
            ))
            .into_any_element(),
        State::Restarting(_) => {
            pill("update-restarting", "restart", tr!("Restarting…", "Reiniciando…"), t.accent, t.accent_soft, t.accent.opacity(0.35))
                .into_any_element()
        }
        State::Failed(r, e) => clickable(pill(
            "update-failed",
            "warning",
            tr!("Retry update", "Tentar atualizar de novo"),
            t.critical,
            t.tint(t.critical),
            t.critical.opacity(0.4),
        ))
        .tooltip(tip(trf!("Harmony {} could not be installed: {}", "Não foi possível instalar o Harmony {}: {}", r.version, e), &t))
        .into_any_element(),
        State::Idle | State::Current => return None,
    };
    Some(el)
}

/// "Check for updates" in Settings: the switch, where things stand, and "Check now".
pub fn settings_row(t: &Theme, cx: &mut App) -> AnyElement {
    let Some(updater) = handle(cx) else { return div().into_any_element() };
    let on = prefs(cx).check_updates;
    let u = updater.read(cx);
    let checking = u.checking;
    let release = match &u.state {
        State::Available(r) | State::Downloading(r, _) | State::Ready(r) | State::Restarting(r) | State::Failed(r, _) => Some(r.clone()),
        _ => None,
    };
    let (status, color) = match (&u.state, &u.check_error) {
        _ if checking => (tr!("Checking…", "Verificando…").to_string(), t.text2),
        (State::Idle | State::Current, Some(e)) => (trf!("Couldn't check: {}", "Não foi possível verificar: {}", e), t.caution),
        (State::Current, None) => (trf!("{} is the latest version.", "A {} é a versão mais recente.", update::current()), t.success),
        (State::Idle, None) => (trf!("This is Harmony {}.", "Este é o Harmony {}.", update::current()), t.text2),
        (State::Failed(r, e), _) => {
            (trf!("Harmony {} could not be installed: {}", "Não foi possível instalar o Harmony {}: {}", r.version, e), t.critical)
        }
        (_, _) => (
            release.as_ref().map(|r| trf!("Harmony {} is out.", "O Harmony {} já está disponível.", r.version)).unwrap_or_default(),
            t.accent,
        ),
    };
    let hover = t.layer_hover;
    let check = updater.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(14.))
        .rounded(px(radius::CARD))
        .bg(t.layer)
        .border_1()
        .border_color(t.stroke)
        .child(
            div()
                .id("updates-auto")
                .flex()
                .items_center()
                .gap(px(16.))
                .m(px(-6.))
                .p(px(6.))
                .rounded(px(radius::CONTROL))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(body(tr!("Check for updates", "Procurar atualizações"), t.text))
                        .child(caption(
                            tr!(
                                "Asks GitHub for a newer version when Harmony opens and every 6 hours. Nothing about you is sent.",
                                "Pergunta ao GitHub por uma versão nova quando o Harmony abre e a cada 6 horas. Nada sobre você é enviado."
                            ),
                            t.text2,
                        )),
                )
                .child(switch(on, t))
                .on_click(move |_, _, cx| {
                    set_prefs(cx, |p| p.check_updates = !p.check_updates);
                    if prefs(cx).check_updates {
                        updater.update(cx, |u, cx| u.check(cx));
                    }
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().flex_1().min_w(px(0.)).child(caption(status, color)))
                .when_some(release, |d, r| {
                    d.child(
                        crate::widgets::button("update-notes", tr!("What's new", "Novidades"), Kind::Subtle, t)
                            .on_click(move |_, _, cx| cx.open_url(&r.page)),
                    )
                })
                .child(
                    crate::widgets::button("update-check", tr!("Check now", "Verificar agora"), Kind::Standard, t)
                        .when(checking, |b| b.opacity(0.5))
                        .on_click(move |_, _, cx| check.update(cx, |u, cx| u.check(cx))),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_read_as_plain_text() {
        let notes = "## What's Changed\n* **Faster** joins by @predo in #12\n\n**Full Changelog**: v4.0.0...v4.0.1";
        assert_eq!(excerpt(notes), "What's Changed\n• Faster joins by @predo in #12");
    }
}
